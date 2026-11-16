//! The real binary, a real daemon on a scratch home, real child processes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_pressroom");

struct Machine {
    home: PathBuf,
    daemon: Child,
}

impl Machine {
    /// A daemon on its own home in /tmp (a socket path must be short).
    fn start(name: &str, conf: &str) -> Machine {
        Machine::start_with(name, |_| conf.to_string())
    }
    /// The configuration is made from the home, for settings that name files in it.
    fn start_with(name: &str, conf: impl Fn(&Path) -> String) -> Machine {
        let home = PathBuf::from(format!("/tmp/pr-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("machine.conf"), conf(&home)).unwrap();
        let daemon = Machine::daemon(&home);
        let machine = Machine { home, daemon };
        machine.wait_until_up();
        machine
    }
    fn daemon(home: &Path) -> Child {
        Command::new(BIN)
            .arg("daemon")
            .env("PRESSROOM_HOME", home)
            .env("PRESSROOM_TEST_LOAD", "0")
            .env("PRESSROOM_TEST_REPORT_SECS", "1")
            .env("PRESSROOM_TEST_SENDER_SECS", "2")
            .env("SECRET_IN_THE_DAEMONS_ENV", "must-not-reach-the-sender")
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }
    fn wait_until_up(&self) {
        let until = Instant::now() + Duration::from_secs(10);
        while !self.cmd(&["status", "--json"]).status.success() {
            assert!(Instant::now() < until, "daemon did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    /// Kill the daemon as an upgrade does, and start another on the same home.
    fn restart(&mut self) {
        self.daemon.kill().unwrap();
        self.daemon.wait().unwrap();
        self.daemon = Machine::daemon(&self.home);
        self.wait_until_up();
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env("PRESSROOM_HOME", &self.home)
            .env("PRESSROOM_HOLDER", "agent:test")
            // These tests may themselves run as a pressroom job; theirs are not part of it.
            .env_remove("PRESSROOM_JOB");
        c
    }
    fn cmd(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
    fn status(&self) -> String {
        text(&self.cmd(&["status", "--json"]).stdout)
    }
    fn wait_for(&self, what: &str, test: impl Fn(&str) -> bool) {
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            let status = self.status();
            if test(&status) {
                return;
            }
            assert!(Instant::now() < until, "never saw: {what}\n{status}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = fs::remove_dir_all(&self.home);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

const WORKER: &str =
    "kind = worker\nshare_away = 1\nmemory_reserve_gb = 0\ndisk_floor_gb = 0\ngrace_hours = 0\n";
/// A machine that offers one thread, whatever it has.
const ONE_THREAD: &str =
    "kind = worker\nshare_away = 0.001\nmemory_reserve_gb = 0\ndisk_floor_gb = 0\n";

#[test]
fn a_job_runs_within_what_it_was_given() {
    let m = Machine::start("env", WORKER);
    let out = m.cmd(&[
        "run", "--threads", "2", "--lane", "demo", "--toolchain", "rust-1.99.1", "--",
        "sh", "-c", "echo $PRESSROOM_THREADS $CARGO_BUILD_JOBS $RUST_TEST_THREADS $MAKEFLAGS; echo $CARGO_TARGET_DIR",
    ]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    let mut lines = stdout.lines();
    assert_eq!(lines.next(), Some("2 2 2 -j2"));
    let target = lines.next().unwrap();
    assert!(
        target.ends_with("/lanes/demo/rust-1.99.1/0/target"),
        "{target}"
    );
    assert!(Path::new(target).parent().unwrap().is_dir());
    // Nothing is left behind.
    assert!(m.status().contains("\"running\":[]"));
    assert_eq!(text(&m.cmd(&["leases"]).stdout), "no live leases");
}

#[test]
fn exit_codes_pass_through() {
    let m = Machine::start("exit", WORKER);
    assert_eq!(
        m.cmd(&["run", "--", "sh", "-c", "exit 7"]).status.code(),
        Some(7)
    );
    assert_eq!(
        m.cmd(&["run", "--", "/no/such/program"]).status.code(),
        Some(127)
    );
    // A request the machine can never meet is refused, not queued.
    let out = m.cmd(&["run", "--memory-gb", "100000", "--", "true"]);
    assert_eq!(out.status.code(), Some(69));
    assert!(text(&out.stderr).contains("refused"));
    // Bad names never become paths.
    let out = m.cmd(&["run", "--lane", "../../etc", "--", "true"]);
    assert_eq!(out.status.code(), Some(69));
}

#[test]
fn no_daemon_means_no_run() {
    let home = format!("/tmp/pr-{}-none", std::process::id());
    let out = Command::new(BIN)
        .args(["run", "--", "true"])
        .env("PRESSROOM_HOME", &home)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(69));
    assert!(text(&out.stderr).contains("no pressroom daemon"));
}

#[test]
fn the_second_job_waits_for_the_first() {
    let m = Machine::start("queue", ONE_THREAD);
    let log = m.home.join("order.log");
    let script = format!(
        "echo start-$0 >> {0}; sleep 1; echo end-$0 >> {0}",
        log.display()
    );
    let first = m
        .command(&[
            "run",
            "--threads",
            "8",
            "--label",
            "first",
            "--",
            "sh",
            "-c",
            &script,
            "a",
        ])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    m.wait_for("first running", |s| {
        s.contains("\"label\":\"first\"") && s.contains("\"usedThreads\":1")
    });
    let second = m
        .command(&[
            "run",
            "--threads",
            "8",
            "--label",
            "second",
            "--",
            "sh",
            "-c",
            &script,
            "b",
        ])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    m.wait_for("second waiting with a reason", |s| {
        s.contains("\"waiting\":[{") && s.contains("1 of 1 offered threads in use")
    });
    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    assert!(first.status.success() && second.status.success());
    assert!(text(&first.stderr).contains("admitted on 1 threads (asked for 8)"));
    assert!(text(&second.stderr).contains("waiting: 1 of 1 offered threads in use"));
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "start-a\nend-a\nstart-b\nend-b\n"
    );
}

#[test]
fn someone_waiting_goes_before_batch_work() {
    let m = Machine::start("class", ONE_THREAD);
    let log = m.home.join("order.log");
    let script = format!("echo $0 >> {}; sleep 0.5", log.display());
    let run = |label: &str, class: &str| {
        m.command(&[
            "run", "--class", class, "--label", label, "--", "sh", "-c", &script, label,
        ])
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
    };
    let a = run("holding", "batch");
    m.wait_for("holding running", |s| s.contains("\"usedThreads\":1"));
    let b = run("batch", "batch");
    m.wait_for("batch queued", |s| s.contains("\"label\":\"batch\""));
    let c = run("urgent", "waiting");
    m.wait_for("urgent queued", |s| s.contains("\"label\":\"urgent\""));
    for mut child in [a, b, c] {
        assert!(child.wait().unwrap().success());
    }
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "holding\nurgent\nbatch\n"
    );
}

#[test]
fn two_builds_at_once_get_two_lanes() {
    let m = Machine::start("lanes", WORKER);
    let spawn = || {
        m.command(&[
            "run",
            "--threads",
            "1",
            "--lane",
            "colophon",
            "--",
            "sh",
            "-c",
            "echo $PRESSROOM_LANE; sleep 1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
    };
    let (a, b) = (spawn(), spawn());
    let a = text(&a.wait_with_output().unwrap().stdout);
    let b = text(&b.wait_with_output().unwrap().stdout);
    assert_ne!(a, b);
    let mut ends: Vec<&str> = vec![a.rsplit('/').next().unwrap(), b.rsplit('/').next().unwrap()];
    ends.sort();
    assert_eq!(ends, ["0", "1"]);
    // Afterwards the first lane is free again and still warm.
    let again = m.cmd(&[
        "run",
        "--lane",
        "colophon",
        "--",
        "sh",
        "-c",
        "echo $PRESSROOM_LANE",
    ]);
    assert!(text(&again.stdout).ends_with("/colophon/default/0"));
}

#[test]
fn a_cleanup_tool_cannot_take_what_a_job_holds() {
    let m = Machine::start("clean", WORKER);
    let registry = m.home.join("outside/registry");
    fs::create_dir_all(&registry).unwrap();
    let registry = fs::canonicalize(&registry).unwrap();
    let lane0 = fs::canonicalize(&m.home)
        .unwrap()
        .join("lanes/colophon/default/0");

    let job = m
        .command(&[
            "run",
            "--lane",
            "colophon",
            "--lease",
            registry.to_str().unwrap(),
            "--lease-kind",
            "registry",
            "--label",
            "offline build",
            "--",
            "sleep",
            "30",
        ])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    m.wait_for("job running", |s| {
        s.contains("offline build") && s.contains("\"usedThreads\":2")
    });

    // The lease files are there for a tool that only reads files.
    let files: Vec<_> = fs::read_dir(m.home.join("leases"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(files.len(), 2);
    let listed = text(&m.cmd(&["leases"]).stdout);
    assert!(
        listed.contains("registry (in-use)") && listed.contains(registry.to_str().unwrap()),
        "{listed}"
    );

    // The lane: leased. Its parent: holds something leased. The registry: outside the
    // managed folders, so never.
    for (path, why) in [
        (lane0.clone(), "leased by agent:test"),
        (
            lane0.parent().unwrap().to_path_buf(),
            "leased by agent:test",
        ),
        (registry.clone(), "outside the managed folders"),
    ] {
        let out = m.cmd(&["may-delete", path.to_str().unwrap()]);
        assert_eq!(out.status.code(), Some(1));
        assert!(text(&out.stdout).contains(why), "{}", text(&out.stdout));
        let out = m.cmd(&["delete-begin", path.to_str().unwrap()]);
        assert_eq!(
            out.status.code(),
            Some(1),
            "delete-begin must refuse {path:?}"
        );
    }
    // A job's leases cannot be released from outside the job.
    let id = files[0]
        .file_name()
        .to_string_lossy()
        .trim_end_matches(".json")
        .to_string();
    assert_eq!(m.cmd(&["lease", "release", &id]).status.code(), Some(1));

    // The job's process dies without a word: its leases die with it.
    let mut job = job;
    job.kill().unwrap();
    job.wait().unwrap();
    m.wait_for("job gone", |s| {
        s.contains("\"running\":[]") && s.contains("\"leases\":[]")
    });
    assert_eq!(
        m.cmd(&["may-delete", lane0.to_str().unwrap()])
            .status
            .code(),
        Some(0)
    );

    // While a tool is removing lane 0, a new build is given lane 1, not lane 0.
    let begin = m.cmd(&[
        "delete-begin",
        "--holder",
        "tool:cleanup",
        lane0.to_str().unwrap(),
    ]);
    assert!(begin.status.success(), "{}", text(&begin.stderr));
    let delete_id = text(&begin.stdout);
    let out = m.cmd(&[
        "run",
        "--lane",
        "colophon",
        "--",
        "sh",
        "-c",
        "echo $PRESSROOM_LANE",
    ]);
    assert!(
        text(&out.stdout).ends_with("/colophon/default/1"),
        "{}",
        text(&out.stdout)
    );
    // And nobody can lease what is being removed.
    let out = m.cmd(&["lease", "add", lane0.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("being removed by tool:cleanup"));
    assert!(m.cmd(&["delete-end", &delete_id]).status.success());
    assert!(m
        .cmd(&["lease", "add", lane0.to_str().unwrap()])
        .status
        .success());
}

#[test]
fn a_declared_lease_holds_until_released() {
    let m = Machine::start("declare", WORKER);
    let tree = fs::canonicalize(&m.home)
        .unwrap()
        .join("scratch/worktree-a");
    fs::create_dir_all(&tree).unwrap();
    assert_eq!(
        m.cmd(&["may-delete", tree.to_str().unwrap()]).status.code(),
        Some(0)
    );
    let add = m.cmd(&[
        "lease",
        "add",
        "--kind",
        "worktree",
        "--ttl-hours",
        "1",
        tree.to_str().unwrap(),
    ]);
    assert!(add.status.success());
    let id = text(&add.stdout);
    assert_eq!(
        m.cmd(&["may-delete", tree.to_str().unwrap()]).status.code(),
        Some(1)
    );
    assert!(m.cmd(&["lease", "release", &id]).status.success());
    assert_eq!(
        m.cmd(&["may-delete", tree.to_str().unwrap()]).status.code(),
        Some(0)
    );
    // Kinds and paths are checked.
    assert_eq!(
        m.cmd(&["lease", "add", "--kind", "delete", tree.to_str().unwrap()])
            .status
            .code(),
        Some(1)
    );
    assert_eq!(m.cmd(&["lease", "add", "/"]).status.code(), Some(1));
}

#[test]
fn a_leased_path_that_vanishes_is_the_machines_fault() {
    let m = Machine::start("vanish", WORKER);
    let registry = m.home.join("registry");
    fs::create_dir_all(&registry).unwrap();
    let script = format!("rm -rf {}; exit 101", registry.display());
    let out = m.cmd(&[
        "run",
        "--lease",
        registry.to_str().unwrap(),
        "--",
        "sh",
        "-c",
        &script,
    ]);
    assert_eq!(out.status.code(), Some(75));
    let err = text(&out.stderr);
    assert!(
        err.contains("removed while this job held a lease") && err.contains("not of the code"),
        "{err}"
    );
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let events = fs::read_to_string(m.home.join("events.log")).unwrap_or_default();
        if events.contains("\"event\":\"leased_path_removed\"") {
            break;
        }
        assert!(Instant::now() < until, "{events}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Speak to the socket directly, one JSON line, and keep the connection.
fn raw(m: &Machine, line: &str) -> std::os::unix::net::UnixStream {
    use std::io::Write;
    // A burst of connections can be turned away for a moment; try again.
    let mut tries = 0;
    let mut stream = loop {
        match std::os::unix::net::UnixStream::connect(m.home.join("sock")) {
            Ok(stream) => break stream,
            Err(e) => {
                tries += 1;
                assert!(tries < 200, "cannot connect: {e}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    stream.write_all(line.as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    stream
}

fn first_line(stream: std::os::unix::net::UnixStream) -> String {
    use std::io::BufRead;
    let mut line = String::new();
    std::io::BufReader::new(stream)
        .read_line(&mut line)
        .unwrap();
    line
}

#[test]
fn a_client_that_stops_reading_does_not_hold_the_queue() {
    let m = Machine::start("stall", ONE_THREAD);
    let mut holding = m
        .command(&["run", "--label", "holding", "--", "sleep", "60"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    m.wait_for("holding running", |s| s.contains("\"usedThreads\":1"));
    // Next in line: connected, never reads, never leaves.
    let stalled = raw(
        &m,
        r#"{"op":"run","holder":"agent:stalled","label":"stalled","threads":1}"#,
    );
    m.wait_for("stalled queued", |s| s.contains("\"label\":\"stalled\""));
    let behind = m
        .command(&["run", "--label", "behind", "--", "true"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    m.wait_for("behind queued", |s| s.contains("\"label\":\"behind\""));
    // Every change makes the daemon write to the stalled client. Keep making changes
    // until its buffer is full and the daemon, unable to tell it anything, drops it from
    // the line while it is still connected. How many changes that takes depends on the
    // system's socket buffers and on how many changes one write covers, so count on
    // neither: go on until it has happened.
    let until = Instant::now() + Duration::from_secs(40);
    while m.status().contains("\"label\":\"stalled\"") {
        assert!(
            Instant::now() < until,
            "a client that stopped reading is still in the line:\n{}",
            m.status()
        );
        for _ in 0..200 {
            drop(raw(&m, r#"{"op":"release","id":"zz"}"#));
        }
    }
    // The place frees: the one behind runs, not the stalled one.
    holding.kill().unwrap();
    holding.wait().unwrap();
    let mut behind = behind;
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = behind.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < until,
            "still queued behind a stalled client:\n{}",
            m.status()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(stalled);
}

#[test]
fn waiting_work_rejoins_the_line_when_the_daemon_is_replaced() {
    let mut m = Machine::start("rejoin", ONE_THREAD);
    let mut holding = m
        .command(&["run", "--label", "holding", "--", "sleep", "5"])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    m.wait_for("holding running", |s| s.contains("\"usedThreads\":1"));
    let behind = m
        .command(&["run", "--label", "behind", "--", "echo", "ran"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    m.wait_for("behind queued", |s| s.contains("\"label\":\"behind\""));
    m.restart();
    // The one that was waiting did not fail: it joined the new daemon's line and ran.
    let out = behind.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "ran");
    assert!(text(&out.stderr).contains("rejoining"), "{}", text(&out.stderr));
    // The one that was running was not disturbed.
    assert!(holding.wait().unwrap().success());
}

#[test]
fn dot_dot_through_a_link_is_not_a_way_out() {
    let m = Machine::start("dotdot", WORKER);
    let home = fs::canonicalize(&m.home).unwrap();
    fs::create_dir_all(home.join("outside/deep")).unwrap();
    fs::create_dir_all(home.join("outside/victim")).unwrap();
    std::os::unix::fs::symlink(home.join("outside/deep"), home.join("scratch/link")).unwrap();
    let sneaky = home.join("scratch/link/../victim");
    let out = m.cmd(&["may-delete", sneaky.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert!(text(&out.stdout).contains("outside the managed folders"));
    assert_eq!(
        m.cmd(&["delete-begin", sneaky.to_str().unwrap()])
            .status
            .code(),
        Some(1)
    );
    // And a lease taken that way protects the real place.
    assert!(m
        .cmd(&["lease", "add", sneaky.to_str().unwrap()])
        .status
        .success());
    assert!(
        text(&m.cmd(&["leases"]).stdout).contains(home.join("outside/victim").to_str().unwrap())
    );
}

#[test]
fn a_build_waits_while_its_lanes_are_being_removed() {
    let m = Machine::start("lanewait", WORKER);
    let base = fs::canonicalize(&m.home).unwrap().join("lanes/del/T");
    fs::create_dir_all(base.join("0")).unwrap();
    // Leases held by live processes used to make this take most of a minute.
    for i in 0..10 {
        let dir = fs::canonicalize(&m.home)
            .unwrap()
            .join(format!("scratch/held-{i}"));
        let pid = std::process::id().to_string();
        assert!(m
            .cmd(&["lease", "add", "--pid", &pid, dir.to_str().unwrap()])
            .status
            .success());
    }
    let begin = m.cmd(&["delete-begin", base.to_str().unwrap()]);
    assert!(begin.status.success(), "{}", text(&begin.stderr));
    let started = Instant::now();
    let job = m
        .command(&[
            "run",
            "--lane",
            "del",
            "--toolchain",
            "T",
            "--label",
            "lane job",
            "--",
            "true",
        ])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    m.wait_for("waiting on the removal", |s| {
        s.contains("is being removed by")
    });
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the daemon stalled"
    );
    assert!(m
        .cmd(&["delete-end", &text(&begin.stdout)])
        .status
        .success());
    let out = job.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
}

#[test]
fn only_a_removal_is_blamed_on_the_machine() {
    let m = Machine::start("blame", WORKER);
    // A leased path that was never there is not a removal.
    let out = m.cmd(&[
        "run",
        "--lease",
        "/nonexistent-pressroom-test",
        "--",
        "sh",
        "-c",
        "exit 3",
    ]);
    assert_eq!(out.status.code(), Some(3));
    assert!(!text(&out.stderr).contains("removed while"));
    // A command that could not be started is 127, whatever it leased.
    let out = m.cmd(&[
        "run",
        "--lease",
        "/nonexistent-pressroom-test",
        "--",
        "/no/such/program",
    ]);
    assert_eq!(out.status.code(), Some(127));
    // A script over several lines needs no label.
    let out = m.cmd(&["run", "--", "sh", "-c", "true\n\ttrue"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    // A command ended by a signal reports it.
    let out = m.cmd(&["run", "--", "sh", "-c", "kill -9 $$"]);
    assert_eq!(out.status.code(), Some(137));
    // Wrong arguments are 64, not a refusal.
    assert_eq!(
        m.cmd(&["run", "--class", "bogus", "--", "true"])
            .status
            .code(),
        Some(64)
    );
}

#[test]
fn bad_requests_are_refused_in_words() {
    let m = Machine::start("strict", WORKER);
    for bad in [
        r#"{"op":"run","threads":-1}"#,
        r#"{"op":"run","threads":"8"}"#,
        r#"{"op":"run","threads":1.5}"#,
        r#"{"op":"run","threads":0}"#,
        r#"{"op":"run","memoryGb":"lots"}"#,
        r#"{"op":"run","class":7}"#,
        r#"{"op":"run","lane":"colophon"}"#,
        r#"{"op":"run","leases":"/x"}"#,
        r#"{"op":"run","pid":999999999}"#,
        r#"{"op":"run","threads":1,"threads":2}"#,
        r#"{"op":"run","holder":["agent:a"]}"#,
        r#"{"op":"nonsense"}"#,
        "not json",
    ] {
        let reply = first_line(raw(&m, bad));
        assert!(
            reply.contains("\"state\":\"refused\"") && reply.contains("\"reason\""),
            "{bad} -> {reply}"
        );
    }
    assert!(m.status().contains("\"running\":[]"));
    let events = fs::read_to_string(m.home.join("events.log")).unwrap();
    assert!(
        events.matches("\"event\":\"refused\"").count() >= 9,
        "{events}"
    );
}

#[test]
fn one_daemon_per_home() {
    let m = Machine::start("twice", WORKER);
    let second = Command::new(BIN)
        .arg("daemon")
        .env("PRESSROOM_HOME", &m.home)
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(text(&second.stderr).contains("already running"));
    // A login agent asks it to leave a running daemon be, and gets success, not a retry.
    let polite = Command::new(BIN)
        .args(["daemon", "--if-not-running"])
        .env("PRESSROOM_HOME", &m.home)
        .output()
        .unwrap();
    assert!(polite.status.success(), "{}", text(&polite.stderr));
    assert!(text(&polite.stderr).contains("already running"));
    assert!(
        m.cmd(&["status"]).status.success(),
        "the first daemon still answers"
    );
}

/// A stand-in for Galley's sender: it keeps its arguments, its environment and the report it was
/// given, and exits as told by the file `exit` beside it (0 when absent).
fn fake_sender(dir: &Path) -> PathBuf {
    let script = dir.join("sender.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nd={d}\necho \"$@\" > $d/args\nenv > $d/env\ncat > $d/report.tmp && mv $d/report.tmp $d/report\n\
             n=$(cat $d/count 2>/dev/null || echo 0); echo $((n+1)) > $d/count\n\
             if [ -f $d/slow ]; then sleep $(cat $d/slow); fi\n\
             if [ -f $d/exit ]; then echo authorization_denied >&2; exit $(cat $d/exit); fi\n",
            d = dir.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    script
}

fn galley_conf(home: &Path) -> String {
    let out = home.join("out");
    fs::create_dir_all(&out).unwrap();
    format!(
        "{WORKER}galley = https://galley.example.test\ngalley_credential = {}\ngalley_sender = {}\n",
        home.join("identity.json").display(),
        fake_sender(&out).display()
    )
}

fn wait_file(path: &Path, test: impl Fn(&str) -> bool) -> String {
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        let found = fs::read_to_string(path).unwrap_or_default();
        if test(&found) {
            return found;
        }
        assert!(Instant::now() < until, "never saw what was wanted in {}:\n{found}", path.display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_report_goes_out_through_the_sender_on_a_timer() {
    let m = Machine::start_with("report", galley_conf);
    let out = m.home.join("out");
    // A job is running and a lease is held while a report is made.
    let mut job = m
        .command(&["run", "--threads", "1", "--lane", "demo", "--label", "cargo test", "--", "sleep", "5"])
        .spawn()
        .unwrap();
    let report = wait_file(&out.join("report"), |r| r.contains("cargo test"));
    let _ = job.wait();

    assert!(report.starts_with("{\"v\":\"pressroom.report/1\""), "{report}");
    for field in [
        "\"measuredAt\":", "\"name\":", "\"kind\":\"worker\"", "\"os\":", "\"arch\":",
        "\"threads\":", "\"memoryBytes\":", "\"offeredThreads\":", "\"offeredMemoryBytes\":",
        "\"usedThreads\":1", "\"usedMemoryBytes\":", "\"personPresent\":false",
        "\"loadHundredths\":0", "\"outsideThreads\":0", "\"diskFreeBytes\":", "\"diskFloorBytes\":0",
        "\"toolchains\":[", "\"holder\":\"agent:test\"", "\"class\":\"batch\"", "\"forSecs\":",
        "\"waiting\":[]", "\"kind\":\"cache\"", "\"mode\":\"in-use\"", "\"unreadableLeases\":0",
    ] {
        assert!(report.contains(field), "{field} missing from {report}");
    }
    // Whole numbers only, and nothing the contract leaves out.
    for absent in ["\"pid\"", "\"lane\"", "\"managed\"", "\"lanePath\"", "\"load1\"", "Gb\""] {
        assert!(!report.contains(absent), "{absent} in {report}");
    }
    assert!(
        !report.split([',', ':']).any(|part| {
            let p = part.trim_end_matches(['}', ']']);
            p.parse::<f64>().is_ok() && p.contains('.')
        }),
        "a fractional number in {report}"
    );

    // The sender is told where and as whom, by paths only, with nothing else in its environment.
    let args = fs::read_to_string(out.join("args")).unwrap();
    assert_eq!(
        args.trim(),
        format!("--origin https://galley.example.test --identity {}", m.home.join("identity.json").display())
    );
    let env = fs::read_to_string(out.join("env")).unwrap();
    assert!(!env.contains("must-not-reach-the-sender"), "{env}");
    assert!(!env.contains("PRESSROOM_HOME"), "{env}");
    for line in env.lines() {
        let key = line.split('=').next().unwrap();
        assert!(["PATH", "HOME", "PWD", "SHLVL", "_", "OLDPWD"].contains(&key), "{line}");
    }

    // On a timer: more reports follow without anything asking.
    let count = |m: &Machine| -> u32 {
        fs::read_to_string(m.home.join("out/count")).unwrap_or_default().trim().parse().unwrap_or(0)
    };
    let seen = count(&m);
    wait_file(&out.join("count"), |c| c.trim().parse::<u32>().unwrap_or(0) >= seen + 2);
}

#[test]
fn a_refused_report_is_written_once_and_the_machine_carries_on() {
    let m = Machine::start_with("refused", galley_conf);
    let out = m.home.join("out");
    wait_file(&out.join("count"), |c| !c.trim().is_empty());
    fs::write(out.join("exit"), "1").unwrap();
    let start = fs::read_to_string(out.join("count")).unwrap().trim().parse::<u32>().unwrap();
    wait_file(&out.join("count"), |c| c.trim().parse::<u32>().unwrap_or(0) >= start + 3);
    let events = fs::read_to_string(m.home.join("events.log")).unwrap();
    let failed: Vec<&str> = events.lines().filter(|l| l.contains("report_failed")).collect();
    assert_eq!(failed.len(), 1, "{events}");
    assert!(failed[0].contains("\"exit\":1") && failed[0].contains("authorization_denied"), "{events}");
    // Work is still admitted while reports fail.
    assert!(m.cmd(&["run", "--", "true"]).status.success());
    // Back to accepted: one line says so.
    fs::remove_file(out.join("exit")).unwrap();
    wait_file(&m.home.join("events.log"), |e| e.contains("report_sent"));
}

#[test]
fn without_galley_settings_nothing_is_sent_and_half_set_ones_stop_the_daemon() {
    // None of the three: the daemon works and runs no sender.
    let m = Machine::start("quiet", WORKER);
    assert!(m.cmd(&["run", "--", "true"]).status.success());
    std::thread::sleep(Duration::from_millis(1500));
    let events = fs::read_to_string(m.home.join("events.log")).unwrap_or_default();
    assert!(!events.contains("report_"), "{events}");

    // One or two of the three: the daemon refuses to start.
    let home = PathBuf::from(format!("/tmp/pr-{}-half", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("machine.conf"), "galley = https://galley.example.test\n").unwrap();
    let out = Command::new(BIN)
        .arg("daemon")
        .env("PRESSROOM_HOME", &home)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("set all three or none"), "{}", text(&out.stderr));
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn a_slow_or_stuck_sender_neither_holds_the_daemon_nor_moves_the_timetable() {
    let m = Machine::start_with("slow", galley_conf);
    let out = m.home.join("out");
    let count = || -> u32 {
        fs::read_to_string(out.join("count")).unwrap_or_default().trim().parse().unwrap_or(0)
    };
    wait_file(&out.join("count"), |c| !c.trim().is_empty());
    // A send that takes 0.6 s of each 1 s tick: still one report a tick, not one every 1.6 s.
    fs::write(out.join("slow"), "0.6").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let before = count();
    std::thread::sleep(Duration::from_millis(6200));
    let sent = count() - before;
    assert!(sent >= 6, "only {sent} reports in 6.2 s");

    // A sender that never finishes is killed at its deadline and recorded once; work goes on.
    fs::write(out.join("slow"), "30").unwrap();
    let events = wait_file(&m.home.join("events.log"), |e| e.contains("did not finish in time"));
    assert_eq!(events.matches("report_failed").count(), 1, "{events}");
    assert!(m.cmd(&["run", "--", "true"]).status.success());
    fs::remove_file(out.join("slow")).unwrap();
    wait_file(&m.home.join("events.log"), |e| e.contains("report_sent"));
}

#[test]
fn the_person_can_hold_the_machine_and_give_it_back() {
    let m = Machine::start("person", ONE_THREAD);
    let threads = |status: &str| -> String {
        let at = status.find("\"offeredThreads\":").unwrap() + 17;
        status[at..].split(',').next().unwrap().to_string()
    };
    assert_eq!(threads(&m.status()), "1");
    assert!(m.status().contains("\"personSays\":null"));

    // Hold: nothing new starts, and the wait says why.
    let out = m.cmd(&["person", "hold", "--minutes", "30", "--holder", "user:test"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let status = m.status();
    assert!(
        status.contains("\"personSays\":{\"mode\":\"hold\",\"holder\":\"user:test\""),
        "{status}"
    );
    assert_eq!(threads(&status), "0");
    let job = m
        .command(&[
            "run",
            "--threads",
            "1",
            "--memory-gb",
            "0",
            "--class",
            "waiting",
            "--",
            "true",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    m.wait_for("the job held", |s| {
        s.contains("asked for the whole machine")
    });
    assert!(m.status().contains("\"running\":[]"));

    // Auto: back to what is measured, and the held job runs.
    let out = m.cmd(&["person", "auto"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let done = job.wait_with_output().unwrap();
    assert!(done.status.success(), "{}", text(&done.stderr));
    assert!(
        text(&done.stderr).contains("whole machine"),
        "it was told why it waited"
    );
    assert!(m.status().contains("\"personSays\":null"));

    // Give: every thread on the machine, more than the owner's away share.
    let out = m.cmd(&["person", "give", "--minutes", "60"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let status = m.status();
    let all = status
        .split("\"threads\":")
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap()
        .to_string();
    assert_eq!(threads(&status), all, "{status}");
    assert!(status.contains("\"personPresent\":false"));

    // What the person says is checked like everything else.
    for bad in [
        r#"{"op":"person-says","mode":"sleep","forSecs":600}"#,
        r#"{"op":"person-says","mode":"hold"}"#,
        r#"{"op":"person-says","mode":"hold","forSecs":10}"#,
        r#"{"op":"person-says","mode":"hold","forSecs":"600"}"#,
        r#"{"op":"person-says","mode":"give","forSecs":99999999}"#,
        r#"{"op":"person-says","mode":"auto","forSecs":600}"#,
        r#"{"op":"person-says","mode":"hold","forSecs":600,"holder":7}"#,
    ] {
        let reply = first_line(raw(&m, bad));
        assert!(reply.contains("\"state\":\"refused\""), "{bad} -> {reply}");
    }
    let events = fs::read_to_string(m.home.join("events.log")).unwrap();
    assert_eq!(
        events.matches("\"event\":\"person_says\"").count(),
        3,
        "{events}"
    );
}

/// A shell script that runs `n` busy loops for about `secs` seconds of wall time, then
/// prints the processor seconds its children used, from the shell's `times`.
fn spinner(n: u32, secs: u32) -> String {
    format!(
        "pids=; i=0; while [ $i -lt {n} ]; do (while :; do :; done) & pids=\"$pids $!\"; i=$((i+1)); done; \
         sleep {secs}; kill $pids; wait 2>/dev/null; times"
    )
}

/// Processor seconds of the children, from the second line of `times`: `0m31.52s 0m0.10s`.
fn children_cpu(stdout: &str) -> f64 {
    let line = stdout.lines().last().unwrap_or("");
    line.split_whitespace()
        .map(|part| {
            let (m, s) = part.trim_end_matches('s').split_once('m').unwrap_or(("0", "0"));
            m.parse::<f64>().unwrap_or(0.0) * 60.0 + s.parse::<f64>().unwrap_or(0.0)
        })
        .sum()
}

/// Run a job asking for one thread that spins eight, and say how many threads' worth of
/// processor it used over its life, what it was told, and what the log says.
fn spin_eight_on_one(m: &Machine) -> (f64, String, String) {
    let started = Instant::now();
    let out = m.cmd(&["run", "--threads", "1", "--class", "waiting", "--", "sh", "-c", &spinner(8, 4)]);
    let wall = started.elapsed().as_secs_f64();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let cpu = children_cpu(&text(&out.stdout));
    assert!(cpu > 0.5, "the spinner did not spin: {}", text(&out.stdout));
    let events = fs::read_to_string(m.home.join("events.log")).unwrap();
    (cpu / wall, text(&out.stderr), events)
}

#[test]
fn a_job_asking_for_one_thread_and_spinning_eight_is_held_to_one() {
    // By the best means here: a systemd scope on Linux where the cpu controller is
    // delegated, else by stopping and continuing it.
    let m = Machine::start("hold", WORKER);
    let (threads, stderr, events) = spin_eight_on_one(&m);
    assert!(threads < 1.5, "used {threads:.2} threads on a grant of 1\n{stderr}");
    assert!(stderr.contains("held to 1 and runs slower"), "{stderr}");
    assert!(events.contains("\"event\":\"held\""), "{events}");
    let ended = events.lines().find(|l| l.contains("\"event\":\"ended\"")).unwrap();
    assert!(!ended.contains("\"heldSecs\":0"), "{ended}");
    // On Linux, by a scope wherever systemd will make one that holds; elsewhere by stopping.
    let means = if scope_here() { "scope" } else { "stop" };
    assert!(ended.contains(&format!("\"means\":\"{means}\"")), "{ended}");
}

/// Whether `systemd-run --user --scope` makes a scope here with the cpu controller.
fn scope_here() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    let mut probe = Command::new("systemd-run");
    probe.args([
        "--user", "--scope", "--quiet", "--expand-environment=no", "-p", "CPUQuota=100%", "--",
        "/bin/sh", "-c", "cat \"/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)/cpu.max\"",
    ]);
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        let uid = text(&Command::new("id").arg("-u").output().unwrap().stdout);
        probe.env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));
    }
    probe
        .output()
        .is_ok_and(|out| text(&out.stdout).starts_with("100000 "))
}

#[test]
fn held_by_stopping_where_that_is_the_means() {
    // The means macOS has, forced here so it is tested on Linux too.
    let m = Machine::start("stop", &format!("{WORKER}cap = stop\n"));
    let (threads, stderr, _) = spin_eight_on_one(&m);
    assert!(threads < 1.5, "used {threads:.2} threads on a grant of 1\n{stderr}");
}

#[test]
fn use_is_measured_and_shown_against_the_grant() {
    let m = Machine::start("measure", &format!("{WORKER}cap = measure\n"));
    let job = m
        .command(&["run", "--threads", "1", "--label", "greedy", "--", "sh", "-c", &spinner(4, 6)])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Measured, charged and flagged, not held (rule 24).
    m.wait_for("the job measured over its grant", |s| {
        s.contains("\"over\":true") && s.contains("\"means\":\"measure\"")
    });
    let status = m.status();
    let used: f64 = status
        .split("\"useThreads\":")
        .nth(1)
        .and_then(|rest| rest.split([',', '}']).next())
        .and_then(|v| v.parse().ok())
        .unwrap();
    assert!(used > 2.0, "{status}");
    let charged: f64 = status
        .split("\"usedThreads\":")
        .nth(1)
        .and_then(|rest| rest.split([',', '}']).next())
        .and_then(|v| v.parse().ok())
        .unwrap();
    assert!(charged >= used.round() - 0.01, "charged {charged} for {used}\n{status}");
    let out = job.wait_with_output().unwrap();
    assert!(text(&out.stderr).contains("does not hold them"), "{}", text(&out.stderr));
    let cpu = children_cpu(&text(&out.stdout));
    assert!(cpu > 8.0, "it was held although cap = measure: {cpu}");
}

#[test]
fn a_run_inside_a_job_is_part_of_it() {
    // The outer job takes every thread offered. A build it starts through pressroom does
    // not wait for the machine, and gets no more than the outer job has (rule 26).
    let m = Machine::start("within", ONE_THREAD);
    let inner = format!(
        "{BIN} run --threads 8 --label inner --lane demo -- sh -c 'echo inner $PRESSROOM_THREADS; echo $CARGO_TARGET_DIR'"
    );
    let out = m
        .command(&["run", "--threads", "8", "--label", "outer", "--", "sh", "-c", &inner])
        .env("PRESSROOM_HOME", &m.home)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("inner 1"), "{stdout}");
    assert!(stdout.contains("/lanes/demo/"), "{stdout}");
    let events = fs::read_to_string(m.home.join("events.log")).unwrap();
    let outer = events
        .lines()
        .find(|l| l.contains("\"label\":\"outer\""))
        .and_then(|l| l.split("\"job\":\"").nth(1))
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_string();
    assert!(
        events.contains(&format!("\"within\":\"{outer}\"")),
        "{events}"
    );
    // A PRESSROOM_JOB that names no running job is ignored.
    let out = m
        .command(&["run", "--", "sh", "-c", "echo $PRESSROOM_THREADS"])
        .env("PRESSROOM_JOB", "j-1-1")
        .env("PRESSROOM_JOB_PID", std::process::id().to_string())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
}

#[test]
fn one_emulator_or_simulator_at_a_time() {
    let m = Machine::start("device", WORKER);
    let first = m
        .command(&["run", "--device", "--label", "emulator", "--", "sleep", "3"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    m.wait_for("the emulator running", |s| s.contains("\"devicesInUse\":1"));
    let second = m
        .command(&["run", "--device", "--label", "simulator", "--", "true"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    m.wait_for("the simulator waiting for the slot", |s| {
        s.contains("device slot is taken") && s.contains("(emulator)")
    });
    // Work that needs no device does not wait behind it.
    let out = m.cmd(&["run", "--", "true"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(first.wait_with_output().unwrap().status.success());
    let out = second.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(text(&out.stderr).contains("device slot"), "{}", text(&out.stderr));

    let none = Machine::start("nodevice", &format!("{WORKER}device_slots = 0\n"));
    let out = none.cmd(&["run", "--device", "--", "true"]);
    assert_eq!(out.status.code(), Some(69));
    assert!(text(&out.stderr).contains("device_slots = 0"));
}

#[test]
fn a_killed_run_leaves_nothing_stopped() {
    let m = Machine::start("killed", &format!("{WORKER}cap = stop\n"));
    let pids = m.home.join("pids");
    let script = format!(
        // They ignore the hang-up the system sends a stopped group left without its parent,
        // so they must be found running, not stopped.
        "trap '' HUP; i=0; while [ $i -lt 4 ]; do (while :; do :; done) & echo $! >> {}; i=$((i+1)); done; wait",
        pids.display()
    );
    let mut run = m
        .command(&["run", "--threads", "1", "--", "sh", "-c", &script])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    m.wait_for("the job held", |s| s.contains("\"held\":true"));
    run.kill().unwrap();
    run.wait().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let spinners = fs::read_to_string(&pids).unwrap();
    let pids: Vec<&str> = spinners.split_whitespace().collect();
    // Look a few times: none is left stopped (rule 28).
    for _ in 0..5 {
        let out = Command::new("ps").args(["-o", "stat=", "-p", &pids.join(",")]).output().unwrap();
        let states = text(&out.stdout);
        assert!(!states.is_empty() && !states.contains('T'), "{states}");
        std::thread::sleep(Duration::from_millis(100));
    }
    for pid in pids {
        let _ = Command::new("kill").arg(pid).status();
    }
}

/// Processor seconds from `ps -o time=`: `[[dd-]hh:]mm:ss[.xx]`.
fn ps_time(text: &str) -> f64 {
    text.split_whitespace()
        .map(|t| {
            let (days, rest) = t.split_once('-').unwrap_or(("0", t));
            let secs: f64 = rest
                .split(':')
                .fold(0.0, |acc, part| acc * 60.0 + part.parse::<f64>().unwrap_or(0.0));
            days.parse::<f64>().unwrap_or(0.0) * 86400.0 + secs
        })
        .sum()
}

#[test]
fn a_tool_that_runs_commands_in_groups_of_their_own_is_still_held() {
    // As Codex does: each command in a process group of its own, and inside it, spinners
    // that fork twice so their parent is gone at once. They are the job's all the same.
    if Command::new("perl").arg("-e1").status().map_or(true, |s| !s.success()) {
        eprintln!("no perl here; skipped");
        return;
    }
    for cap in ["hold", "stop"] {
        let m = Machine::start(&format!("groups-{cap}"), &format!("{WORKER}cap = {cap}\n"));
        let pids = m.home.join("pids");
        let script = format!(
            r#"setpgrp(0,0); open(my $f, ">", "{0}"); my @p;
               for (1..4) {{ pipe(my $r, my $w); if (!fork) {{ if (my $k = fork) {{ print $w "$k\n"; exit }} 1 while 1 }}
                 close $w; my $k = <$r>; chomp $k; push @p, $k; waitpid(-1, 0) }}
               print $f join(" ", @p); close $f; sleep 5;
               print `ps -o time= -p @{{[join(",", @p)]}}`; kill "KILL", @p;"#,
            pids.display()
        );
        // The tool is the job's first process; the command runs one level down.
        let out = m.cmd(&["run", "--threads", "1", "--", "sh", "-c", "perl -e \"$1\"; exit $?", "sh", &script]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        let used = ps_time(&text(&out.stdout));
        // Four spinners for five seconds would be twenty seconds unheld.
        assert!(used > 0.5 && used < 9.0, "{cap}: the spinners used {used:.1}s in 5s\n{}", text(&out.stdout));
        assert!(text(&out.stderr).contains("held to 1"), "{cap}: {}", text(&out.stderr));
    }
}

#[test]
fn a_run_left_behind_by_its_job_becomes_a_job_of_its_own() {
    // The outer command starts a build through pressroom in the background and ends at
    // once. The build must not go on unheld and uncharged (rule 26).
    let m = Machine::start("orphan", &format!("{WORKER}cap = stop\n"));
    let timed = m.home.join("times");
    let inner = format!(
        "{BIN} run --threads 1 --label inner -- sh -c '{} > {}' &",
        spinner(4, 4),
        timed.display()
    );
    let started = Instant::now();
    let out = m
        .command(&["run", "--threads", "8", "--label", "outer", "--", "sh", "-c", &inner])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    let times = wait_file(&timed, |t| t.lines().count() >= 2);
    let wall = started.elapsed().as_secs_f64();
    let threads = children_cpu(&times) / wall;
    assert!(stderr.contains("now a job of its own"), "{stderr}");
    assert!(threads < 1.6, "the left-behind build used {threads:.2} threads on 1\n{stderr}");
    let events = fs::read_to_string(m.home.join("events.log")).unwrap();
    assert!(events.contains("\"event\":\"on_its_own\""), "{events}");
}

#[test]
fn a_killed_run_leaves_no_group_of_its_job_stopped() {
    // A second process group whose parent lives is not hung up by the system when `run`
    // dies; only the daemon continues it (rule 28).
    if Command::new("perl").arg("-e1").status().map_or(true, |s| !s.success()) {
        eprintln!("no perl here; skipped");
        return;
    }
    let m = Machine::start("killed2", &format!("{WORKER}cap = stop\n"));
    let pids = m.home.join("pids");
    let script = format!(
        r#"$SIG{{HUP}} = "IGNORE"; setpgrp(0,0); open(my $f, ">", "{0}"); my @p;
           for (1..3) {{ my $k = fork; if (!$k) {{ 1 while 1 }} push @p, $k }}
           print $f join(" ", @p, $$); close $f; sleep 60;"#,
        pids.display()
    );
    let mut run = m
        .command(&["run", "--threads", "1", "--", "sh", "-c", "trap '' HUP; perl -e \"$1\"", "sh", &script])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    m.wait_for("the job held", |s| s.contains("\"held\":true"));
    run.kill().unwrap();
    run.wait().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let listed = fs::read_to_string(&pids).unwrap();
    let pids: Vec<&str> = listed.split_whitespace().collect();
    let mut states = String::new();
    for _ in 0..5 {
        let out = Command::new("ps").args(["-o", "stat=", "-p", &pids.join(",")]).output().unwrap();
        states = text(&out.stdout);
        if !states.contains('T') {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    for pid in &pids {
        let _ = Command::new("kill").args(["-9", pid]).status();
    }
    assert!(!states.is_empty() && !states.contains('T'), "left stopped: {states}");
}
