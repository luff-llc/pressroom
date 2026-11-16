//! The daemon: one per machine. It is the only thing that admits heavy work, assigns
//! cache lanes, and writes leases. It listens on a socket only its own user can open.

use crate::admit::{self, ahead_of, Class, Decision, InUse, Measured, Request, Said, GB};
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
/// The report to Galley (contracts/report-v1.md).
pub const REPORT: &str = "pressroom.report/1";
const REPORT_EVERY: Duration = Duration::from_secs(20);
const SENDER_LIMIT: Duration = Duration::from_secs(15);
/// The most a report may weigh as JSON; Galley's sender refuses more.
const REPORT_BYTES: usize = 256 * 1024;
const TICK: Duration = Duration::from_secs(5);
/// A job's leases die with its process; the expiry is only a last resort.
const JOB_LEASE_SECS: u64 = 7 * 24 * 3600;
const MAX_LINE: usize = 64 * 1024;
const FIRST_LINE: Duration = Duration::from_secs(10);
const WRITE: Duration = Duration::from_secs(2);
const MAX_PATHS: usize = 32;
/// The longest a person's word stands: a week.
const MAX_SAID_SECS: u64 = 7 * 24 * 3600;

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
    /// It holds a device slot (rule 27).
    device: bool,
    /// It asked for one; inside a job that holds one it shares that job's (rule 26).
    asked_device: bool,
    /// The job it runs inside (rule 26); charged nothing of its own.
    within: Option<String>,
    /// What `run` last measured it to use: threads and bytes (rule 24).
    used: Option<(f64, u64)>,
    held: bool,
    held_secs: f64,
    /// How `run` holds it: `scope`, `stop` or `measure`; `within` for rule 26.
    means: Option<String>,
    /// Its process groups and loose processes, to continue if its `run` dies (rule 28).
    pgid: Option<i32>,
    groups: Vec<i32>,
    pids: Vec<i32>,
}

impl Job {
    /// What it is charged for admission (rule 24).
    fn charge(&self) -> (u32, u64) {
        if self.within.is_some() {
            return (0, 0);
        }
        admit::charge(self.threads, self.memory, self.used)
    }
}

struct Waiter {
    request: Request,
    /// The job it waits inside (rule 26). While that job runs, it holds up no one.
    within: Option<String>,
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
    /// What the person last said (rules 19 to 21). Not kept across a restart.
    person: Option<PersonSays>,
}

#[derive(Clone)]
struct PersonSays {
    said: Said,
    since: u64,
    until: u64,
    holder: String,
}

impl State {
    /// What the person said, if it still stands at `at`.
    fn said(&self, at: u64) -> Said {
        match &self.person {
            Some(p)
                if admit::still_stands(p.said, p.since, p.until, at, self.measured.idle_secs) =>
            {
                p.said
            }
            _ => Said::Nothing,
        }
    }

    fn in_use(&self) -> InUse {
        InUse {
            threads: self.running.iter().map(|j| j.charge().0).sum(),
            memory: self.running.iter().map(|j| j.charge().1).sum(),
            devices: self.running.iter().filter(|j| j.device).count() as u32,
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

/// Why the daemon did not run: another daemon already owns this home, or something else.
pub enum ServeError {
    AlreadyRunning(String),
    Failed(String),
}

impl From<String> for ServeError {
    fn from(why: String) -> Self {
        ServeError::Failed(why)
    }
}

pub fn serve(home: PathBuf) -> Result<(), ServeError> {
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
    claim(&layout).map_err(ServeError::AlreadyRunning)?;
    let _ = fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)
        .map_err(|e| ServeError::Failed(format!("cannot listen on {}: {e}", socket.display())))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("cannot protect the socket: {e}"))?;

    let store = Store::new(layout.leases());
    store.sweep(now());
    let budget = admit::budget(&config.limits, &measured, Said::Nothing);
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
            person: None,
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
        {
            // A job's leases last as long as the job, however long that is.
            let mut state = ticker.lock();
            state.measured = fresh;
            let at = now();
            if state.person.is_some() && state.said(at) == Said::Nothing {
                let ended = state.person.take();
                if let Some(p) = ended {
                    ticker.event(vec![
                        ("event", s("person_says_ended")),
                        ("mode", s(p.said.name())),
                        ("holder", s(&p.holder)),
                    ]);
                }
            }
            for id in state.running.iter().flat_map(|j| &j.lease_ids) {
                if let Some(mut lease) = ticker.store.get(id) {
                    if lease.expires.saturating_sub(at) < JOB_LEASE_SECS / 2 {
                        lease.expires = at + JOB_LEASE_SECS;
                        let _ = ticker.store.put(&lease);
                    }
                }
            }
        }
        ticker.changed.notify_all();
    });

    let galley = shared.lock().config.galley.clone();
    if let Some(galley) = galley {
        let reporter = shared.clone();
        std::thread::spawn(move || report_loop(&reporter, &galley));
    }

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

/// One daemon per home. The first to create `daemon.pid` is it; a file left by a daemon
/// that is gone is replaced.
fn claim(layout: &Layout) -> Result<(), String> {
    let file = layout.home.join("daemon.pid");
    for _ in 0..3 {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file)
        {
            Ok(mut f) => {
                return writeln!(f, "{}", std::process::id())
                    .map_err(|e| format!("cannot write {}: {e}", file.display()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let other = fs::read_to_string(&file)
                    .ok()
                    .and_then(|t| t.trim().parse::<u32>().ok());
                let young = fs::metadata(&file)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_none_or(|age| age < Duration::from_secs(5));
                let answering = UnixStream::connect(layout.socket()).is_ok();
                if let Some(pid) = other.filter(|pid| lease::pid_alive(*pid)) {
                    // Alive and answering, or alive and only just started.
                    if answering || young {
                        return Err(format!(
                            "a pressroom daemon (process {pid}) is already running on {}",
                            layout.home.display()
                        ));
                    }
                }
                let _ = fs::remove_file(&file);
            }
            Err(e) => return Err(format!("cannot write {}: {e}", file.display())),
        }
    }
    Err("another pressroom daemon is starting on this home".into())
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
    // A client that connects and says nothing, or stops reading, is dropped: it must
    // not hold a thread, or its place in the queue, for ever.
    let _ = stream.set_read_timeout(Some(FIRST_LINE));
    let _ = stream.set_write_timeout(Some(WRITE));
    let request = match read_line(&mut reader) {
        Ok(Some(request)) => request,
        Ok(None) => return Ok(()),
        Err(why) => return refuse(&mut stream, format!("bad request: {why}")),
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
        Some("person-says") => {
            let reply = person_says(shared, &request);
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
    // A field of the wrong type is refused, never replaced by the default.
    let value = match request.get(key) {
        None | Some(Value::Null) => default,
        Some(Value::Str(value)) => value.as_str(),
        _ => return Err(format!("{key} must be text")),
    };
    if value.is_empty() || value.len() > max || value.chars().any(|c| c.is_control()) {
        return Err(format!("{key} must be 1 to {max} printable characters"));
    }
    Ok(value.to_string())
}

/// Paths from a request: absolute, normalised, bounded in number.
fn paths(request: &Value, key: &str) -> Result<Vec<PathBuf>, String> {
    let given = match request.get(key) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Arr(items)) if items.iter().all(|i| matches!(i, Value::Str(_))) => {
            request.strings(key)
        }
        _ => return Err(format!("{key} must be a list of paths")),
    };
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
        // A field of the wrong type is refused, never quietly replaced by a default.
        let class = match request.get("class") {
            None | Some(Value::Null) => Class::Batch,
            Some(Value::Str(name)) => {
                Class::parse(name).ok_or("class is waiting, gate or batch")?
            }
            _ => return Err("class is waiting, gate or batch".into()),
        };
        let threads = match request.get("threads") {
            None | Some(Value::Null) => 1,
            Some(Value::Num(t)) if *t >= 1.0 && t.fract() == 0.0 => t.min(4096.0) as u32,
            _ => return Err("threads must be a whole number, 1 or more".into()),
        };
        let memory = match request.get("memoryGb") {
            None | Some(Value::Null) => GB,
            Some(Value::Num(gb)) if *gb >= 0.0 => (gb.min(1.0e6) * GB as f64) as u64,
            _ => return Err("memoryGb must be a number, 0 or more".into()),
        };
        let lane = match request.get("lane") {
            None | Some(Value::Null) => None,
            Some(lane @ Value::Obj(_)) => {
                Some((part(lane, "repo", "")?, part(lane, "toolchain", "default")?))
            }
            _ => return Err("lane must name a repo".into()),
        };
        // A job's leases live while its process does, so the process must exist.
        if let Some(pid) = request.get("pid") {
            let alive =
                matches!(pid, Value::Num(_)) && pid_of(request).is_some_and(lease::pid_alive);
            if !alive {
                return Err("pid must be a running process".into());
            }
        }
        let leased = paths(request, "leases")?;
        let kind = request.str("leaseKind").unwrap_or("cache").to_string();
        if kind == "delete" || !lease::KINDS.contains(&kind.as_str()) {
            return Err("unknown lease kind".to_string());
        }
        let device = match request.get("device") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            _ => return Err("device must be true or false".into()),
        };
        let within = match request.get("within") {
            None | Some(Value::Null) => None,
            Some(Value::Str(job)) if !job.is_empty() && job.len() <= 64 => Some(job.clone()),
            _ => return Err("within must name a job".into()),
        };
        Ok((holder, label, class, threads, memory, lane, leased, kind, device, within))
    })();
    let deny = |stream: &mut UnixStream, holder: &str, reason: String| {
        shared.event(vec![
            ("event", s("refused")),
            ("holder", s(holder)),
            ("reason", s(&reason)),
        ]);
        refuse(stream, reason)
    };
    let (holder, label, class, threads, memory, lane, leased, lease_kind, device, within) = match parsed {
        Ok(parsed) => parsed,
        Err(reason) => return deny(stream, request.str("holder").unwrap_or("unknown"), reason),
    };
    let pid = pid_of(request);
    let lane_base = lane.as_ref().map(|(repo, toolchain)| {
        lease::normalise(
            &shared.layout.lanes().join(repo).join(toolchain),
            Path::new("/"),
        )
    });

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
                device,
            },
            within: within.clone(),
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
        let in_use = state.in_use();
        let limits = state.config.limits.clone();
        // Work inside a running job, and work that waits only for a device slot, holds up
        // no one behind it (rules 26 and 27).
        let requests: Vec<Request> = state
            .waiting
            .iter()
            .filter(|w| {
                let inside = w
                    .within
                    .as_ref()
                    .is_some_and(|id| state.running.iter().any(|j| &j.id == id));
                w.request.seq == seq
                    || !(inside || admit::waits_for_device(&limits, in_use, &w.request))
            })
            .map(|w| w.request.clone())
            .collect();
        // The job this runs inside, if it still runs (rule 26).
        let outer = within
            .as_ref()
            .and_then(|id| state.running.iter().find(|j| &j.id == id))
            .map(|j| (j.id.clone(), j.threads, j.device));
        let mut me = Request {
            seq,
            class,
            threads,
            memory,
            device,
        };
        if let Some((_, _, true)) = outer {
            // It shares its outer job's device slot.
            me.device = false;
        }
        let ahead = ahead_of(&requests, seq);
        let mut decision = match &outer {
            Some((_, outer_threads, _)) => {
                if admit::waits_for_device(&limits, in_use, &me) {
                    admit::decide(&limits, &state.measured, Said::Nothing, in_use, &me, 0)
                } else {
                    Decision::Admit {
                        threads: threads.min(*outer_threads).max(1),
                    }
                }
            }
            None => admit::decide(
                &limits,
                &state.measured,
                state.said(now()),
                in_use,
                &me,
                ahead,
            ),
        };
        if let Decision::Wait { reason } = &mut decision {
            if admit::waits_for_device(&limits, in_use, &me) {
                if let Some(j) = state.running.iter().find(|j| j.device) {
                    reason.push_str(&format!(": {} ({})", j.id, j.label));
                }
            }
        }
        // Read the leases once per pass; every check below uses this one reading.
        let live = if matches!(decision, Decision::Admit { .. }) {
            shared.store.live(now())
        } else {
            Vec::new()
        };
        if matches!(decision, Decision::Admit { .. }) {
            // Something this job depends on, or the folder its lanes live in, is being
            // removed right now: wait for that.
            let removing = live.iter().filter(|l| l.kind == "delete").find(|l| {
                leased.iter().any(|path| l.covers(path))
                    || lane_base.as_ref().is_some_and(|base| {
                        l.paths.iter().any(|p| lease::within(base, Path::new(p)))
                    })
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
                    &live,
                    seq,
                    &holder,
                    &label,
                    class,
                    threads,
                    memory,
                    &lane_base,
                    &leased,
                    &lease_kind,
                    pid,
                    (me.device, device),
                    outer.as_ref().map(|o| o.0.clone()),
                ) {
                    Ok(job) => job,
                    Err(reason) => {
                        drop(state);
                        shared.changed.notify_all();
                        return deny(stream, &holder, reason);
                    }
                };
                break job;
            }
            Decision::Refuse { reason } => {
                state.waiting.retain(|w| w.request.seq != seq);
                drop(state);
                shared.changed.notify_all();
                return deny(stream, &holder, reason);
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
    let (job_id, granted, lane_path, lease_ids, within) = admitted;
    shared.changed.notify_all();
    let (cap, cap_memory) = {
        let state = shared.lock();
        let offer =
            admit::budget(&state.config.limits, &state.measured, Said::Nothing).memory;
        let cap_memory = if state.config.cap_memory_at_offer {
            offer
        } else {
            memory
        };
        (state.config.cap.name(), cap_memory)
    };
    shared.event(vec![
        ("event", s("started")),
        ("job", s(&job_id)),
        ("holder", s(&holder)),
        ("label", s(&label)),
        ("threads", n(granted)),
        ("device", Value::Bool(device)),
        ("within", within.as_ref().map_or(Value::Null, s)),
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
            ("cap", s(cap)),
            ("capMemoryGb", n(cap_memory as f64 / GB as f64)),
            ("within", within.as_ref().map_or(Value::Null, s)),
        ]),
    );

    // The job runs for as long as the client keeps the connection open. While it runs
    // the client says what it uses (rule 24) and what it was told (rule 25). Its last
    // line says how it ended; a connection that closes without one ended we know not how.
    let mut outcome = Value::Null;
    let _ = stream.set_read_timeout(None);
    if told.is_ok() {
        while let Ok(Some(line)) = read_line(reader) {
            match line.str("op") {
                Some("done") => {
                    outcome = line;
                    break;
                }
                Some("use") => {
                    let mut state = shared.lock();
                    if let Some(job) = state.running.iter_mut().find(|j| j.id == job_id) {
                        took_use(job, &line);
                    }
                }
                Some("event") => {
                    let event = line.str("event").unwrap_or("");
                    if ["held", "memory_ended", "not_held", "on_its_own"].contains(&event) {
                        shared.event(vec![
                            ("event", s(event)),
                            ("job", s(&job_id)),
                            ("holder", s(&holder)),
                            ("text", clip(line.str("text").unwrap_or(""), 300)),
                        ]);
                    }
                }
                _ => {}
            }
        }
    }
    let ended = {
        let mut state = shared.lock();
        let at = state.running.iter().position(|j| j.id == job_id);
        let ended = at.map(|i| state.running.remove(i));
        // Runs inside this job become jobs of their own, charged and holding a device slot
        // if they asked for one, under the same lock, so nothing is admitted in their
        // place meanwhile (rule 26). Their `run`s see this job's `run` gone and hold them.
        for inner in state
            .running
            .iter_mut()
            .filter(|j| j.within.as_deref() == Some(job_id.as_str()))
        {
            inner.within = None;
            inner.device = inner.asked_device;
        }
        ended
    };
    if outcome == Value::Null {
        if let Some(job) = &ended {
            continue_job(job);
        }
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
    let held_secs = outcome
        .num("heldSecs")
        .or(ended.as_ref().map(|j| j.held_secs))
        .unwrap_or(0.0);
    fields.push(("heldSecs", n64(held_secs.max(0.0).round() as u64)));
    let means = outcome
        .str("means")
        .filter(|m| ["scope", "stop", "measure"].contains(m))
        .map(str::to_string)
        .or(ended.as_ref().and_then(|j| j.means.clone()));
    fields.push(("means", means.map_or(Value::Null, s)));
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

type Started = (String, u32, Option<PathBuf>, Vec<String>, Option<String>);

/// Take what `run` measured its job to use (rule 24). It is the client's word, as its
/// holder is; anything of the wrong type is left as it was.
fn took_use(job: &mut Job, line: &Value) {
    if let (Some(threads), Some(gb)) = (line.num("threads"), line.num("memoryGb")) {
        if threads.is_finite() && gb.is_finite() && threads >= 0.0 && gb >= 0.0 {
            job.used = Some((threads.min(4096.0), (gb.min(1.0e6) * GB as f64) as u64));
        }
    }
    if let Some(Value::Bool(held)) = line.get("held") {
        job.held = *held;
    }
    if let Some(secs) = line.num("heldSecs").filter(|s| s.is_finite() && *s >= 0.0) {
        job.held_secs = secs;
    }
    if let Some(means) = line.str("means") {
        if ["scope", "stop", "measure"].contains(&means) {
            job.means = Some(means.to_string());
        }
    }
    if let Some(pgid) = line.u64("pgid").and_then(|p| i32::try_from(p).ok()) {
        job.pgid = Some(pgid);
    }
    let ids = |key: &str| -> Option<Vec<i32>> {
        let Some(Value::Arr(items)) = line.get(key) else {
            return None;
        };
        Some(
            items
                .iter()
                .filter_map(|v| match v {
                    Value::Num(p) if p.fract() == 0.0 && *p > 1.0 && *p < i32::MAX as f64 => {
                        Some(*p as i32)
                    }
                    _ => None,
                })
                .take(crate::cap::MAX_REPORTED)
                .collect(),
        )
    };
    if let Some(groups) = ids("groups") {
        job.groups = groups;
    }
    if let Some(pids) = ids("pids") {
        job.pids = pids;
    }
}

/// Its `run` is gone without saying how the job ended: continue whatever it may have left
/// stopped (rule 28). Never the daemon itself or its own group.
fn continue_job(job: &Job) {
    if job.means.as_deref() != Some("stop") {
        return;
    }
    let me = std::process::id() as i32;
    let my_group = crate::cap::sys::own_group();
    for &g in job.pgid.iter().chain(&job.groups) {
        if g > 1 && g != me && g != my_group {
            crate::cap::sys::kill(-g, crate::cap::sys::SIGCONT);
        }
    }
    for &pid in job.pids.iter().filter(|&&p| p > 1 && p != me) {
        crate::cap::sys::kill(pid, crate::cap::sys::SIGCONT);
    }
}

#[allow(clippy::too_many_arguments)]
fn start(
    shared: &Arc<Shared>,
    state: &mut State,
    live: &[Lease],
    seq: u64,
    holder: &str,
    label: &str,
    class: Class,
    threads: u32,
    memory: u64,
    lane_base: &Option<PathBuf>,
    leased: &[PathBuf],
    lease_kind: &str,
    pid: Option<u32>,
    (device, asked_device): (bool, bool),
    within: Option<String>,
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
    if let Some(base) = lane_base {
        let free = (0..4096u32)
            .map(|i| base.join(i.to_string()))
            .find(|candidate| {
                !state
                    .running
                    .iter()
                    .any(|j| j.lane.as_ref() == Some(candidate))
                    && !live.iter().any(|l| l.covers(candidate))
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
        device,
        asked_device,
        means: within.as_ref().map(|_| "within".to_string()),
        within: within.clone(),
        used: None,
        held: false,
        held_secs: 0.0,
        pgid: None,
        groups: Vec::new(),
        pids: Vec::new(),
    });
    Ok((job_id, threads, lane_path, lease_ids, within))
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

/// The person says "I need the whole machine" (`hold`), "take it all, I am away" (`give`),
/// or "go back to what you measure" (`auto`). Rules 19 to 21.
fn person_says(shared: &Arc<Shared>, request: &Value) -> Result<Value, String> {
    let holder = text(request, "holder", "unknown", 128)?;
    let said = match request.get("mode") {
        Some(Value::Str(mode)) => Said::parse(mode),
        _ => None,
    }
    .ok_or("mode is hold, give or auto")?;
    let secs = match (said, request.get("forSecs")) {
        (Said::Nothing, None | Some(Value::Null)) => 0,
        (Said::Nothing, _) => return Err("auto takes no forSecs".into()),
        (_, Some(Value::Num(secs)))
            if secs.fract() == 0.0 && *secs >= 60.0 && *secs <= MAX_SAID_SECS as f64 =>
        {
            *secs as u64
        }
        _ => {
            return Err(format!(
                "forSecs must be a whole number from 60 to {MAX_SAID_SECS}"
            ))
        }
    };
    let at = now();
    let until = at + secs;
    let lapsed = {
        let mut state = shared.lock();
        // What was said before may have ended since the last tick: record that first.
        let lapsed = match &state.person {
            Some(p) if state.said(at) == Said::Nothing => Some(p.clone()),
            _ => None,
        };
        state.person = (said != Said::Nothing).then(|| PersonSays {
            said,
            since: at,
            until,
            holder: holder.clone(),
        });
        lapsed
    };
    shared.changed.notify_all();
    if let Some(p) = lapsed {
        shared.event(vec![
            ("event", s("person_says_ended")),
            ("mode", s(p.said.name())),
            ("holder", s(&p.holder)),
        ]);
    }
    shared.event(vec![
        ("event", s("person_says")),
        ("mode", s(said.name())),
        ("holder", s(&holder)),
        (
            "until",
            if said == Said::Nothing {
                Value::Null
            } else {
                n64(until)
            },
        ),
    ]);
    Ok(obj(vec![
        ("ok", Value::Bool(true)),
        ("mode", s(said.name())),
        (
            "until",
            if said == Said::Nothing {
                Value::Null
            } else {
                n64(until)
            },
        ),
    ]))
}

fn status(shared: &Arc<Shared>) -> Value {
    let state = shared.lock();
    let at = now();
    let said = state.said(at);
    let budget = admit::budget(&state.config.limits, &state.measured, said);
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
                (
                    "useThreads",
                    j.used.map_or(Value::Null, |u| n((u.0 * 10.0).round() / 10.0)),
                ),
                (
                    "useMemoryGb",
                    j.used
                        .map_or(Value::Null, |u| n((u.1 as f64 / GB as f64 * 10.0).round() / 10.0)),
                ),
                ("over", Value::Bool(admit::over(j.threads, j.memory, j.used))),
                ("held", Value::Bool(j.held)),
                ("heldSecs", n64(j.held_secs.round() as u64)),
                ("means", j.means.as_ref().map_or(Value::Null, s)),
                ("device", Value::Bool(j.device)),
                ("within", j.within.as_ref().map_or(Value::Null, s)),
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
        (
            "personSays",
            match &state.person {
                Some(p) if said != Said::Nothing => obj(vec![
                    ("mode", s(p.said.name())),
                    ("holder", s(&p.holder)),
                    ("since", n64(p.since)),
                    ("until", n64(p.until)),
                ]),
                _ => Value::Null,
            },
        ),
        ("usedThreads", n(in_use.threads)),
        ("usedMemoryGb", n(in_use.memory as f64 / GB as f64)),
        ("load1", n(state.measured.load1)),
        (
            "busyThreads",
            state
                .measured
                .busy
                .map_or(Value::Null, |b| n((b * 10.0).round() / 10.0)),
        ),
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
        ("cap", s(state.config.cap.name())),
        ("deviceSlots", n(state.config.limits.device_slots)),
        ("devicesInUse", n(in_use.devices)),
        ("running", Value::Arr(running)),
        ("waiting", Value::Arr(waiting)),
        ("leases", Value::Arr(leases)),
        ("unreadableLeases", json::strs(&unreadable)),
    ])
}

/// Text cut to at most `max` bytes at a character boundary, never empty or blank, with
/// control characters replaced, so Galley's checks never refuse a whole report for one label.
fn clip(text: &str, max: usize) -> Value {
    let clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut end = clean.len().min(max);
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    let cut = clean[..end].trim();
    s(if cut.is_empty() { "-" } else { cut })
}

/// The report (contracts/report-v1.md): what `status` measures, in whole numbers, without
/// process ids, lane folders or managed folders.
fn report(shared: &Arc<Shared>, max_leases: usize) -> Value {
    let state = shared.lock();
    let at = now();
    let budget = admit::budget(&state.config.limits, &state.measured, state.said(at));
    let in_use = state.in_use();
    let running = state
        .running
        .iter()
        .take(64)
        .map(|j| {
            obj(vec![
                ("job", clip(&j.id, 128)),
                ("holder", clip(&j.holder, 256)),
                ("label", clip(&j.label, 256)),
                ("class", s(j.class.name())),
                ("threads", n(j.threads)),
                ("memoryBytes", n64(j.memory)),
                ("forSecs", n64(at.saturating_sub(j.started))),
            ])
        })
        .collect();
    let mut order: Vec<&Waiter> = state.waiting.iter().collect();
    order.sort_by_key(|w| (w.request.class, w.request.seq));
    let waiting = order
        .iter()
        .take(64)
        .map(|w| {
            obj(vec![
                ("holder", clip(&w.holder, 256)),
                ("label", clip(&w.label, 256)),
                ("class", s(w.request.class.name())),
                ("threads", n(w.request.threads)),
                ("reason", clip(&w.reason, 256)),
                ("forSecs", n64(at.saturating_sub(w.since))),
            ])
        })
        .collect();
    let (leases, unreadable) = shared.store.all();
    let leases = leases
        .iter()
        .filter(|l| l.live(at, &lease::pid_alive))
        .take(max_leases)
        .map(|l| {
            obj(vec![
                ("id", s(&l.id)),
                ("holder", clip(&l.holder, 256)),
                ("kind", s(&l.kind)),
                ("mode", s(&l.mode)),
                (
                    "paths",
                    Value::Arr(
                        l.paths
                            .iter()
                            // A path cut short would name something else: leave it out.
                            .filter(|p| p.len() <= 1024)
                            .take(32)
                            .map(s)
                            .collect(),
                    ),
                ),
                ("expires", n64(l.expires)),
            ])
        })
        .collect();
    obj(vec![
        ("v", s(REPORT)),
        ("measuredAt", n64(at)),
        ("name", clip(&state.config.name, 128)),
        (
            "kind",
            s(match state.config.limits.kind {
                admit::Kind::Workstation => "workstation",
                admit::Kind::Worker => "worker",
            }),
        ),
        ("os", clip(&state.os, 512)),
        ("arch", s(std::env::consts::ARCH)),
        ("threads", n(state.measured.threads)),
        ("memoryBytes", n64(state.measured.memory)),
        (
            "offeredThreads",
            n(budget.threads.min(state.measured.threads)),
        ),
        ("offeredMemoryBytes", n64(budget.memory)),
        ("personPresent", Value::Bool(budget.person_present)),
        ("usedThreads", n(in_use.threads)),
        ("usedMemoryBytes", n64(in_use.memory)),
        (
            "loadHundredths",
            n64((state.measured.load1.max(0.0) * 100.0).round() as u64),
        ),
        (
            "outsideThreads",
            n(admit::outside_load(&state.measured, in_use)),
        ),
        ("diskFreeBytes", n64(state.measured.disk_free)),
        ("diskFloorBytes", n64(state.config.limits.disk_floor)),
        (
            "toolchains",
            Value::Arr(
                state
                    .toolchains
                    .iter()
                    .take(16)
                    .map(|(name, version)| {
                        obj(vec![("name", s(name)), ("version", clip(version, 512))])
                    })
                    .collect(),
            ),
        ),
        ("running", Value::Arr(running)),
        ("waiting", Value::Arr(waiting)),
        ("leases", Value::Arr(leases)),
        ("unreadableLeases", n64(unreadable.len() as u64)),
    ])
}

/// The report as one line, within `REPORT_BYTES`: leases are left out from the end until it fits.
fn report_line(shared: &Arc<Shared>) -> String {
    let mut max = 256;
    loop {
        let line = json::write(&report(shared, max));
        if line.len() <= REPORT_BYTES || max == 0 {
            return line;
        }
        max /= 2;
    }
}

/// Send one report at once and then one on each tick of a fixed timetable, for as long as the
/// daemon runs. A send that overruns a tick waits for the next one; nothing drifts.
fn report_loop(shared: &Arc<Shared>, galley: &machine::Galley) {
    let every = report_every();
    let mut failing = false;
    let mut due = std::time::Instant::now();
    loop {
        let line = report_line(shared);
        match send_report(galley, &line) {
            Ok(()) if failing => {
                failing = false;
                shared.event(vec![("event", s("report_sent"))]);
            }
            Ok(()) => {}
            Err((exit, why)) if !failing => {
                failing = true;
                shared.event(vec![
                    ("event", s("report_failed")),
                    ("exit", exit.map_or(Value::Null, n)),
                    ("why", clip(&why, 200)),
                ]);
            }
            Err(_) => {}
        }
        let now = std::time::Instant::now();
        while due <= now {
            due += every;
        }
        std::thread::sleep(due - now);
    }
}

fn report_every() -> Duration {
    // Debug builds only, for tests: a shorter interval.
    #[cfg(debug_assertions)]
    {
        if let Some(secs) = std::env::var("PRESSROOM_TEST_REPORT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            return Duration::from_secs(secs.max(1));
        }
    }
    REPORT_EVERY
}

fn sender_limit() -> Duration {
    // Debug builds only, for tests: a shorter deadline.
    #[cfg(debug_assertions)]
    {
        if let Some(secs) = std::env::var("PRESSROOM_TEST_SENDER_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            return Duration::from_secs(secs.max(1));
        }
    }
    SENDER_LIMIT
}

/// Run the sender with the report on its standard input. `Err` carries its exit code (none if
/// it could not start or was killed) and the first line it wrote to standard error.
fn send_report(galley: &machine::Galley, line: &str) -> Result<(), (Option<i32>, String)> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(&galley.sender)
        .arg("--origin")
        .arg(&galley.origin)
        .arg("--identity")
        .arg(&galley.credential)
        .env_clear()
        .env(
            "PATH",
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
        )
        .env(
            "HOME",
            std::env::var_os("HOME").unwrap_or_else(|| "/".into()),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| (None, format!("cannot start the sender: {e}")))?;
    // Written beside the deadline, so a sender that never reads cannot hold the daemon past it;
    // killing the sender closes the pipe and ends the write.
    let writer = child.stdin.take().map(|mut stdin| {
        let line = format!("{line}\n");
        std::thread::spawn(move || {
            let _ = stdin.write_all(line.as_bytes());
        })
    });
    let stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut first = String::new();
        if let Some(stderr) = stderr {
            let _ = BufReader::new(stderr).read_line(&mut first);
        }
        first.trim().to_string()
    });
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= sender_limit() => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break None,
        }
    };
    let Some(status) = status else {
        // A process the sender started may still hold its pipes: leave the two threads to end
        // with it rather than wait on them.
        return Err((None, "the sender did not finish in time".into()));
    };
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    let why = reader.join().unwrap_or_default();
    if status.success() {
        Ok(())
    } else {
        Err((status.code(), why))
    }
}
