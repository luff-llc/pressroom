# Holding a job to its grant: the check (2026-11-16)

Checked by a separate agent (Codex, `gpt-daybreak-blue-latest`, read-only) that built none
of it, on commit `c8c4faf` of branch `hold-to-grant`, against `contracts/local-v1.md` rules
23 to 28. It read the code and the contract; it ran nothing. The builder (Claude Opus) fixed
everything it found in one round, with a test for each where one could be written. **The
fixes were tested by the builder on macOS and Linux; they have not been checked again by a
separate agent.**

After the fixes, on macOS (M3 Max) and on Linux (systemd 262, cpu controller delegated):
unit `34 passed; 0 failed`, end-to-end `30 passed; 0 failed`, clippy clean, on both.

Verdict at `c8c4faf`: "not ready: two direct macOS hold escapes, a stopped-process cleanup
gap, and a backwards-compatibility deadlock."

| # | Found | Done |
|---|---|---|
| 1 | High. A `run` killed while its job was stopped could leave part of it stopped for ever: the daemon heard only every two seconds, knew only the first process group, at most 512 processes, and nothing before the first report. | Fixed: `run` tells the daemon how it holds the job as soon as it starts, and before every stop whose targets changed, every process group of the job and its processes outside them (up to 4,096 each). The daemon continues all of them. Test `a_killed_run_leaves_no_group_of_its_job_stopped` (a second group whose parent lives, so the system does not hang it up). |
| 2 | High. A run inside a job that outlived the outer command ran unheld and uncharged on macOS, and a shared device slot was freed under it. | Fixed: when the outer job ends the daemon, under the same lock, makes each run inside it a job of its own, charged and holding a slot if it asked for one; its `run` sees the outer `run` gone and holds it by stopping. Liveness treats a zombie as gone (found while testing: a parent that reads the pipes before waiting keeps the outer `run` a zombie). Test `a_run_left_behind_by_its_job_becomes_a_job_of_its_own`. |
| 3 | High. A new client against an old daemon: the inner run sent `within`, the old daemon queued it behind the job it was part of, and neither could finish. | Fixed: a `run` sets `PRESSROOM_JOB_PID` only when its daemon says how it holds work (it knows rule 26), and an inner run asks to be part of a job only when it sees that. Against an old daemon it behaves as before. |
| 4 | Medium. Past 4,096 processes, descendants were dropped, and a group made by one of them was never found. | Partly: the ceiling is 8,192; beyond it processes are still stopped with their groups, and the job is told. A group first made past the ceiling is still not found. Stated in the contract. |
| 5 | Medium. A waiter with a stale `PRESSROOM_JOB` stopped holding its place in line. | Fixed: it is passed over only while its outer job really runs. |
| 6 | Low. A scope's held time counted every throttled period as a whole period. | Changed: "held" over a report now means throttled in at least half the periods it ran in; `heldSecs` for a scope is stated in the contract as the periods in which it was throttled at all, an upper bound. The kernel's `throttled_usec` is summed over processors, so it is no truer. |

## Found by the builder before the check

- `codex exec` runs each command in a process group of its own, and `(yes &)` forks twice
  out of the tree: the first build missed it (four spinners at full speed beside a job
  measured at 0.1 threads). Fixed by treating every group a job's process is in as the
  job's. Test `a_tool_that_runs_commands_in_groups_of_their_own_is_still_held`, which fails
  with that change undone. Then re-run with real Codex: the spinners were stopped and the
  job measured 1.1 threads.
- `systemd-run --scope` expands `$NAME` and `$$` in the command's own arguments
  (`sh -c 'kill -9 $$'` exited 0). Fixed with `--expand-environment=no`, which the probe
  also uses, so a systemd older than 254 falls back to stopping.
- A `run` killed while its job is stopped leaves an orphaned stopped process group, which
  the system itself sends `SIGHUP` and `SIGCONT`. Stated in rule 28.
- A scope's `memory.current` counts file cache; use is now the working set.
