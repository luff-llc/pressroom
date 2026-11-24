//! Placed work end to end: a real daemon, a real git folder, `place` choosing a machine
//! reached directly, and the results brought back (contracts/job-v1.md).

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_pressroom");

const RECIPE: &str = "\
repo = demo
kind = check
argv = sh {script} {results} {threads}
param script = checks/*.sh
pass_env = DEMO_
env FROM_THE_RECIPE = yes
threads = 2
memory_gb = 1
timeout_minutes = 1
";

const OK: &str = "\
echo \"ran in $(pwd)\"
echo \"threads $2\"
env | sort > \"$1/env.txt\"
echo made > \"$1/out.txt\"
";

struct World {
    root: PathBuf,
    home: PathBuf,
    repo: PathBuf,
    daemon: Child,
}

impl World {
    fn new(name: &str) -> World {
        let root = PathBuf::from(format!("/tmp/pp-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("h");
        let repo = root.join("repo");
        fs::create_dir_all(home.join("recipes")).unwrap();
        fs::write(
            home.join("machine.conf"),
            "kind = worker\nshare_away = 1\nmemory_reserve_gb = 0\ndisk_floor_gb = 0\ngrace_hours = 0\n",
        )
        .unwrap();
        fs::write(home.join("recipes/demo-check.conf"), RECIPE).unwrap();
        fs::write(home.join("machines.conf"), "machine here = local\n").unwrap();
        fs::create_dir_all(repo.join("checks")).unwrap();
        fs::write(repo.join("checks/ok.sh"), OK).unwrap();
        fs::write(repo.join("checks/fail.sh"), "echo failing; exit 3\n").unwrap();
        fs::write(repo.join("checks/slow.sh"), "sleep 600\n").unwrap();
        let git = |args: &[&str]| {
            let st = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                st.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&st.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["add", "checks/ok.sh", "checks/fail.sh"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "one",
        ]);
        let daemon = Command::new(BIN)
            .arg("daemon")
            .env("PRESSROOM_HOME", &home)
            .env("PRESSROOM_TEST_LOAD", "0")
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let w = World {
            root,
            home,
            repo,
            daemon,
        };
        let until = Instant::now() + Duration::from_secs(10);
        while !w.cmd(&["status", "--json"]).status.success() {
            assert!(Instant::now() < until, "daemon did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        w
    }
    fn cmd(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env("PRESSROOM_HOME", &self.home)
            .env("PRESSROOM_HOLDER", "agent:test")
            .env("SECRET_OF_THE_ASKER", "must-not-reach-the-job")
            .output()
            .unwrap()
    }
    fn place(&self, extra: &[&str]) -> Output {
        let repo = self.repo.to_string_lossy().to_string();
        let mut args = vec![
            "place",
            "--repo",
            "demo",
            "--kind",
            "check",
            "--source",
            repo.as_str(),
        ];
        args.extend_from_slice(extra);
        self.cmd(&args)
    }
    fn results(&self, id: &str) -> PathBuf {
        self.root.join("got").join(id)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

fn lane_src(w: &World) -> PathBuf {
    w.home.join("lanes/demo/default/0/src")
}

#[test]
fn placed_work_runs_the_recipe_on_the_snapshot_and_comes_back() {
    let w = World::new("run");
    // Not committed, not staged: the snapshot still carries it.
    fs::write(w.repo.join("checks/new.sh"), "echo new\n").unwrap();
    let got = w.results("one");
    let out = w.place(&[
        "--param",
        "script=checks/ok.sh",
        "--env",
        "DEMO_FLAG=on",
        "--id",
        "one",
        "--results",
        got.to_str().unwrap(),
    ]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("here: starts now"), "{err}");
    let answer = text(&out.stdout);
    assert!(
        answer.contains("\"state\":\"done\"") && answer.contains("\"exit\":0"),
        "{answer}"
    );
    assert_eq!(
        fs::read_to_string(got.join("out/out.txt")).unwrap().trim(),
        "made"
    );
    let log = fs::read_to_string(got.join("log.txt")).unwrap();
    assert!(log.contains("threads 2"), "{log}");
    assert!(
        log.contains(&format!(
            "ran in {}",
            lane_src(&w).canonicalize().unwrap().display()
        )),
        "{log}"
    );
    assert!(lane_src(&w).join("checks/new.sh").is_file());
    // The job's environment is the machine's few, the recipe's and what it passes.
    let env = fs::read_to_string(got.join("out/env.txt")).unwrap();
    assert!(env.contains("DEMO_FLAG=on"), "{env}");
    assert!(env.contains("FROM_THE_RECIPE=yes"), "{env}");
    assert!(env.contains("PRESSROOM_TREE="), "{env}");
    assert!(!env.contains("SECRET_OF_THE_ASKER"), "{env}");
    assert!(!env.contains("PRESSROOM_HOLDER"), "{env}");
    // Nothing is left running or leased.
    let status = text(&w.cmd(&["status", "--json"]).stdout);
    assert!(status.contains("\"running\":[]"), "{status}");

    // The same tree again: the machine has it, so nothing is sent, and the source folder
    // is not rewritten, so a warm build stays warm.
    let before = fs::metadata(lane_src(&w).join("checks/ok.sh"))
        .unwrap()
        .modified()
        .unwrap();
    let plan = w.place(&["--param", "script=checks/ok.sh", "--dry-run"]);
    assert!(
        text(&plan.stdout).contains("\"pack\":false"),
        "{}",
        text(&plan.stdout)
    );
    fs::write(w.repo.join("checks/another.sh"), "echo another\n").unwrap();
    // A job that changed a file of the tree gets it put back; one it left alone is not
    // rewritten. (Git cannot trust a file written in the same second as its index, so the
    // test waits out that second, as any real job does.)
    fs::write(lane_src(&w).join("checks/fail.sh"), "changed by a job\n").unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    let plan = w.place(&["--param", "script=checks/ok.sh", "--dry-run"]);
    assert!(text(&plan.stdout).contains("\"pack\":true"));
    let out = w.place(&[
        "--param",
        "script=checks/ok.sh",
        "--id",
        "two",
        "--results",
        w.results("two").to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let after = fs::metadata(lane_src(&w).join("checks/ok.sh"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(before, after);
    assert!(lane_src(&w).join("checks/another.sh").is_file());
    assert_eq!(
        fs::read_to_string(lane_src(&w).join("checks/fail.sh")).unwrap(),
        "echo failing; exit 3\n"
    );
}

#[test]
fn a_request_can_choose_only_what_the_recipe_allows() {
    let w = World::new("refuse");
    for bad in [
        "script=tools/evil.sh",
        "script=checks/../../x.sh",
        "script=-rf",
        "other=x",
    ] {
        let out = w.place(&["--param", bad]);
        assert_eq!(out.status.code(), Some(69), "{bad}: {}", text(&out.stderr));
        assert!(
            text(&out.stderr).contains("refused"),
            "{bad}: {}",
            text(&out.stderr)
        );
    }
    let out = w.place(&["--param", "script=checks/ok.sh", "--env", "PATH=/evil"]);
    assert_eq!(out.status.code(), Some(69));
    assert!(text(&out.stderr).contains("does not pass PATH"));
    // A kind no machine has a recipe for is not placed at all.
    let out = w.cmd(&[
        "place",
        "--repo",
        "demo",
        "--kind",
        "deploy",
        "--source",
        w.repo.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(69));
    assert!(
        text(&out.stderr).contains("no recipe"),
        "{}",
        text(&out.stderr)
    );
    // A tool version no machine has.
    let out = w.place(&["--param", "script=checks/ok.sh", "--needs", "xcode=99.9"]);
    assert_eq!(out.status.code(), Some(69));
    // `place` takes no command at all.
    let out = w.place(&["--", "rm", "-rf", "/"]);
    assert_eq!(out.status.code(), Some(64));
}

#[test]
fn a_request_line_with_a_command_is_refused_by_the_machine() {
    let w = World::new("raw");
    let tree = "a".repeat(40);
    let line = format!(
        "{{\"version\":\"pressroom.job/1\",\"id\":\"x\",\"repo\":\"demo\",\"kind\":\"check\",\"tree\":\"{tree}\",\
         \"params\":{{\"script\":\"checks/ok.sh\"}},\"class\":\"batch\",\"holder\":\"a\",\"argv\":[\"rm\"]}}\n"
    );
    let mut child = Command::new(BIN)
        .args(["job", "run"])
        .env("PRESSROOM_HOME", &w.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(line.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(69));
    assert!(text(&out.stdout).contains("argv"), "{}", text(&out.stdout));
    // And a tree the machine does not have, sent without its objects, is refused.
    let line = line.replace(",\"argv\":[\"rm\"]", "");
    let mut child = Command::new(BIN)
        .args(["job", "run"])
        .env("PRESSROOM_HOME", &w.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(line.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(69));
    assert!(
        text(&out.stdout).contains("does not have all of tree"),
        "{}",
        text(&out.stdout)
    );
    assert!(!w.home.join("results/x").exists());
}

#[test]
fn the_jobs_own_exit_comes_back_and_a_forced_key_reaches_only_jobs() {
    let w = World::new("exit");
    let out = w.place(&[
        "--param",
        "script=checks/fail.sh",
        "--results",
        w.results("f").to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
    assert!(fs::read_to_string(w.results("f").join("log.txt"))
        .unwrap()
        .contains("failing"));

    let forced = |asked: &str| {
        Command::new(BIN)
            .args(["job", "ssh"])
            .env("PRESSROOM_HOME", &w.home)
            .env("SSH_ORIGINAL_COMMAND", asked)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    assert!(text(&forced(".local/bin/pressroom job probe").stdout).contains("pressroom.job/1"));
    for asked in [
        "sh -c id",
        "pressroom run -- id",
        "pressroom job exec x",
        "pressroom job show ../x",
    ] {
        let out = forced(asked);
        assert_ne!(out.status.code(), Some(0), "{asked}");
    }
}

#[test]
fn a_machine_that_cannot_be_reached_is_passed_over() {
    let w = World::new("reach");
    fs::write(
        w.home.join("machines.conf"),
        "machine gone = ssh nobody@host.invalid\nmachine here = local\n",
    )
    .unwrap();
    let out = w.place(&[
        "--param",
        "script=checks/ok.sh",
        "--results",
        w.results("r").to_str().unwrap(),
    ]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("gone: cannot:"), "{err}");
    assert!(err.contains("placing on here"), "{err}");
}
