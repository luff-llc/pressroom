//! The commands people, agents and cleanup tools run. Each talks to the daemon on this
//! machine, except `leases` and `may-delete`, which only read the lease files and so
//! work whether or not the daemon is up.

use crate::admit::GB;
use crate::cap::{self, Means, Notice, Watch};
use crate::json::{self, n, n64, obj, s, strs, Value};
use crate::lease::{self, DeleteRules, Store, Verdict};
use crate::machine::{self, now, Layout};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Exit codes, from sysexits: the daemon could not be reached or refused.
pub const UNAVAILABLE: i32 = 69;
/// The machine failed the job, not the code: try again.
pub const TEMPFAIL: i32 = 75;
pub const USAGE: i32 = 64;

fn connect(layout: &Layout) -> Result<UnixStream, String> {
    // A busy daemon can turn a connection away for a moment. Only a socket that is
    // missing, or still refuses after a few tries, means there is no daemon.
    let mut tries = 0;
    loop {
        match UnixStream::connect(layout.socket()) {
            Ok(stream) => return Ok(stream),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused && tries < 20 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(50 * tries));
            }
            Err(_) => {
                return Err(format!(
                    "no pressroom daemon on this machine (looked for {}). Start one with: pressroom daemon",
                    layout.socket().display()
                ))
            }
        }
    }
}

/// How long waiting work goes on looking for a daemon that went away.
const REJOIN_FOR: std::time::Duration = std::time::Duration::from_secs(120);

/// Connect and send a `run` request; the replies are read from the second half.
fn join(layout: &Layout, line: &str) -> Result<(UnixStream, BufReader<UnixStream>), String> {
    let mut stream = connect(layout)?;
    stream
        .write_all(line.as_bytes())
        .map_err(|_| "the daemon closed the connection".to_string())?;
    let reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    Ok((stream, reader))
}

pub fn ask(layout: &Layout, request: &Value) -> Result<Value, String> {
    let mut stream = connect(layout)?;
    let mut line = json::write(request);
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .map_err(|e| e.to_string())?;
    let reply = json::parse(reply.trim()).map_err(|e| format!("bad reply from the daemon: {e}"))?;
    if reply.get("ok") == Some(&Value::Bool(false)) {
        return Err(reply.str("reason").unwrap_or("refused").to_string());
    }
    Ok(reply)
}

pub fn default_holder() -> String {
    std::env::var("PRESSROOM_HOLDER")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| {
            format!(
                "user:{}",
                std::env::var("USER").unwrap_or_else(|_| "unknown".into())
            )
        })
}

fn absolute(path: &str) -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    lease::normalise(Path::new(path), &cwd)
        .to_string_lossy()
        .to_string()
}

pub struct RunArgs {
    pub threads: u32,
    pub memory_gb: f64,
    pub class: String,
    pub lane: Option<String>,
    pub toolchain: String,
    pub leases: Vec<String>,
    pub lease_kind: String,
    pub holder: String,
    pub label: Option<String>,
    pub nice: bool,
    /// It runs an emulator or simulator (rule 27).
    pub device: bool,
    pub command: Vec<String>,
}

/// Ask to run a command, wait to be admitted, run it within what was granted, and
/// report how it ended. Returns the exit code for this process.
pub fn run(layout: &Layout, args: RunArgs) -> i32 {
    let label = args.label.clone().unwrap_or_else(|| {
        // A script given on the command line may run over several lines.
        let mut text: String = args
            .command
            .join(" ")
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        if text.trim().is_empty() {
            text = "work".into();
        }
        text.truncate(text.char_indices().nth(160).map_or(text.len(), |(i, _)| i));
        text
    });
    let leases: Vec<String> = args.leases.iter().map(|p| absolute(p)).collect();
    let mut request = vec![
        ("op", s("run")),
        ("holder", s(&args.holder)),
        ("label", s(label)),
        ("class", s(&args.class)),
        ("threads", n(args.threads)),
        ("memoryGb", n(args.memory_gb)),
        ("pid", n(std::process::id())),
        ("leases", strs(&leases)),
        ("leaseKind", s(&args.lease_kind)),
    ];
    if let Some(repo) = &args.lane {
        request.push((
            "lane",
            obj(vec![("repo", s(repo)), ("toolchain", s(&args.toolchain))]),
        ));
    }
    if args.device {
        request.push(("device", Value::Bool(true)));
    }
    // Started inside a running job: part of it (rule 26). Only where that job's `run`
    // saw a daemon that knows rule 26 (it then sets PRESSROOM_JOB_PID): an older daemon
    // would queue this behind the job it is part of.
    let outer_run: Option<u32> = std::env::var("PRESSROOM_JOB_PID")
        .ok()
        .and_then(|p| p.parse().ok())
        .filter(|&p| p > 1);
    if let (Ok(outer), Some(_)) = (std::env::var("PRESSROOM_JOB"), outer_run) {
        if !outer.is_empty() && outer.len() <= 64 {
            request.push(("within", s(outer)));
        }
    }
    let mut line = json::write(&obj(request));
    line.push('\n');
    let (mut stream, mut reader) = match join(layout, &line) {
        Ok(joined) => joined,
        Err(why) => {
            eprintln!("pressroom: {why}");
            return UNAVAILABLE;
        }
    };

    let mut last_reason = String::new();
    // When a daemon was last lost without any answer since.
    let mut unanswered_since: Option<std::time::Instant> = None;
    let admitted = loop {
        let mut reply = String::new();
        match reader.read_line(&mut reply) {
            Ok(0) | Err(_) => {
                // The daemon stopped, most often to be replaced by a newer one. Work that
                // was only waiting has lost nothing: join the new daemon's line (rule 22).
                //
                // The two minutes run from when a daemon last answered, not from each lost
                // connection: one that accepts and hangs up, over and over, is no daemon.
                eprintln!("pressroom: the daemon went away while this was waiting; rejoining");
                let until =
                    *unanswered_since.get_or_insert_with(std::time::Instant::now) + REJOIN_FOR;
                let rejoined = loop {
                    if std::time::Instant::now() >= until {
                        break None;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    if let Ok(joined) = join(layout, &line) {
                        break Some(joined);
                    }
                };
                let Some(joined) = rejoined else {
                    eprintln!(
                        "pressroom: no daemon answered within {} seconds",
                        REJOIN_FOR.as_secs()
                    );
                    return UNAVAILABLE;
                };
                (stream, reader) = joined;
                last_reason.clear();
                continue;
            }
            Ok(_) => unanswered_since = None,
        }
        let Ok(reply) = json::parse(reply.trim()) else {
            eprintln!("pressroom: bad reply from the daemon");
            return UNAVAILABLE;
        };
        match reply.str("state") {
            Some("admitted") => break reply,
            Some("waiting") => {
                let reason = reply.str("reason").unwrap_or("").to_string();
                if reason != last_reason {
                    eprintln!("pressroom: waiting: {reason}");
                    last_reason = reason;
                }
            }
            _ => {
                eprintln!(
                    "pressroom: refused: {}",
                    reply.str("reason").unwrap_or("no reason given")
                );
                return UNAVAILABLE;
            }
        }
    };

    let threads = admitted.u64("threads").unwrap_or(1).to_string();
    let job = admitted.str("job").unwrap_or("").to_string();
    let lane = admitted.str("lanePath").map(str::to_string);
    if threads != args.threads.to_string() {
        eprintln!(
            "pressroom: admitted on {threads} threads (asked for {})",
            args.threads
        );
    }

    // Inside another job (rule 26): that job's hold holds this one too, until it ends.
    let inside = admitted.str("within").is_some();
    // A daemon that knows rules 23 to 28 says how it holds work.
    let knows_rule_26 = admitted.str("cap").is_some();
    let granted: u32 = threads.parse().unwrap_or(1);
    let cap_memory = admitted
        .num("capMemoryGb")
        .filter(|g| g.is_finite() && *g > 0.0)
        .map(|g| (g * GB as f64) as u64);
    let means = match admitted.str("cap") {
        _ if inside => Means::Within,
        Some("measure") => Means::Measure,
        Some("stop") => Means::Stop,
        // `hold`, and a daemon too old to say: the best means here.
        _ if cap::scope_works() => Means::Scope(std::path::PathBuf::new()),
        _ => Means::Stop,
    };

    let mut argv: Vec<String> = Vec::new();
    if args.nice && args.class == "batch" {
        argv.extend(["nice", "-n", "10"].map(String::from));
    }
    argv.extend(args.command.iter().cloned());
    let unit = cap::scope_unit(&job);
    let mut bus_env = None;
    if let Means::Scope(_) = means {
        // A program that cannot be found gives 127, not systemd-run's own failure.
        if cap::find_program(&args.command[0]).is_none() {
            eprintln!("pressroom: cannot start {}: not found", args.command[0]);
            return finish_job(&mut stream, 127, &[], 0.0, None);
        }
        let mut scoped: Vec<String> = [
            "systemd-run",
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            // Otherwise systemd-run reads `$NAME` and `$$` in the command's own arguments.
            "--expand-environment=no",
        ]
        .map(String::from)
        .to_vec();
        scoped.push(format!("--unit={unit}"));
        scoped.extend(["-p".into(), format!("CPUQuota={}%", granted * 100)]);
        if let Some(bytes) = cap_memory {
            scoped.extend(["-p".into(), format!("MemoryMax={bytes}")]);
        }
        scoped.push("--".into());
        scoped.extend(argv);
        argv = scoped;
        bus_env = cap::user_bus_env();
    }

    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    if let Some((k, v)) = bus_env {
        command.env(k, v);
    }
    // Its own process group, so all of it can be held, and passed a signal, at once.
    command.process_group(0);
    if knows_rule_26 {
        command.env("PRESSROOM_JOB_PID", std::process::id().to_string());
    } else {
        command.env_remove("PRESSROOM_JOB_PID");
    }
    command
        .env("PRESSROOM_JOB", &job)
        .env("PRESSROOM_THREADS", &threads)
        // The job counts the common build tools read. Xcode takes `-jobs` as an
        // argument: pass `-jobs "$PRESSROOM_THREADS"`.
        .env("CARGO_BUILD_JOBS", &threads)
        .env("RUST_TEST_THREADS", &threads)
        .env("MAKEFLAGS", format!("-j{threads}"))
        .env("CMAKE_BUILD_PARALLEL_LEVEL", &threads);
    if let Some(lane) = &lane {
        command
            .env("PRESSROOM_LANE", lane)
            .env("CARGO_TARGET_DIR", Path::new(lane).join("target"));
    }
    // Only what was there when the job started can have been removed under it.
    let present: Vec<&String> = leases.iter().filter(|p| Path::new(p).exists()).collect();
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(1)));
    let (exit, started, held_secs, held_by) = match command.spawn() {
        Ok(child) => {
            let outer = if inside { outer_run } else { None };
            let (status, held_secs, held_by) =
                watch(&mut stream, child, means, granted, cap_memory, &unit, outer);
            // A command ended by a signal reports 128 plus the signal, as a shell does.
            let code = status
                .and_then(|status| status.code().or_else(|| status.signal().map(|sig| 128 + sig)))
                .unwrap_or(1);
            (code, true, held_secs, held_by)
        }
        Err(e) => {
            eprintln!("pressroom: cannot start {}: {e}", args.command[0]);
            (127, false, 0.0, None)
        }
    };

    // Did anything this job leased vanish while it ran? That is the machine's fault.
    let missing: Vec<String> = present
        .into_iter()
        .filter(|p| started && !Path::new(p).exists())
        .cloned()
        .collect();
    finish_job(&mut stream, exit, &missing, held_secs, held_by);
    if !missing.is_empty() {
        eprintln!(
            "pressroom: removed while this job held a lease on it: {}",
            missing.join(", ")
        );
        if exit != 0 {
            eprintln!(
                "pressroom: the job failed (exit {exit}) and that is the likely cause: \
                 a fault of the machine, not of the code. Restore it and run again."
            );
            return TEMPFAIL;
        }
    }
    exit
}

/// Tell the daemon how the job ended. Returns the exit code, for the early returns.
fn finish_job(
    stream: &mut UnixStream,
    exit: i32,
    missing: &[String],
    held_secs: f64,
    held_by: Option<&str>,
) -> i32 {
    let done = obj(vec![
        ("op", s("done")),
        ("exit", n(exit)),
        ("missing", strs(missing)),
        ("heldSecs", n((held_secs * 10.0).round() / 10.0)),
        ("means", held_by.map_or(Value::Null, s)),
    ]);
    let mut line = json::write(&done);
    line.push('\n');
    let _ = stream.write_all(line.as_bytes());
    exit
}

fn send_line(stream: &mut UnixStream, value: &Value) {
    let mut line = json::write(value);
    line.push('\n');
    // The daemon may be gone (rule 22); the job goes on, and so does its hold.
    let _ = stream.write_all(line.as_bytes());
}

/// Tell the job, and the daemon's log, what its hold did (rule 25).
fn tell(stream: &mut UnixStream, notices: Vec<Notice>) {
    for notice in notices {
        eprintln!("pressroom: {}", notice.text);
        send_line(
            stream,
            &obj(vec![
                ("op", s("event")),
                ("event", s(notice.event)),
                ("text", s(&notice.text)),
            ]),
        );
    }
}

/// Tell the daemon how the job is held and what it may be stopped by (rules 24 and 28),
/// with what it used when that is known.
fn send_use(stream: &mut UnixStream, w: &Watch, pid: i32, used: Option<&cap::Use>) {
    send_targets(stream, w.means.name(), pid, w.targets(), used);
}

fn send_targets(
    stream: &mut UnixStream,
    means: &str,
    pid: i32,
    targets: &cap::Targets,
    used: Option<&cap::Use>,
) {
    let mut fields = vec![
        ("op", s("use")),
        ("means", s(means)),
        ("pgid", n(pid)),
        (
            "groups",
            Value::Arr(targets.groups.iter().take(cap::MAX_REPORTED).map(|&g| n(g)).collect()),
        ),
        (
            "pids",
            Value::Arr(targets.pids.iter().take(cap::MAX_REPORTED).map(|&p| n(p)).collect()),
        ),
    ];
    if let Some(used) = used {
        fields.push(("held", Value::Bool(used.held)));
        fields.push(("heldSecs", n((used.held_secs * 10.0).round() / 10.0)));
        if let (Some(threads), Some(memory)) = (used.threads, used.memory) {
            fields.push(("threads", n((threads * 100.0).round() / 100.0)));
            fields.push(("memoryGb", n((memory as f64 / GB as f64 * 100.0).round() / 100.0)));
        }
    }
    send_line(stream, &obj(fields));
}

/// Wait for the job, holding it to its grant and saying what it uses (rules 23 to 25).
/// `outer` is the `run` of the job this one is part of (rule 26): once it is gone, this
/// one holds itself. Returns how it ended, for how many seconds it was held back, and by
/// what means.
fn watch(
    stream: &mut UnixStream,
    mut child: std::process::Child,
    mut means: Means,
    granted: u32,
    cap_memory: Option<u64>,
    unit: &str,
    outer: Option<u32>,
) -> (Option<std::process::ExitStatus>, f64, Option<&'static str>) {
    let pid = child.id() as i32;
    if let Means::Scope(_) = means {
        // systemd-run makes the scope, then becomes the command. Should no scope appear,
        // hold it by stopping instead.
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        means = loop {
            if let Some(dir) = cap::scope_of(pid, unit) {
                break Means::Scope(dir);
            }
            if std::time::Instant::now() >= until || matches!(child.try_wait(), Ok(Some(_))) {
                break Means::Stop;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
    }
    cap::sys::catch_endings();
    let gave_terminal = cap::sys::give_terminal(pid);
    let mut w = Watch::new(means, pid, pid, granted, cap_memory.unwrap_or(0));
    // Before anything can be stopped, the daemon knows how and what (rule 28).
    send_use(stream, &w, pid, None);
    if w.means == Means::Measure {
        tell(
            stream,
            vec![Notice {
                event: "not_held",
                text: "this machine measures jobs but does not hold them to their grant \
                       (cap = measure)"
                    .into(),
            }],
        );
    }
    let mut last_report = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if w.means == Means::Within && outer.is_some_and(|run| !cap::sys::running(run as i32)) {
            // The job this was part of has ended; the daemon now charges this one.
            let notice = w.on_its_own();
            tell(stream, vec![notice]);
            send_use(stream, &w, pid, None);
        }
        // Told before they are stopped, so a `run` killed meanwhile leaves none unknown.
        let notices = w.tick(&mut |targets| send_targets(stream, "stop", pid, targets, None));
        tell(stream, notices);
        if let Some(sig) = cap::sys::take_caught() {
            // Asked to end: pass it on, and hold the job no more so it can end promptly.
            w.let_go();
            cap::sys::kill(-pid, sig);
        }
        if last_report.elapsed() >= cap::REPORT_EVERY {
            last_report = std::time::Instant::now();
            let (used, notices) = w.report();
            tell(stream, notices);
            send_use(stream, &w, pid, Some(&used));
        }
        std::thread::sleep(cap::TICK);
    };
    let (held_secs, notices) = w.finish();
    tell(stream, notices);
    if gave_terminal {
        cap::sys::take_terminal();
    }
    let held_by = (w.means != Means::Within).then(|| w.means.name());
    (status, held_secs, held_by)
}

pub fn status(layout: &Layout, as_json: bool) -> i32 {
    let reply = match ask(layout, &obj(vec![("op", s("status"))])) {
        Ok(reply) => reply,
        Err(why) => {
            eprintln!("pressroom: {why}");
            return UNAVAILABLE;
        }
    };
    if as_json {
        println!("{}", json::write(&reply));
        return 0;
    }
    let num = |key: &str| reply.num(key).unwrap_or(0.0);
    let present = match (reply.str("kind"), reply.get("personPresent")) {
        (Some("workstation"), Some(Value::Bool(true))) => ", its person is using it",
        (Some("workstation"), _) => ", its person is away",
        _ => "",
    };
    println!(
        "{} ({}, {} {}{present})",
        reply.str("name").unwrap_or("?"),
        reply.str("kind").unwrap_or("?"),
        reply.str("os").unwrap_or("?"),
        reply.str("arch").unwrap_or("?"),
    );
    if let Some(says) = reply.get("personSays").filter(|v| **v != Value::Null) {
        let left = lease::span(says.u64("until").unwrap_or(0).saturating_sub(now()));
        match says.str("mode") {
            Some("hold") => {
                println!("  its person has asked for the whole machine, for {left} more")
            }
            Some("give") => println!("  its person has given the whole machine, for {left} more"),
            _ => {}
        }
    }
    let busy = match reply.num("busyThreads") {
        Some(busy) => format!("{busy:.1} busy; load {:.1}", num("load1")),
        None => format!("load {:.1}", num("load1")),
    };
    println!(
        "  threads  {} of {} offered in use ({} on the machine, {busy})",
        num("usedThreads"),
        num("offeredThreads"),
        num("threads"),
    );
    if num("outsideLoad") >= 1.0 {
        println!(
            "           about {} busy threads were not started through pressroom",
            num("outsideLoad")
        );
        for line in busiest() {
            println!("             {line}");
        }
    }
    println!(
        "  memory   {:.1} of {:.1} GB offered in use ({:.0} GB on the machine)",
        num("usedMemoryGb"),
        num("offeredMemoryGb"),
        num("memoryGb")
    );
    let low = if num("diskFreeGb") < num("diskFloorGb") {
        "  BELOW THE FLOOR: nothing new starts"
    } else {
        ""
    };
    println!(
        "  disk     {:.0} GB free, floor {:.0} GB{low}",
        num("diskFreeGb"),
        num("diskFloorGb")
    );
    for tool in reply.arr("toolchains") {
        println!(
            "  has      {}: {}",
            tool.str("name").unwrap_or(""),
            tool.str("version").unwrap_or("")
        );
    }
    let running = reply.arr("running");
    println!(
        "running: {}",
        if running.is_empty() { "nothing" } else { "" }
    );
    for job in running {
        println!(
            "  {}  {} threads, {}  for {}  {}",
            job.str("job").unwrap_or(""),
            job.u64("threads").unwrap_or(0),
            job.str("class").unwrap_or(""),
            lease::span(job.u64("forSecs").unwrap_or(0)),
            job.str("holder").unwrap_or(""),
        );
        println!("      {}", job.str("label").unwrap_or(""));
        if let Some(lane) = job.str("lane") {
            println!("      lane {lane}");
        }
    }
    let waiting = reply.arr("waiting");
    println!(
        "waiting: {}",
        if waiting.is_empty() { "nothing" } else { "" }
    );
    for w in waiting {
        println!(
            "  {} threads, {}  for {}  {}",
            w.u64("threads").unwrap_or(0),
            w.str("class").unwrap_or(""),
            lease::span(w.u64("forSecs").unwrap_or(0)),
            w.str("holder").unwrap_or(""),
        );
        println!("      {}", w.str("label").unwrap_or(""));
        println!("      because: {}", w.str("reason").unwrap_or(""));
    }
    let leases = reply.arr("leases");
    println!("leases: {}", if leases.is_empty() { "none" } else { "" });
    print_leases(leases);
    for name in reply.strings("unreadableLeases") {
        println!("  UNREADABLE lease file {name}: cleanup is refused until it is fixed or removed");
    }
    0
}

fn print_leases(leases: &[Value]) {
    let at = now();
    for l in leases {
        let left = l.u64("expires").unwrap_or(0).saturating_sub(at);
        let until = match l.u64("pid") {
            Some(pid) => format!("while process {pid} lives"),
            None => format!("for {} more", lease::span(left)),
        };
        println!(
            "  {}  {} ({})  {}  {until}",
            l.str("id").unwrap_or(""),
            l.str("kind").unwrap_or(""),
            l.str("mode").unwrap_or(""),
            l.str("holder").unwrap_or(""),
        );
        for path in l.strings("paths") {
            println!("      {path}");
        }
    }
}

/// List live leases from the files. Needs no daemon.
pub fn leases(layout: &Layout, as_json: bool) -> i32 {
    let store = Store::new(layout.leases());
    let live: Vec<Value> = store
        .live(now())
        .iter()
        .map(lease::Lease::to_json)
        .collect();
    if as_json {
        println!("{}", json::write(&Value::Arr(live)));
    } else if live.is_empty() {
        println!("no live leases");
    } else {
        print_leases(&live);
    }
    0
}

pub fn lease_add(
    layout: &Layout,
    holder: &str,
    kind: &str,
    mode: &str,
    ttl_hours: f64,
    pid: Option<u32>,
    paths: &[String],
) -> i32 {
    let paths: Vec<String> = paths.iter().map(|p| absolute(p)).collect();
    let mut request = vec![
        ("op", s("lease")),
        ("holder", s(holder)),
        ("kind", s(kind)),
        ("mode", s(mode)),
        ("ttlSecs", n64((ttl_hours * 3600.0).max(1.0) as u64)),
        ("paths", strs(&paths)),
    ];
    if let Some(pid) = pid {
        request.push(("pid", n(pid)));
    }
    finish(ask(layout, &obj(request)), |reply| {
        println!("{}", reply.str("id").unwrap_or(""))
    })
}

/// Say "I need the whole machine" (`hold`), "take it all" (`give`) or go back to what is
/// measured (`auto`).
pub fn person_says(layout: &Layout, holder: &str, mode: &str, minutes: f64) -> i32 {
    let mut request = vec![
        ("op", s("person-says")),
        ("holder", s(holder)),
        ("mode", s(mode)),
    ];
    if mode != "auto" {
        request.push(("forSecs", n64((minutes * 60.0).round() as u64)));
    }
    finish(ask(layout, &obj(request)), |reply| {
        match reply.u64("until") {
            Some(until) => println!("{mode} for {}", lease::span(until.saturating_sub(now()))),
            None => println!("auto: back to what is measured"),
        }
    })
}

pub fn release(layout: &Layout, op: &str, id: &str) -> i32 {
    finish(
        ask(layout, &obj(vec![("op", s(op)), ("id", s(id))])),
        |_| {},
    )
}

/// Take a delete lease on one path. Prints its id; end it with `delete-end`.
pub fn delete_begin(layout: &Layout, holder: &str, ttl_minutes: f64, path: &str) -> i32 {
    let request = obj(vec![
        ("op", s("delete-begin")),
        ("holder", s(holder)),
        ("ttlSecs", n64((ttl_minutes * 60.0).max(1.0) as u64)),
        ("paths", strs(&[absolute(path)])),
    ]);
    match ask(layout, &request) {
        Ok(reply) => {
            println!("{}", reply.str("id").unwrap_or(""));
            0
        }
        Err(why) => {
            eprintln!("pressroom: keep {}: {why}", absolute(path));
            1
        }
    }
}

fn finish(reply: Result<Value, String>, on_ok: impl Fn(&Value)) -> i32 {
    match reply {
        Ok(reply) => {
            on_ok(&reply);
            0
        }
        Err(why) => {
            eprintln!("pressroom: {why}");
            1
        }
    }
}

/// May this path be removed? Exit 0 for yes, 1 for no with the reason. Reads the lease
/// files and `machine.conf` only, so it answers even when the daemon is down. A tool
/// that is about to remove something should use `delete-begin`, which also stops a job
/// from starting on the path in the meantime.
pub fn may_delete(layout: &Layout, path: &str) -> i32 {
    let config = match machine::load_config(layout, machine::total_memory()) {
        Ok(config) => config,
        Err(why) => {
            eprintln!("pressroom: keep: {why}");
            return 1;
        }
    };
    let store = Store::new(layout.leases());
    let rules = DeleteRules {
        managed: &config.managed,
        grace_secs: config.grace_secs,
    };
    let path = absolute(path);
    match lease::may_delete(&store, &rules, Path::new(&path), now(), None) {
        Verdict::Yes => {
            println!("may delete {path}");
            0
        }
        Verdict::No(why) => {
            println!("keep {path}: {why}");
            1
        }
    }
}

/// What this machine is, measured now. Needs no daemon.
pub fn measure(layout: &Layout) -> i32 {
    let m = machine::measure(&layout.home);
    let tools = machine::toolchains()
        .iter()
        .map(|(name, version)| obj(vec![("name", s(name)), ("version", s(version))]))
        .collect();
    let value = obj(vec![
        ("name", s(machine::hostname())),
        ("os", s(machine::os())),
        ("arch", s(std::env::consts::ARCH)),
        ("threads", n(m.threads)),
        ("memoryGb", n(m.memory as f64 / GB as f64)),
        ("diskFreeGb", n(m.disk_free as f64 / GB as f64)),
        ("load1", n(m.load1)),
        ("busyThreads", m.busy.map_or(Value::Null, n)),
        ("idleSecs", m.idle_secs.map_or(Value::Null, n64)),
        ("toolchains", Value::Arr(tools)),
    ]);
    println!("{}", json::write(&value));
    0
}

/// The programs using the most processor right now, as `ps` reports them: at most four,
/// each using a tenth of a thread or more. Shown beside outside load so a person can see
/// whether it is the desktop or a stray build. Empty if `ps` cannot be run.
fn busiest() -> Vec<String> {
    let sort: &[&str] = if cfg!(target_os = "macos") {
        &["-Ao", "pcpu=,comm=", "-r"]
    } else {
        &["-eo", "pcpu=,comm=", "--sort=-pcpu"]
    };
    let Ok(out) = std::process::Command::new("ps").args(sort).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (percent, program) = line.trim().split_once(char::is_whitespace)?;
            let percent: f64 = percent.parse().ok()?;
            let name = program.trim().rsplit('/').next().unwrap_or("").trim();
            (percent >= 10.0 && !name.is_empty() && name != "ps")
                .then(|| format!("{:.1} threads  {name}", percent / 100.0))
        })
        .take(4)
        .collect()
}
