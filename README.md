# Pressroom

The machines agents run work on. One small gatekeeper runs on each machine. Nothing heavy
starts without it; it knows what the machine offers and how busy it is; and it keeps the
list of what is in use, so that a cleanup tool can see what it must leave alone.

This repository is the daemon and its contracts. The design is in the Galley repository
(`docs/design/2026-11-15-device-capacity.md`). **Status: slice 1, the local half.** It
runs on one machine and talks to nothing else yet.

## Install

On a Mac with Apple silicon, the menu bar app with the daemon inside it:

```sh
brew install --cask luff-llc/tap/pressroom
```

Open Pressroom once. It starts the daemon at login unless one is already running, and links
the `pressroom` command. On Linux, build from source (below) and use `deploy/pressroom.service`.

## Use

```sh
pressroom daemon                      # once per machine, as the user that runs the work

pressroom run --threads 8 --memory-gb 8 --lane colophon --toolchain rust-1.99.1 \
    --lease ~/.cargo/registry --lease-kind registry \
    -- cargo test --workspace --offline

pressroom status                      # what is running, what waits and why
```

`run` waits until the machine has room, says why it is waiting, then runs the command
with the thread count it was given (`CARGO_BUILD_JOBS` and the like), in a build folder
of its own (`CARGO_TARGET_DIR`), holding a lease on that folder and on anything named
with `--lease`. `pressroom help` lists everything.

For a cleanup tool:

```sh
id=$(pressroom delete-begin /path/under/a/managed/folder) && rm -rf /path/... ; pressroom delete-end "$id"
```

`delete-begin` exits 1 and says why when the path must be kept. A tool that knows nothing
of pressroom can read `~/.pressroom/leases/*.json` directly. The rules are in
[contracts/local-v1.md](contracts/local-v1.md).

## Seen in Galley

With `galley`, `galley_credential` and `galley_sender` set in `machine.conf`, the daemon
sends Galley a report every 20 seconds: what `status` measures, in whole numbers. It sends
it by running Galley's `galley-machine-report` with the report on standard input. The
sender signs with the machine's own key, the way any agent does. The daemon opens no port.
Galley shows the machine on the board's Machines tab. The rules are in
[contracts/report-v1.md](contracts/report-v1.md).

## In the menu bar

`apps/menubar` is a macOS menu bar item that shows what `status` shows and lets the person
hold the machine or give it all for a while (`pressroom person hold|give|auto`). It is a
client of the socket only. See [apps/menubar/README.md](apps/menubar/README.md).

## What it would have changed on 14 November

- Four builds on one laptop, load above 30: the second build waits, and says so.
- A slow suite taken for a hung one: `status` shows it running and how busy the machine is.
- Stale binaries from one build folder shared by several worktrees: one lane per build.
- A cache removed under an offline build: the lease says keep, and if it is removed
  anyway the job fails as a fault of the machine, with the path named.

## Limits

Read "What version 1 does not do" in the contract. The main one: it governs only what is
started through it.

## Build

Rust 1.99.1, no dependencies. `cargo test` runs the unit tests and the end-to-end tests,
which start a real daemon on a scratch folder in `/tmp`.

Example service files are in `deploy/`. Nothing installs them.

## Licence

Apache-2.0. Copyright 2026 LUFF LLC.
