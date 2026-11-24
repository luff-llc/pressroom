//! Placing work on one of several machines (contracts/job-v1.md): ask each what it is
//! and holds, choose one, send the request and the source it lacks, wait, and bring the
//! results back.
//!
//! The choice is advisory. The machine chosen admits the work by its own rules, so two
//! placers that pick the same machine at once only make one of them wait there.

use crate::client::{TEMPFAIL, UNAVAILABLE, USAGE};
use crate::json::{self, Value};
use crate::machine::Layout;
use crate::recipe::{self, Request};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub enum Reach {
    /// This machine: `pressroom job ...` run directly.
    Local,
    /// Over SSH to `destination`, running the `pressroom` at `program` there.
    Ssh {
        destination: String,
        program: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub name: String,
    pub reach: Reach,
}

/// Read `machines.conf`: `machine NAME = local`, `machine NAME = ssh DESTINATION`, and
/// `pressroom NAME = PATH` for where the command is on a machine reached over SSH.
pub fn parse_machines(text: &str) -> Result<Vec<Target>, String> {
    let mut out: Vec<Target> = Vec::new();
    let mut programs: BTreeMap<String, String> = BTreeMap::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split(" #").next().unwrap_or("").trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = |why: String| format!("machines.conf line {}: {why}", n + 1);
        let Some((key, value)) = line.split_once('=') else {
            return Err(at(format!("expected key = value, not {line:?}")));
        };
        let words: Vec<&str> = key.split_whitespace().collect();
        let value: Vec<&str> = value.split_whitespace().collect();
        let good_name = |name: &str| recipe::valid_id(name);
        match (words.as_slice(), value.as_slice()) {
            (["machine", name], ["local"]) if good_name(name) => out.push(Target {
                name: name.to_string(),
                reach: Reach::Local,
            }),
            (["machine", name], ["ssh", destination])
                if good_name(name)
                    && !destination.starts_with('-')
                    && destination
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "@.-_:".contains(c)) =>
            {
                out.push(Target {
                    name: name.to_string(),
                    reach: Reach::Ssh {
                        destination: destination.to_string(),
                        program: String::new(),
                    },
                })
            }
            (["machine", _], ["hosted", ..]) => {
                return Err(at(
                    "the hosted tier is designed (docs/design/placement.md) but not built yet"
                        .into(),
                ))
            }
            (["pressroom", name], [path])
                if good_name(name)
                    && path
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "/._-~".contains(c)) =>
            {
                programs.insert(name.to_string(), path.to_string());
            }
            _ => return Err(at(format!("unknown setting or bad value: {line:?}"))),
        }
        if out
            .iter()
            .filter(|t| t.name == out.last().map(|l| l.name.clone()).unwrap_or_default())
            .count()
            > 1
        {
            return Err(at("that machine is named twice".into()));
        }
    }
    for t in &mut out {
        if let Reach::Ssh { program, .. } = &mut t.reach {
            *program = programs
                .remove(&t.name)
                .unwrap_or_else(|| ".local/bin/pressroom".into());
        }
    }
    if let Some(name) = programs.keys().next() {
        return Err(format!(
            "machines.conf names pressroom for {name}, which is not a machine"
        ));
    }
    Ok(out)
}

impl Target {
    /// `pressroom job VERB [ID]` on this target. Every word is fixed or a checked id, so
    /// the remote shell that SSH passes it through has nothing to interpret.
    fn job(&self, verb: &str, id: Option<&str>) -> Command {
        let mut words = vec!["job".to_string(), verb.to_string()];
        words.extend(id.map(str::to_string));
        match &self.reach {
            Reach::Local => {
                let mut c =
                    Command::new(std::env::current_exe().unwrap_or_else(|_| "pressroom".into()));
                c.args(words);
                c
            }
            Reach::Ssh {
                destination,
                program,
            } => {
                let mut c = Command::new("ssh");
                c.args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=10",
                    "-o",
                    "ServerAliveInterval=30",
                ])
                .arg(destination)
                .arg(program)
                .args(words);
                c
            }
        }
    }
}

/// A recipe as the choice sees it: threads, memory and disk in GB, and the tools it needs.
pub type Needs = (f64, f64, f64, Vec<(String, String)>);

/// What one machine said when asked, reduced to what the choice needs.
#[derive(Clone, Debug, PartialEq)]
pub struct Seen {
    pub name: String,
    /// Why it cannot be used at all, if it cannot.
    pub unusable: Option<String>,
    pub workstation: bool,
    pub person_present: bool,
    pub held_by_person: bool,
    pub threads: f64,
    pub offered: f64,
    pub used: f64,
    pub outside: f64,
    pub offered_memory: f64,
    pub disk_free: f64,
    pub disk_floor: f64,
    pub waiting: usize,
    pub toolchains: Vec<(String, String)>,
    /// The recipe's threads, memory, disk and needs, if it has the recipe.
    pub recipe: Option<Needs>,
    pub trees: Vec<String>,
}

pub fn read_probe(name: &str, probe: &Value, repo: &str, kind: &str) -> Seen {
    let status = probe.get("status").cloned().unwrap_or(Value::Null);
    let recipe = probe
        .arr("recipes")
        .iter()
        .find(|r| r.str("repo") == Some(repo) && r.str("kind") == Some(kind))
        .map(|r| {
            (
                r.num("threads").unwrap_or(1.0),
                r.num("memoryGb").unwrap_or(1.0),
                r.num("diskGb").unwrap_or(1.0),
                r.arr("needs")
                    .iter()
                    .map(|n| {
                        (
                            n.str("tool").unwrap_or("").to_string(),
                            n.str("version").unwrap_or("").to_string(),
                        )
                    })
                    .collect(),
            )
        });
    let num = |k: &str| status.num(k).unwrap_or(0.0);
    Seen {
        name: name.to_string(),
        unusable: if status == Value::Null {
            Some(format!(
                "its daemon did not answer: {}",
                probe.str("statusError").unwrap_or("no status")
            ))
        } else {
            None
        },
        workstation: status.str("kind") == Some("workstation"),
        person_present: status.get("personPresent") == Some(&Value::Bool(true)),
        held_by_person: status
            .get("personSays")
            .and_then(|p| p.str("mode").map(str::to_string))
            .as_deref()
            == Some("hold"),
        threads: num("threads"),
        offered: num("offeredThreads"),
        used: num("usedThreads"),
        outside: num("outsideLoad"),
        offered_memory: num("offeredMemoryGb"),
        disk_free: num("diskFreeGb"),
        disk_floor: num("diskFloorGb"),
        waiting: status.arr("waiting").len(),
        toolchains: status
            .arr("toolchains")
            .iter()
            .map(|t| {
                (
                    t.str("name").unwrap_or("").to_string(),
                    t.str("version").unwrap_or("").to_string(),
                )
            })
            .collect(),
        recipe,
        trees: probe
            .get("trees")
            .map(|t| t.strings(repo))
            .unwrap_or_default(),
    }
}

/// One machine's place in the choice: whether it can take the work, and why or why not.
#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    pub name: String,
    pub ok: bool,
    pub starts_now: bool,
    pub why: String,
    rank: (bool, bool, bool, i64),
}

/// Rules J6 to J9: which machines can take the work, best first.
pub fn choose(seen: &[Seen], tree: &str, needs: &[(String, String)]) -> Vec<Verdict> {
    let mut out: Vec<Verdict> = seen
        .iter()
        .map(|m| {
            let no = |why: String| Verdict {
                name: m.name.clone(),
                ok: false,
                starts_now: false,
                why,
                rank: (false, false, false, 0),
            };
            if let Some(why) = &m.unusable {
                return no(why.clone());
            }
            let Some((threads, memory, disk, recipe_needs)) = &m.recipe else {
                return no("it has no recipe for this work".into());
            };
            for (tool, version) in recipe_needs.iter().chain(needs.iter()) {
                match m.toolchains.iter().find(|(t, _)| t == tool) {
                    Some((_, have)) if recipe::version_meets(have, version) => {}
                    Some((_, have)) => return no(format!("needs {tool} {version}; it has {have}")),
                    None => return no(format!("needs {tool} {version}; it reports none")),
                }
            }
            if m.held_by_person {
                return no("its person has asked for the whole machine".into());
            }
            if *memory > m.offered_memory {
                return no(format!(
                    "needs {memory} GB of memory; it offers {:.0}",
                    m.offered_memory
                ));
            }
            if m.disk_free - disk < m.disk_floor {
                return no(format!(
                    "{:.0} GB free less up to {disk} GB written is below its floor of {:.0} GB",
                    m.disk_free, m.disk_floor
                ));
            }
            // As admission counts it (local rule 2): outside load comes first out of the
            // threads the machine keeps back, then out of the offer.
            let kept = (m.threads - m.offered).max(0.0);
            let free = (m.offered - m.used - (m.outside - kept).max(0.0)).max(0.0);
            let want = threads.min(m.offered).max(1.0);
            let starts_now = m.waiting == 0 && free >= (want / 2.0).max(1.0);
            let spare_person = !(m.workstation && m.person_present);
            let warm = m.trees.iter().any(|t| t == tree);
            let why = format!(
                "{}{:.0} of {:.0} offered threads free{}{}",
                if starts_now {
                    "starts now: "
                } else {
                    "would wait: "
                },
                free,
                m.offered,
                if m.waiting > 0 {
                    format!(", {} waiting", m.waiting)
                } else {
                    String::new()
                },
                if warm { ", has the tree" } else { "" },
            );
            Verdict {
                name: m.name.clone(),
                ok: true,
                starts_now,
                why,
                rank: (
                    starts_now,
                    spare_person,
                    warm,
                    (free * 10.0) as i64 - m.waiting as i64 * 100,
                ),
            }
        })
        .collect();
    out.sort_by(|a, b| b.ok.cmp(&a.ok).then(b.rank.cmp(&a.rank)));
    out
}

pub struct PlaceArgs {
    pub repo: String,
    pub kind: String,
    pub tree: Option<String>,
    pub source: PathBuf,
    pub params: Vec<(String, String)>,
    pub env: Vec<(String, String)>,
    pub needs: Vec<(String, String)>,
    pub class: String,
    pub holder: String,
    pub machine: Option<String>,
    pub results: Option<PathBuf>,
    pub id: Option<String>,
    pub dry_run: bool,
}

fn say(text: &str) {
    let _ = writeln!(std::io::stderr(), "pressroom: {text}");
}

fn git_in(source: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(source).stdin(Stdio::null());
    c
}

fn output(mut c: Command) -> Result<String, String> {
    let out = c.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The tree of everything in the folder now, tracked or not, less what git ignores. It
/// stages into a copy of the index, so the folder's own index is never touched.
pub fn snapshot(source: &Path) -> Result<String, String> {
    let index = output({
        let mut c = git_in(source);
        c.args(["rev-parse", "--path-format=absolute", "--git-path", "index"]);
        c
    })?;
    let copy = std::env::temp_dir().join(format!("pressroom-index-{}", std::process::id()));
    let _ = std::fs::remove_file(&copy);
    if Path::new(&index).exists() {
        std::fs::copy(&index, &copy).map_err(|e| e.to_string())?;
    }
    let result = (|| {
        if !copy.exists() {
            let mut c = git_in(source);
            c.args(["read-tree", "HEAD"]).env("GIT_INDEX_FILE", &copy);
            output(c)?;
        }
        let mut c = git_in(source);
        c.args(["add", "-A"]).env("GIT_INDEX_FILE", &copy);
        output(c)?;
        let mut c = git_in(source);
        c.arg("write-tree").env("GIT_INDEX_FILE", &copy);
        output(c)
    })();
    let _ = std::fs::remove_file(&copy);
    result
}

fn probe(target: &Target) -> Result<Value, String> {
    let mut c = target.job("probe", None);
    c.stdin(Stdio::null()).stderr(Stdio::piped());
    let out = c.output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout);
    match json::parse(text.trim()) {
        Ok(v) if v.str("version") == Some(recipe::VERSION) => Ok(v),
        _ => Err(format!(
            "it did not answer a probe ({})",
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .last()
                .unwrap_or("no reply")
                .trim()
        )),
    }
}

/// `pressroom place`: choose a machine, run the work there, bring the results here.
pub fn place(layout: &Layout, a: PlaceArgs) -> i32 {
    let conf = layout.home.join("machines.conf");
    let targets = match std::fs::read_to_string(&conf)
        .map_err(|_| {
            format!(
                "no {} saying which machines work may be placed on",
                conf.display()
            )
        })
        .and_then(|t| parse_machines(&t))
    {
        Ok(t) if !t.is_empty() => t,
        Ok(_) => {
            say(&format!("{} names no machines", conf.display()));
            return USAGE;
        }
        Err(why) => {
            say(&why);
            return USAGE;
        }
    };
    let targets: Vec<Target> = match &a.machine {
        Some(only) => targets.into_iter().filter(|t| &t.name == only).collect(),
        None => targets,
    };
    if targets.is_empty() {
        say(&format!(
            "machines.conf has no machine {}",
            a.machine.unwrap_or_default()
        ));
        return USAGE;
    }
    let tree = match &a.tree {
        Some(t) if recipe::valid_tree(t) => t.clone(),
        Some(t) => {
            say(&format!("--tree needs a 40-character tree hash, not {t}"));
            return USAGE;
        }
        None => match snapshot(&a.source) {
            Ok(t) => t,
            Err(why) => {
                say(&format!("cannot snapshot {}: {why}", a.source.display()));
                return USAGE;
            }
        },
    };

    // Ask every machine at once; a slow one does not hold up the rest.
    let probes: Vec<(Target, Result<Value, String>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .map(|t| (t.clone(), scope.spawn(move || probe(t))))
            .collect();
        handles
            .into_iter()
            .map(|(t, h)| {
                (
                    t,
                    h.join().unwrap_or_else(|_| Err("the probe failed".into())),
                )
            })
            .collect()
    });
    let seen: Vec<Seen> = probes
        .iter()
        .map(|(t, p)| match p {
            Ok(v) => read_probe(&t.name, v, &a.repo, &a.kind),
            Err(why) => Seen {
                name: t.name.clone(),
                unusable: Some(why.clone()),
                ..read_probe(&t.name, &Value::Null, &a.repo, &a.kind)
            },
        })
        .collect();
    let verdicts = choose(&seen, &tree, &a.needs);
    for v in &verdicts {
        say(&format!(
            "{}: {}{}",
            v.name,
            if v.ok { "" } else { "cannot: " },
            v.why
        ));
    }
    let Some(chosen) = verdicts.iter().find(|v| v.ok) else {
        say("no machine can take this work");
        return UNAVAILABLE;
    };
    let (target, probe) = probes
        .iter()
        .find(|(t, _)| t.name == chosen.name)
        .expect("chosen from these");
    let probe = probe.as_ref().expect("a chosen machine answered");
    say(&format!("placing on {}", target.name));

    // Parameters are lists or single values as the chosen machine's recipe says.
    let lists: Vec<String> = probe
        .arr("recipes")
        .iter()
        .find(|r| r.str("repo") == Some(a.repo.as_str()) && r.str("kind") == Some(a.kind.as_str()))
        .map(|r| {
            r.arr("params")
                .iter()
                .filter(|p| p.get("list") == Some(&Value::Bool(true)))
                .filter_map(|p| p.str("name").map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut params: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (k, v) in &a.params {
        let entry = params.entry(k.clone()).or_default();
        if !entry.is_empty() && !lists.contains(k) {
            say(&format!("--param {k} is one value, given twice"));
            return USAGE;
        }
        entry.push(v.clone());
    }
    let id = a.id.clone().unwrap_or_else(|| {
        format!(
            "{}-{}-{}-{}",
            a.repo,
            a.kind,
            crate::machine::now(),
            std::process::id()
        )
    });
    // What the machine already has, and this one has too, need not be sent.
    let held: Vec<String> = probe
        .get("trees")
        .map(|t| t.strings(&a.repo))
        .unwrap_or_default();
    let has_object = |t: &str| {
        let mut c = git_in(&a.source);
        c.args(["cat-file", "-e", t]);
        c.status().map(|s| s.success()).unwrap_or(false)
    };
    let pack = !held.contains(&tree);
    let base: Option<&String> = if pack {
        held.iter().find(|t| has_object(t))
    } else {
        None
    };
    let request = Request {
        id: id.clone(),
        repo: a.repo.clone(),
        kind: a.kind.clone(),
        tree: tree.clone(),
        params,
        env: a.env.iter().cloned().collect(),
        class: a.class.clone(),
        holder: a.holder.clone(),
        pack,
    };
    if a.dry_run {
        println!("{}", json::write(&recipe::request_json(&request)));
        return 0;
    }
    let results = a
        .results
        .clone()
        .unwrap_or_else(|| PathBuf::from("pressroom-results").join(&id));
    if let Err(e) = std::fs::create_dir_all(&results) {
        say(&format!("cannot make {}: {e}", results.display()));
        return USAGE;
    }

    let mut run = target.job("run", None);
    run.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = match run.spawn() {
        Ok(c) => c,
        Err(e) => {
            say(&format!("cannot reach {}: {e}", target.name));
            return UNAVAILABLE;
        }
    };
    let mut stdin = child.stdin.take().expect("piped");
    let mut line = json::write(&recipe::request_json(&request));
    line.push('\n');
    if stdin.write_all(line.as_bytes()).is_err() {
        say(&format!("{} closed the connection", target.name));
    } else if pack {
        let sent = send_pack(&a.source, &tree, base.map(String::as_str), stdin);
        if let Err(why) = sent {
            say(&format!("could not send tree {}: {why}", &tree[..12]));
        }
    } else {
        drop(stdin);
    }
    let mut answer: Option<Value> = None;
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if let Ok(v) = json::parse(line.trim()) {
                answer = Some(v);
            }
        }
    }
    let _ = child.wait();
    let answer = match answer {
        Some(a) => a,
        // The connection went but the job may not have (rule J10): ask after it.
        None => match wait_after(target, &id) {
            Some(a) => a,
            None => {
                say(&format!(
                    "lost {} and could not learn how job {id} ended",
                    target.name
                ));
                return TEMPFAIL;
            }
        },
    };
    if answer.str("state") == Some("refused") {
        say(&format!(
            "{} refused: {}",
            target.name,
            answer.str("reason").unwrap_or("")
        ));
        return UNAVAILABLE;
    }
    let fetched = bring_back(target, &id, &results);
    let code = answer.u64("exit").map(|c| c as i32).unwrap_or(1);
    say(&format!(
        "job {id} ended on {} with exit {code} after {}s; results in {}",
        target.name,
        answer.u64("seconds").unwrap_or(0),
        results.display()
    ));
    if let Err(why) = fetched {
        say(&format!("could not bring the results back: {why}"));
        return TEMPFAIL;
    }
    println!("{}", json::write(&answer));
    code
}

/// The objects of `tree` that the machine lacks: all of them, or those not in `base`.
fn send_pack(
    source: &Path,
    tree: &str,
    base: Option<&str>,
    to: std::process::ChildStdin,
) -> Result<(), String> {
    let mut list = git_in(source);
    list.args(["rev-list", "--objects", tree]);
    if let Some(base) = base {
        list.arg(format!("^{base}"));
    }
    let mut list = list
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let objects = list.stdout.take().expect("piped");
    let mut pack = git_in(source);
    pack.args(["pack-objects", "--stdout", "-q"])
        .stdin(Stdio::from(objects))
        .stdout(Stdio::from(to));
    let packed = pack.status().map_err(|e| e.to_string())?;
    let listed = list.wait().map_err(|e| e.to_string())?;
    if !listed.success() || !packed.success() {
        return Err("git could not pack it".into());
    }
    Ok(())
}

fn wait_after(target: &Target, id: &str) -> Option<Value> {
    let until = Instant::now() + Duration::from_secs(6 * 3600);
    let mut failures = 0;
    while Instant::now() < until {
        let out = target
            .job("show", Some(id))
            .stdin(Stdio::null())
            .output()
            .ok();
        match out.and_then(|o| json::parse(String::from_utf8_lossy(&o.stdout).trim()).ok()) {
            Some(v) if v.str("state") == Some("done") => return Some(v),
            Some(v) if v.str("state") == Some("unknown") => return None,
            Some(_) => failures = 0,
            None => {
                failures += 1;
                // Thirty minutes without an answer: the machine is gone, not busy.
                if failures > 60 {
                    return None;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(30));
    }
    None
}

fn bring_back(target: &Target, id: &str, into: &Path) -> Result<(), String> {
    let mut fetch = target.job("fetch", Some(id));
    let mut fetch = fetch
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let tar = Command::new("tar")
        .args(["-x", "-f", "-", "-C"])
        .arg(into)
        .stdin(Stdio::from(fetch.stdout.take().expect("piped")))
        .status()
        .map_err(|e| e.to_string())?;
    let fetched = fetch.wait().map_err(|e| e.to_string())?;
    if !tar.success() || !fetched.success() {
        return Err(format!("fetching job {id} from {} failed", target.name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(name: &str) -> Seen {
        Seen {
            name: name.into(),
            unusable: None,
            workstation: false,
            person_present: false,
            held_by_person: false,
            threads: 12.0,
            offered: 6.0,
            used: 0.0,
            outside: 0.0,
            offered_memory: 12.0,
            disk_free: 40.0,
            disk_floor: 10.0,
            waiting: 0,
            toolchains: vec![("xcode".into(), "Xcode 27.1, Build version 27A9275".into())],
            recipe: Some((4.0, 4.0, 5.0, vec![])),
            trees: vec![],
        }
    }

    #[test]
    fn machines_are_read_strictly() {
        let t = parse_machines("machine here = local\nmachine box = ssh me@box.example\npressroom box = bin/pressroom\n").unwrap();
        assert_eq!(t[0].reach, Reach::Local);
        assert_eq!(
            t[1].reach,
            Reach::Ssh {
                destination: "me@box.example".into(),
                program: "bin/pressroom".into()
            }
        );
        assert!(parse_machines("machine box = ssh -oProxyCommand=evil\n").is_err());
        assert!(parse_machines("machine box = ssh me@box;rm\n").is_err());
        assert!(parse_machines("machine a = local\nmachine a = local\n").is_err());
        assert!(parse_machines("machine cloud = hosted runner-6vcpu\n")
            .unwrap_err()
            .contains("not built"));
        assert!(parse_machines("pressroom ghost = bin/pressroom\n").is_err());
    }

    #[test]
    fn a_free_server_is_chosen_before_a_person_at_their_machine() {
        let mut laptop = machine("laptop");
        laptop.workstation = true;
        laptop.person_present = true;
        laptop.offered = 8.0;
        laptop.threads = 16.0;
        let v = choose(&[laptop.clone(), machine("server")], "t", &[]);
        assert_eq!(v[0].name, "server");
        assert!(v[0].starts_now);
        // A busy server waits; then the laptop, which can start now, goes first.
        let mut busy = machine("server");
        busy.used = 6.0;
        let v = choose(&[laptop, busy], "t", &[]);
        assert_eq!(v[0].name, "laptop");
        assert!(!v[1].starts_now && v[1].ok);
    }

    #[test]
    fn outside_load_counts_as_admission_counts_it() {
        let mut m = machine("server");
        // Six kept threads absorb six busy ones; the seventh comes off the offer.
        m.outside = 6.0;
        assert!(choose(&[m.clone()], "t", &[])[0].starts_now);
        m.outside = 11.0;
        assert!(!choose(&[m], "t", &[])[0].starts_now);
    }

    #[test]
    fn floors_toolchains_and_recipes_rule_machines_out() {
        let mut low = machine("low");
        low.disk_free = 14.0;
        let v = choose(&[low], "t", &[]);
        assert!(!v[0].ok && v[0].why.contains("floor"), "{}", v[0].why);
        let v = choose(&[machine("old")], "t", &[("xcode".into(), "27.2".into())]);
        assert!(!v[0].ok && v[0].why.contains("27.1"), "{}", v[0].why);
        let mut none = machine("none");
        none.recipe = None;
        assert!(!choose(&[none], "t", &[])[0].ok);
        let mut held = machine("held");
        held.held_by_person = true;
        assert!(!choose(&[held], "t", &[])[0].ok);
    }

    #[test]
    fn a_warm_machine_wins_a_tie() {
        let mut warm = machine("b");
        warm.trees = vec!["t".into()];
        let v = choose(&[machine("a"), warm], "t", &[]);
        assert_eq!(v[0].name, "b");
    }
}
