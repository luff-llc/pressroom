//! What the owner declared about this machine, and what can be measured on it.

use crate::admit::{Kind, Limits, Measured, GB};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Where pressroom keeps its files: `$PRESSROOM_HOME`, else `~/.pressroom`.
pub fn home() -> PathBuf {
    if let Some(dir) = std::env::var_os("PRESSROOM_HOME") {
        return PathBuf::from(dir);
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into())).join(".pressroom")
}

pub struct Layout {
    pub home: PathBuf,
}

impl Layout {
    pub fn socket(&self) -> PathBuf {
        self.home.join("sock")
    }
    pub fn leases(&self) -> PathBuf {
        self.home.join("leases")
    }
    pub fn lanes(&self) -> PathBuf {
        self.home.join("lanes")
    }
    pub fn events(&self) -> PathBuf {
        self.home.join("events.log")
    }
    pub fn config(&self) -> PathBuf {
        self.home.join("machine.conf")
    }
    /// The folders a cleanup tool may touch that pressroom itself creates.
    pub fn built_in_managed(&self) -> Vec<PathBuf> {
        ["lanes", "checkouts", "scratch", "results"]
            .iter()
            .map(|d| self.home.join(d))
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub name: String,
    pub limits: Limits,
    /// Folders a cleanup tool may touch, the built-in ones included.
    pub managed: Vec<PathBuf>,
    pub grace_secs: u64,
    /// Where to send the report (contracts/report-v1.md), if anywhere.
    pub galley: Option<Galley>,
}

/// The three settings that turn the report on. All three or none.
#[derive(Clone, Debug, PartialEq)]
pub struct Galley {
    /// `https://host[:port]`, nothing after it; `http://` only for a loopback host.
    pub origin: String,
    /// The machine's agent identity configuration.
    pub credential: PathBuf,
    /// The program that sends the report.
    pub sender: PathBuf,
}

/// An address with a scheme, a host and perhaps a port, and nothing else.
pub fn galley_origin(value: &str) -> Option<String> {
    let (scheme, rest) = value.split_once("://")?;
    let host = rest.rsplit_once(':').map_or(rest, |(h, port)| {
        if !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit()) {
            h
        } else {
            rest
        }
    });
    let host_ok = !host.is_empty()
        && (host == "[::1]"
            || host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'));
    let loopback = ["localhost", "127.0.0.1", "[::1]"].contains(&host);
    let ok = host_ok && (scheme == "https" || (scheme == "http" && loopback));
    ok.then(|| value.to_string())
}

/// Read `machine.conf`: lines of `key = value`, `#` for comments. Unknown keys and bad
/// values are errors, so a typing mistake never silently loosens a limit.
pub fn load_config(layout: &Layout, total_memory: u64) -> Result<Config, String> {
    let mut config = Config {
        name: hostname(),
        limits: Limits {
            // The careful default: treat an undeclared machine as somebody's own.
            kind: Kind::Workstation,
            share_present: 0.5,
            share_away: 0.75,
            memory_reserve: total_memory / 4,
            disk_floor: 20 * GB,
            away_after_secs: 300,
        },
        managed: layout
            .built_in_managed()
            .iter()
            .map(|dir| crate::lease::normalise(dir, Path::new("/")))
            .collect(),
        grace_secs: 24 * 3600,
        galley: None,
    };
    let (mut origin, mut credential, mut sender) = (None, None, None);
    let path = layout.config();
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(config),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    for (i, line) in text.lines().enumerate() {
        // A comment starts with `#` at the start of a line or after a space, so a `#`
        // inside a path is part of the path.
        let line = match line.find(" #").or_else(|| line.find("\t#")) {
            _ if line.trim_start().starts_with('#') => "",
            Some(at) => &line[..at],
            None => line,
        }
        .trim();
        if line.is_empty() {
            continue;
        }
        let bad = |why: &str| format!("{} line {}: {why}", path.display(), i + 1);
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| bad("expected key = value"))?;
        let (key, value) = (key.trim(), value.trim());
        let number = || {
            value
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite() && *n >= 0.0)
        };
        let share = || number().filter(|n| *n > 0.0 && *n <= 1.0);
        match key {
            "name" if !value.is_empty() => config.name = value.to_string(),
            "kind" => {
                config.limits.kind = match value {
                    "workstation" => Kind::Workstation,
                    "worker" | "server" => Kind::Worker,
                    _ => return Err(bad("kind is workstation, worker or server")),
                }
            }
            "share_present" => {
                config.limits.share_present =
                    share().ok_or_else(|| bad("a share is above 0, up to 1"))?
            }
            "share_away" => {
                config.limits.share_away =
                    share().ok_or_else(|| bad("a share is above 0, up to 1"))?
            }
            "memory_reserve_gb" => {
                config.limits.memory_reserve =
                    (number().ok_or_else(|| bad("expected a number"))? * GB as f64) as u64
            }
            "disk_floor_gb" => {
                config.limits.disk_floor =
                    (number().ok_or_else(|| bad("expected a number"))? * GB as f64) as u64
            }
            "away_after_minutes" => {
                config.limits.away_after_secs =
                    (number().ok_or_else(|| bad("expected a number"))? * 60.0) as u64
            }
            "grace_hours" => {
                config.grace_secs =
                    (number().ok_or_else(|| bad("expected a number"))? * 3600.0) as u64
            }
            "managed" => {
                let dir = Path::new(value);
                if !dir.is_absolute() || dir.parent().is_none() {
                    return Err(bad("a managed folder is an absolute path, and not /"));
                }
                config
                    .managed
                    .push(crate::lease::normalise(dir, Path::new("/")));
            }
            "galley" => {
                origin = Some(galley_origin(value).ok_or_else(|| {
                    bad("galley is https:// and a host, with nothing after it")
                })?)
            }
            "galley_credential" | "galley_sender" => {
                let file = Path::new(value);
                if !file.is_absolute() || file.components().any(|c| {
                    matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir)
                }) {
                    return Err(bad("expected an absolute path"));
                }
                if key == "galley_credential" {
                    credential = Some(file.to_path_buf());
                } else {
                    sender = Some(file.to_path_buf());
                }
            }
            _ => return Err(bad("unknown setting")),
        }
    }
    config.galley = match (origin, credential, sender) {
        (None, None, None) => None,
        (Some(origin), Some(credential), Some(sender)) => Some(Galley {
            origin,
            credential,
            sender,
        }),
        _ => {
            return Err(format!(
                "{}: galley, galley_credential and galley_sender go together; set all three or none",
                path.display()
            ))
        }
    };
    Ok(config)
}

fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn hostname() -> String {
    output("hostname", &[])
        .map(|h| h.split('.').next().unwrap_or("").to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "machine".into())
}

pub fn total_memory() -> u64 {
    if let Ok(text) = fs::read_to_string("/proc/meminfo") {
        return parse_meminfo(&text).unwrap_or(0);
    }
    output("sysctl", &["-n", "hw.memsize"])
        .and_then(|t| t.parse().ok())
        .unwrap_or(0)
}

pub fn parse_meminfo(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with("MemTotal:"))?;
    line.split_whitespace()
        .nth(1)?
        .parse::<u64>()
        .ok()
        .map(|kb| kb * 1024)
}

/// Processor ticks since boot, as `(busy, all)`.
#[cfg(target_os = "macos")]
fn processor_ticks() -> Option<(u64, u64)> {
    extern "C" {
        // From libSystem, which every program on macOS already links.
        fn mach_host_self() -> u32;
        fn host_statistics(host: u32, flavor: i32, info: *mut u32, count: *mut u32) -> i32;
    }
    const HOST_CPU_LOAD_INFO: i32 = 3;
    static HOST: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    // Ticks in the order user, system, idle, nice.
    let mut ticks = [0u32; 4];
    let mut count = ticks.len() as u32;
    // SAFETY: `ticks` has room for the four counters this flavor writes, and `count`
    // says so; the host port is the caller's own.
    let result = unsafe {
        let host = *HOST.get_or_init(|| mach_host_self());
        host_statistics(host, HOST_CPU_LOAD_INFO, ticks.as_mut_ptr(), &mut count)
    };
    if result != 0 || count < 4 {
        return None;
    }
    let [user, system, idle, nice] = ticks.map(u64::from);
    Some((user + system + nice, user + system + nice + idle))
}

#[cfg(not(target_os = "macos"))]
fn processor_ticks() -> Option<(u64, u64)> {
    parse_proc_stat(&fs::read_to_string("/proc/stat").ok()?)
}

/// The first line of Linux's `/proc/stat`: `cpu  user nice system idle iowait irq softirq
/// steal ...`. Waiting on a disk is not use of the processor.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn parse_proc_stat(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let n: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .filter_map(|v| v.parse().ok())
        .collect();
    if n.len() < 4 {
        return None;
    }
    let all: u64 = n.iter().sum();
    let idle = n[3] + n.get(4).copied().unwrap_or(0);
    Some((all - idle, all))
}

/// Threads' worth of processor used between two readings of the ticks. The counters on
/// macOS are 32 bits wide and wrap; a reading that went backwards gives nothing.
pub fn busy_between(before: (u64, u64), after: (u64, u64), threads: u32) -> Option<f64> {
    let all = after.1.checked_sub(before.1)?;
    let busy = after.0.checked_sub(before.0)?;
    (all > 0 && busy <= all).then(|| threads as f64 * busy as f64 / all as f64)
}

/// Threads' worth of processor in use since this was last asked, a few seconds ago. The
/// first time, it reads twice a short moment apart.
fn busy(threads: u32) -> Option<f64> {
    #[cfg(debug_assertions)]
    {
        if let Some(fixed) = std::env::var("PRESSROOM_TEST_LOAD")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            return Some(fixed);
        }
    }
    static LAST: Mutex<Option<(u64, u64)>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let before = match *last {
        Some(before) => before,
        None => {
            let first = processor_ticks()?;
            std::thread::sleep(std::time::Duration::from_millis(250));
            first
        }
    };
    let after = processor_ticks()?;
    *last = Some(after);
    busy_between(before, after, threads)
}

fn load1() -> f64 {
    // Debug builds only, for tests: a fixed load, so that a busy machine does not change
    // their result.
    #[cfg(debug_assertions)]
    {
        if let Some(fixed) = std::env::var("PRESSROOM_TEST_LOAD")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            return fixed;
        }
    }
    if let Ok(text) = fs::read_to_string("/proc/loadavg") {
        return parse_load(&text).unwrap_or(0.0);
    }
    output("sysctl", &["-n", "vm.loadavg"])
        .and_then(|t| parse_load(&t))
        .unwrap_or(0.0)
}

/// Linux gives `5.65 6.56 5.72 2/1234 99`; macOS gives `{ 5.65 6.56 5.72 }`.
pub fn parse_load(text: &str) -> Option<f64> {
    text.split_whitespace()
        .find_map(|part| part.parse::<f64>().ok())
}

fn disk_free(dir: &Path) -> u64 {
    // -P: one line per filesystem; -k: kilobytes. The same on Linux and macOS.
    let mut probe = dir.to_path_buf();
    while !probe.exists() {
        if !probe.pop() {
            break;
        }
    }
    output("df", &["-Pk", &probe.to_string_lossy()])
        .and_then(|t| parse_df(&t))
        .unwrap_or(0)
}

pub fn parse_df(text: &str) -> Option<u64> {
    let line = text.lines().nth(1)?;
    line.split_whitespace()
        .nth(3)?
        .parse::<u64>()
        .ok()
        .map(|kb| kb * 1024)
}

/// Seconds since the person last used the keyboard or pointer. Known on macOS only.
fn idle_secs() -> Option<u64> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    parse_hid_idle(&output("ioreg", &["-c", "IOHIDSystem", "-d", "4"])?)
}

pub fn parse_hid_idle(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.contains("\"HIDIdleTime\""))?;
    let nanos: u64 = line.rsplit('=').next()?.trim().parse().ok()?;
    Some(nanos / 1_000_000_000)
}

pub fn measure(home: &Path) -> Measured {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    Measured {
        threads,
        busy: busy(threads),
        memory: total_memory(),
        disk_free: disk_free(home),
        load1: load1(),
        idle_secs: idle_secs(),
    }
}

/// The parts of a measurement that change from second to second.
pub fn remeasure(home: &Path, m: &mut Measured) {
    m.disk_free = disk_free(home);
    m.load1 = load1();
    // A reading over too short a time says little: keep the last one if this one fails.
    m.busy = busy(m.threads).or(m.busy);
    m.idle_secs = idle_secs();
}

/// Toolchains found on this machine, as `(name, version line)`. Measured, never typed.
/// Where to look for a toolchain: the daemon's PATH, then the folders people usually install
/// into. A daemon started at login by launchd gets only the system PATH, and version managers
/// (rustup, mise, nvm, bun) put their programs under the person's home.
pub fn tool_dirs(path: Option<&str>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut more: Vec<PathBuf> = path
        .unwrap_or("")
        .split(':')
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .collect();
    more.push(PathBuf::from("/opt/homebrew/bin"));
    more.push(PathBuf::from("/usr/local/bin"));
    if let Some(home) = home {
        for sub in [".local/bin", ".cargo/bin", ".local/share/mise/shims", ".bun/bin"] {
            more.push(home.join(sub));
        }
        // Newest first: toolchain and node version folders sort by name.
        for (parent, child) in [(".rustup/toolchains", "bin"), (".nvm/versions/node", "bin")] {
            let mut found: Vec<PathBuf> = fs::read_dir(home.join(parent))
                .map(|entries| entries.flatten().map(|e| e.path().join(child)).collect())
                .unwrap_or_default();
            found.sort_by_key(|p| std::cmp::Reverse(version_key(p)));
            more.extend(found);
        }
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    for dir in more {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// Sort key for a version folder name: runs of digits compare as numbers, so `1.100` is
/// newer than `1.99` and `v22` than `v9`.
fn version_key(path: &Path) -> Vec<(u64, String)> {
    let name = path
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut key = Vec::new();
    let mut digits = String::new();
    let mut other = String::new();
    for c in name.chars() {
        if c.is_ascii_digit() {
            if !other.is_empty() {
                key.push((0, std::mem::take(&mut other)));
            }
            digits.push(c);
        } else {
            if !digits.is_empty() {
                key.push((digits.parse().unwrap_or(u64::MAX), String::new()));
                digits.clear();
            }
            other.push(c);
        }
    }
    if !digits.is_empty() {
        key.push((digits.parse().unwrap_or(u64::MAX), String::new()));
    }
    if !other.is_empty() {
        key.push((0, other));
    }
    key
}

pub fn toolchains() -> Vec<(String, String)> {
    let path = std::env::var("PATH").ok();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dirs = tool_dirs(path.as_deref(), home.as_deref());
    let mut found = Vec::new();
    let probes: [(&str, &str, &[&str]); 5] = [
        ("rustc", "rustc", &["--version"]),
        ("cargo", "cargo", &["--version"]),
        ("bun", "bun", &["--version"]),
        ("node", "node", &["--version"]),
        ("xcode", "xcodebuild", &["-version"]),
    ];
    for (name, program, args) in probes {
        // The first copy that answers: a broken link or a stale shim is passed over.
        let text = dirs
            .iter()
            .map(|dir| dir.join(program))
            .filter(|candidate| candidate.is_file())
            .find_map(|candidate| output(&candidate.to_string_lossy(), args));
        if let Some(text) = text {
            let line = text.lines().collect::<Vec<_>>().join(", ");
            if !line.is_empty() {
                found.push((name.to_string(), line));
            }
        }
    }
    found
}

pub fn os() -> String {
    output("uname", &["-sr"]).unwrap_or_else(|| std::env::consts::OS.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_are_looked_for_beyond_the_path() {
        let home = std::env::temp_dir().join(format!("pr-tools-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        for dir in [
            ".rustup/toolchains/1.98.0-x/bin",
            ".rustup/toolchains/1.99.1-x/bin",
            ".nvm/versions/node/v9.0.0/bin",
            ".nvm/versions/node/v22.1.0/bin",
        ] {
            fs::create_dir_all(home.join(dir)).unwrap();
        }
        let dirs = tool_dirs(Some("/usr/bin:/bin:/usr/bin"), Some(&home));
        assert_eq!(dirs[0], PathBuf::from("/usr/bin"));
        assert_eq!(dirs[1], PathBuf::from("/bin"));
        let usr_bin = Path::new("/usr/bin");
        assert_eq!(dirs.iter().filter(|d| d.as_path() == usr_bin).count(), 1);
        let cargo = dirs.iter().position(|d| d.ends_with(".cargo/bin")).unwrap();
        let newest = dirs.iter().position(|d| d.ends_with("1.99.1-x/bin")).unwrap();
        let older = dirs.iter().position(|d| d.ends_with("1.98.0-x/bin")).unwrap();
        assert!(cargo < newest && newest < older, "{dirs:?}");
        let v22 = dirs.iter().position(|d| d.ends_with("v22.1.0/bin")).unwrap();
        let v9 = dirs.iter().position(|d| d.ends_with("v9.0.0/bin")).unwrap();
        assert!(v22 < v9, "numbers compare as numbers: {dirs:?}");
        assert!(dirs.contains(&home.join(".local/share/mise/shims")));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn parsers() {
        assert_eq!(parse_load("5.65 6.56 5.72 2/1234 99"), Some(5.65));
        assert_eq!(parse_load("{ 30.12 6.56 5.72 }"), Some(30.12));
        // user nice system idle iowait irq softirq steal: idle and iowait are not use.
        let stat = "cpu  100 20 30 800 40 5 5 0 0 0\ncpu0 1 1 1 1 1 1 1 1 0 0\n";
        assert_eq!(parse_proc_stat(stat), Some((160, 1000)));
        assert_eq!(parse_proc_stat("intr 5\n"), None);
        // A third of 16 threads in use between two readings.
        assert_eq!(busy_between((100, 1000), (460, 2000), 16), Some(5.76));
        // No time passed, or a counter wrapped: nothing to say.
        assert_eq!(busy_between((100, 1000), (100, 1000), 16), None);
        assert_eq!(busy_between((100, 1000), (50, 900), 16), None);
        assert_eq!(
            parse_meminfo("MemTotal:       32000000 kB\nMemFree: 1 kB"),
            Some(32_768_000_000)
        );
        let df = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk3s1 971350180 13000000 130000000 10% /";
        assert_eq!(parse_df(df), Some(130_000_000 * 1024));
        assert_eq!(
            parse_hid_idle("  | \"HIDIdleTime\" = 4200000000\n"),
            Some(4)
        );
        assert_eq!(parse_hid_idle("nothing"), None);
    }

    #[test]
    fn config_file() {
        let home = PathBuf::from(format!("/tmp/pr-conf-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        let layout = Layout { home: home.clone() };

        let c = load_config(&layout, 64 * GB).unwrap();
        assert_eq!(c.limits.kind, Kind::Workstation);
        assert_eq!(c.limits.memory_reserve, 16 * GB);
        assert_eq!(c.managed.len(), 4);

        fs::write(
            layout.config(),
            "# omarchy\nname = omarchy\nkind = worker\nshare_away = 1\nmemory_reserve_gb = 4\n\
             disk_floor_gb = 30 # keep room\ngrace_hours = 12\nmanaged = /home/galley/ci\n",
        )
        .unwrap();
        let c = load_config(&layout, 64 * GB).unwrap();
        assert_eq!(c.name, "omarchy");
        assert_eq!(c.limits.kind, Kind::Worker);
        assert_eq!(c.limits.share_away, 1.0);
        assert_eq!(c.limits.memory_reserve, 4 * GB);
        assert_eq!(c.limits.disk_floor, 30 * GB);
        assert_eq!(c.grace_secs, 12 * 3600);
        assert_eq!(c.managed.len(), 5);
        fs::write(layout.config(), "managed = /srv/ci#keep\n").unwrap();
        let c = load_config(&layout, GB).unwrap();
        assert!(c.managed[4].to_string_lossy().ends_with("ci#keep"));

        assert!(c.galley.is_none());
        fs::write(
            layout.config(),
            "galley = https://galley.example.test\ngalley_credential = /home/galley/.pressroom/identity.json\n\
             galley_sender = /opt/galley/bin/galley-machine-report\n",
        )
        .unwrap();
        let g = load_config(&layout, GB).unwrap().galley.unwrap();
        assert_eq!(g.origin, "https://galley.example.test");
        assert_eq!(g.sender, PathBuf::from("/opt/galley/bin/galley-machine-report"));
        fs::write(
            layout.config(),
            "galley = http://127.0.0.1:8080\ngalley_credential = /a/b\ngalley_sender = /c/d\n",
        )
        .unwrap();
        assert!(load_config(&layout, GB).is_ok());

        for bad in [
            "galley = https://galley.example.test",
            "galley = https://galley.example.test\ngalley_credential = /a/b",
            "galley_sender = /c/d",
            "galley = http://galley.example.test\ngalley_credential = /a/b\ngalley_sender = /c/d",
            "galley = https://galley.example.test/v1\ngalley_credential = /a/b\ngalley_sender = /c/d",
            "galley = https://user@galley.example.test\ngalley_credential = /a/b\ngalley_sender = /c/d",
            "galley = https://galley.example.test\ngalley_credential = a/b\ngalley_sender = /c/d",
            "galley = https://galley.example.test\ngalley_credential = /a/../b\ngalley_sender = /c/d",
            "share_present = 1.5",
            "share_away = 0",
            "kind = laptop",
            "threads = 4",
            "managed = /",
            "managed = ci",
            "oops",
        ] {
            fs::write(layout.config(), bad).unwrap();
            assert!(load_config(&layout, GB).is_err(), "{bad}");
        }
        let _ = fs::remove_dir_all(&home);
    }
}
