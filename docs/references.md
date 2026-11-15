# Dependencies

None. The Rust standard library only.

To add one: its name, version, licence (MIT, Apache-2.0 or BSD; no GPL), and why the
standard library is not enough, in this file, before it appears in `Cargo.toml`.

The daemon runs these programs, which every supported system ships: `df`, `hostname`,
`uname`, `nice`, and on macOS `sysctl`, `ioreg` and `/bin/kill`. It probes for `rustc`,
`cargo`, `bun`, `node` and `xcodebuild` to report toolchains.
