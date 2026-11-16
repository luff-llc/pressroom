#!/bin/sh
# Build Pressroom.app from the Swift package: release build, Info.plist, ad-hoc signature.
# Usage: scripts/bundle.sh [output folder, default build]. Start it through pressroom:
#   pressroom run --threads 4 --memory-gb 4 --lane pressroom-menubar -- scripts/bundle.sh
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
codesign --force --sign - --timestamp=none "$app"
echo "$app"
