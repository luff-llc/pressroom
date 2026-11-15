//! What the owner declared about this machine, and what can be measured on it.

use crate::admit::{Kind, Limits, Measured, GB};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
    };
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
            _ => return Err(bad("unknown setting")),
        }
    }
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
    Measured {
        threads: std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1),
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
    m.idle_secs = idle_secs();
}

/// Toolchains found on this machine, as `(name, version line)`. Measured, never typed.
pub fn toolchains() -> Vec<(String, String)> {
    let mut found = Vec::new();
    let probes: [(&str, &str, &[&str]); 5] = [
        ("rustc", "rustc", &["--version"]),
        ("cargo", "cargo", &["--version"]),
        ("bun", "bun", &["--version"]),
        ("node", "node", &["--version"]),
        ("xcode", "xcodebuild", &["-version"]),
    ];
    for (name, program, args) in probes {
        if let Some(text) = output(program, args) {
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
    fn parsers() {
        assert_eq!(parse_load("5.65 6.56 5.72 2/1234 99"), Some(5.65));
        assert_eq!(parse_load("{ 30.12 6.56 5.72 }"), Some(30.12));
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

        for bad in [
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
