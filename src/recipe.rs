//! Recipes: the fixed commands a machine's owner lets placed work run (contracts/job-v1.md).
//!
//! A recipe is a file the owner puts in `recipes/`. A job request names a recipe by its
//! repository and kind and fills in its parameters; it never carries a command. Each
//! parameter value must match one of the recipe's patterns, and becomes exactly one
//! argument: nothing is ever passed through a shell.

use crate::json::{self, Value};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub name: String,
    /// Fewest and most values. A scalar parameter is `1..1` and appears in `argv` as
    /// `{name}`; a list appears as `{name...}`.
    pub min: usize,
    pub max: usize,
    pub list: bool,
    pub patterns: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Recipe {
    /// The file's name, without `.conf`.
    pub name: String,
    pub repo: String,
    pub kind: String,
    pub argv: Vec<String>,
    pub params: Vec<Param>,
    pub env: Vec<(String, String)>,
    pub pass_env: Vec<String>,
    pub threads: u32,
    pub memory_gb: f64,
    pub disk_gb: f64,
    pub timeout_secs: u64,
    pub toolchain: String,
    /// `(tool, version)`: the machine must report this tool at this version (rule J7).
    pub needs: Vec<(String, String)>,
}

/// What a request asks for, once read. Values are checked against a recipe by `expand`.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub id: String,
    pub repo: String,
    pub kind: String,
    pub tree: String,
    pub params: BTreeMap<String, Vec<String>>,
    pub env: BTreeMap<String, String>,
    pub class: String,
    pub holder: String,
    pub pack: bool,
}

pub const VERSION: &str = "pressroom.job/1";

fn word(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !text.starts_with('-')
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !id.starts_with('-')
}

pub fn valid_tree(tree: &str) -> bool {
    tree.len() == 40
        && tree
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

fn env_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
}

fn env_value(value: &str) -> bool {
    value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.:-".contains(c))
}

/// Does `value` match `pattern`? A `*` matches one or more of `A-Z a-z 0-9 _ . -`; any
/// other character matches itself. Whatever the pattern, a value never starts with `-`
/// (it would read as an option), never contains `..`, and is at most 200 characters.
pub fn matches(pattern: &str, value: &str) -> bool {
    if value.is_empty() || value.len() > 200 || value.starts_with('-') || value.contains("..") {
        return false;
    }
    fn go(p: &[u8], v: &[u8]) -> bool {
        match p.first() {
            None => v.is_empty(),
            Some(b'*') => {
                let ok = |c: u8| c.is_ascii_alphanumeric() || b"_.-".contains(&c);
                let mut i = 0;
                while i < v.len() && ok(v[i]) {
                    i += 1;
                    if go(&p[1..], &v[i..]) {
                        return true;
                    }
                }
                false
            }
            Some(c) => v.first() == Some(c) && go(&p[1..], &v[1..]),
        }
    }
    go(pattern.as_bytes(), value.as_bytes())
}

fn positive(key: &str, value: &str) -> Result<f64, String> {
    match value.parse::<f64>() {
        Ok(n) if n.is_finite() && n > 0.0 => Ok(n),
        _ => Err(format!("{key} needs a positive number, not {value:?}")),
    }
}

/// Read one recipe file. Anything unknown or malformed is an error: a recipe is never
/// read more loosely than it was written.
pub fn parse(name: &str, text: &str) -> Result<Recipe, String> {
    let mut r = Recipe {
        name: name.to_string(),
        repo: String::new(),
        kind: String::new(),
        argv: Vec::new(),
        params: Vec::new(),
        env: Vec::new(),
        pass_env: Vec::new(),
        threads: 2,
        memory_gb: 2.0,
        disk_gb: 1.0,
        timeout_secs: 3600,
        toolchain: "default".into(),
        needs: Vec::new(),
    };
    for (n, raw) in text.lines().enumerate() {
        let line = match raw.find(" #") {
            Some(i) => &raw[..i],
            None if raw.trim_start().starts_with('#') => "",
            None => raw,
        }
        .trim();
        if line.is_empty() {
            continue;
        }
        let at = |why: String| format!("line {}: {why}", n + 1);
        let Some((key, value)) = line.split_once('=') else {
            return Err(at(format!("expected key = value, not {line:?}")));
        };
        let (key, value) = (key.trim(), value.trim());
        let words: Vec<&str> = key.split_whitespace().collect();
        match words.as_slice() {
            ["repo"] if word(value) => r.repo = value.into(),
            ["kind"] if word(value) => r.kind = value.into(),
            ["toolchain"] if word(value) => r.toolchain = value.into(),
            ["argv"] => r.argv = value.split_whitespace().map(str::to_string).collect(),
            ["threads"] => r.threads = positive(key, value).map_err(at)?.ceil() as u32,
            ["memory_gb"] => r.memory_gb = positive(key, value).map_err(at)?,
            ["disk_gb"] => r.disk_gb = positive(key, value).map_err(at)?,
            ["timeout_minutes"] => {
                r.timeout_secs = (positive(key, value).map_err(at)? * 60.0) as u64
            }
            ["needs", tool] if word(tool) && !value.is_empty() && !value.contains(' ') => {
                r.needs.push((tool.to_string(), value.to_string()))
            }
            ["env", var] if env_name(var) => r.env.push((var.to_string(), value.to_string())),
            ["pass_env"] if env_name(value) => r.pass_env.push(value.into()),
            ["param", pname] | ["list", pname, _] if word(pname) => {
                let (min, max, list) = match words.as_slice() {
                    ["list", _, range] => {
                        let (lo, hi) = range
                            .split_once("..")
                            .ok_or_else(|| at(format!("list range is MIN..MAX, not {range}")))?;
                        let (lo, hi) = (lo.parse::<usize>(), hi.parse::<usize>());
                        match (lo, hi) {
                            (Ok(lo), Ok(hi)) if lo <= hi && (1..=200).contains(&hi) => {
                                (lo, hi, true)
                            }
                            _ => return Err(at(format!("bad list range {range}"))),
                        }
                    }
                    _ => (1, 1, false),
                };
                let patterns: Vec<String> = value
                    .split('|')
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect();
                if patterns.is_empty() {
                    return Err(at(format!("{pname} needs at least one pattern")));
                }
                if r.params.iter().any(|p| p.name == *pname)
                    || ["results", "threads"].contains(pname)
                {
                    return Err(at(format!("{pname} is named twice")));
                }
                r.params.push(Param {
                    name: pname.to_string(),
                    min,
                    max,
                    list,
                    patterns,
                });
            }
            _ => return Err(at(format!("unknown setting or bad value: {line:?}"))),
        }
    }
    if r.repo.is_empty() || r.kind.is_empty() || r.argv.is_empty() {
        return Err("a recipe needs repo, kind and argv".into());
    }
    // Every placeholder must name a parameter of the right shape, and every parameter
    // must be used, so a typo cannot silently drop a value or pass `{scrpit}` literally.
    let mut used = Vec::new();
    for arg in &r.argv {
        let holes = placeholders(arg)?;
        if holes.iter().filter(|(_, list)| *list).count() > 1 {
            return Err(format!("argv word {arg:?} has more than one list"));
        }
        for (hole, list) in holes {
            if hole == "results" || hole == "threads" {
                if list {
                    return Err(format!("{{{hole}}} is not a list"));
                }
                continue;
            }
            let Some(p) = r.params.iter().find(|p| p.name == hole) else {
                return Err(format!("argv uses {{{hole}}}, which is not a parameter"));
            };
            if p.list != list {
                return Err(format!(
                    "argv uses {hole} as {}, but it is {}",
                    if list { "a list" } else { "one value" },
                    if p.list {
                        "a list ({name...})"
                    } else {
                        "one value ({name})"
                    },
                ));
            }
            used.push(hole);
        }
    }
    if let Some(p) = r.params.iter().find(|p| !used.contains(&p.name)) {
        return Err(format!("parameter {} is never used in argv", p.name));
    }
    Ok(r)
}

/// The `{name}` and `{name...}` holes in one argv word.
fn placeholders(arg: &str) -> Result<Vec<(String, bool)>, String> {
    let mut out = Vec::new();
    let mut rest = arg;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}') else {
            return Err(format!("unclosed {{ in argv word {arg:?}"));
        };
        let inner = &rest[open + 1..open + close];
        let (name, list) = match inner.strip_suffix("...") {
            Some(name) => (name, true),
            None => (inner, false),
        };
        if !word(name) {
            return Err(format!("bad placeholder {{{inner}}} in {arg:?}"));
        }
        out.push((name.to_string(), list));
        rest = &rest[open + close + 1..];
    }
    if rest.contains('}') {
        return Err(format!("stray }} in argv word {arg:?}"));
    }
    Ok(out)
}

fn fill(arg: &str, values: &BTreeMap<&str, &str>) -> String {
    let mut out = String::new();
    let mut rest = arg;
    while let Some(open) = rest.find('{') {
        let close = open + rest[open..].find('}').unwrap_or(0);
        out.push_str(&rest[..open]);
        let name = rest[open + 1..close].trim_end_matches("...");
        out.push_str(values.get(name).copied().unwrap_or(""));
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// The argv a request gives under this recipe, or why it is refused.
pub fn expand(
    recipe: &Recipe,
    request: &Request,
    results: &Path,
    threads: u32,
) -> Result<Vec<String>, String> {
    check(recipe, request)?;
    let results = results.to_string_lossy().to_string();
    let threads = threads.to_string();
    let mut argv = Vec::new();
    for arg in &recipe.argv {
        let holes = placeholders(arg)?;
        let mut values: BTreeMap<&str, &str> = BTreeMap::new();
        values.insert("results", &results);
        values.insert("threads", &threads);
        let mut list: Option<(&str, &Vec<String>)> = None;
        for (hole, is_list) in &holes {
            if let Some(given) = request.params.get(hole) {
                if *is_list {
                    list = Some((hole, given));
                } else {
                    values.insert(hole, &given[0]);
                }
            }
        }
        match list {
            // One argument per value; none at all for an empty list.
            Some((hole, given)) => {
                for value in given {
                    let mut with = values.clone();
                    with.insert(hole, value);
                    argv.push(fill(arg, &with));
                }
            }
            None => argv.push(fill(arg, &values)),
        }
    }
    Ok(argv)
}

/// Check a request's parameters and environment against a recipe.
pub fn check(recipe: &Recipe, request: &Request) -> Result<(), String> {
    for name in request.params.keys() {
        if !recipe.params.iter().any(|p| &p.name == name) {
            return Err(format!("recipe {} has no parameter {name}", recipe.name));
        }
    }
    for p in &recipe.params {
        let given = request
            .params
            .get(&p.name)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if given.len() < p.min || given.len() > p.max {
            return Err(if p.list {
                format!(
                    "{} takes {} to {} values, not {}",
                    p.name,
                    p.min,
                    p.max,
                    given.len()
                )
            } else {
                format!("{} takes one value", p.name)
            });
        }
        for value in given {
            if !p.patterns.iter().any(|pat| matches(pat, value)) {
                return Err(format!(
                    "{} {value:?} matches none of: {}",
                    p.name,
                    p.patterns.join(" | ")
                ));
            }
        }
    }
    for (name, value) in &request.env {
        if !env_name(name)
            || !recipe
                .pass_env
                .iter()
                .any(|prefix| name.starts_with(prefix.as_str()))
        {
            return Err(format!("recipe {} does not pass {name}", recipe.name));
        }
        if !env_value(value) {
            return Err(format!(
                "{name}'s value must be at most 64 of A-Z a-z 0-9 _ . : -"
            ));
        }
    }
    Ok(())
}

/// Every recipe in a folder, and the files that could not be read.
pub fn load_all(dir: &Path) -> (Vec<Recipe>, Vec<String>) {
    let (mut ok, mut bad) = (Vec::new(), Vec::new());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (ok, bad);
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".conf"))
        .collect();
    names.sort();
    for file in names {
        let name = file.trim_end_matches(".conf");
        match std::fs::read_to_string(dir.join(&file))
            .map_err(|e| e.to_string())
            .and_then(|text| parse(name, &text))
        {
            Ok(r)
                if ok
                    .iter()
                    .any(|o: &Recipe| o.repo == r.repo && o.kind == r.kind) =>
            {
                bad.push(format!(
                    "{file}: another recipe is already {} {}",
                    r.repo, r.kind
                ))
            }
            Ok(r) => ok.push(r),
            Err(why) => bad.push(format!("{file}: {why}")),
        }
    }
    (ok, bad)
}

pub fn to_json(r: &Recipe) -> Value {
    use json::{n, obj, s, strs};
    obj(vec![
        ("name", s(&r.name)),
        ("repo", s(&r.repo)),
        ("kind", s(&r.kind)),
        ("toolchain", s(&r.toolchain)),
        ("threads", n(r.threads)),
        ("memoryGb", n(r.memory_gb)),
        ("diskGb", n(r.disk_gb)),
        ("timeoutSecs", json::n64(r.timeout_secs)),
        (
            "needs",
            Value::Arr(
                r.needs
                    .iter()
                    .map(|(t, v)| obj(vec![("tool", s(t)), ("version", s(v))]))
                    .collect(),
            ),
        ),
        (
            "params",
            Value::Arr(
                r.params
                    .iter()
                    .map(|p| {
                        obj(vec![
                            ("name", s(&p.name)),
                            ("list", Value::Bool(p.list)),
                            ("min", n(p.min as f64)),
                            ("max", n(p.max as f64)),
                            ("patterns", strs(&p.patterns)),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("passEnv", strs(&r.pass_env)),
    ])
}

/// Read a request line. Every field is required except `params`, `env` and `pack`, and a
/// field this version does not know is refused: a request that tries to say more than
/// the contract lets it (a `command`, say) is not quietly half-obeyed.
pub fn read_request(line: &str) -> Result<Request, String> {
    let v = json::parse(line.trim()).map_err(|e| format!("the request is not JSON: {e}"))?;
    let Value::Obj(fields) = &v else {
        return Err("the request is not an object".into());
    };
    const KNOWN: [&str; 10] = [
        "version", "id", "repo", "kind", "tree", "params", "env", "class", "holder", "pack",
    ];
    if let Some((k, _)) = fields.iter().find(|(k, _)| !KNOWN.contains(&k.as_str())) {
        return Err(format!(
            "the request has a field this version does not take: {k}"
        ));
    }
    let text = |k: &str| {
        v.str(k)
            .map(str::to_string)
            .ok_or_else(|| format!("the request needs {k} as a string"))
    };
    if text("version")? != VERSION {
        return Err(format!("this machine takes {VERSION} requests"));
    }
    let r = Request {
        id: text("id")?,
        repo: text("repo")?,
        kind: text("kind")?,
        tree: text("tree")?,
        class: text("class")?,
        holder: text("holder")?,
        pack: match v.get("pack") {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err("pack is true or false".into()),
        },
        params: match v.get("params") {
            None => BTreeMap::new(),
            Some(Value::Obj(items)) => {
                let mut out = BTreeMap::new();
                for (k, val) in items {
                    let values = match val {
                        Value::Str(one) => vec![one.clone()],
                        Value::Arr(many) => many
                            .iter()
                            .map(|m| match m {
                                Value::Str(t) => Ok(t.clone()),
                                _ => Err(format!("{k}'s values must be strings")),
                            })
                            .collect::<Result<_, _>>()?,
                        _ => return Err(format!("{k} must be a string or a list of strings")),
                    };
                    if out.insert(k.clone(), values).is_some() {
                        return Err(format!("{k} is given twice"));
                    }
                }
                out
            }
            Some(_) => return Err("params must be an object".into()),
        },
        env: match v.get("env") {
            None => BTreeMap::new(),
            Some(Value::Obj(items)) => {
                let mut out = BTreeMap::new();
                for (k, val) in items {
                    let Value::Str(t) = val else {
                        return Err(format!("env {k} must be a string"));
                    };
                    if out.insert(k.clone(), t.clone()).is_some() {
                        return Err(format!("env {k} is given twice"));
                    }
                }
                out
            }
            Some(_) => return Err("env must be an object".into()),
        },
    };
    if !valid_id(&r.id) {
        return Err("id is letters, digits, - and _, at most 100".into());
    }
    if !word(&r.repo) || !word(&r.kind) {
        return Err("repo and kind are lower-case letters, digits and -".into());
    }
    if !valid_tree(&r.tree) {
        return Err("tree is a 40-character git tree hash".into());
    }
    if !["waiting", "gate", "batch"].contains(&r.class.as_str()) {
        return Err("class is waiting, gate or batch".into());
    }
    if r.holder.is_empty() || r.holder.len() > 100 || r.holder.chars().any(char::is_control) {
        return Err("holder is 1 to 100 printable characters".into());
    }
    Ok(r)
}

pub fn request_json(r: &Request) -> Value {
    use json::{obj, s, strs};
    obj(vec![
        ("version", s(VERSION)),
        ("id", s(&r.id)),
        ("repo", s(&r.repo)),
        ("kind", s(&r.kind)),
        ("tree", s(&r.tree)),
        (
            "params",
            Value::Obj(r.params.iter().map(|(k, v)| (k.clone(), strs(v))).collect()),
        ),
        (
            "env",
            Value::Obj(r.env.iter().map(|(k, v)| (k.clone(), s(v))).collect()),
        ),
        ("class", s(&r.class)),
        ("holder", s(&r.holder)),
        ("pack", Value::Bool(r.pack)),
    ])
}

/// Does a machine's reported tool version meet a recipe's or a request's need? `27.1`
/// is met by `Xcode 27.1, Build version 27A9275` and by `27.1.2`, not by `27.10`.
pub fn version_meets(reported: &str, need: &str) -> bool {
    reported
        .split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')')
        .any(|w| w == need || w.starts_with(&format!("{need}.")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "\
# Host test scripts.
repo = demo
kind = host
toolchain = xcode
argv = python3 {script} --results {results}
param script = apple/scripts/test-*.py
threads = 4
memory_gb = 4
timeout_minutes = 20
needs xcode = 27.1
env DEVELOPER_DIR = /Applications/Xcode.app/Contents/Developer
pass_env = DEMO_
";

    fn request(params: &[(&str, &[&str])]) -> Request {
        Request {
            id: "j1".into(),
            repo: "demo".into(),
            kind: "host".into(),
            tree: "0".repeat(40),
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
                .collect(),
            env: BTreeMap::new(),
            class: "batch".into(),
            holder: "agent:test".into(),
            pack: false,
        }
    }

    #[test]
    fn patterns_match_one_safe_word() {
        assert!(matches(
            "apple/scripts/test-*.py",
            "apple/scripts/test-queue-drop-plan.py"
        ));
        assert!(!matches(
            "apple/scripts/test-*.py",
            "apple/scripts/test-.py"
        ));
        assert!(!matches(
            "apple/scripts/test-*.py",
            "apple/scripts/test-a/b.py"
        ));
        assert!(!matches(
            "apple/scripts/test-*.py",
            "apple/scripts/test-..py"
        ));
        assert!(!matches("*", "-rf"));
        assert!(!matches("*", "a b"));
        assert!(!matches("*", "$(id)"));
        assert!(matches("Demo/*/*", "Demo/Suite/testOne"));
        assert!(!matches("Demo/*", "Demo/Suite/testOne"));
    }

    #[test]
    fn a_request_fills_the_recipe_and_nothing_else() {
        let r = parse("demo-host", HOST).unwrap();
        assert_eq!(r.threads, 4);
        assert_eq!(r.needs, vec![("xcode".to_string(), "27.1".to_string())]);
        let argv = expand(
            &r,
            &request(&[("script", &["apple/scripts/test-x.py"])]),
            Path::new("/r"),
            4,
        )
        .unwrap();
        assert_eq!(
            argv,
            ["python3", "apple/scripts/test-x.py", "--results", "/r"]
        );
        let refused = expand(
            &r,
            &request(&[("script", &["tools/evil.py"])]),
            Path::new("/r"),
            4,
        );
        assert!(refused.unwrap_err().contains("matches none"));
        let refused = expand(&r, &request(&[]), Path::new("/r"), 4);
        assert!(refused.unwrap_err().contains("takes one value"));
        let refused = expand(
            &r,
            &request(&[("script", &["apple/scripts/test-x.py"]), ("other", &["x"])]),
            Path::new("/r"),
            4,
        );
        assert!(refused.unwrap_err().contains("no parameter other"));
        let mut with_env = request(&[("script", &["apple/scripts/test-x.py"])]);
        with_env.env.insert("PATH".into(), "/evil".into());
        assert!(check(&r, &with_env)
            .unwrap_err()
            .contains("does not pass PATH"));
        with_env.env.clear();
        with_env.env.insert("DEMO_FLAG".into(), "1".into());
        assert!(check(&r, &with_env).is_ok());
        with_env.env.insert("DEMO_FLAG".into(), "a b".into());
        assert!(check(&r, &with_env).is_err());
    }

    #[test]
    fn a_list_gives_one_argument_per_value() {
        let r = parse(
            "demo-ui",
            "repo = demo\nkind = ui\nargv = xcodebuild -only-testing:{selector...} test\nlist selector 1..3 = Demo/* | Demo/*/*\n",
        )
        .unwrap();
        let argv = expand(
            &r,
            &request(&[("selector", &["Demo/A", "Demo/B/testC"])]),
            Path::new("/r"),
            2,
        )
        .unwrap();
        assert_eq!(
            argv,
            [
                "xcodebuild",
                "-only-testing:Demo/A",
                "-only-testing:Demo/B/testC",
                "test"
            ]
        );
        assert!(expand(&r, &request(&[("selector", &[])]), Path::new("/r"), 2).is_err());
        assert!(expand(
            &r,
            &request(&[("selector", &["Demo/A"; 4])]),
            Path::new("/r"),
            2
        )
        .is_err());
    }

    #[test]
    fn a_recipe_is_never_read_loosely() {
        assert!(parse("x", "repo = x\nkind = y\nargv = a {nope}\n")
            .unwrap_err()
            .contains("not a parameter"));
        assert!(parse("x", "repo = x\nkind = y\nargv = a\nparam p = *\n")
            .unwrap_err()
            .contains("never used"));
        assert!(
            parse("x", "repo = x\nkind = y\nargv = a {p...}\nparam p = *\n")
                .unwrap_err()
                .contains("one value")
        );
        assert!(parse("x", "repo = x\nkind = y\nargv = a\ncolour = red\n")
            .unwrap_err()
            .contains("unknown"));
        assert!(parse("x", "repo = X\nkind = y\nargv = a\n").is_err());
        assert!(parse("x", "kind = y\nargv = a\n").is_err());
    }

    #[test]
    fn a_request_says_only_what_the_contract_lets_it() {
        let tree = "a".repeat(40);
        let good = format!(
            r#"{{"version":"pressroom.job/1","id":"j-1","repo":"demo","kind":"host","tree":"{tree}","params":{{"script":"x"}},"class":"batch","holder":"agent:a","pack":true}}"#
        );
        let r = read_request(&good).unwrap();
        assert_eq!(r.params["script"], vec!["x".to_string()]);
        assert_eq!(read_request(&json::write(&request_json(&r))).unwrap(), r);
        let with_command = good.replace(r#""pack":true"#, r#""pack":true,"command":"rm -rf ~""#);
        assert!(read_request(&with_command).unwrap_err().contains("command"));
        assert!(read_request(&good.replace(&tree, "HEAD")).is_err());
        assert!(read_request(&good.replace("j-1", "../j")).is_err());
    }

    #[test]
    fn versions_meet_by_whole_parts() {
        assert!(version_meets("Xcode 27.1, Build version 27A9275", "27.1"));
        assert!(version_meets("rustc 1.99.1 (58cf8f9c3 2026-10-13)", "1.99"));
        assert!(!version_meets("Xcode 27.10", "27.1"));
        assert!(!version_meets("Xcode 27.2", "27.1"));
    }
}
