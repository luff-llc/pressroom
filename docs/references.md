# Dependencies

None. The Rust standard library only.

To add one: its name, version, licence (MIT, Apache-2.0 or BSD; no GPL), and why the
standard library is not enough, in this file, before it appears in `Cargo.toml`.

The daemon runs these programs, which every supported system ships: `df`, `hostname`,
`uname`, `nice`, and on macOS `sysctl`, `ioreg` and `/bin/kill`. `run` uses
`systemd-run` (systemd 254 or later, for `--expand-environment=no`) on Linux where it is
there, and `/bin/sh` to ask it whether a scope holds. It calls libc and, on macOS,
libproc (`proc_listchildpids`, `proc_listpgrppids`, `proc_pidinfo`, `proc_pid_rusage`),
which every program there already links. It probes for `rustc`,
`cargo`, `bun`, `node` and `xcodebuild` to report toolchains.

Placed work (`contracts/job-v1.md`) also runs `git`, `tar` and, for a machine reached over
SSH, `ssh`.
