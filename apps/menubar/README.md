# Pressroom in the menu bar

A small macOS menu bar item for the Pressroom daemon on this Mac. It shows:

- whether the daemon is running;
- threads and memory in use against what the machine offers, and load not started
  through pressroom;
- whether the machine thinks its person is present;
- what is running, what waits and why;
- free disk against the floor;
- live leases.

It also lets the person say **Hold the machine** ("I need the whole machine": nothing new
starts) or **Take it all** ("I am away": every thread is offered), for a set time, and
**Back to automatic**. Those are the person's rules, 19 to 21 of
[`contracts/local-v1.md`](../../contracts/local-v1.md).

It is only a client. It talks to the daemon over its socket (`$PRESSROOM_HOME/sock`, else
`~/.pressroom/sock`) with the `status` and `person-says` ops, every three seconds. It never
starts, admits, leases or cleans anything, and with no daemon it says so and does nothing.

![Held](docs/panel-held.png) ![Given](docs/panel-given.png)

## The daemon inside

The release build (`scripts/release.sh`) carries the `pressroom` command in
`Contents/MacOS/pressroom` and a LaunchAgent for it, which the app registers with
`SMAppService`. **Run the daemon at login** is a setting with a default: on, applied once,
the first time an installed app finds no daemon answering. A Mac whose daemon was started
another way (a hand-made LaunchAgent, a systemd unit) keeps that one; the app refuses to start
a second. The Homebrew cask links the command onto the PATH.

## Start at login

On by default, applied once, the first time the app runs from `/Applications` or
`~/Applications`. Turn it off with the checkbox in the panel. A copy run from a build
folder is never registered.

## Build

Swift 6 language mode, macOS 15 or later, no dependencies. Start builds through pressroom:

```sh
pressroom run --threads 4 --memory-gb 4 --lane pressroom-menubar -- swift test
pressroom run --threads 4 --memory-gb 4 --lane pressroom-menubar -- scripts/bundle.sh
```

A notarized release: `scripts/release.sh VERSION` (Apple silicon, LUFF LLC's Developer ID
and an App Store Connect key for `asc`) builds the daemon and the app, signs both, notarizes,
staples and writes `dist/<VERSION>/Pressroom-<VERSION>-macOS-arm64.zip`.

`scripts/bundle.sh` writes `build/Pressroom.app` and signs it with `scripts/sign.sh`: as
LUFF LLC's Developer ID (hardened runtime) where the keychain has it, otherwise ad hoc. Build
anywhere, then sign on a Mac that holds the identity: `scripts/sign.sh path/to/Pressroom.app`.
Copy it to `~/Applications` to keep it. It is not notarized yet, so a copy downloaded from
the internet is refused by Gatekeeper; a copy made locally runs.

`PressroomKit` holds the socket client and the status decoding; its tests use a fake
daemon on a scratch socket and synthetic fixtures.

`Pressroom.app/Contents/MacOS/PressroomMenuBar --window` opens the same panel in an
ordinary window, for a menu bar that is hidden or full, and prints the window number so
`screencapture -l` can take a picture of the panel alone.
