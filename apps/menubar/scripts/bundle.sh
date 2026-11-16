#!/bin/sh
# Build Pressroom.app from the Swift package: release build, Info.plist, signature.
# Usage: scripts/bundle.sh [output folder, default build]. Start it through pressroom:
#   pressroom run --threads 4 --memory-gb 4 --lane pressroom-menubar -- scripts/bundle.sh
# Signing: scripts/sign.sh, as LUFF LLC's Developer ID when this Mac has it, else ad hoc.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-"$here/build"}
jobs=${PRESSROOM_THREADS:-2}
cd "$here"
swift build -c release --jobs "$jobs" --product PressroomMenuBar
bin=$(swift build -c release --show-bin-path)/PressroomMenuBar
app="$out/Pressroom.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp "$bin" "$app/Contents/MacOS/PressroomMenuBar"
cp Resources/Info.plist "$app/Contents/Info.plist"
# With PRESSROOM_DAEMON pointing at a built `pressroom`, the app carries the daemon and the
# LaunchAgent that runs it (registered by the app, scripts/release.sh sets this).
if [ -n "${PRESSROOM_DAEMON:-}" ]; then
    cp "$PRESSROOM_DAEMON" "$app/Contents/MacOS/pressroom"
    mkdir -p "$app/Contents/Library/LaunchAgents"
    cp Resources/works.luff.pressroom.daemon.plist "$app/Contents/Library/LaunchAgents/"
fi
"$here/scripts/sign.sh" "$app"
echo "$app"
