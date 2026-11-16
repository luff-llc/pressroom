//! Pressroom: one gatekeeper per machine. See README.md and contracts/local-v1.md.

mod admit;
mod client;
mod daemon;
mod json;
mod lease;
mod machine;

use machine::Layout;
use std::process::exit;

const HELP: &str = "\
pressroom: the gatekeeper for work on this machine

  pressroom daemon
      Run the gatekeeper. One per machine, as the user that runs the work.

  pressroom run [options] -- COMMAND [ARGS...]
      Wait to be admitted, then run COMMAND within what was granted.
        --threads N        threads the work will use (default 2)
        --memory-gb N      memory it will use (default 2)
        --class CLASS      waiting (someone is waiting on it), gate, or batch (default)
        --lane REPO        give it a cache lane of its own for REPO; sets CARGO_TARGET_DIR
        --toolchain NAME   the lane's toolchain, e.g. rust-1.99.1 (default: default)
        --lease PATH       something it depends on that must not be removed; repeatable
        --lease-kind KIND  cache (default), registry, toolchain, worktree, result, scratch
        --holder WHO       who is asking (default $PRESSROOM_HOLDER, else user:$USER)
        --label TEXT       what to call it in `status` (default: the command)
        --no-nice          do not lower the priority of batch work
      The command sees PRESSROOM_THREADS, CARGO_BUILD_JOBS, RUST_TEST_THREADS, MAKEFLAGS.

  pressroom status [--json]       what is running, what waits and why, what is leased
  pressroom measure               this machine, measured now, as JSON

  pressroom lease add [--kind K] [--mode in-use|warm] [--ttl-hours N] [--pid P]
                      [--holder WHO] PATH...
      Declare something you depend on. Prints the lease id.
  pressroom lease release ID
  pressroom leases [--json]       live leases, read from the files (no daemon needed)

  pressroom person hold|give|auto [--minutes N] [--holder WHO]
      The person says: hold (I need the whole machine: nothing new starts), give (take
      it all, I am away: every thread is offered) or auto (back to what is measured).
      hold and give last N minutes (default 60, at most a week).

  pressroom may-delete PATH       exit 0 if a cleanup tool may remove PATH, else 1 and why
  pressroom delete-begin [--ttl-minutes N] [--holder WHO] PATH
      Take a delete lease before removing PATH. Prints its id, or exits 1 and says why.
  pressroom delete-end ID

Files live in $PRESSROOM_HOME, else ~/.pressroom. Limits are in machine.conf there.
";

fn usage(why: &str) -> ! {
    eprintln!("pressroom: {why}\n\n{HELP}");
    exit(client::USAGE);
}

/// Options before the positional arguments. Returns `(options, positionals)`.
fn split(args: &[String], flags: &[&str], valued: &[&str]) -> (Vec<(String, String)>, Vec<String>) {
    let mut options = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            i += 1;
            break;
        }
        if flags.contains(&arg) {
            options.push((arg.to_string(), String::new()));
            i += 1;
        } else if valued.contains(&arg) {
            let Some(value) = args.get(i + 1) else {
                usage(&format!("{arg} needs a value"))
            };
            options.push((arg.to_string(), value.clone()));
            i += 2;
        } else if arg.starts_with("--") {
            usage(&format!("unknown option {arg}"));
        } else {
            break;
        }
    }
    (options, args[i..].to_vec())
}

fn number(name: &str, value: &str) -> f64 {
    match value.parse::<f64>() {
        Ok(n) if n.is_finite() && n >= 0.0 => n,
        _ => usage(&format!("{name} needs a number, not {value:?}")),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let layout = Layout {
        home: machine::home(),
    };
    let Some(command) = args.first() else {
        usage("no command")
    };
    let rest = &args[1..];
    let code = match command.as_str() {
        "daemon" => match daemon::serve(layout.home) {
            Ok(()) => 0,
            Err(why) => {
                eprintln!("pressroom: {why}");
                1
            }
        },
        "run" => {
            let valued = [
                "--threads",
                "--memory-gb",
                "--class",
                "--lane",
                "--toolchain",
                "--lease",
                "--lease-kind",
                "--holder",
                "--label",
            ];
            let (options, command) = split(rest, &["--no-nice"], &valued);
            if command.is_empty() {
                usage("run needs a command after --");
            }
            let mut a = client::RunArgs {
                threads: 2,
                memory_gb: 2.0,
                class: "batch".into(),
                lane: None,
                toolchain: "default".into(),
                leases: Vec::new(),
                lease_kind: "cache".into(),
                holder: client::default_holder(),
                label: None,
                nice: true,
                command,
            };
            for (name, value) in options {
                match name.as_str() {
                    "--threads" => a.threads = (number(&name, &value) as u32).max(1),
                    "--memory-gb" => a.memory_gb = number(&name, &value),
                    "--class" => a.class = value,
                    "--lane" => a.lane = Some(value),
                    "--toolchain" => a.toolchain = value,
                    "--lease" => a.leases.push(value),
                    "--lease-kind" => a.lease_kind = value,
                    "--holder" => a.holder = value,
                    "--label" => a.label = Some(value),
                    _ => a.nice = false,
                }
            }
            if !["waiting", "gate", "batch"].contains(&a.class.as_str()) {
                usage("--class is waiting, gate or batch");
            }
            if ![
                "cache",
                "registry",
                "toolchain",
                "worktree",
                "result",
                "scratch",
            ]
            .contains(&a.lease_kind.as_str())
            {
                usage("--lease-kind is cache, registry, toolchain, worktree, result or scratch");
            }
            client::run(&layout, a)
        }
        "status" => {
            let (options, _) = split(rest, &["--json"], &[]);
            client::status(&layout, !options.is_empty())
        }
        "measure" => client::measure(&layout),
        "leases" => {
            let (options, _) = split(rest, &["--json"], &[]);
            client::leases(&layout, !options.is_empty())
        }
        "lease" => match rest.first().map(String::as_str) {
            Some("add") => {
                let valued = ["--kind", "--mode", "--ttl-hours", "--pid", "--holder"];
                let (options, paths) = split(&rest[1..], &[], &valued);
                if paths.is_empty() {
                    usage("lease add needs at least one path");
                }
                let (mut kind, mut mode, mut ttl, mut pid, mut holder) = (
                    "cache".to_string(),
                    "in-use".to_string(),
                    24.0,
                    None,
                    client::default_holder(),
                );
                for (name, value) in options {
                    match name.as_str() {
                        "--kind" => kind = value,
                        "--mode" => mode = value,
                        "--ttl-hours" => ttl = number(&name, &value),
                        "--pid" => pid = Some(number(&name, &value) as u32),
                        _ => holder = value,
                    }
                }
                client::lease_add(&layout, &holder, &kind, &mode, ttl, pid, &paths)
            }
            Some("release") => match rest.get(1) {
                Some(id) => client::release(&layout, "release", id),
                None => usage("lease release needs an id"),
            },
            _ => usage("lease add or lease release"),
        },
        "person" => {
            let Some(mode) = rest.first() else {
                usage("person needs hold, give or auto")
            };
            let (options, extra) = split(&rest[1..], &[], &["--minutes", "--holder"]);
            if !extra.is_empty() {
                usage("person takes hold, give or auto, then options");
            }
            if !["hold", "give", "auto"].contains(&mode.as_str()) {
                usage("person needs hold, give or auto");
            }
            let (mut minutes, mut holder) = (60.0, client::default_holder());
            for (name, value) in options {
                match name.as_str() {
                    "--minutes" => minutes = number(&name, &value),
                    _ => holder = value,
                }
            }
            client::person_says(&layout, &holder, mode, minutes)
        }
        "may-delete" => match rest.first() {
            Some(path) => client::may_delete(&layout, path),
            None => usage("may-delete needs a path"),
        },
        "delete-begin" => {
            let (options, paths) = split(rest, &[], &["--ttl-minutes", "--holder"]);
            let [path] = paths.as_slice() else {
                usage("delete-begin needs one path")
            };
            let (mut ttl, mut holder) = (10.0, client::default_holder());
            for (name, value) in options {
                match name.as_str() {
                    "--ttl-minutes" => ttl = number(&name, &value),
                    _ => holder = value,
                }
            }
            client::delete_begin(&layout, &holder, ttl, path)
        }
        "delete-end" => match rest.first() {
            Some(id) => client::release(&layout, "delete-end", id),
            None => usage("delete-end needs an id"),
        },
        "help" | "--help" | "-h" => {
            print!("{HELP}");
            0
        }
        other => usage(&format!("unknown command {other}")),
    };
    exit(code);
}
