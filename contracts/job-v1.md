# The job contract, version 1

What a machine promises to work placed on it from elsewhere, and what `pressroom place`
promises to the agent that asks. Requests are `pressroom.job/1`. Tested by
`tests/placement.rs` and the unit tests in `src/recipe.rs` and `src/place.rs`. The design
behind it is `docs/design/placement.md`.

A placed job is local work on the machine that runs it: every rule of `local-v1.md`
applies to it there. This contract adds how it gets there, what it may ask for, and how
its results come back.

## Files

On a machine that takes placed work, under `$PRESSROOM_HOME` (else `~/.pressroom`):

| Path | What |
|---|---|
| `recipes/<name>.conf` | The commands placed work may run here, put here by the owner (rule J2) |
| `checkouts/<repo>.git` | The source this machine has been sent, as git objects (rule J4) |
| `lanes/<repo>/<toolchain>/<n>/src` | A job's source, inside its lane (rule J5) |
| `results/<id>/` | One job: `request.json`, `log.txt`, `result.json` and `out/` (rule J12) |

On a machine that places work: `machines.conf`, the machines it may place on.

```
machine laptop = local
machine build-mac = ssh builder@build-mac.example
pressroom build-mac = .local/bin/pressroom    # where the command is there; this is the default
```

`local` is this machine. `ssh DESTINATION` runs `pressroom job ...` there over SSH, with
batch mode on, so a key that needs a passphrase or a prompt fails rather than waits.
`hosted ...` is refused until the hosted tier is built (`docs/design/placement.md` §5).

## A recipe

```
# recipes/demo-host.conf
repo = demo
kind = host
argv = python3 {script}
param script = scripts/test-*.py
pass_env = DEMO_
env DEVELOPER_DIR = /Applications/Xcode.app/Contents/Developer
needs xcode = 27.1
toolchain = xcode
threads = 4
memory_gb = 4
disk_gb = 4
timeout_minutes = 20
```

| Key | Default | Meaning |
|---|---|---|
| `repo`, `kind` | required | What a request names to use this recipe. Lower-case letters, digits and `-`. |
| `argv` | required | The command, split on spaces. `{name}` is a parameter's value, `{name...}` one argument per value of a list, `{results}` the job's `out/` folder, `{threads}` the threads it was given. |
| `param NAME` | | A parameter that takes one value, matching one of the patterns, separated by `\|` |
| `list NAME MIN..MAX` | | A parameter that takes MIN to MAX values (MAX at most 200) |
| `pass_env` | | A prefix: request variables whose names start with it are passed. Repeatable. |
| `env NAME` | | A variable the job always gets |
| `needs TOOL` | | The machine must report TOOL at this version (rule J7). Repeatable. |
| `toolchain` | `default` | The lane's toolchain (local rules 8 and 29) |
| `threads`, `memory_gb` | 2, 2 | What the job asks the machine for (local rules 1 to 4) |
| `disk_gb` | 1 | The most the job may write (rule J8) |
| `timeout_minutes` | 60 | When the job is ended (rule J12) |

A pattern is literal text in which `*` stands for one or more of `A-Z a-z 0-9 _ . -`.

## Rules

**What a request may say.**

1. A request names a recipe by `repo` and `kind`, a `tree`, values for the recipe's
   parameters, variables for it to pass, a `class` and a `holder`. **It never carries a
   command,** a path to run, a working folder, or a variable the recipe does not pass. A
   request with any field this version does not name is refused whole, not half obeyed.
2. Recipes are files the machine's owner puts in `recipes/`. They are read when a job
   arrives. A recipe that cannot be read, or a second one for the same repository and
   kind, is never used, and `pressroom job recipes` and `probe` name it. Nothing a job
   sends can add or change a recipe: the job's own tree is not where its recipe comes from.
3. Each value must match one of its parameter's patterns, and becomes exactly one
   argument. Whatever the pattern, a value never starts with `-`, never contains `..`, is
   at most 200 characters, and is never passed through a shell. A passed variable's name
   is upper-case letters, digits and `_`; its value at most 64 of `A-Z a-z 0-9 _ . : -`.

**The source.**

4. The source is a git tree, named by its hash. The placer sends a pack of the objects the
   machine lacks: all of them, or those not in the newest tree the machine holds that the
   placer also has. The machine checks that it has every object of the tree before the
   job is admitted, and refuses it otherwise. It keeps the newest eight trees of each
   repository named; older ones are no longer offered as a base. A placer with no tree
   given sends a snapshot of its folder: everything there now, tracked or not, less what
   git ignores, staged into a copy of its index so the folder's own index is not touched.
5. The job runs in `src/` inside its lane, made exactly the tree: a file a job before it
   changed is put back, a file that already matches is not rewritten (so a warm build
   stays warm), and files the tree does not name, such as build output, stay. `src/` is
   the root of an empty git repository, so tools that look for one find it; history is
   not sent.

**Admission.**

6. Once its source is whole, a job is local work: a `pressroom run` with the recipe's
   threads and memory, the request's class and holder, the lane for its repository and
   the recipe's toolchain, and a lease on the repository's objects. It waits, is refused
   or is held exactly as any local work is.
7. **Tools.** A placer passes over a machine that does not report each tool the recipe
   needs (and each the request needs, `place --needs TOOL=VERSION`) at that version, and
   the machine refuses a job whose recipe needs one it does not report. `27.1` is met by
   `27.1` and `27.1.2`, not by `27.10` or `27.2`. Work is never run on the wrong one.
8. **Disk.** A machine refuses a job when its free disk less the recipe's `disk_gb` is
   below its floor (`disk_floor_gb`), and a placer passes over it. Local admission still
   holds new work below the floor.

**Choosing a machine.**

9. A placer asks every machine in `machines.conf` at once (`pressroom job probe`) and
   rules out each that did not answer, has no recipe for the work, lacks a tool (rule J7),
   is held by its person (local rule 19), offers less memory than the recipe asks, or
   would pass its floor (rule J8), saying why for each. Of the rest it chooses, in order:
   one where the work would start now (no work waiting, and at least half the recipe's
   threads free, counting outside load as local rule 2 does); one that is not a
   workstation whose person is present; one that already holds the tree; then the most
   free threads, less a penalty for each job waiting. The choice is advisory: the chosen
   machine admits by its own rules, so two placers that choose it at once make one wait.

**While it runs, and after.**

10. A job does not depend on the connection that placed it. Its output goes to its
    `log.txt` as well as back to the placer; if the connection is lost the job goes on,
    and the placer asks after it (`job show`) every 30 seconds for up to six hours, then
    fetches its results. A placer that cannot learn how the job ended exits 75.
11. **Environment.** A job starts with nothing of the placer's. It has the machine's
    `PATH`, `HOME`, `USER`, `LOGNAME`, `TMPDIR`, `LANG` and `SHELL`; what `run` gives it
    (`PRESSROOM_JOB`, `PRESSROOM_THREADS`, `PRESSROOM_LANE`, `CARGO_TARGET_DIR` and the
    thread counts); `PRESSROOM_RESULTS` (its `out/` folder) and `PRESSROOM_TREE`; then the
    recipe's `env`; then the request's passed variables.
12. **Results.** Everything a job leaves is in `results/<id>/`: the request, the log
    (stdout and stderr as they came), `result.json` (`exit`, `seconds`, `ranSeconds`,
    `timedOut`, the machine and the tree) and `out/`. The placer fetches it as one tar
    stream into `--results` (default `pressroom-results/<id>`). A job past its
    `timeout_minutes` is sent `SIGTERM` in its process group, then `SIGKILL` ten seconds
    later. `results/` is a managed folder (local rules 14 to 17); pressroom does not empty
    it itself. An id that already exists on the machine is refused.
13. **A key that reaches only jobs.** `pressroom job ssh` is for a forced command in
    `authorized_keys` (`command="/path/to/pressroom job ssh",no-pty,no-port-forwarding`).
    It runs `job probe`, `job run`, `job show ID` or `job fetch ID` from
    `SSH_ORIGINAL_COMMAND`, an id being letters, digits, `-` and `_`, and refuses anything
    else, so the key that places work cannot open a shell.

## The request

One JSON line on the standard input of `pressroom job run`, then, if `pack` is true, a git
pack until the end of the input.

```json
{"version":"pressroom.job/1","id":"demo-host-1795400000-42","repo":"demo","kind":"host",
 "tree":"882cbb01c88eacd3d9cca61b5aae94a8176f31fd","params":{"script":["scripts/test-x.py"]},
 "env":{"DEMO_FLAG":"on"},"class":"batch","holder":"agent:builder-a","pack":true}
```

| Field | |
|---|---|
| `id` | Letters, digits, `-`, `_`, at most 100. New on that machine. |
| `repo`, `kind` | The recipe |
| `tree` | 40 hexadecimal characters |
| `params` | Name to a string or a list of strings. Optional. |
| `env` | Name to string. Optional. |
| `class` | `waiting`, `gate` or `batch` |
| `holder` | Who is asking: a label, as in `local-v1.md` |
| `pack` | Whether a pack follows. Optional, default false. |

`job run` writes the job's output to standard error as it runs, and one line to standard
output: `{"state":"done","id","machine","repo","kind","tree","exit","seconds","ended",
"ranSeconds","timedOut"}`, or `{"state":"refused","reason"}`. `exit` is the job's own
code, or `run`'s (69 refused, 75 a fault of the machine) if it never started.
`ranSeconds` is null if it never started.

`job probe` answers `{"version","status","statusError","recipes","unreadableRecipes",
"trees"}`: `status` is what local `status` answers (null with `statusError` if the daemon
did not answer), `recipes` each recipe without its command, and `trees` each repository's
held trees, newest first.

## Exit codes of `pressroom place`

| Code | Meaning |
|---|---|
| the job's own | It ran |
| 64 | Wrong arguments, or no usable `machines.conf` |
| 69 | No machine could take it, or the machine chosen refused it |
| 75 | The connection was lost and the job's end could not be learnt, or its results could not be brought back |

## What version 1 does not do

- **No grants yet.** Whoever can reach a machine's `pressroom job` (its user, or an SSH
  key) can place any work its recipes allow, and the holder is a label. Rule J13 narrows
  a key to job verbs; grants signed with the system's keys come with enrolment.
- **SSH is the wire.** The placer connects to the machine. The machine does not yet dial
  out and pull work (design §4).
- **No hosted tier.** `machines.conf` refuses `hosted` (design §5).
- **A recipe cannot ask for a device slot** (local rule 27) or for a simulator; a recipe
  that boots one is counted as outside load there.
- **A placed job cannot be cancelled** from the placer; stopping `place` leaves it running.
- **The objects of old trees stay** in `checkouts/<repo>.git`; only their names are pruned.
- **A placer asks every machine each time;** it keeps no picture between placements.
- **Jobs run as the machine's daemon user,** with no isolation between one job's files and
  another's beyond their lanes.
