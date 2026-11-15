# The report to Galley, version 1

What the daemon on one machine tells Galley about itself, how often, and how. Version
`pressroom.report/1`. The local contract (`local-v1.md`) still governs everything on the
machine; this adds one outbound message and three settings. Galley's side (what it stores,
what it works out, what it shows) is `contracts/machine/README.md` in the Galley repository.
Tested by `tests/end_to_end.rs` here and `tests/machines.test.ts` in Galley.

## When it is sent

Only when `machine.conf` sets all three of these. With none set the daemon sends nothing
and behaves exactly as `local-v1.md` says. With one or two set it does not start.

| Key | Meaning |
|---|---|
| `galley` | Galley's address: `https://` and a host, nothing after it. Plain `http://` only for `localhost`, `127.0.0.1` or `[::1]`. |
| `galley_credential` | Absolute path of the machine's agent identity configuration: the same file and key an agent uses (`docs/agent-connect.md` in Galley). It names the project, the grant and the machine's principal. |
| `galley_sender` | Absolute path of the program that sends the report (Galley's `galley-machine-report`). |

The daemon sends one report when it starts and then one every 20 seconds, counted from when
it started, not from when the last one finished. It opens no port. A report that fails is not
retried before the next one is due; a report still being sent when the next is due makes that
one wait for the following tick.

## How it is sent

The daemon does not speak HTTPS. Galley signs every request with the agent's own key (a
DPoP proof per request), which a Rust program with no dependencies cannot do, and a TLS or
crypto crate would need an allowlist entry. So the daemon runs the sender:

```
<galley_sender> --origin <galley> --identity <galley_credential>
```

with the report as one line of JSON on standard input, an environment holding only `PATH`
and `HOME`, and 15 seconds to finish, after which it is killed, whether or not it has read
its input. A sender that is a wrapper script should `exec` the program it wraps, so
that killing it ends the program too. Its arguments are paths and
an address, never a secret. The sender loads the identity configuration with Galley's own
loader, refuses it if its `resource` is not at `<galley>`, and sends the command
`host.heartbeat` with body `{ "report": <the report> }`, resource the configuration's
`agentId`, scope and grant from the configuration, through Galley's ordinary signed client.

| Sender exit | Meaning |
|---|---|
| 0 | Galley recorded the report |
| 1 | Not recorded: Galley refused it or could not be reached. One line on standard error names the code; it never prints a token. |
| 3 | The machine's access has ended or was ended by its owner |
| 64 | Wrong arguments, or a configuration that cannot be read |

The daemon writes `report_failed` (with the exit) to `events.log` when a report fails after
one that did not, and `report_sent` when one succeeds after a failure, so a machine that is
refused every 20 seconds writes one line, not thousands.

## The report

One JSON object. Every field is required; there are no others. Every number is a whole
number (Galley's records hold no fractions), so memory and disk are in bytes and load is in
hundredths.

```json
{"v":"pressroom.report/1","measuredAt":1763240000,
 "name":"worker-a","kind":"worker","os":"Linux 6.12.1","arch":"x86_64",
 "threads":32,"memoryBytes":68719476736,"offeredThreads":24,"offeredMemoryBytes":51539607552,
 "usedThreads":8,"usedMemoryBytes":6442450944,"personPresent":false,
 "loadHundredths":950,"outsideThreads":2,
 "diskFreeBytes":442381631488,"diskFloorBytes":21474836480,
 "toolchains":[{"name":"rustc","version":"rustc 1.99.1 (abc 2026-10-30)"}],
 "running":[{"job":"j-1763240000-7","holder":"agent:builder-a","label":"cargo test",
             "class":"gate","threads":8,"memoryBytes":6442450944,"forSecs":340}],
 "waiting":[{"holder":"agent:builder-b","label":"cargo build","class":"batch","threads":8,
             "reason":"14 of 24 threads free; it needs 16","forSecs":20}],
 "leases":[{"id":"j-1763240000-7-lane","holder":"agent:builder-a","kind":"cache",
            "mode":"in-use","paths":["/home/galley/.pressroom/lanes/colophon/default/0"],
            "expires":1763844800}],
 "unreadableLeases":0}
```

| Field | What | Measured or declared |
|---|---|---|
| `measuredAt` | Unix seconds on the machine's clock when the report was made | |
| `name` | `name` in `machine.conf`, else the host name | declared |
| `kind` | `workstation` or `worker` (`server` reports as `worker`, as in `status`) | declared |
| `os`, `arch` | `uname -sr`; the processor family the daemon was built for | measured |
| `threads`, `memoryBytes` | What the machine has | measured |
| `offeredThreads`, `offeredMemoryBytes` | What it offers agents now (`local-v1.md`, rule 1); `offeredThreads` is at most `threads` | worked out |
| `usedThreads`, `usedMemoryBytes` | What admitted work holds | measured |
| `personPresent` | Whether its person counts as present | measured (macOS), else as `local-v1.md` says |
| `loadHundredths` | One-minute load times 100, rounded | measured |
| `outsideThreads` | Load the daemon did not admit, in whole threads, as admission counts it | worked out |
| `diskFreeBytes`, `diskFloorBytes` | Free disk where the daemon keeps its files, and the owner's floor | measured; declared |
| `toolchains` | Each toolchain found, with its version line as the tool prints it, at most 16 | measured |
| `running` | Admitted work: its job, holder, label, class, threads, `memoryBytes` and seconds running, at most 64 | measured |
| `waiting` | Work in line, in order, with the reason in plain words, at most 64 | measured |
| `leases` | Live leases: id, holder, kind, mode, paths, end, at most 256 | measured |
| `unreadableLeases` | How many lease files cannot be read | measured |

Text fields are at most 256 bytes, except `name` and `job` (128), `os` and a toolchain's
`version` (512) and a lease path (1024); longer text is cut, blank text is sent as `-`, and a
lease path that is too long is left out rather than cut. Lists longer than their limit are
cut to it, the first in order kept. **The whole report is at most 256 KiB:** while it is
larger, leases are left out from the end. A report never carries a process id, the lane field
or managed folders that `status` shows, a token or a key. Lease paths are sent as they are,
so they name lane folders and other paths under the daemon's home.

## What the report is not

- **It declares no capability.** Galley works capabilities out of `os`, `arch` and
  `toolchains`; the daemon never sends a list of them.
- **It is not an enrolment.** The machine's principal, its key and its grant exist before
  the first report, made the way any agent's are. A report from a principal that is not the
  host's is refused by Galley. Making machines into device members is slice 3.
- **Holders are still labels.** Galley shows them as the daemon wrote them.
- **The clock is the machine's.** Galley judges freshness by when it received the report,
  not by `measuredAt`.
