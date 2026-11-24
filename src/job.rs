//! The machine's half of placed work (contracts/job-v1.md): take a request and the source
//! it names, admit it like any other work, run the recipe's fixed argv on that source, and
//! keep what it left for the asker to fetch.
//!
//! `pressroom job run` reads one request line, then a git pack, on standard input. It
//! writes the job's output to standard error as it runs, and one JSON line to standard
//! output when it ends. Over SSH that is the whole exchange, so an `authorized_keys` entry
//! can force `pressroom job ssh` and a key then reaches nothing but these verbs.

use crate::client::{self, UNAVAILABLE, USAGE};
use crate::json::{self, n, n64, obj, s, Value};
use crate::machine::{now, Layout};
use crate::recipe::{self, Recipe, Request};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub fn recipes_dir(layout: &Layout) -> PathBuf {
    layout.home.join("recipes")
}
fn results_dir(layout: &Layout, id: &str) -> PathBuf {
    layout.home.join("results").join(id)
}
fn bare(layout: &Layout, repo: &str) -> PathBuf {
    layout.home.join("checkouts").join(format!("{repo}.git"))
}
/// How many trees of one repository a machine keeps named, newest first.
const KEEP_TREES: usize = 8;

fn say(text: &str) {
    // Standard error may be a connection that has gone; the job goes on (rule J10).
    let _ = writeln!(std::io::stderr(), "pressroom: {text}");
}

fn answer(value: Value) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{}", json::write(&value));
    let _ = out.flush();
}

fn refuse(reason: &str) -> i32 {
    say(&format!("refused: {reason}"));
    answer(obj(vec![("state", s("refused")), ("reason", s(reason))]));
    UNAVAILABLE
}

fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("--git-dir").arg(dir).stdin(Stdio::null());
    c
}

fn quiet(mut c: Command) -> Result<String, String> {
    let out = c.output().map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The trees this machine holds for a repository, newest first.
fn trees(layout: &Layout, repo: &str) -> Vec<String> {
    let dir = bare(layout, repo);
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut c = git(&dir);
    c.args([
        "for-each-ref",
        "--format=%(refname:lstrip=2)",
        "refs/pressroom",
    ]);
    // A tree has no date of its own, so the ref's file time orders them.
    let mut found: Vec<(std::time::SystemTime, String)> = quiet(c)
        .unwrap_or_default()
        .lines()
        .filter(|t| recipe::valid_tree(t))
        .map(|t| {
            let at = fs::metadata(dir.join("refs/pressroom").join(t))
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (at, t.to_string())
        })
        .collect();
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, t)| t).collect()
}

/// `pressroom job probe`: what this machine is and holds, for whoever places work.
pub fn probe(layout: &Layout) -> i32 {
    let (recipes, bad) = recipe::load_all(&recipes_dir(layout));
    let status = client::ask(layout, &obj(vec![("op", s("status"))]));
    let mut repos: Vec<String> = recipes.iter().map(|r| r.repo.clone()).collect();
    repos.dedup();
    let held = Value::Obj(
        repos
            .iter()
            .map(|r| (r.clone(), json::strs(&trees(layout, r))))
            .collect(),
    );
    answer(obj(vec![
        ("version", s(recipe::VERSION)),
        ("status", status.clone().unwrap_or(Value::Null)),
        ("statusError", status.err().map(s).unwrap_or(Value::Null)),
        (
            "recipes",
            Value::Arr(recipes.iter().map(recipe::to_json).collect()),
        ),
        ("unreadableRecipes", json::strs(&bad)),
        ("trees", held),
    ]));
    0
}

/// The one line before the pack, read without reading past it: the pack that follows
/// goes to git as it is.
fn read_line_raw() -> Result<String, String> {
    use std::os::fd::AsFd;
    let fd = std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| e.to_string())?;
    let mut input = fs::File::from(fd);
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match input.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => line.push(byte[0]),
            Err(e) => return Err(e.to_string()),
        }
        if line.len() > 256 * 1024 {
            return Err("the request line is longer than 256 KB".into());
        }
    }
    String::from_utf8(line).map_err(|_| "the request is not UTF-8".to_string())
}

fn find<'a>(recipes: &'a [Recipe], request: &Request) -> Result<&'a Recipe, String> {
    recipes
        .iter()
        .find(|r| r.repo == request.repo && r.kind == request.kind)
        .ok_or_else(|| {
            format!(
                "this machine has no recipe for {} {} (it has: {})",
                request.repo,
                request.kind,
                if recipes.is_empty() {
                    "none".to_string()
                } else {
                    recipes
                        .iter()
                        .map(|r| format!("{} {}", r.repo, r.kind))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            )
        })
}

/// Rules J7 and J8, on the machine itself: a placer's picture may be stale.
fn fits_here(recipe: &Recipe, status: &Value) -> Result<(), String> {
    for (tool, version) in &recipe.needs {
        let reported = status
            .arr("toolchains")
            .iter()
            .find(|t| t.str("name") == Some(tool.as_str()))
            .and_then(|t| t.str("version"));
        match reported {
            Some(have) if recipe::version_meets(have, version) => {}
            Some(have) => return Err(format!("needs {tool} {version}; this machine has {have}")),
            None => {
                return Err(format!(
                    "needs {tool} {version}; this machine reports no {tool}"
                ))
            }
        }
    }
    let free = status.num("diskFreeGb").unwrap_or(0.0);
    let floor = status.num("diskFloorGb").unwrap_or(0.0);
    if free - recipe.disk_gb < floor {
        return Err(format!(
            "it would write up to {} GB and this machine has {free:.0} GB free, floor {floor:.0} GB",
            recipe.disk_gb
        ));
    }
    Ok(())
}

/// Store the pack on standard input, then check the tree it was meant to carry is whole.
fn receive(layout: &Layout, request: &Request) -> Result<(), String> {
    let dir = bare(layout, &request.repo);
    if !dir.join("objects").is_dir() {
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mut init = Command::new("git");
        init.args(["init", "-q", "--bare"]).arg(&dir);
        quiet(init)?;
    }
    if request.pack {
        let mut c = git(&dir);
        c.args(["index-pack", "--stdin"]).stdin(Stdio::inherit());
        quiet(c).map_err(|e| format!("the pack could not be stored: {e}"))?;
    }
    // Every object of the tree, not only its top: a pack that left something out, or a
    // tree this machine only half has, is refused here rather than failing in checkout.
    let mut c = git(&dir);
    c.args(["rev-list", "--objects", "--missing=error", &request.tree]);
    quiet(c).map_err(|_| {
        format!(
            "this machine does not have all of tree {}; send it with the request",
            &request.tree[..12]
        )
    })?;
    let mut c = git(&dir);
    c.args(["cat-file", "-t", &request.tree]);
    if quiet(c)? != "tree" {
        return Err(format!("{} is not a tree", request.tree));
    }
    let mut c = git(&dir);
    c.args([
        "update-ref",
        &format!("refs/pressroom/{}", request.tree),
        &request.tree,
    ]);
    quiet(c)?;
    let _ = fs::File::open(dir.join("refs/pressroom").join(&request.tree))
        .and_then(|f| f.set_modified(std::time::SystemTime::now()));
    for old in trees(layout, &request.repo).into_iter().skip(KEEP_TREES) {
        let mut c = git(&dir);
        c.args(["update-ref", "-d", &format!("refs/pressroom/{old}")]);
        let _ = quiet(c);
    }
    Ok(())
}

/// `pressroom job run`: the request and its source on standard input.
pub fn run(layout: &Layout) -> i32 {
    let line = match read_line_raw() {
        Ok(line) => line,
        Err(why) => return refuse(&why),
    };
    let request = match recipe::read_request(&line) {
        Ok(r) => r,
        Err(why) => return refuse(&why),
    };
    let (recipes, _) = recipe::load_all(&recipes_dir(layout));
    let recipe = match find(&recipes, &request) {
        Ok(r) => r.clone(),
        Err(why) => return refuse(&why),
    };
    if let Err(why) = recipe::check(&recipe, &request) {
        return refuse(&why);
    }
    let status = match client::ask(layout, &obj(vec![("op", s("status"))])) {
        Ok(status) => status,
        Err(why) => return refuse(&why),
    };
    if let Err(why) = fits_here(&recipe, &status) {
        return refuse(&why);
    }
    let results = results_dir(layout, &request.id);
    if results.exists() {
        return refuse(&format!(
            "job {} already exists on this machine",
            request.id
        ));
    }
    if let Err(e) = fs::create_dir_all(results.join("out")) {
        return refuse(&format!("cannot make {}: {e}", results.display()));
    }
    let _ = fs::write(
        results.join("request.json"),
        json::write(&recipe::request_json(&request)),
    );
    if let Err(why) = receive(layout, &request) {
        let _ = fs::remove_dir_all(&results);
        return refuse(&why);
    }
    let machine = status.str("name").unwrap_or("?").to_string();
    say(&format!(
        "{machine}: {} {} on tree {}, asking for room",
        request.repo,
        request.kind,
        &request.tree[..12]
    ));

    let started = Instant::now();
    let me = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("pressroom"));
    let mut admit = Command::new(&me);
    admit
        .arg("run")
        .args(["--threads", &recipe.threads.to_string()])
        .args(["--memory-gb", &recipe.memory_gb.to_string()])
        .args(["--class", &request.class])
        .args(["--lane", &request.repo])
        .args(["--toolchain", &recipe.toolchain])
        .args(["--holder", &request.holder])
        .args([
            "--label",
            &format!("job {}: {} {}", request.id, request.repo, request.kind),
        ])
        .arg("--lease")
        .arg(bare(layout, &request.repo))
        .arg("--")
        .arg(&me)
        .args(["job", "exec", &request.id])
        .current_dir(&results)
        .env("PRESSROOM_HOME", &layout.home)
        .stdin(Stdio::null())
        // Standard output carries only the answer, so the job's goes to standard error.
        .stdout(Stdio::from(std::io::stderr().as_fd_owned()))
        .stderr(Stdio::inherit());
    let code = match admit.status() {
        Ok(st) => st
            .code()
            .or_else(|| st.signal().map(|g| 128 + g))
            .unwrap_or(1),
        Err(e) => return refuse(&format!("cannot start pressroom run: {e}")),
    };
    let ran =
        json::parse(&fs::read_to_string(results.join("result.json")).unwrap_or_default()).ok();
    let mut result = vec![
        ("state", s("done")),
        ("id", s(&request.id)),
        ("machine", s(&machine)),
        ("repo", s(&request.repo)),
        ("kind", s(&request.kind)),
        ("tree", s(&request.tree)),
        ("exit", n(code)),
        ("seconds", n64(started.elapsed().as_secs())),
        ("ended", n64(now())),
    ];
    match &ran {
        Some(r) => {
            result.push((
                "ranSeconds",
                r.get("seconds").cloned().unwrap_or(Value::Null),
            ));
            result.push((
                "timedOut",
                r.get("timedOut").cloned().unwrap_or(Value::Bool(false)),
            ));
        }
        // It never ran: refused at admission, or the machine failed it before the start.
        None => {
            result.push(("ranSeconds", Value::Null));
            result.push(("timedOut", Value::Bool(false)));
        }
    }
    let result = obj(result);
    let _ = fs::write(results.join("result.json"), json::write(&result));
    answer(result);
    code
}

trait OwnedStderr {
    fn as_fd_owned(&self) -> std::os::fd::OwnedFd;
}
impl OwnedStderr for std::io::Stderr {
    fn as_fd_owned(&self) -> std::os::fd::OwnedFd {
        use std::os::fd::AsFd;
        self.as_fd()
            .try_clone_to_owned()
            .expect("standard error can be duplicated")
    }
}

/// Bring the lane's source folder to exactly the tree, rewriting only files that changed,
/// so a warm build stays warm (rule J5).
fn checkout(layout: &Layout, request: &Request, lane: &Path) -> Result<PathBuf, String> {
    let src = lane.join("src");
    let index = lane.join("src.index");
    fs::create_dir_all(&src).map_err(|e| e.to_string())?;
    if !src.join(".git").exists() {
        // Tools that look for a repository's root find one; its history is not sent.
        let mut init = Command::new("git");
        init.args(["init", "-q"]).arg(&src);
        quiet(init)?;
    }
    // `--reset` makes the folder exactly the tree: a file an earlier job changed is put
    // back, and one that matches is left alone. The index keeps what each file looked like
    // when written, which is how git tells the two apart.
    let mut c = git(&bare(layout, &request.repo));
    c.arg("--work-tree")
        .arg(&src)
        .env("GIT_INDEX_FILE", &index)
        .args(["read-tree", "--reset", "-u", &request.tree]);
    quiet(c)?;
    Ok(src)
}

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// The environment a job starts with: nothing of the asker's, little of the machine's.
const KEPT: [&str; 7] = ["PATH", "HOME", "USER", "LOGNAME", "TMPDIR", "LANG", "SHELL"];
const FROM_RUN: [&str; 9] = [
    "PRESSROOM_JOB",
    "PRESSROOM_JOB_PID",
    "PRESSROOM_THREADS",
    "PRESSROOM_LANE",
    "CARGO_BUILD_JOBS",
    "CARGO_TARGET_DIR",
    "RUST_TEST_THREADS",
    "MAKEFLAGS",
    "CMAKE_BUILD_PARALLEL_LEVEL",
];

/// `pressroom job exec ID`: inside an admitted `run`, check out and run the job.
pub fn exec(layout: &Layout, id: &str) -> i32 {
    if !recipe::valid_id(id) {
        say("job exec needs a job id");
        return USAGE;
    }
    let results = results_dir(layout, id);
    let request = match fs::read_to_string(results.join("request.json"))
        .map_err(|e| e.to_string())
        .and_then(|t| recipe::read_request(&t))
    {
        Ok(r) => r,
        Err(why) => {
            say(&format!("job {id}: {why}"));
            return UNAVAILABLE;
        }
    };
    let (recipes, _) = recipe::load_all(&recipes_dir(layout));
    let recipe = match find(&recipes, &request) {
        Ok(r) => r.clone(),
        Err(why) => {
            say(&why);
            return UNAVAILABLE;
        }
    };
    let Some(lane) = std::env::var_os("PRESSROOM_LANE").map(PathBuf::from) else {
        say("job exec runs only inside pressroom run, which gives it a lane");
        return USAGE;
    };
    let threads: u32 = std::env::var("PRESSROOM_THREADS")
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or(1);
    let out = results.join("out");
    let argv = match recipe::expand(&recipe, &request, &out, threads) {
        Ok(argv) => argv,
        Err(why) => {
            say(&why);
            return UNAVAILABLE;
        }
    };
    let started = Instant::now();
    let src = match checkout(layout, &request, &lane) {
        Ok(src) => src,
        Err(why) => {
            say(&format!("cannot check out tree {}: {why}", request.tree));
            return client::TEMPFAIL;
        }
    };
    let log = match fs::File::create(results.join("log.txt")) {
        Ok(f) => Arc::new(Mutex::new(f)),
        Err(e) => {
            say(&format!("cannot write the log: {e}"));
            return client::TEMPFAIL;
        }
    };
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).current_dir(&src).env_clear();
    for key in KEPT.iter().chain(FROM_RUN.iter()) {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("PRESSROOM_RESULTS", &out)
        .env("PRESSROOM_TREE", &request.tree)
        .envs(recipe.env.iter().map(|(k, v)| (k, v)))
        .envs(request.env.iter())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    {
        let mut f = log.lock().unwrap();
        let _ = writeln!(
            f,
            "[pressroom] job {id}: {} {} tree {}",
            request.repo, request.kind, request.tree
        );
        let _ = writeln!(f, "[pressroom] in {}: {}", src.display(), argv.join(" "));
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            say(&format!("cannot start {}: {e}", argv[0]));
            return 127;
        }
    };
    let pgid = child.id() as i32;
    let pumps: Vec<_> = [
        child
            .stdout
            .take()
            .map(|o| Box::new(o) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|e| Box::new(e) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|mut from| {
        let log = log.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(got) = from.read(&mut buf) {
                if got == 0 {
                    break;
                }
                let _ = log.lock().unwrap().write_all(&buf[..got]);
                let _ = std::io::stderr().write_all(&buf[..got]);
            }
        })
    })
    .collect();
    let deadline = started + Duration::from_secs(recipe.timeout_secs);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {}
            Err(_) => break child.wait().expect("the job can be waited for"),
        }
        if !timed_out && Instant::now() >= deadline {
            timed_out = true;
            say(&format!(
                "job {id} ran past its {} minutes; ending it",
                recipe.timeout_secs / 60
            ));
            // SAFETY: signals the job's own process group, which it was started in.
            unsafe { kill(-pgid, 15) };
            let grace = Instant::now() + Duration::from_secs(10);
            while Instant::now() < grace && matches!(child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(100));
            }
            unsafe { kill(-pgid, 9) };
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    for p in pumps {
        let _ = p.join();
    }
    let code = status
        .code()
        .or_else(|| status.signal().map(|g| 128 + g))
        .unwrap_or(1);
    let _ = writeln!(
        log.lock().unwrap(),
        "[pressroom] exit {code}{}",
        if timed_out { " (timed out)" } else { "" }
    );
    let ran = obj(vec![
        ("exit", n(code)),
        ("seconds", n64(started.elapsed().as_secs())),
        ("timedOut", Value::Bool(timed_out)),
    ]);
    let _ = fs::write(results.join("result.json"), json::write(&ran));
    code
}

/// `pressroom job show ID`: its result once it has ended, else that it is still going.
pub fn show(layout: &Layout, id: &str) -> i32 {
    if !recipe::valid_id(id) {
        say("job show needs a job id");
        return USAGE;
    }
    let dir = results_dir(layout, id);
    if !dir.join("request.json").exists() {
        answer(obj(vec![("state", s("unknown")), ("id", s(id))]));
        return 1;
    }
    match fs::read_to_string(dir.join("result.json"))
        .ok()
        .and_then(|t| json::parse(&t).ok())
    {
        Some(r) if r.str("state") == Some("done") => answer(r),
        _ => answer(obj(vec![("state", s("running")), ("id", s(id))])),
    }
    0
}

/// `pressroom job fetch ID`: the job's results folder as a tar stream on standard output.
pub fn fetch(layout: &Layout, id: &str) -> i32 {
    if !recipe::valid_id(id) {
        say("job fetch needs a job id");
        return USAGE;
    }
    let dir = results_dir(layout, id);
    if !dir.is_dir() {
        say(&format!("no job {id} on this machine"));
        return 1;
    }
    match Command::new("tar")
        .args(["-c", "-f", "-", "-C"])
        .arg(&dir)
        .arg(".")
        .status()
    {
        Ok(st) if st.success() => 0,
        _ => 1,
    }
}

/// `pressroom job ssh`: for an `authorized_keys` forced command. Only these verbs, and
/// only a job id as an argument, whatever the connection asked for.
pub fn ssh(layout: &Layout) -> i32 {
    let asked = std::env::var("SSH_ORIGINAL_COMMAND").unwrap_or_default();
    let words: Vec<&str> = asked.split_whitespace().collect();
    let at = words
        .iter()
        .position(|w| *w == "job")
        .map(|i| i + 1)
        .unwrap_or(words.len());
    match &words[at.min(words.len())..] {
        ["probe"] => probe(layout),
        ["run"] => run(layout),
        ["show", id] => show(layout, id),
        ["fetch", id] => fetch(layout, id),
        _ => {
            say("this key may only run: pressroom job probe | run | show ID | fetch ID");
            USAGE
        }
    }
}

/// `pressroom job recipes`: what an owner reads after putting a recipe in place.
pub fn recipes(layout: &Layout) -> i32 {
    let dir = recipes_dir(layout);
    let (ok, bad) = recipe::load_all(&dir);
    if ok.is_empty() && bad.is_empty() {
        println!("no recipes in {}", dir.display());
    }
    for r in &ok {
        println!("{} {} ({}): {}", r.repo, r.kind, r.name, r.argv.join(" "));
        println!(
            "    {} threads, {} GB memory, up to {} GB disk, {} minutes, lane toolchain {}",
            r.threads,
            r.memory_gb,
            r.disk_gb,
            r.timeout_secs / 60,
            r.toolchain
        );
        for p in &r.params {
            println!(
                "    {} {}: {}",
                if p.list { "list" } else { "param" },
                p.name,
                p.patterns.join(" | ")
            );
        }
        for (tool, version) in &r.needs {
            println!("    needs {tool} {version}");
        }
    }
    for b in &bad {
        println!("UNREADABLE {b}");
    }
    if bad.is_empty() {
        0
    } else {
        1
    }
}
