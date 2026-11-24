# The local contract, version 1

What the daemon on one machine promises to the things on that machine: agents and people
who run work, and tools that clean up. Version `pressroom.local/1`; lease files are
`pressroom.lease/1`. Tested by `tests/end_to_end.rs`. Checked once by a separate agent on
2026-11-15; what it found and what was done is in `docs/review/2026-11-15-slice-1.md`.

This covers one machine. The report to Galley is `report-v1.md`; work placed from another
machine is `job-v1.md`; enrolment, grants and power are later contracts.

## Files

Everything is under `$PRESSROOM_HOME`, else `~/.pressroom`, readable by its user only.

| Path | What |
|---|---|
| `machine.conf` | What the owner declared. `key = value` lines. An unknown key or a bad value stops the daemon; it never falls back to a looser limit. |
| `sock` | The daemon's socket, mode 600 |
| `daemon.pid` | The running daemon's process. One daemon per home; a second refuses to start and exits 1, or 0 with `--if-not-running` (for login agents, which must not retry). |
| `leases/<id>.json` | One file per live lease |
| `lanes/<repo>/<toolchain>/<n>/` | Cache lanes |
| `checkouts/`, `scratch/`, `results/` | Other managed folders |
| `events.log` | One JSON line per job start, end, refusal and leased path found missing |

`machine.conf` settings, all optional:

| Key | Default | Meaning |
|---|---|---|
| `name` | the host name | |
| `kind` | `workstation` | `workstation` (a person's own machine), `worker` or `server` |
| `share_present` | `0.5` | Share of threads offered while the person is using the machine |
| `share_away` | `0.75` | Share while they are away; the only share on a worker |
| `away_after_minutes` | `5` | Minutes without input before the person counts as away |
| `memory_reserve_gb` | a quarter of memory | Memory never offered |
| `disk_floor_gb` | `20` | Nothing new starts below this much free disk |
| `grace_hours` | `24` | A path changed more recently than this is not removable |
| `managed` | | A further folder a cleanup tool may touch. Repeatable. |
| `cap` | `hold` | `hold`: a running job is held to its grant by the best means here (rule 23). `stop`: held by stopping, even where a scope would work. `measure`: its use is measured and charged (rule 24), not held. |
| `cap_memory` | `offer` | The memory a job may not pass where memory can be held (rule 23): `offer`, all the memory offered, so the reserve is never taken; or `grant`, what the job asked for. |
| `device_slots` | `1` | How many jobs that run an emulator or simulator (`--device`, rule 27) may run at once. `0` refuses them. |
| `galley`, `galley_credential`, `galley_sender` | | Where and how to send the report to Galley. All three or none (`report-v1.md`). |

A `#` starts a comment at the start of a line or after a space. The daemon reads
`machine.conf` when it starts; restart it after a change.

Whether a person is present is measured on macOS only. Where it cannot be told, a
workstation counts its person as present.

## Rules

**Admission.**

1. The machine offers a number of threads (its threads times the share that applies,
   at least one, or none while its person holds it, rule 19) and an amount of memory
   (its memory less the reserve).
2. Work is admitted only while what is already admitted plus the new work fits the offer,
   and while those plus load on the machine that pressroom did not admit fit the machine.
   So outside load is taken first from the threads the machine keeps back (its person's
   share), and comes off the offer only once it passes them. Outside load is the
   processor in use, measured over the last few seconds, less what pressroom admitted.
   The load average stands in only where processor use cannot be measured: it also counts
   threads waiting on something else, and on a desktop runs several times above use.
3. Work asking for more threads than the offer is given the offer. Work is given what is
   free if that is at least half of what it asked. It is never given more than is free.
4. Work asking for more memory than the offer is refused, not queued.
5. Waiting work is ordered by class (`waiting`, then `gate`, then `batch`), then by
   arrival. Only the first in line may be admitted.
6. Every wait has a reason in plain words, shown to the asker and in `status`. An asker
   that stays connected but stops reading for two seconds loses its place in line.
7. With no daemon, `run` does not run the command. It exits 69.

**Lanes.**

8. A job that asks for a lane gets a folder no other running job has, the lowest-numbered
   free one for that repository and toolchain. The lane stays after the job, warm.
9. A lane that a tool is removing is not given out.

**Leases.**

10. A lease names paths, a holder, a kind and an end. It is live until its end time, and,
    if it names a process, only while that process exists.
11. A lease covers a path that is a leased path, is inside one, or contains one. Paths
    are compared after symbolic links and `..` are resolved the way the system resolves
    them. On macOS, case is ignored.
12. A job's leases (its lane, and what it named with `--lease`) are written before the
    command starts and removed when the job ends, however long it runs. They name the
    `run` process, which must exist when the job is admitted, so they also end if it dies. They cannot be released from outside the job.
13. A path being removed (rule 17) cannot be leased. A job that depends on it, or whose
    lanes are inside it, waits.

**Cleanup.** A tool may remove a path only if all of these hold:

14. It is inside a managed folder, and is not a managed folder itself.
15. No live lease covers it.
16. It was last changed longer ago than the grace time.
17. The tool holds a delete lease on it, taken with `delete-begin`, which checks 14 to 16
    at that moment and under the same lock as admission.

If any lease file cannot be read, every removal is refused until the file is fixed or
removed: it might be the lease on that path. Unreadable includes a path with `.` or `..`
in it and the same key written twice.

`may-delete` checks 14 to 16 from the files alone and needs no daemon. It is for asking.
A tool that is about to remove something uses `delete-begin`.

**When something leased vanishes anyway.**

18. After the command ends, `run` checks each path it leased with `--lease` that existed
    when the command started. If one is gone it says so, the daemon records `leased_path_removed`, and if
    the command also failed, `run` exits 75 in place of the command's own code: a fault
    of the machine, not of the code.

**What the person says.** Added 2026-11-15 for the menu bar item (`apps/menubar`).

19. The person can say "I need the whole machine" (`hold`): nothing new is admitted, and
    work waits with that reason. Work already running goes on; version 1 cannot stop it.
    Work that could never fit is still refused, not held.
20. The person can say "take it all, I am away" (`give`): every thread is offered, as if
    the share were 1, and the person counts as away. The memory reserve still applies.
21. Either lasts until its end (1 minute to 7 days), until the person says `auto`, or
    until a new `hold` or `give` replaces it. `give` also ends once the machine sees
    input more than a minute after it was said: the person is back, and their own use
    comes first. A restart of the daemon forgets it. Each is written to `events.log`
    (`person_says`, and `person_says_ended` when it runs out).

**When the daemon is replaced.**

22. A daemon that stops forgets its line and what it admitted. Work that was waiting is
    not failed by that: its `run` looks for a daemon for up to two minutes and joins the
    new one's line, at the back of its class. Work that was running goes on undisturbed;
    the new daemon did not admit it, so it counts as outside load (rule 2) until it ends,
    and its leases, which are files, still stand.

**Holding a job to its grant.** Added 2026-11-16, after a night when six admitted jobs
holding 12 threads between them put a laptop's load average at 385.

23. A running job is held to the threads it was granted, by one of two means:
    - **On Linux, where the cpu controller is delegated to the user** (`systemd-run --user
      --scope` works), the command runs in a transient scope with a `CPUQuota` of its
      threads times 100% and a `MemoryMax` of the memory cap (`cap_memory`). The kernel
      holds everything in the scope, whatever it starts, `nohup` and double forks
      included.
    - **Elsewhere (macOS, and Linux without delegation),** `run` measures the processor
      its job uses twenty times a second. Once the job has used more than its grant, with
      a fifth of a second's worth to spare, `run` stops its processes (`SIGSTOP`) until
      the grant has caught up, then continues them (`SIGCONT`). Over any second the job
      averages no more than its grant. Memory is measured, not held.

    The job's processes are the command, which runs in a process group of its own; every
    descendant of it; everything in the process group of any of those, since only the
    job's own processes make such groups (a tool that runs each command in a group of its
    own, as Codex does, and a daemon that forks twice inside it, are found this way); and
    every process `run` has seen, even after its parent has gone. With `cap = measure` a
    job is measured and charged but not held.
24. The daemon charges each running job the larger of its grant and what it was last
    measured to use, in threads (rounded) and in memory, when it admits work (rules 2 and
    3). `run` measures every two seconds: in a Linux scope, from the scope's own counters
    (memory is its working set: what it holds less file cache it is not using);
    elsewhere, by adding up the processor time and memory (resident on Linux, footprint on
    macOS) of the job's processes, with the time of children that have ended. `status` shows use against grant and flags a
    job that is over: more than a quarter, and at least half a thread, above its
    threads, or above its memory.
25. A job is told on its standard error, in one plain line each: when it is first held
    back (it tried to use more than its grant and now runs slower), when the system ended
    a process of it for passing the memory cap, and when it cannot be held here and is
    only measured. The daemon writes the same to `events.log` (`held`, `memory_ended`,
    `not_held`, and `on_its_own` for rule 26), and the job's `ended` event says how it was
    held (`means`) and for how many seconds it was held back (`heldSecs`: the time it was
    stopped; for a scope, the tenths of a second in which the kernel throttled it at all,
    which is more). A job held by a scope counts as held back over a report when it was
    throttled in at least half the periods it ran in. Pressroom never ends a job itself.
26. A `run` started inside a running job is part of that job. It knows it is inside one
    from `PRESSROOM_JOB` and `PRESSROOM_JOB_PID` (the outer `run`), which a `run` sets
    only when its daemon knows this rule, since an older one would queue the inner run
    behind the job it is part of. It is admitted at once, charged nothing, given at most
    the outer job's threads, and gets its own lane and leases as usual. It holds nothing
    itself; the outer job's hold holds it. So a whole tool run, `pressroom run -- codex
    exec …`, is one job: the builds the tool starts through pressroom inside it neither
    wait for the machine nor take more of it. If the outer job ends first, the daemon
    makes each run inside it a job of its own at that moment, charged, holding a device
    slot if it asked for one; its `run` sees the outer `run` gone within a twentieth of a
    second and holds it from then on by stopping. A `PRESSROOM_JOB` that names no running
    job is ignored.
27. A job that runs an Android emulator or an iOS or visionOS simulator says so with
    `--device`. Only `device_slots` such jobs run at once, one by default; the next waits,
    and says which job holds the slot. The device's own processes count as the job's
    where they are its descendants (an emulator), and as outside load (rule 2) where the
    system starts them (a simulator).
28. A `run` tells the daemon how it holds its job as soon as the command starts, and,
    before it stops anything it has not told of, every process group of the job and the
    job's processes outside them. If a `run` ends without saying how its job ended (it
    was killed), the daemon continues all of those, so nothing is left stopped. If the job was
    stopped at that moment, the system also sends its process group `SIGHUP` and then
    `SIGCONT`, as it does any stopped group left without its parent: most programs end on
    the first. A job that was running at that moment goes on, no longer held.

## Lease file

```json
{"version":"pressroom.lease/1","id":"j-1763240000-7-lane","holder":"agent:builder-a",
 "kind":"cache","mode":"in-use","paths":["/home/galley/.pressroom/lanes/colophon/rust-1.99.1/0"],
 "created":1763240000,"expires":1763844800,"pid":4242}
```

| Field | |
|---|---|
| `id` | Letters, digits, `-`, `_`. The file is `<id>.json`. |
| `holder` | Who holds it. A label in this version, not a verified identity. |
| `kind` | `cache`, `worktree`, `toolchain`, `registry`, `result`, `scratch`, or `delete` |
| `mode` | `in-use` or `warm` |
| `paths` | Absolute, no `.` or `..`, symbolic links resolved |
| `created`, `expires` | Unix seconds |
| `pid` | Optional. The lease is dead once this process is gone. |

A reader that does not use the `pressroom` command applies rule 10 itself: skip a file
whose `expires` has passed or whose `pid` no longer exists. Files are written whole and
renamed into place, so a reader never sees half of one.

## The socket

One JSON object per line. The first line from the client names an `op`.

| `op` | Client sends | Daemon answers |
|---|---|---|
| `run` | `holder`, `label`, `class`, `threads`, `memoryGb`, `pid`, optional `lane` (`repo`, `toolchain`), `leases` (paths), `leaseKind`, `device` (true for rule 27), `within` (the outer job, rule 26) | Zero or more `{"state":"waiting","reason","ahead"}`, then `{"state":"admitted","job","threads","lanePath","leases","cap","capMemoryGb","within"}`, or `{"state":"refused","reason"}`. `cap` is `hold` or `measure`; `within` is the outer job or null. The job lasts while the connection is open. While it runs the client sends `{"op":"use","means","pgid","groups","pids"}` when the command starts and before a stop (rule 28), the same with `"threads","memoryGb","held","heldSecs"` every two seconds (rule 24), and `{"op":"event","event","text"}` for rule 25. It ends the job with `{"op":"done","exit","missing","heldSecs","means"}`. |
| `status` | | The machine, what is running, what waits and why, live leases |
| `lease` | `holder`, `kind`, `mode`, `paths`, `ttlSecs`, optional `pid` | `{"ok":true,"id","expires"}` |
| `release` | `id` | `{"ok":true,"existed"}` |
| `delete-begin` | `holder`, `paths` (one), `ttlSecs`, optional `pid` | `{"ok":true,"id","expires"}`, or refused with the reason |
| `delete-end` | `id` | `{"ok":true,"existed"}` |
| `person-says` | `mode` (`hold`, `give` or `auto`), `forSecs` (whole, 60 to 604800; only for `hold` and `give`), optional `holder` | `{"ok":true,"mode","until"}`; `until` is null for `auto` |

### What `status` answers

One object. Numbers in GB may have fractions; seconds are whole.

| Field | |
|---|---|
| `protocol` | `pressroom.local/1` |
| `name`, `kind`, `os`, `arch` | The machine. `kind` is `workstation` or `worker` (`server` reads as `worker`). |
| `threads`, `memoryGb` | What the machine has |
| `offeredThreads`, `offeredMemoryGb` | What it offers now (rule 1) |
| `personPresent` | Whether its person counts as present, after what they said |
| `personSays` | Null, or `{"mode","holder","since","until"}` while a `hold` or `give` stands |
| `usedThreads`, `usedMemoryGb` | What admitted work holds |
| `busyThreads` | Threads' worth of processor in use over the last few seconds, by anything; null where it cannot be measured |
| `load1`, `outsideLoad` | Load average, and the whole busy threads pressroom did not admit (rule 2) |
| `diskFreeGb`, `diskFloorGb` | Free disk and the floor below which nothing new starts |
| `toolchains` | `[{"name","version"}]`, for `rustc`, `cargo`, `bun`, `node` and `xcode`: the first copy that answers, on the daemon's PATH, then in `/opt/homebrew/bin`, `/usr/local/bin`, and under the home folder `.local/bin`, `.cargo/bin`, `.local/share/mise/shims`, `.bun/bin`, and the newest `.rustup/toolchains/*/bin` and `.nvm/versions/node/*/bin` |
| `managed` | The managed folders |
| `running` | `[{"job","holder","label","class","threads","memoryGb","lane","pid","forSecs","useThreads","useMemoryGb","over","held","heldSecs","means","device","within"}]`; `lane`, `pid`, `useThreads`, `useMemoryGb`, `means` and `within` may be null. `useThreads` and `useMemoryGb` are what it was last measured to use (rule 24); `over` flags it; `held` is whether it was being held back at the last measure; `means` is how it is held: `scope`, `stop`, `measure` (measured only), or `within` (by its outer job) |
| `cap`, `deviceSlots`, `devicesInUse` | The `cap` setting, and the device slots (rule 27) and how many are taken |
| `waiting` | `[{"holder","label","class","threads","reason","forSecs"}]`, in the order they will be admitted |
| `leases` | Live leases, each as its lease file |
| `unreadableLeases` | Names of lease files that cannot be read |

A client ignores fields it does not know; later versions may add some.

A refusal is `{"ok":false,"state":"refused","reason":"..."}`. A request that cannot be
read, or has a field of the wrong type, is refused in the same way; no field is quietly
replaced by a default. A connection that sends nothing for ten seconds is closed.

## Exit codes of `pressroom run`

| Code | Meaning |
|---|---|
| the command's own | It ran. A command ended by a signal gives 128 plus the signal. |
| 64 | Wrong arguments |
| 69 | No daemon, or refused |
| 75 | The command failed and something it leased had been removed |
| 127 | The command could not be started (126 or 127 for batch work, which starts through `nice`) |

## What version 1 does not do

- **It cannot stop work started outside it.** It counts that load against the machine
  (rule 2) and shows it in `status`, with the programs using the most processor, so its
  own work backs off when the machine is full.
- **A hold covers the job's own processes only** (rule 23). In a Linux scope, what the
  job has someone else start escapes it: a service started through systemd or D-Bus
  (including another `systemd-run --user`), a container run by a system daemon such as
  Docker's, and work handed to a server that was already running (a Gradle or Bazel
  daemon started earlier or by another job, a tool's own background server). On macOS,
  and on Linux without delegation, so does a process that starts a session or group of
  its own (`setsid`) and whose parent ends before `run` has seen it, within a twentieth
  of a second; a process of another user (`sudo`); and anything launchd starts for the
  job: the iOS simulator runs under launchd, so it is outside load, not the job's. What
  escapes is counted as outside load (rule 2), not charged to the job.
- **Stopping is coarse.** A job held by stopping runs in bursts of a twentieth of a second
  or so and then waits, and one held by a scope is throttled in tenths of a second. A test
  that measures time can fail under its hold, as it would on a busy machine. Ask for the
  threads a suite really uses.
- **The thread count is still a hint to the tools.** The job is told it through
  `PRESSROOM_THREADS`, `CARGO_BUILD_JOBS`, `RUST_TEST_THREADS`, `MAKEFLAGS` and
  `CMAKE_BUILD_PARALLEL_LEVEL`, and batch work runs at lower priority. A tool that starts
  sixteen threads on a grant of two is held to two and runs slower than if it had started
  two. Xcode needs `-jobs "$PRESSROOM_THREADS"`.
- **What a job leaves running after its command ends is no longer held,** except in a
  Linux scope, which lasts as long as anything in it: `run` returns when the command
  does, as before. A run inside the job is the exception (rule 26).
- **Past 8,192 processes a job is held by its process groups only.** Processes beyond
  that are stopped with their groups but not measured, and a group first made by one of
  them is not found. The job is told.
- **Memory is held on Linux only.** On macOS a job above its memory is charged and
  flagged (rule 24), not stopped.
- **Rule 18 cannot tell who removed a path.** A job that removes what it leased and then
  fails is also reported as a fault of the machine.
- **A `run` that is killed outright frees its place at once,** even if the command it
  started is still going. In a Linux scope that command is still held; elsewhere it goes
  on unheld, or, if it was stopped at that moment, the system hangs it up (rule 28).
- **Holders are labels.** Anything running as the daemon's user can use the socket, and
  the process a job names is taken on trust beyond being alive. That includes
  `person-says`: an agent running as the person's user could say `give`. The events log
  records who said it.
  Identity and grants come with enrolment.
- **It does not judge "stuck".** `status` shows how long a job has run and how busy the
  machine is.
- **The grace time looks at the path itself,** not at everything inside it.
- **Many idle connections can use up the daemon's file handles** for up to ten seconds.
- **Linux is tested on two machines only,** both with the cpu controller delegated.
  Holding by stopping is tested on Linux with `cap` forced to it, not on a machine without
  delegation.
- No power, no energy figure, no data classes. The report to Galley is `report-v1.md`.
