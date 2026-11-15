//! The daemon: one per machine. It is the only thing that admits heavy work, assigns
//! cache lanes, and writes leases. It listens on a socket only its own user can open.

use crate::admit::{self, ahead_of, Class, Decision, InUse, Measured, Request, GB};
use crate::json::{self, n, n64, obj, s, Value};
use crate::lease::{self, DeleteRules, Lease, Store, Verdict};
use crate::machine::{self, now, Config, Layout};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

pub const PROTOCOL: &str = "pressroom.local/1";
const TICK: Duration = Duration::from_secs(5);
/// A job's leases die with its process; the expiry is only a last resort.
const JOB_LEASE_SECS: u64 = 7 * 24 * 3600;
const MAX_LINE: usize = 64 * 1024;
const MAX_PATHS: usize = 32;

struct Job {
    id: String,
    holder: String,
    label: String,
    class: Class,
    threads: u32,
    memory: u64,
    lane: Option<PathBuf>,
    pid: Option<u32>,
    started: u64,
    lease_ids: Vec<String>,
}

struct Waiter {
    request: Request,
    holder: String,
    label: String,
    since: u64,
    reason: String,
}

struct State {
    config: Config,
    measured: Measured,
    toolchains: Vec<(String, String)>,
    os: String,
    running: Vec<Job>,
    waiting: Vec<Waiter>,
    seq: u64,
}

impl State {
    fn in_use(&self) -> InUse {
        InUse {
            threads: self.running.iter().map(|j| j.threads).sum(),
            memory: self.running.iter().map(|j| j.memory).sum(),
        }
    }
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    layout: Layout,
    store: Store,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic in one connection must not stop the machine admitting work.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn event(&self, fields: Vec<(&str, Value)>) {
        let mut all = vec![("at", n64(now()))];
        all.extend(fields);
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.layout.events())
        {
            let _ = writeln!(f, "{}", json::write(&obj(all)));
        }
    }
}

pub fn serve(home: PathBuf) -> Result<(), String> {
    let layout = Layout { home };
    fs::create_dir_all(&layout.home)
        .map_err(|e| format!("cannot create {:?}: {e}", layout.home))?;
    let _ = fs::set_permissions(&layout.home, fs::Permissions::from_mode(0o700));
    let measured = machine::measure(&layout.home);
    let config = machine::load_config(&layout, measured.memory)?;
    for dir in layout.built_in_managed() {
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {dir:?}: {e}"))?;
    }
    let socket = layout.socket();
    if UnixStream::connect(&socket).is_ok() {
        return Err(format!(
            "a pressroom daemon is already listening on {}",
            socket.display()
        ));
    }
    let _ = fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)
        .map_err(|e| format!("cannot listen on {}: {e}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("cannot protect the socket: {e}"))?;

    let store = Store::new(layout.leases());
    store.sweep(now());
    let budget = admit::budget(&config.limits, &measured);
    eprintln!(
        "pressroom: {} offers {} of {} threads and {} of memory; listening on {}",
        config.name,
        budget.threads,
        measured.threads,
        admit::bytes(budget.memory),
        socket.display()
    );
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            config,
            measured,
            toolchains: machine::toolchains(),
            os: machine::os(),
            running: Vec::new(),
            waiting: Vec::new(),
            seq: 0,
        }),
        changed: Condvar::new(),
        layout,
        store,
    });

    let ticker = shared.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(TICK);
        // Measure outside the lock: it runs other programs.
        let mut fresh = ticker.lock().measured.clone();
        machine::remeasure(&ticker.layout.home, &mut fresh);
        ticker.store.sweep(now());
        ticker.lock().measured = fresh;
        ticker.changed.notify_all();
    });

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let shared = shared.clone();
        std::thread::spawn(move || {
            if let Err(why) = handle(&shared, stream) {
                // The other side is gone or sent rubbish; nothing more to do for it.
                let _ = why;
            }
        });
    }
    Ok(())
}

fn send(stream: &mut UnixStream, value: &Value) -> Result<(), String> {
    let mut line = json::write(value);
    line.push('\n');
    stream.write_all(line.as_bytes()).map_err(|e| e.to_string())
}

fn refuse(stream: &mut UnixStream, reason: impl Into<String>) -> Result<(), String> {
    send(
        stream,
        &obj(vec![
            ("ok", Value::Bool(false)),
            ("state", s("refused")),
            ("reason", s(reason)),
        ]),
    )
}

/// One line, bounded. `Ok(None)` at end of stream.
fn read_line(reader: &mut BufReader<UnixStream>) -> Result<Option<Value>, String> {
    let mut line = Vec::new();
    let mut limited = std::io::Read::take(&mut *reader, MAX_LINE as u64 + 1);
    let read = limited
        .read_until(b'\n', &mut line)
        .map_err(|e| e.to_string())?;
    if read == 0 {
        return Ok(None);
    }
    if line.len() > MAX_LINE {
        return Err("request too long".into());
    }
    let text = std::str::from_utf8(&line).map_err(|_| "request is not utf-8")?;
    json::parse(text.trim()).map(Some)
}

fn handle(shared: &Arc<Shared>, stream: UnixStream) -> Result<(), String> {
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut stream = stream;
    let Some(request) = read_line(&mut reader)? else {
        return Ok(());
    };
    match request.str("op") {
        Some("run") => run(shared, &mut stream, &mut reader, &request),
        Some("status") => {
            let status = status(shared);
            send(&mut stream, &status)
        }
        Some("lease") => {
            let reply = add_lease(shared, &request);
            reply_to(&mut stream, reply)
        }
        Some("release") => {
            let reply = release(shared, &request);
            reply_to(&mut stream, reply)
        }
        Some("delete-begin") => {
            let reply = delete_begin(shared, &request);
            reply_to(&mut stream, reply)
        }
        Some("delete-end") => {
            let reply = release(shared, &request);
            reply_to(&mut stream, reply)
        }
        _ => refuse(&mut stream, "unknown op"),
    }
}

fn reply_to(stream: &mut UnixStream, reply: Result<Value, String>) -> Result<(), String> {
    match reply {
        Ok(value) => send(stream, &value),
        Err(reason) => refuse(stream, reason),
    }
}

fn text(request: &Value, key: &str, default: &str, max: usize) -> Result<String, String> {
    let value = request.str(key).unwrap_or(default);
    if value.is_empty() || value.len() > max || value.chars().any(|c| c.is_control()) {
        return Err(format!("{key} must be 1 to {max} printable characters"));
    }
    Ok(value.to_string())
}

/// Paths from a request: absolute, normalised, bounded in number.
fn paths(request: &Value, key: &str) -> Result<Vec<PathBuf>, String> {
    let given = request.strings(key);
    if given.len() > MAX_PATHS {
        return Err(format!("at most {MAX_PATHS} paths"));
    }
    given
        .iter()
        .map(|p| {
            let path = Path::new(p);
            if !path.is_absolute() || p.len() > 4096 {
                return Err(format!("{key} must be absolute paths"));
            }
            let path = lease::normalise(path, Path::new("/"));
            if path.parent().is_none() {
                return Err("the root folder cannot be leased".into());
            }
            Ok(path)
        })
        .collect()
}

/// A lane name is one path part: letters, digits, dot, dash, underscore.
fn part(request: &Value, key: &str, default: &str) -> Result<String, String> {
    let value = request.str(key).unwrap_or(default);
    let ok = !value.is_empty()
        && value.len() <= 64
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    if ok {
        Ok(value.to_string())
    } else {
        Err(format!("{key} must be letters, digits, '.', '-' or '_'"))
    }
}

fn pid_of(request: &Value) -> Option<u32> {
    request
        .u64("pid")
        .and_then(|p| u32::try_from(p).ok())
        .filter(|p| *p > 0)
}

fn run(
    shared: &Arc<Shared>,
    stream: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    request: &Value,
) -> Result<(), String> {
    let parsed = (|| -> Result<_, String> {
        let holder = text(request, "holder", "unknown", 128)?;
        let label = text(request, "label", "work", 200)?;
        let class = Class::parse(request.str("class").unwrap_or("batch")).ok_or("unknown class")?;
        let threads = request.u64("threads").unwrap_or(1).clamp(1, 4096) as u32;
        let memory = (request.num("memoryGb").unwrap_or(1.0).clamp(0.0, 1.0e6) * GB as f64) as u64;
        let lane = match request.get("lane") {
            Some(lane @ Value::Obj(_)) => {
                Some((part(lane, "repo", "")?, part(lane, "toolchain", "default")?))
            }
            _ => None,
        };
        let leased = paths(request, "leases")?;
        let kind = request.str("leaseKind").unwrap_or("cache").to_string();
        if kind == "delete" || !lease::KINDS.contains(&kind.as_str()) {
            return Err("unknown lease kind".to_string());
        }
        Ok((holder, label, class, threads, memory, lane, leased, kind))
    })();
    let (holder, label, class, threads, memory, lane, leased, lease_kind) = match parsed {
        Ok(parsed) => parsed,
        Err(reason) => return refuse(stream, reason),
    };
    let pid = pid_of(request);

    let seq = {
        let mut state = shared.lock();
        state.seq += 1;
        let seq = state.seq;
        state.waiting.push(Waiter {
            request: Request {
                seq,
                class,
                threads,
                memory,
            },
            holder: holder.clone(),
            label: label.clone(),
            since: now(),
            reason: String::new(),
        });
        seq
    };
    let leave_queue = |shared: &Arc<Shared>| {
        shared.lock().waiting.retain(|w| w.request.seq != seq);
        shared.changed.notify_all();
    };

    // Wait in line. Each pass writes a line to the client, which is also how a client
    // that has gone away is noticed.
    let admitted = loop {
        let mut state = shared.lock();
        let requests: Vec<Request> = state.waiting.iter().map(|w| w.request.clone()).collect();
        let me = Request {
            seq,
            class,
            threads,
            memory,
        };
        let ahead = ahead_of(&requests, seq);
        let mut decision = admit::decide(
            &state.config.limits,
            &state.measured,
            state.in_use(),
            &me,
            ahead,
        );
        if matches!(decision, Decision::Admit { .. }) {
            // Something this job depends on is being removed right now: wait for that.
            let removing = leased.iter().find_map(|path| {
                shared
                    .store
                    .blockers(path, now(), None)
                    .into_iter()
                    .find(|l| l.kind == "delete")
            });
            if let Some(delete) = removing {
                decision = Decision::Wait {
                    reason: format!(
                        "{} is being removed by {}",
                        delete.paths.join(", "),
                        delete.holder
                    ),
                };
            }
        }
        match decision {
            Decision::Admit { threads } => {
                state.waiting.retain(|w| w.request.seq != seq);
                let job = match start(
                    shared,
                    &mut state,
                    seq,
                    &holder,
                    &label,
                    class,
                    threads,
                    memory,
                    &lane,
                    &leased,
                    &lease_kind,
                    pid,
                ) {
                    Ok(job) => job,
                    Err(reason) => {
                        drop(state);
                        shared.changed.notify_all();
                        return refuse(stream, reason);
                    }
                };
                break job;
            }
            Decision::Refuse { reason } => {
                state.waiting.retain(|w| w.request.seq != seq);
                drop(state);
                shared.changed.notify_all();
                shared.event(vec![
                    ("event", s("refused")),
                    ("holder", s(&holder)),
                    ("reason", s(&reason)),
                ]);
                return refuse(stream, reason);
            }
            Decision::Wait { reason } => {
                if let Some(w) = state.waiting.iter_mut().find(|w| w.request.seq == seq) {
                    w.reason = reason.clone();
                }
                drop(state);
                let line = obj(vec![
                    ("state", s("waiting")),
                    ("reason", s(reason)),
                    ("ahead", n64(ahead as u64)),
                ]);
                if send(stream, &line).is_err() {
                    leave_queue(shared);
                    return Ok(());
                }
                let state = shared.lock();
                let _ = shared.changed.wait_timeout(state, TICK);
            }
        }
    };
    let (job_id, granted, lane_path, lease_ids) = admitted;
    shared.changed.notify_all();
    shared.event(vec![
        ("event", s("started")),
        ("job", s(&job_id)),
        ("holder", s(&holder)),
        ("label", s(&label)),
        ("threads", n(granted)),
    ]);
    let told = send(
        stream,
        &obj(vec![
            ("state", s("admitted")),
            ("job", s(&job_id)),
            ("threads", n(granted)),
            (
                "lanePath",
                lane_path
                    .as_ref()
                    .map_or(Value::Null, |p| s(p.to_string_lossy())),
            ),
            ("leases", json::strs(&lease_ids)),
        ]),
    );

    // The job runs for as long as the client keeps the connection open. Its last line
    // says how it ended; a connection that closes without one ended we know not how.
    let mut outcome = Value::Null;
    if told.is_ok() {
        while let Ok(Some(line)) = read_line(reader) {
            if line.str("op") == Some("done") {
                outcome = line;
                break;
            }
        }
    }
    {
        let mut state = shared.lock();
        state.running.retain(|j| j.id != job_id);
    }
    for id in &lease_ids {
        shared.store.remove(id);
    }
    shared.changed.notify_all();
    let missing = outcome.strings("missing");
    let mut fields = vec![
        ("event", s("ended")),
        ("job", s(&job_id)),
        ("holder", s(&holder)),
    ];
    match outcome.get("exit") {
        Some(Value::Num(code)) => fields.push(("exit", n(*code))),
        _ => fields.push(("exit", Value::Null)),
    }
    shared.event(fields);
    if !missing.is_empty() {
        // Something leased was removed under the job: the machine's fault, not the code's.
        shared.event(vec![
            ("event", s("leased_path_removed")),
            ("job", s(&job_id)),
            ("holder", s(&holder)),
            ("paths", json::strs(&missing)),
        ]);
    }
    Ok(())
}

type Started = (String, u32, Option<PathBuf>, Vec<String>);

#[allow(clippy::too_many_arguments)]
fn start(
    shared: &Arc<Shared>,
    state: &mut State,
    seq: u64,
    holder: &str,
    label: &str,
    class: Class,
    threads: u32,
    memory: u64,
    lane: &Option<(String, String)>,
    leased: &[PathBuf],
    lease_kind: &str,
    pid: Option<u32>,
) -> Result<Started, String> {
    let at = now();
    let job_id = format!("j-{at}-{seq}");
    let mut lease_ids = Vec::new();
    let mut put = |id: String, kind: &str, paths: Vec<String>| -> Result<(), String> {
        shared.store.put(&Lease {
            id: id.clone(),
            holder: holder.to_string(),
            kind: kind.to_string(),
            mode: "in-use".into(),
            paths,
            created: at,
            expires: at + JOB_LEASE_SECS,
            pid,
        })?;
        lease_ids.push(id);
        Ok(())
    };

    // One lane per running build: the lowest-numbered lane for this repository and
    // toolchain that no running job holds and nobody is removing.
    let mut lane_path = None;
    if let Some((repo, toolchain)) = lane {
        let base = lease::normalise(
            &shared.layout.lanes().join(repo).join(toolchain),
            Path::new("/"),
        );
        let free = (0..4096u32)
            .map(|i| base.join(i.to_string()))
            .find(|candidate| {
                !state
                    .running
                    .iter()
                    .any(|j| j.lane.as_ref() == Some(candidate))
                    && shared.store.blockers(candidate, at, None).is_empty()
            });
        let path = free.ok_or("no free lane")?;
        fs::create_dir_all(&path).map_err(|e| format!("cannot create lane {path:?}: {e}"))?;
        // Normalise after creating, so the lease names the real path.
        let path = lease::normalise(&path, Path::new("/"));
        put(
            format!("{job_id}-lane"),
            "cache",
            vec![path.to_string_lossy().to_string()],
        )?;
        lane_path = Some(path);
    }
    if !leased.is_empty() {
        let all = leased
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        if let Err(why) = put(format!("{job_id}-deps"), lease_kind, all) {
            for id in &lease_ids {
                shared.store.remove(id);
            }
            return Err(why);
        }
    }
    state.running.push(Job {
        id: job_id.clone(),
        holder: holder.to_string(),
        label: label.to_string(),
        class,
        threads,
        memory,
        lane: lane_path.clone(),
        pid,
        started: at,
        lease_ids: lease_ids.clone(),
    });
    Ok((job_id, threads, lane_path, lease_ids))
}

fn add_lease(shared: &Arc<Shared>, request: &Value) -> Result<Value, String> {
    let holder = text(request, "holder", "unknown", 128)?;
    let kind = request.str("kind").unwrap_or("cache").to_string();
    if kind == "delete" || !lease::KINDS.contains(&kind.as_str()) {
        return Err(format!("kind is one of: {}", lease::KINDS[..6].join(", ")));
    }
    let mode = match request.str("mode").unwrap_or("in-use") {
        m @ ("in-use" | "warm") => m.to_string(),
        _ => return Err("mode is in-use or warm".into()),
    };
    let leased = paths(request, "paths")?;
    if leased.is_empty() {
        return Err("a lease names at least one path".into());
    }
    let ttl = request
        .u64("ttlSecs")
        .unwrap_or(24 * 3600)
        .clamp(1, lease::MAX_TTL_SECS);
    // Under the lock, so a lease and a removal of the same path cannot both succeed.
    let mut state = shared.lock();
    let at = now();
    for path in &leased {
        if let Some(delete) = shared
            .store
            .blockers(path, at, None)
            .into_iter()
            .find(|l| l.kind == "delete")
        {
            return Err(format!(
                "{} is being removed by {}",
                path.display(),
                delete.holder
            ));
        }
    }
    state.seq += 1;
    let id = format!("l-{at}-{}", state.seq);
    shared.store.put(&Lease {
        id: id.clone(),
        holder,
        kind,
        mode,
        paths: leased
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect(),
        created: at,
        expires: at + ttl,
        pid: pid_of(request),
    })?;
    Ok(obj(vec![
        ("ok", Value::Bool(true)),
        ("id", s(id)),
        ("expires", n64(at + ttl)),
    ]))
}

/// End a lease taken with `lease` or `delete-begin`. A running job's leases end with
/// the job and cannot be released from outside it.
fn release(shared: &Arc<Shared>, request: &Value) -> Result<Value, String> {
    let id = request
        .str("id")
        .filter(|id| lease::valid_id(id))
        .ok_or("bad lease id")?;
    let state = shared.lock();
    if state
        .running
        .iter()
        .any(|j| j.lease_ids.iter().any(|l| l == id))
    {
        return Err("that lease belongs to a running job".into());
    }
    let existed = shared.store.get(id).is_some();
    shared.store.remove(id);
    drop(state);
    shared.changed.notify_all();
    Ok(obj(vec![
        ("ok", Value::Bool(true)),
        ("existed", Value::Bool(existed)),
    ]))
}

fn delete_begin(shared: &Arc<Shared>, request: &Value) -> Result<Value, String> {
    let holder = text(request, "holder", "unknown", 128)?;
    let target = paths(request, "paths")?;
    let [path] = target.as_slice() else {
        return Err("name one path".into());
    };
    let ttl = request.u64("ttlSecs").unwrap_or(600).clamp(1, 24 * 3600);
    // Under the lock: admission assigns lanes and writes leases under the same lock.
    let mut state = shared.lock();
    let at = now();
    let rules = DeleteRules {
        managed: &state.config.managed,
        grace_secs: state.config.grace_secs,
    };
    if let Verdict::No(reason) = lease::may_delete(&shared.store, &rules, path, at, None) {
        return Err(reason);
    }
    state.seq += 1;
    let id = format!("d-{at}-{}", state.seq);
    shared.store.put(&Lease {
        id: id.clone(),
        holder,
        kind: "delete".into(),
        mode: "in-use".into(),
        paths: vec![path.to_string_lossy().to_string()],
        created: at,
        expires: at + ttl,
        pid: pid_of(request),
    })?;
    Ok(obj(vec![
        ("ok", Value::Bool(true)),
        ("id", s(id)),
        ("expires", n64(at + ttl)),
    ]))
}

fn status(shared: &Arc<Shared>) -> Value {
    let state = shared.lock();
    let at = now();
    let budget = admit::budget(&state.config.limits, &state.measured);
    let in_use = state.in_use();
    let running = state
        .running
        .iter()
        .map(|j| {
            obj(vec![
                ("job", s(&j.id)),
                ("holder", s(&j.holder)),
                ("label", s(&j.label)),
                ("class", s(j.class.name())),
                ("threads", n(j.threads)),
                ("memoryGb", n(j.memory as f64 / GB as f64)),
                (
                    "lane",
                    j.lane
                        .as_ref()
                        .map_or(Value::Null, |p| s(p.to_string_lossy())),
                ),
                ("pid", j.pid.map_or(Value::Null, n)),
                ("forSecs", n64(at.saturating_sub(j.started))),
            ])
        })
        .collect();
    let mut order: Vec<&Waiter> = state.waiting.iter().collect();
    order.sort_by_key(|w| (w.request.class, w.request.seq));
    let waiting = order
        .iter()
        .map(|w| {
            obj(vec![
                ("holder", s(&w.holder)),
                ("label", s(&w.label)),
                ("class", s(w.request.class.name())),
                ("threads", n(w.request.threads)),
                ("reason", s(&w.reason)),
                ("forSecs", n64(at.saturating_sub(w.since))),
            ])
        })
        .collect();
    let (leases, unreadable) = shared.store.all();
    let leases = leases
        .iter()
        .filter(|l| l.live(at, &lease::pid_alive))
        .map(Lease::to_json)
        .collect();
    obj(vec![
        ("protocol", s(PROTOCOL)),
        ("name", s(&state.config.name)),
        ("os", s(&state.os)),
        ("arch", s(std::env::consts::ARCH)),
        (
            "kind",
            s(match state.config.limits.kind {
                admit::Kind::Workstation => "workstation",
                admit::Kind::Worker => "worker",
            }),
        ),
        ("threads", n(state.measured.threads)),
        ("memoryGb", n(state.measured.memory as f64 / GB as f64)),
        ("offeredThreads", n(budget.threads)),
        ("offeredMemoryGb", n(budget.memory as f64 / GB as f64)),
        ("personPresent", Value::Bool(budget.person_present)),
        ("usedThreads", n(in_use.threads)),
        ("usedMemoryGb", n(in_use.memory as f64 / GB as f64)),
        ("load1", n(state.measured.load1)),
        (
            "outsideLoad",
            n(admit::outside_load(&state.measured, in_use)),
        ),
        ("diskFreeGb", n(state.measured.disk_free as f64 / GB as f64)),
        (
            "diskFloorGb",
            n(state.config.limits.disk_floor as f64 / GB as f64),
        ),
        (
            "toolchains",
            Value::Arr(
                state
                    .toolchains
                    .iter()
                    .map(|(name, version)| obj(vec![("name", s(name)), ("version", s(version))]))
                    .collect(),
            ),
        ),
        (
            "managed",
            Value::Arr(
                state
                    .config
                    .managed
                    .iter()
                    .map(|p| s(p.to_string_lossy()))
                    .collect(),
            ),
        ),
        ("running", Value::Arr(running)),
        ("waiting", Value::Arr(waiting)),
        ("leases", Value::Arr(leases)),
        ("unreadableLeases", json::strs(&unreadable)),
    ])
}
