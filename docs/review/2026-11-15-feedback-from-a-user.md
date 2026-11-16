# Feedback from an agent that used Pressroom all day (2026-11-15)

From the session that built Quoin's first slice (luff-llc/galley#100), passed on at Mike's
request. It is feedback to weigh, not a plan. Most of it is design slice 3 (placing work
across machines) in Galley's `docs/design/2026-11-15-device-capacity.md`.

## What happened

Every test and typecheck went through `pressroom run --class waiting`. On the MacBook the
first run waited about 5 minutes. A later one waited over 10 minutes and was never admitted.
Load was 140 to 165, none of it started through Pressroom, and the job was a 26 ms test and a
typecheck. The Mac mini was at load 3.5 with 0 of 6 offered threads in use at the time; the
agent did not know until Mike told it. Moving cost two tries of shipping the checkout over
SSH by hand.

What worked: it refused to add to a swamped machine. One admitted job for tests, typecheck
and format saved two more waits.

## What it asked for, most valuable first, and where each belongs

| # | Ask | Where it belongs |
|---|---|---|
| 1 | While waiting, say which machine could take the job now; better, `pressroom run --anywhere` that picks one | Slice 3, placement. Needs each daemon to know the others' state: through Galley's machines read (slice 2, galley#97) or peer to peer on the private network. |
| 2 | One command that ships the git tree (uncommitted changes too), gets dependencies there, runs, and brings back output and changed files | Slice 3. The design's "checkouts" folder and result leases are the place; the false typecheck errors from a partial copy are the reason to build it rather than leave it to each agent. |
| 3 | `pressroom machines`: each machine's load, free threads and toolchains; a remote run finds the toolchain the daemon already found | Slice 3, reading Galley's machines read. Toolchain lookup: the daemon already records versions; it should also record the path it found them at (mise shims are not on a non-login SSH PATH). |
| 4 | No estimate and no `--max-wait N` with its own exit code, so an agent cannot fall back by itself; the waiting line repeats | Local contract, small. The client prints only when the reason changes, but the reason carries the load figure, which changes every tick: compare the reason without the number, or print at most once a minute. `--max-wait` with a new exit code is a contract addition. |
| 5 | A one-thread job of a few seconds is queued like a build | Local contract. A `small` class, or admitting one thread when the machine's own share is untouched, is an admission rule change; it must not let a stream of small jobs starve a build (rule 5). |
| 6 | "About 165 threads of load were not started through pressroom", but not what it was | Local, small. Status could name the top few processes by CPU (name and owner only), so a person or agent knows whom to ask. Privacy: process names can carry paths; clip them. |
| 7 | `pressroom run --help` is refused as an unknown option | Local, trivial: `--help` and `-h` anywhere print the help. |
| 8 | A documented "check" recipe per repository (tests, typecheck, format as one admitted job) | Each repository's `AGENTS.md`, not Pressroom. |

The menu bar item (apps/menubar) answers part of 3 for one machine only.
