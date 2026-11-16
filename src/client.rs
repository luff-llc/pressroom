//! The commands people, agents and cleanup tools run. Each talks to the daemon on this
//! machine, except `leases` and `may-delete`, which only read the lease files and so
//! work whether or not the daemon is up.

use crate::admit::GB;
use crate::json::{self, n, n64, obj, s, strs, Value};
use crate::lease::{self, DeleteRules, Store, Verdict};
use crate::machine::{self, now, Layout};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
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

fn ask(layout: &Layout, request: &Value) -> Result<Value, String> {
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

    let mut command = if args.nice && args.class == "batch" {
        let mut c = Command::new("nice");
        c.args(["-n", "10"]).args(&args.command);
        c
    } else {
        let mut c = Command::new(&args.command[0]);
        c.args(&args.command[1..]);
        c
    };
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
    let (exit, started) = match command.status() {
        // A command ended by a signal reports 128 plus the signal, as a shell does.
        Ok(status) => (
            status
                .code()
                .or_else(|| status.signal().map(|sig| 128 + sig))
                .unwrap_or(1),
            true,
        ),
        Err(e) => {
            eprintln!("pressroom: cannot start {}: {e}", args.command[0]);
            (127, false)
        }
    };

    // Did anything this job leased vanish while it ran? That is the machine's fault.
    let missing: Vec<String> = present
        .into_iter()
        .filter(|p| started && !Path::new(p).exists())
        .cloned()
        .collect();
    let done = obj(vec![
        ("op", s("done")),
        ("exit", n(exit)),
        ("missing", strs(&missing)),
    ]);
    let mut line = json::write(&done);
    line.push('\n');
    let _ = stream.write_all(line.as_bytes());
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
