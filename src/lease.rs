//! Leases: what is in use on this machine, written as one small file each so that a
//! cleanup tool that knows nothing else about pressroom can honour them.
//!
//! See `contracts/local-v1.md` for the file format and the rules.

use crate::json::{self, n64, obj, s, strs, Value};
use std::fs;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

pub const VERSION: &str = "pressroom.lease/1";
pub const KINDS: [&str; 7] = [
    "cache",
    "worktree",
    "toolchain",
    "registry",
    "result",
    "scratch",
    "delete",
];
/// The longest a lease without a live process may run before it must be renewed.
pub const MAX_TTL_SECS: u64 = 30 * 24 * 3600;

#[derive(Clone, Debug, PartialEq)]
pub struct Lease {
    pub id: String,
    pub holder: String,
    /// One of `KINDS`. `delete` means the holder is removing the paths.
    pub kind: String,
    /// `in-use` or `warm`.
    pub mode: String,
    /// Absolute, with no `.` or `..` parts.
    pub paths: Vec<String>,
    /// Unix seconds.
    pub created: u64,
    /// Unix seconds. Past this the lease is dead whatever else is true.
    pub expires: u64,
    /// When set, the lease is also dead once this process is gone.
    pub pid: Option<u32>,
}

impl Lease {
    pub fn to_json(&self) -> Value {
        let mut fields = vec![
            ("version", s(VERSION)),
            ("id", s(&self.id)),
            ("holder", s(&self.holder)),
            ("kind", s(&self.kind)),
            ("mode", s(&self.mode)),
            ("paths", strs(&self.paths)),
            ("created", n64(self.created)),
            ("expires", n64(self.expires)),
        ];
        if let Some(pid) = self.pid {
            fields.push(("pid", n64(pid as u64)));
        }
        obj(fields)
    }

    pub fn from_json(v: &Value) -> Result<Lease, String> {
        if v.str("version") != Some(VERSION) {
            return Err("unknown lease version".into());
        }
        let lease = Lease {
            id: v.str("id").ok_or("lease has no id")?.to_string(),
            holder: v.str("holder").ok_or("lease has no holder")?.to_string(),
            kind: v.str("kind").ok_or("lease has no kind")?.to_string(),
            mode: v.str("mode").unwrap_or("in-use").to_string(),
            paths: v.strings("paths"),
            created: v.u64("created").unwrap_or(0),
            expires: v.u64("expires").ok_or("lease has no expiry")?,
            pid: v.u64("pid").and_then(|p| u32::try_from(p).ok()),
        };
        if !valid_id(&lease.id) {
            return Err("bad lease id".into());
        }
        if !KINDS.contains(&lease.kind.as_str()) {
            return Err(format!("unknown lease kind {:?}", lease.kind));
        }
        if lease.paths.is_empty() || lease.paths.iter().any(|p| !Path::new(p).is_absolute()) {
            return Err("lease paths must be absolute".into());
        }
        Ok(lease)
    }

    pub fn live(&self, now: u64, alive: &dyn Fn(u32) -> bool) -> bool {
        now < self.expires && self.pid.is_none_or(alive)
    }

    /// A lease covers a path when the path is a leased path, is inside one, or contains
    /// one. Removing a parent of something leased is as bad as removing the thing.
    pub fn covers(&self, path: &Path) -> bool {
        self.paths.iter().any(|leased| {
            let leased = Path::new(leased);
            path.starts_with(leased) || leased.starts_with(path)
        })
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Absolute and free of `.` and `..`, without requiring the path to exist. Symbolic
/// links are resolved for the longest part of the path that exists, so that
/// `/tmp/x` and `/private/tmp/x` are the same lease on macOS.
pub fn normalise(path: &Path, cwd: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    let mut tail = Vec::new();
    let mut head = out.clone();
    loop {
        if let Ok(real) = fs::canonicalize(&head) {
            let mut resolved = real;
            for part in tail.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        match head.file_name() {
            Some(name) => tail.push(name.to_os_string()),
            None => return out,
        }
        if !head.pop() {
            return out;
        }
    }
}

pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if cfg!(target_os = "linux") {
        return Path::new(&format!("/proc/{pid}")).exists();
    }
    // `kill -0` sends nothing; it only asks whether the process exists. A process owned
    // by another user reports a permission error, which also means it exists.
    match Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(out) => {
            out.status.success()
                || String::from_utf8_lossy(&out.stderr)
                    .to_lowercase()
                    .contains("not permitted")
        }
        // If we cannot tell, say alive: keeping something is the safe mistake.
        Err(_) => true,
    }
}

/// The folder of lease files.
pub struct Store {
    pub dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Store {
        Store { dir }
    }

    fn file(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Write the whole file under another name, then rename it into place, so a reader
    /// never sees half a lease.
    pub fn put(&self, lease: &Lease) -> Result<(), String> {
        fs::create_dir_all(&self.dir).map_err(|e| format!("cannot create {:?}: {e}", self.dir))?;
        let tmp = self.dir.join(format!(".{}.tmp", lease.id));
        let mut f = fs::File::create(&tmp).map_err(|e| format!("cannot write lease: {e}"))?;
        f.write_all(json::write(&lease.to_json()).as_bytes())
            .and_then(|_| f.write_all(b"\n"))
            .map_err(|e| format!("cannot write lease: {e}"))?;
        fs::rename(&tmp, self.file(&lease.id)).map_err(|e| format!("cannot write lease: {e}"))
    }

    pub fn remove(&self, id: &str) {
        if valid_id(id) {
            let _ = fs::remove_file(self.file(id));
        }
    }

    pub fn get(&self, id: &str) -> Option<Lease> {
        if !valid_id(id) {
            return None;
        }
        let text = fs::read_to_string(self.file(id)).ok()?;
        Lease::from_json(&json::parse(&text).ok()?).ok()
    }

    /// Every readable lease, and the names of files that could not be read. A file that
    /// cannot be read is reported, never ignored: see `blockers`.
    pub fn all(&self) -> (Vec<Lease>, Vec<String>) {
        let mut leases = Vec::new();
        let mut unreadable = Vec::new();
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return (leases, unreadable);
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".json") || name.starts_with('.') {
                continue;
            }
            let parsed = fs::read_to_string(entry.path())
                .map_err(|e| e.to_string())
                .and_then(|text| json::parse(&text))
                .and_then(|v| Lease::from_json(&v));
            match parsed {
                Ok(lease) => leases.push(lease),
                Err(_) => unreadable.push(name),
            }
        }
        leases.sort_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)));
        (leases, unreadable)
    }

    pub fn live(&self, now: u64) -> Vec<Lease> {
        self.all()
            .0
            .into_iter()
            .filter(|l| l.live(now, &pid_alive))
            .collect()
    }

    /// Remove the files of dead leases. Returns how many went.
    pub fn sweep(&self, now: u64) -> usize {
        let dead: Vec<Lease> = self
            .all()
            .0
            .into_iter()
            .filter(|l| !l.live(now, &pid_alive))
            .collect();
        for lease in &dead {
            self.remove(&lease.id);
        }
        dead.len()
    }

    /// Live leases that cover a path, leaving out `except` (the asker's own lease).
    pub fn blockers(&self, path: &Path, now: u64, except: Option<&str>) -> Vec<Lease> {
        self.live(now)
            .into_iter()
            .filter(|l| Some(l.id.as_str()) != except && l.covers(path))
            .collect()
    }
}

/// May a cleanup tool remove this path? The answer the `may-delete` command gives.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    Yes,
    No(String),
}

pub struct DeleteRules<'a> {
    pub managed: &'a [PathBuf],
    /// A path used more recently than this many seconds ago is left alone.
    pub grace_secs: u64,
}

pub fn may_delete(
    store: &Store,
    rules: &DeleteRules,
    path: &Path,
    now: u64,
    own_lease: Option<&str>,
) -> Verdict {
    // Rule 1: nothing outside the managed folders, and never a managed folder itself.
    let inside = rules
        .managed
        .iter()
        .any(|root| path.starts_with(root) && path != root);
    if !inside {
        return Verdict::No("outside the managed folders".into());
    }
    // An unreadable lease file might be the lease on this path.
    let (_, unreadable) = store.all();
    if let Some(name) = unreadable.first() {
        return Verdict::No(format!(
            "lease file {name} cannot be read; fix or remove it first"
        ));
    }
    if let Some(lease) = store.blockers(path, now, own_lease).first() {
        let what = if lease.kind == "delete" {
            "being removed by"
        } else {
            "leased by"
        };
        return Verdict::No(format!(
            "{what} {} ({}, lease {})",
            lease.holder, lease.kind, lease.id
        ));
    }
    if let Ok(meta) = fs::symlink_metadata(path) {
        let age = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|t| now.saturating_sub(t.as_secs()));
        if let Some(age) = age {
            if age < rules.grace_secs {
                return Verdict::No(format!(
                    "changed {} ago; left alone until {} unused",
                    span(age),
                    span(rules.grace_secs)
                ));
            }
        }
    }
    Verdict::Yes
}

pub fn span(secs: u64) -> String {
    match secs {
        0..=119 => format!("{secs} s"),
        120..=7199 => format!("{} min", secs / 60),
        7200..=172_799 => format!("{} h", secs / 3600),
        _ => format!("{} days", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pressroom-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(&dir).unwrap()
    }
    fn lease(id: &str, kind: &str, path: &Path, expires: u64, pid: Option<u32>) -> Lease {
        Lease {
            id: id.into(),
            holder: "agent:test".into(),
            kind: kind.into(),
            mode: "in-use".into(),
            paths: vec![path.to_string_lossy().to_string()],
            created: 1,
            expires,
            pid,
        }
    }

    #[test]
    fn file_round_trip() {
        let root = temp("round");
        let store = Store::new(root.join("leases"));
        let l = lease("a-1", "registry", &root.join("registry"), 100, Some(42));
        store.put(&l).unwrap();
        assert_eq!(store.get("a-1"), Some(l.clone()));
        assert_eq!(store.all().0, vec![l]);
        store.remove("a-1");
        assert!(store.all().0.is_empty());
        assert_eq!(store.get("../a-1"), None);
    }

    #[test]
    fn liveness() {
        let l = lease("a", "cache", Path::new("/x"), 100, Some(7));
        assert!(l.live(50, &|_| true));
        assert!(!l.live(100, &|_| true), "expired");
        assert!(!l.live(50, &|_| false), "process gone");
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
    }

    #[test]
    fn covering() {
        let l = lease(
            "a",
            "cache",
            Path::new("/home/g/.cargo/registry"),
            100,
            None,
        );
        assert!(l.covers(Path::new("/home/g/.cargo/registry")));
        assert!(l.covers(Path::new("/home/g/.cargo/registry/cache/x.crate")));
        assert!(
            l.covers(Path::new("/home/g/.cargo")),
            "a parent of a leased path"
        );
        assert!(!l.covers(Path::new("/home/g/.cargo/registry2")));
        assert!(!l.covers(Path::new("/home/g/.cargo/bin")));
    }

    #[test]
    fn normalising() {
        let cwd = Path::new("/a/b");
        assert_eq!(
            normalise(Path::new("c/../d/./e"), cwd),
            PathBuf::from("/a/b/d/e")
        );
        assert_eq!(normalise(Path::new("/x/../../y"), cwd), PathBuf::from("/y"));
        let root = temp("norm");
        fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        assert_eq!(
            normalise(&root.join("link/not-yet/there"), cwd),
            root.join("real/not-yet/there")
        );
    }

    #[test]
    fn cleanup_rules() {
        let root = temp("rules");
        let managed = vec![root.join("lanes")];
        let lane = root.join("lanes/colophon/0");
        fs::create_dir_all(&lane).unwrap();
        let store = Store::new(root.join("leases"));
        let rules = DeleteRules {
            managed: &managed,
            grace_secs: 0,
        };
        let now = 1_000_000_000_000; // far past any file time, so grace never applies

        // The registry is not in a managed folder: never, leased or not.
        assert!(matches!(
            may_delete(&store, &rules, &root.join("registry"), now, None),
            Verdict::No(_)
        ));
        // A managed folder itself is not removable.
        assert!(matches!(
            may_delete(&store, &rules, &managed[0], now, None),
            Verdict::No(_)
        ));
        // Unleased and under a managed folder: yes.
        assert_eq!(may_delete(&store, &rules, &lane, now, None), Verdict::Yes);

        // Leased by a live process: no, for the lane, its children and its parents.
        store
            .put(&lease(
                "job",
                "cache",
                &lane,
                now + 60,
                Some(std::process::id()),
            ))
            .unwrap();
        for path in [
            lane.clone(),
            lane.join("target"),
            root.join("lanes/colophon"),
        ] {
            match may_delete(&store, &rules, &path, now, None) {
                Verdict::No(why) => assert!(why.contains("leased by"), "{why}"),
                Verdict::Yes => panic!("{path:?} must be kept"),
            }
        }
        // The holder's process is gone: the lease is dead and the lane may go.
        store
            .put(&lease("job", "cache", &lane, now + 60, Some(0)))
            .unwrap();
        assert_eq!(may_delete(&store, &rules, &lane, now, None), Verdict::Yes);
        assert_eq!(store.sweep(now), 1);

        // A delete lease blocks others, not its own holder.
        store
            .put(&lease("del", "delete", &lane, now + 60, None))
            .unwrap();
        assert!(matches!(
            may_delete(&store, &rules, &lane, now, None),
            Verdict::No(_)
        ));
        assert_eq!(
            may_delete(&store, &rules, &lane, now, Some("del")),
            Verdict::Yes
        );
        store.remove("del");

        // A lease file that cannot be read stops deletion: it might cover the path.
        fs::write(root.join("leases/broken.json"), "{not json").unwrap();
        match may_delete(&store, &rules, &lane, now, None) {
            Verdict::No(why) => assert!(why.contains("broken.json")),
            Verdict::Yes => panic!("must refuse"),
        }
    }

    #[test]
    fn grace() {
        let root = temp("grace");
        let managed = vec![root.clone()];
        let fresh = root.join("fresh");
        fs::create_dir_all(&fresh).unwrap();
        let store = Store::new(root.join("leases"));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let day = DeleteRules {
            managed: &managed,
            grace_secs: 86_400,
        };
        assert!(matches!(
            may_delete(&store, &day, &fresh, now, None),
            Verdict::No(_)
        ));
        assert_eq!(
            may_delete(&store, &day, &fresh, now + 90_000, None),
            Verdict::Yes
        );
    }
}
