//! Holding a running job to its grant, and measuring what it really uses
//! (contracts/local-v1.md, rules 23 to 25 and 28).
//!
//! Two means. On Linux with the cpu controller delegated to the user, the command runs in a
//! transient systemd scope with a processor quota and a memory limit, and the kernel holds
//! it. Elsewhere `run` watches the job's processes twenty times a second and stops them
//! (`SIGSTOP`) whenever they have used more than the grant, continuing them (`SIGCONT`)
//! once the grant has caught up: a token bucket, like the kernel's own quota.
//!
//! The bucket and the ledger are pure and tested on their own; the rest reads the system.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How often a job held by stopping is looked at.
pub const TICK: Duration = Duration::from_millis(50);
/// How often `run` tells the daemon what its job uses.
pub const REPORT_EVERY: Duration = Duration::from_secs(2);
/// The most processes one job is watched for. Beyond this, new ones are held by their
/// process group only, and the job is told.
const MAX_TRACKED: usize = 8192;
/// The most process groups and loose processes sent to the daemon in one line (rule 28).
pub const MAX_REPORTED: usize = 4096;
/// A job may run ahead of its grant by this many seconds of its grant before it is held.
const BURST_SECS: f64 = 0.2;

/// How a job is held.
#[derive(Clone, Debug, PartialEq)]
pub enum Means {
    /// A systemd scope; the kernel holds it. The path is the scope's cgroup folder.
    Scope(PathBuf),
    /// Stopping and continuing its processes.
    Stop,
    /// Measured, not held (`cap = measure`).
    Measure,
    /// Part of another job, whose hold holds it (rule 26), until that job ends.
    Within,
}

impl Means {
    pub fn name(&self) -> &'static str {
        match self {
            Means::Scope(_) => "scope",
            Means::Stop => "stop",
            Means::Measure => "measure",
            Means::Within => "within",
        }
    }
}

/// The kernel's quota, done in user space: a job earns its grant in processor seconds as
/// time passes, spends what it uses, and is held while it is in debt.
#[derive(Clone, Debug)]
pub struct Bucket {
    grant: f64,
    tokens: f64,
}

impl Bucket {
    pub fn new(grant: f64) -> Bucket {
        Bucket {
            grant,
            tokens: grant * BURST_SECS,
        }
    }
    /// `secs` have passed and the job used `used` processor seconds in them. Whether it
    /// must be held now.
    pub fn spend(&mut self, secs: f64, used: f64) -> bool {
        self.tokens = (self.tokens + self.grant * secs - used).min(self.grant * BURST_SECS);
        self.tokens < 0.0
    }
}

/// One process of a job, read once.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    /// When it started, in the system's own units: with the pid, it names the process.
    pub start: u64,
    /// Processor seconds it has used itself.
    pub own: f64,
    /// Processor seconds of its children that have ended and that it has waited for.
    pub children: f64,
    /// Bytes of memory it holds.
    pub memory: u64,
}

#[derive(Clone, Debug)]
struct Seen {
    start: u64,
    ppid: i32,
    total: f64,
}

/// Adds up the processor time of a job's processes over its life, including processes
/// that have ended. A process that ends and whose parent is still watched will be counted
/// in that parent's children's time once the parent has waited for it; one whose parent
/// is not watched keeps what was last seen of it.
#[derive(Debug, Default)]
pub struct Ledger {
    seen: HashMap<i32, Seen>,
    banked: f64,
    cumulative: f64,
}

impl Ledger {
    /// Take one reading of every live process of the job. Returns the processor seconds
    /// the job has used since it began; never less than the last reading gave.
    pub fn update(&mut self, live: &[Sample]) -> f64 {
        let now: HashMap<i32, &Sample> = live.iter().map(|s| (s.pid, s)).collect();
        for (pid, seen) in &self.seen {
            let same = now.get(pid).is_some_and(|s| s.start == seen.start);
            if same {
                continue;
            }
            let parent_watched = now.get(&seen.ppid).is_some_and(|p| {
                self.seen
                    .get(&seen.ppid)
                    .is_some_and(|q| q.start == p.start)
            });
            if !parent_watched {
                self.banked += seen.total;
            }
        }
        self.seen = live
            .iter()
            .map(|s| {
                (
                    s.pid,
                    Seen {
                        start: s.start,
                        ppid: s.ppid,
                        total: s.own + s.children,
                    },
                )
            })
            .collect();
        let total = self.banked + live.iter().map(|s| s.own + s.children).sum::<f64>();
        self.cumulative = self.cumulative.max(total);
        self.cumulative
    }
}

/// Finds a job's processes: its first process; every descendant of it; everything in the
/// process group of any process found, since only the job's own processes make such
/// groups (a tool that runs each command in a group of its own, as Codex does, and a
/// daemon that forks twice inside it, are both found this way); and every process found
/// before that still lives, even after its parent has gone.
#[derive(Debug)]
pub struct Tracker {
    root: i32,
    pgid: i32,
    known: HashMap<i32, u64>,
    groups: HashSet<i32>,
    /// The groups of the processes found at the last look, by process.
    of: HashMap<i32, i32>,
    looks: u64,
    /// More processes than are watched.
    pub crowded: bool,
}

/// On Linux listing a group's members means reading every process, so it is done every
/// so many looks, or when a new group is found.
const GROUP_EVERY: u64 = if cfg!(target_os = "macos") { 1 } else { 10 };

impl Tracker {
    pub fn new(root: i32, pgid: i32) -> Tracker {
        Tracker {
            root,
            pgid,
            known: HashMap::new(),
            groups: HashSet::new(),
            of: HashMap::new(),
            looks: 0,
            crowded: false,
        }
    }

    /// The process groups the job's processes are in, its own first.
    pub fn groups(&self) -> Vec<i32> {
        let mut rest: Vec<i32> = self
            .groups
            .iter()
            .copied()
            .filter(|&g| g != self.pgid)
            .collect();
        rest.sort_unstable();
        let mut out = vec![self.pgid];
        out.extend(rest);
        out
    }

    /// The job's processes outside all of its groups: a process that made a session of its
    /// own, say. A group is reached by one signal; these each need their own.
    pub fn loose(&self) -> Vec<i32> {
        let groups = self.groups();
        let mut out: Vec<i32> = self
            .of
            .iter()
            .filter(|(_, g)| !groups.contains(g))
            .map(|(&pid, _)| pid)
            .collect();
        out.sort_unstable();
        out
    }

    /// Read the job's live processes now.
    pub fn samples(&mut self) -> Vec<Sample> {
        self.looks += 1;
        let not_ours = sys::own_group();
        let mut groups: HashSet<i32> = HashSet::from([self.pgid]);
        let mut listed: HashSet<i32> = HashSet::new();
        let mut todo: Vec<i32> = vec![self.root];
        todo.extend(self.known.keys().copied());
        let list_known = (self.looks - 1).checked_rem(GROUP_EVERY) == Some(0);
        let mut all: HashMap<i32, Sample> = HashMap::new();
        let mut looked: HashSet<i32> = HashSet::new();
        let mut accepted: HashSet<i32> = HashSet::new();
        loop {
            // List the members of groups not listed yet in this look.
            let fresh: Vec<i32> = groups
                .iter()
                .copied()
                .filter(|g| !listed.contains(g) && (list_known || !self.groups.contains(g)))
                .collect();
            if !fresh.is_empty() {
                listed.extend(fresh.iter().copied());
                todo.extend(sys::members(&fresh));
            }
            while let Some(pid) = todo.pop() {
                if looked.len() >= MAX_TRACKED {
                    self.crowded = true;
                    continue;
                }
                if pid <= 1 || !looked.insert(pid) {
                    continue;
                }
                if let Some(sample) = sys::sample(pid) {
                    all.insert(pid, sample);
                }
            }
            // Accept what is ours; the children and groups of what was accepted are
            // looked at next.
            let mut grew = false;
            for (pid, sample) in &all {
                if accepted.contains(pid) {
                    continue;
                }
                let ours = *pid == self.root
                    || groups.contains(&sample.pgid)
                    || accepted.contains(&sample.ppid)
                    || self.known.get(pid) == Some(&sample.start);
                if ours {
                    accepted.insert(*pid);
                    todo.extend(sys::children(*pid));
                    if sample.pgid > 1 && sample.pgid != not_ours {
                        groups.insert(sample.pgid);
                    }
                    grew = true;
                }
            }
            let unlisted = groups.iter().any(|g| !listed.contains(g))
                && groups
                    .iter()
                    .any(|g| !listed.contains(g) && (list_known || !self.groups.contains(g)));
            if !grew && todo.is_empty() && !unlisted {
                break;
            }
        }
        let out: Vec<Sample> = all
            .into_values()
            .filter(|s| accepted.contains(&s.pid))
            .collect();
        self.known = out.iter().map(|s| (s.pid, s.start)).collect();
        self.of = out.iter().map(|s| (s.pid, s.pgid)).collect();
        self.groups = groups;
        out
    }
}

/// Something a job is told (rule 25): one line on its standard error, and an event.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub event: &'static str,
    pub text: String,
}

/// What a job used over the last report.
#[derive(Clone, Debug)]
pub struct Use {
    pub threads: Option<f64>,
    pub memory: Option<u64>,
    pub held: bool,
    pub held_secs: f64,
}

/// What a job held by stopping may be stopped by: its process groups, and its processes
/// outside them. The daemon is told these before any stop (rule 28).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Targets {
    pub groups: Vec<i32>,
    pub pids: Vec<i32>,
}

/// Holds and measures one job.
pub struct Watch {
    pub means: Means,
    grant: u32,
    memory_cap: u64,
    tracker: Tracker,
    ledger: Ledger,
    bucket: Bucket,
    stopped: bool,
    stopped_since: Option<Instant>,
    held_secs: f64,
    told_held: bool,
    told_crowded: bool,
    held_in_window: bool,
    last_tick: Instant,
    last_cpu: f64,
    last_report: Instant,
    reported_cpu: f64,
    memory: Option<u64>,
    pids: Vec<i32>,
    targets: Targets,
    /// Asked to end: it is no longer held, so it can end promptly.
    letting_go: bool,
    // A scope's counters at the last report.
    throttled: u64,
    periods: u64,
    oom_kills: u64,
}

fn threads_word(n: u32) -> String {
    if n == 1 {
        "1 thread".into()
    } else {
        format!("{n} threads")
    }
}

impl Watch {
    pub fn new(means: Means, root: i32, pgid: i32, grant: u32, memory_cap: u64) -> Watch {
        let now = Instant::now();
        let (throttled, periods, oom_kills) = match &means {
            Means::Scope(dir) => (
                scope_counter(dir, "cpu.stat", "nr_throttled").unwrap_or(0),
                scope_counter(dir, "cpu.stat", "nr_periods").unwrap_or(0),
                scope_counter(dir, "memory.events", "oom_kill").unwrap_or(0),
            ),
            _ => (0, 0, 0),
        };
        Watch {
            means,
            grant,
            memory_cap,
            tracker: Tracker::new(root, pgid),
            ledger: Ledger::default(),
            bucket: Bucket::new(grant as f64),
            stopped: false,
            stopped_since: None,
            held_secs: 0.0,
            told_held: false,
            told_crowded: false,
            held_in_window: false,
            last_tick: now,
            last_cpu: 0.0,
            last_report: now,
            reported_cpu: 0.0,
            memory: None,
            pids: Vec::new(),
            targets: Targets {
                groups: vec![pgid],
                pids: vec![root],
            },
            letting_go: false,
            throttled,
            periods,
            oom_kills,
        }
    }

    /// What it may be stopped by, as last told to the daemon.
    pub fn targets(&self) -> &Targets {
        &self.targets
    }

    /// The job this ran inside has ended (rule 26): from now on it holds itself, by
    /// stopping, to what it was given.
    pub fn on_its_own(&mut self) -> Notice {
        self.means = Means::Stop;
        self.bucket = Bucket::new(self.grant as f64);
        self.last_tick = Instant::now();
        Notice {
            event: "on_its_own",
            text: format!(
                "the job this ran inside has ended; it is now a job of its own, held to {}",
                threads_word(self.grant)
            ),
        }
    }

    /// Look at the job once; called every `TICK`. Holds or releases a job held by
    /// stopping. `before_stop` is given the targets whenever they changed, before they
    /// are stopped. Returns what the job is to be told.
    pub fn tick(&mut self, before_stop: &mut dyn FnMut(&Targets)) -> Vec<Notice> {
        let mut notices = Vec::new();
        if let Means::Scope(dir) = &self.means {
            // A scope needs nothing between reports but a look at its memory events.
            let kills = scope_counter(dir, "memory.events", "oom_kill").unwrap_or(self.oom_kills);
            if kills > self.oom_kills {
                self.oom_kills = kills;
                notices.push(Notice {
                    event: "memory_ended",
                    text: format!(
                        "the system ended a process of this job for passing its memory cap of {}",
                        crate::admit::bytes(self.memory_cap)
                    ),
                });
            }
            return notices;
        }
        if self.means != Means::Stop {
            return notices;
        }
        let now = Instant::now();
        let secs = now.duration_since(self.last_tick).as_secs_f64();
        self.last_tick = now;
        let used = self.look();
        if self.tracker.crowded && !self.told_crowded {
            self.told_crowded = true;
            notices.push(Notice {
                event: "not_held",
                text: format!(
                    "this job has more than {MAX_TRACKED} processes; those beyond are held \
                     only through their process groups"
                ),
            });
        }
        let hold = self.bucket.spend(secs, used) && !self.letting_go;
        if hold {
            // Tell the daemon first what is about to be stopped, so a `run` killed while
            // its job is stopped leaves nothing it does not know of.
            let targets = Targets {
                groups: self.tracker.groups(),
                pids: self.tracker.loose(),
            };
            if targets != self.targets {
                before_stop(&targets);
                self.targets = targets;
            }
            self.signal(sys::SIGSTOP);
            if !self.stopped {
                self.stopped = true;
                self.stopped_since = Some(now);
                self.held_in_window = true;
                if !self.told_held {
                    self.told_held = true;
                    notices.push(self.held_notice());
                }
            }
        } else if self.stopped {
            self.release();
        }
        notices
    }

    /// Read the job's processes. Returns the processor seconds used since the last look.
    fn look(&mut self) -> f64 {
        let samples = self.tracker.samples();
        let cpu = self.ledger.update(&samples);
        let used = (cpu - self.last_cpu).max(0.0);
        self.last_cpu = cpu;
        self.memory = Some(samples.iter().map(|s| s.memory).sum());
        self.pids = samples.iter().map(|s| s.pid).collect();
        used
    }

    /// The job was asked to end: continue it and hold it no more.
    pub fn let_go(&mut self) {
        self.letting_go = true;
        self.release();
    }

    fn held_notice(&self) -> Notice {
        Notice {
            event: "held",
            text: format!(
                "this job tried to use more than its {}; it is held to {} and runs slower",
                threads_word(self.grant),
                self.grant
            ),
        }
    }

    fn signal(&self, sig: i32) {
        // Every group of the job catches what was started since the last look.
        let mine = sys::own_group();
        for g in self.tracker.groups() {
            if g > 1 && g != mine {
                sys::kill(-g, sig);
            }
        }
        for &pid in &self.pids {
            if pid > 1 {
                sys::kill(pid, sig);
            }
        }
    }

    /// Continue everything that was stopped.
    pub fn release(&mut self) {
        if let Some(since) = self.stopped_since.take() {
            self.held_secs += since.elapsed().as_secs_f64();
        }
        if self.stopped || self.means == Means::Stop {
            self.signal(sys::SIGCONT);
        }
        self.stopped = false;
    }

    /// What the job used since the last report. For a scope, also what it is to be told.
    pub fn report(&mut self) -> (Use, Vec<Notice>) {
        let mut notices = Vec::new();
        let now = Instant::now();
        let secs = now
            .duration_since(self.last_report)
            .as_secs_f64()
            .max(0.001);
        self.last_report = now;
        let (cpu, held) = match &self.means {
            Means::Scope(dir) => {
                let cpu = scope_counter(dir, "cpu.stat", "usage_usec").map(|us| us as f64 / 1e6);
                // The working set: what the scope holds less file cache it is not using,
                // which the kernel takes back before it would end anything.
                self.memory = scope_counter(dir, "memory.current", "").map(|current| {
                    current.saturating_sub(
                        scope_counter(dir, "memory.stat", "inactive_file").unwrap_or(0),
                    )
                });
                let throttled = scope_counter(dir, "cpu.stat", "nr_throttled").unwrap_or(0);
                let periods = scope_counter(dir, "cpu.stat", "nr_periods").unwrap_or(0);
                let (throttled_now, periods_now) = (
                    throttled.saturating_sub(self.throttled),
                    periods.saturating_sub(self.periods),
                );
                (self.throttled, self.periods) = (throttled, periods);
                // Held back: throttled in at least half the periods it wanted to run in.
                // A short burst past the quota now and then is not that.
                let held = periods_now > 0 && throttled_now * 2 >= periods_now;
                // The periods in which the kernel throttled it: an upper bound.
                self.held_secs += throttled_now as f64 * scope_period(dir);
                if held && !self.told_held {
                    self.told_held = true;
                    notices.push(self.held_notice());
                }
                (cpu, held)
            }
            Means::Measure => {
                self.look();
                (Some(self.last_cpu), false)
            }
            Means::Stop => {
                let held = self.held_in_window || self.stopped;
                self.held_in_window = self.stopped;
                (Some(self.last_cpu), held)
            }
            Means::Within => (None, false),
        };
        let threads = cpu.map(|cpu| {
            let t = (cpu - self.reported_cpu).max(0.0) / secs;
            self.reported_cpu = cpu;
            t
        });
        let mut held_secs = self.held_secs;
        if let Some(since) = self.stopped_since {
            held_secs += since.elapsed().as_secs_f64();
        }
        (
            Use {
                threads,
                memory: self.memory,
                held,
                held_secs,
            },
            notices,
        )
    }

    /// The job's first process has ended: continue anything still stopped, and say for
    /// how long it was held back.
    pub fn finish(&mut self) -> (f64, Vec<Notice>) {
        let notices = self.tick(&mut |_| {});
        self.release();
        if let Means::Scope(_) = self.means {
            let _ = self.report();
        }
        (self.held_secs, notices)
    }
}

fn scope_period(dir: &std::path::Path) -> f64 {
    std::fs::read_to_string(dir.join("cpu.max"))
        .ok()
        .and_then(|t| t.split_whitespace().nth(1)?.parse::<f64>().ok())
        .map_or(0.1, |us| us / 1e6)
}

/// A number from a cgroup file: `key value` lines, or with an empty key the whole file.
fn scope_counter(dir: &std::path::Path, file: &str, key: &str) -> Option<u64> {
    let text = std::fs::read_to_string(dir.join(file)).ok()?;
    parse_counter(&text, key)
}

pub fn parse_counter(text: &str, key: &str) -> Option<u64> {
    if key.is_empty() {
        return text.trim().parse().ok();
    }
    text.lines().find_map(|line| {
        let (k, v) = line.split_once(' ')?;
        (k == key).then(|| v.trim().parse().ok()).flatten()
    })
}

/// The systemd unit a job's scope is given.
pub fn scope_unit(job: &str) -> String {
    let clean: String = job
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("pressroom-{clean}-{}", std::process::id())
}

/// Whether this process is already inside a job's scope. A `run` there must not make a
/// scope of its own: systemd would move it out of the one that holds it.
pub fn in_a_scope() -> bool {
    std::fs::read_to_string("/proc/self/cgroup").is_ok_and(|t| {
        t.lines()
            .any(|l| l.contains("/pressroom-") && l.ends_with(".scope"))
    })
}

/// The environment `systemd-run --user` needs: its runtime folder, which a shell started
/// over SSH without a login may not have set.
pub fn user_bus_env() -> Option<(String, String)> {
    if std::env::var_os("XDG_RUNTIME_DIR").is_some() {
        return None;
    }
    let dir = format!("/run/user/{}", sys::uid());
    std::path::Path::new(&dir)
        .is_dir()
        .then(|| ("XDG_RUNTIME_DIR".to_string(), dir))
}

/// Whether a scope here would hold a job: `systemd-run --user --scope` runs, and the
/// scope it makes has the cpu and memory controllers. Asked once per job; it takes a
/// twentieth of a second or so.
pub fn scope_works() -> bool {
    if !cfg!(target_os = "linux") || in_a_scope() {
        return false;
    }
    let mut probe = std::process::Command::new("systemd-run");
    probe
        .args([
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            // The job's own scope is made with this; a systemd too old for it fails here.
            "--expand-environment=no",
            "-p",
            "CPUQuota=100%",
            "-p",
            "MemoryMax=1073741824",
            "--",
            "/bin/sh",
            "-c",
            "d=/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup); cat \"$d/cpu.max\" \"$d/memory.max\"",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if let Some((k, v)) = user_bus_env() {
        probe.env(k, v);
    }
    let Ok(mut child) = probe.spawn() else {
        return false;
    };
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        use std::io::Read;
        let _ = stdout.read_to_string(&mut out);
    }
    probe_says_held(&out)
}

/// What the probe printed: `cpu.max` then `memory.max` of its scope.
pub fn probe_says_held(out: &str) -> bool {
    let mut lines = out.lines();
    let cpu = lines.next().unwrap_or("");
    let memory = lines.next().unwrap_or("");
    cpu.split_whitespace().next() == Some("100000") && memory.trim() == "1073741824"
}

/// The cgroup folder of a process, if it is the scope named `unit`.
pub fn scope_of(pid: i32, unit: &str) -> Option<PathBuf> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let path = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    path.ends_with(&format!("/{unit}.scope"))
        .then(|| PathBuf::from(format!("/sys/fs/cgroup{path}")))
}

/// Find a program the way `exec` would, so a missing one gives 127 before anything that
/// would wrap it can give its own code.
pub fn find_program(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let runnable = |p: &std::path::Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if name.contains('/') {
        let p = PathBuf::from(name);
        return runnable(&p).then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| runnable(p))
}

/// The system calls this needs. libc is linked into every program already; these are
/// declared here because the standard library does not expose them.
pub mod sys {

    #[cfg(target_os = "macos")]
    pub const SIGSTOP: i32 = 17;
    #[cfg(target_os = "macos")]
    pub const SIGCONT: i32 = 19;
    #[cfg(not(target_os = "macos"))]
    pub const SIGSTOP: i32 = 19;
    #[cfg(not(target_os = "macos"))]
    pub const SIGCONT: i32 = 18;
    pub const SIGHUP: i32 = 1;
    pub const SIGINT: i32 = 2;
    pub const SIGQUIT: i32 = 3;
    pub const SIGTERM: i32 = 15;
    pub const SIGTTOU: i32 = 22;

    extern "C" {
        #[link_name = "kill"]
        fn c_kill(pid: i32, sig: i32) -> i32;
        fn getuid() -> u32;
        fn signal(sig: i32, handler: usize) -> usize;
        fn isatty(fd: i32) -> i32;
        fn tcgetpgrp(fd: i32) -> i32;
        fn tcsetpgrp(fd: i32, pgrp: i32) -> i32;
        fn getpgrp() -> i32;
    }

    /// The process group `run` itself is in: never one of its job's.
    pub fn own_group() -> i32 {
        // SAFETY: no arguments, no memory.
        unsafe { getpgrp() }
    }

    /// Send a signal; a negative pid is a process group.
    pub fn kill(pid: i32, sig: i32) -> bool {
        // SAFETY: kill takes two integers and touches no memory of ours.
        unsafe { c_kill(pid, sig) == 0 }
    }

    pub fn uid() -> u32 {
        // SAFETY: no arguments, no memory.
        unsafe { getuid() }
    }

    static CAUGHT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

    extern "C" fn caught(sig: i32) {
        CAUGHT.store(sig, std::sync::atomic::Ordering::SeqCst);
    }

    /// Catch the signals that ask `run` to end, so it can pass them on to its job.
    pub fn catch_endings() {
        for sig in [SIGHUP, SIGINT, SIGQUIT, SIGTERM] {
            // SAFETY: the handler only stores to an atomic, which is safe in a handler.
            unsafe {
                signal(sig, caught as extern "C" fn(i32) as usize);
            }
        }
    }

    /// A signal caught since this was last asked.
    pub fn take_caught() -> Option<i32> {
        match CAUGHT.swap(0, std::sync::atomic::Ordering::SeqCst) {
            0 => None,
            sig => Some(sig),
        }
    }

    /// If `run` is the terminal's foreground, hand the terminal to the job's group, so
    /// that the person's Ctrl-C and typing reach the job as before. Returns whether it did.
    pub fn give_terminal(pgid: i32) -> bool {
        // SAFETY: plain calls on file descriptor 0; SIGTTOU is ignored around the change
        // so a process group not in the foreground may make it.
        unsafe {
            if isatty(0) != 1 || tcgetpgrp(0) != getpgrp() {
                return false;
            }
            let old = signal(SIGTTOU, 1);
            let ok = tcsetpgrp(0, pgid) == 0;
            signal(SIGTTOU, old);
            ok
        }
    }

    /// Take the terminal back for `run`'s own group.
    pub fn take_terminal() {
        // SAFETY: as in `give_terminal`.
        unsafe {
            let old = signal(SIGTTOU, 1);
            tcsetpgrp(0, getpgrp());
            signal(SIGTTOU, old);
        }
    }

    #[cfg(target_os = "macos")]
    pub mod os {
        use crate::cap::Sample;
        use std::ffi::c_void;

        extern "C" {
            fn proc_listchildpids(ppid: i32, buffer: *mut c_void, size: i32) -> i32;
            fn proc_listpgrppids(pgid: i32, buffer: *mut c_void, size: i32) -> i32;
            fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut c_void, size: i32)
                -> i32;
            fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut c_void) -> i32;
            fn mach_timebase_info(info: *mut [u32; 2]) -> i32;
        }

        const PROC_PIDTBSDINFO: i32 = 3;
        const BSDINFO_SIZE: usize = 136;
        const RUSAGE_INFO_V2: i32 = 2;

        fn list(f: unsafe extern "C" fn(i32, *mut c_void, i32) -> i32, id: i32) -> Vec<i32> {
            let mut buf = vec![0i32; 4096];
            // SAFETY: the buffer holds 4096 pids and its size in bytes is passed.
            let n = unsafe { f(id, buf.as_mut_ptr().cast(), (buf.len() * 4) as i32) };
            if n <= 0 {
                return Vec::new();
            }
            buf.truncate((n as usize).min(4096));
            buf.retain(|&p| p > 0);
            buf
        }

        pub fn children(pid: i32) -> Vec<i32> {
            list(proc_listchildpids, pid)
        }

        /// The processes in any of these groups.
        pub fn members(pgids: &[i32]) -> Vec<i32> {
            pgids
                .iter()
                .filter(|&&g| g > 1)
                .flat_map(|&g| list(proc_listpgrppids, g))
                .collect()
        }

        /// Whether a process exists and has not ended. One that has ended but whose parent
        /// has not yet waited for it (a zombie) still answers a signal, so ask its state.
        pub fn running(pid: i32) -> bool {
            const SZOMB: u32 = 5;
            let mut info = [0u8; BSDINFO_SIZE];
            // SAFETY: the buffer is the size of struct proc_bsdinfo.
            let n = unsafe {
                proc_pidinfo(
                    pid,
                    PROC_PIDTBSDINFO,
                    0,
                    info.as_mut_ptr().cast(),
                    BSDINFO_SIZE as i32,
                )
            };
            n as usize == BSDINFO_SIZE
                && u32::from_ne_bytes(info[4..8].try_into().unwrap()) != SZOMB
        }

        /// Nanoseconds per unit of the processor times rusage gives. On Apple silicon they
        /// are in the machine's time base, not nanoseconds.
        fn ns_per_unit() -> f64 {
            static RATIO: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
            *RATIO.get_or_init(|| {
                let mut tb = [0u32; 2];
                // SAFETY: the struct is two 32-bit numbers.
                let ok = unsafe { mach_timebase_info(&mut tb) } == 0;
                if ok && tb[1] != 0 {
                    tb[0] as f64 / tb[1] as f64
                } else {
                    1.0
                }
            })
        }

        pub fn sample(pid: i32) -> Option<Sample> {
            let mut info = [0u8; BSDINFO_SIZE];
            // SAFETY: the buffer is the size of struct proc_bsdinfo.
            let n = unsafe {
                proc_pidinfo(
                    pid,
                    PROC_PIDTBSDINFO,
                    0,
                    info.as_mut_ptr().cast(),
                    BSDINFO_SIZE as i32,
                )
            };
            if n as usize != BSDINFO_SIZE {
                return None;
            }
            let u32_at = |at: usize| u32::from_ne_bytes(info[at..at + 4].try_into().unwrap());
            let ppid = u32_at(16) as i32;
            let pgid = u32_at(100) as i32;
            let mut ru = [0u64; 20];
            // SAFETY: rusage_info_v2 is 16 bytes of uuid and 18 64-bit numbers: 160 bytes,
            // which the 20-number buffer covers.
            let ok = unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V2, ru.as_mut_ptr().cast()) } == 0;
            if !ok {
                return None;
            }
            // After the uuid (two numbers): user, system, ..., footprint at 9, start at 10,
            // child user at 12, child system at 13.
            let k = ns_per_unit() / 1e9;
            Some(Sample {
                pid,
                ppid,
                pgid,
                start: ru[10],
                own: (ru[2] + ru[3]) as f64 * k,
                children: (ru[12] + ru[13]) as f64 * k,
                memory: ru[9],
            })
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub mod os {
        use crate::cap::Sample;

        extern "C" {
            fn sysconf(name: i32) -> i64;
        }
        const SC_CLK_TCK: i32 = 2;
        const SC_PAGESIZE: i32 = 30;

        fn conf(name: i32, default: f64) -> f64 {
            // SAFETY: sysconf takes an integer and touches no memory of ours.
            let v = unsafe { sysconf(name) };
            if v > 0 {
                v as f64
            } else {
                default
            }
        }

        pub fn children(pid: i32) -> Vec<i32> {
            let mut out = Vec::new();
            let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
                return out;
            };
            for task in tasks.flatten() {
                if let Ok(text) = std::fs::read_to_string(task.path().join("children")) {
                    out.extend(
                        text.split_whitespace()
                            .filter_map(|p| p.parse::<i32>().ok()),
                    );
                }
            }
            out
        }

        /// The processes in any of these groups: one reading of every process.
        pub fn members(pgids: &[i32]) -> Vec<i32> {
            let wanted: Vec<i32> = pgids.iter().copied().filter(|&g| g > 1).collect();
            if wanted.is_empty() {
                return Vec::new();
            }
            let Ok(dir) = std::fs::read_dir("/proc") else {
                return Vec::new();
            };
            dir.flatten()
                .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
                .filter(|&pid| {
                    std::fs::read_to_string(format!("/proc/{pid}/stat"))
                        .ok()
                        .and_then(|t| parse_stat(&t, 1.0, 1.0))
                        .is_some_and(|s| wanted.contains(&s.pgid))
                })
                .collect()
        }

        /// `/proc/<pid>/stat`. A zombie gives nothing: its time is in its parent's.
        pub fn parse_stat(text: &str, ticks: f64, page: f64) -> Option<Sample> {
            let pid = text.split_whitespace().next()?.parse().ok()?;
            let rest = &text[text.rfind(')')? + 1..];
            let f: Vec<&str> = rest.split_whitespace().collect();
            let num = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok());
            if f.first() == Some(&"Z") {
                return None;
            }
            Some(Sample {
                pid,
                ppid: num(1)? as i32,
                pgid: num(2)? as i32,
                start: num(19)?,
                own: (num(11)? + num(12)?) as f64 / ticks,
                children: (num(13)? + num(14)?) as f64 / ticks,
                memory: (num(21)? as f64 * page) as u64,
            })
        }

        /// Whether a process exists and has not ended (a zombie has).
        pub fn running(pid: i32) -> bool {
            std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|t| {
                t.rfind(')')
                    .and_then(|at| t[at + 1..].split_whitespace().next())
                    .is_some_and(|state| state != "Z")
            })
        }

        pub fn sample(pid: i32) -> Option<Sample> {
            let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            parse_stat(&text, conf(SC_CLK_TCK, 100.0), conf(SC_PAGESIZE, 4096.0))
        }
    }

    pub use os::{children, members, running, sample};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bucket_holds_eight_spinning_threads_to_one() {
        // Eight threads, held whenever the bucket is in debt, over ten seconds.
        let mut b = Bucket::new(1.0);
        let (mut used, mut held) = (0.0, false);
        for _ in 0..200 {
            let spent = if held { 0.0 } else { 8.0 * 0.05 };
            used += spent;
            held = b.spend(0.05, spent);
        }
        let average = used / 10.0;
        assert!((0.9..=1.1).contains(&average), "{average}");
    }

    #[test]
    fn the_bucket_leaves_work_within_its_grant_alone() {
        let mut b = Bucket::new(2.0);
        for _ in 0..100 {
            assert!(!b.spend(0.05, 1.9 * 0.05));
        }
        // A short burst is allowed; a long one is not.
        assert!(!b.spend(0.05, 0.2));
        assert!(b.spend(0.05, 1.0));
    }

    fn sample(pid: i32, ppid: i32, own: f64, children: f64) -> Sample {
        Sample {
            pid,
            ppid,
            pgid: pid,
            start: pid as u64,
            own,
            children,
            memory: 0,
        }
    }

    #[test]
    fn the_ledger_counts_children_once() {
        let mut l = Ledger::default();
        // The job (10) runs a child (11).
        assert_eq!(
            l.update(&[sample(10, 1, 1.0, 0.0), sample(11, 10, 2.0, 0.0)]),
            3.0
        );
        // The child ends after using 2.5; its parent has not waited for it yet.
        assert_eq!(l.update(&[sample(10, 1, 1.5, 0.0)]), 3.0);
        // Now it has: its 2.5 is in the parent's children's time, counted once.
        assert_eq!(l.update(&[sample(10, 1, 1.5, 2.5)]), 4.0);
    }

    #[test]
    fn the_ledger_keeps_what_an_orphan_used() {
        let mut l = Ledger::default();
        l.update(&[sample(10, 1, 1.0, 0.0), sample(11, 10, 2.0, 0.0)]);
        // The parent ends; the child is adopted by init and goes on.
        assert_eq!(l.update(&[sample(11, 1, 3.0, 0.0)]), 4.0);
        // Then it ends too: what was seen of both is kept.
        assert_eq!(l.update(&[]), 4.0);
    }

    #[test]
    fn a_reused_pid_is_a_new_process() {
        let mut l = Ledger::default();
        l.update(&[sample(10, 1, 5.0, 0.0)]);
        let mut again = sample(10, 1, 1.0, 0.0);
        again.start = 99;
        assert_eq!(l.update(&[again]), 6.0);
    }

    #[test]
    fn counters_and_probe() {
        let stat = "usage_usec 2599727\nuser_usec 2584130\nnr_throttled 26\nthrottled_usec 1\n";
        assert_eq!(parse_counter(stat, "nr_throttled"), Some(26));
        assert_eq!(parse_counter(stat, "usage_usec"), Some(2599727));
        assert_eq!(parse_counter("123\n", ""), Some(123));
        assert_eq!(parse_counter("oom 0\noom_kill 2\n", "oom_kill"), Some(2));
        assert!(probe_says_held("100000 100000\n1073741824\n"));
        assert!(!probe_says_held("max 100000\nmax\n"));
        assert!(!probe_says_held(""));
        assert!(scope_unit("j-1-2").starts_with("pressroom-j-1-2-"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn proc_stat_is_read() {
        let text = "4242 (cargo test) S 4000 4242 4000 0 -1 0 0 0 0 0 150 50 300 100 20 0 1 0 98765 0 256 0";
        let s = sys::os::parse_stat(text, 100.0, 4096.0).unwrap();
        assert_eq!((s.pid, s.ppid, s.pgid, s.start), (4242, 4000, 4242, 98765));
        assert_eq!((s.own, s.children, s.memory), (2.0, 4.0, 256 * 4096));
    }

    #[test]
    fn a_zombie_is_not_running() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        let until = Instant::now() + Duration::from_secs(5);
        while sys::running(pid) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        // Ended, not yet waited for: it still answers a signal, but it is not running.
        assert!(sys::kill(pid, 0));
        assert!(!sys::running(pid));
        child.wait().unwrap();
        assert!(sys::running(std::process::id() as i32));
    }

    #[test]
    fn this_process_can_be_measured() {
        let me = std::process::id() as i32;
        let start = Instant::now();
        let mut x = 0u64;
        while start.elapsed() < Duration::from_millis(300) {
            x = x.wrapping_add(std::hint::black_box(1));
        }
        std::hint::black_box(x);
        let s = sys::sample(me).expect("sample of self");
        assert!(s.own >= 0.2 && s.own < 60.0, "{s:?}");
        assert!(s.memory > 0);
    }
}
