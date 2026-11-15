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
        let home = PathBuf::from(format!("/tmp/pr-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("machine.conf"), conf).unwrap();
        let daemon = Command::new(BIN)
            .arg("daemon")
            .env("PRESSROOM_HOME", &home)
            .env("PRESSROOM_TEST_LOAD", "0")
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let machine = Machine { home, daemon };
        let until = Instant::now() + Duration::from_secs(10);
        while !machine.cmd(&["status", "--json"]).status.success() {
            assert!(Instant::now() < until, "daemon did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        machine
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env("PRESSROOM_HOME", &self.home)
            .env("PRESSROOM_HOLDER", "agent:test");
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
