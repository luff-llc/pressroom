#!/bin/sh
# Make a notarized release of Pressroom for macOS (Apple silicon): the menu bar app with the
# daemon inside, signed as LUFF LLC, notarized and stapled.
# Usage: scripts/release.sh VERSION    (VERSION must equal Cargo.toml's)
# Needs: LUFF LLC's Developer ID in the keychain, and `asc` signed in with an App Store
# Connect API key of the LUFF LLC team (asc auth status). Start it through pressroom:
#   pressroom run --threads 4 --memory-gb 6 --lane pressroom-release -- apps/menubar/scripts/release.sh 0.1.0
# Writes dist/<VERSION>/Pressroom-<VERSION>-macOS-arm64.zip and prints its SHA-256.
set -eu
version=$1
here=$(cd "$(dirname "$0")/.." && pwd)
root=$(cd "$here/../.." && pwd)
cargo_version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
[ "$cargo_version" = "$version" ] || { echo "release.sh: Cargo.toml says $cargo_version, not $version" >&2; exit 64; }
[ "$(uname -m)" = "arm64" ] || { echo "release.sh: build on Apple silicon" >&2; exit 64; }
out="$root/dist/$version"
rm -rf "$out" && mkdir -p "$out"

(cd "$root" && CARGO_TARGET_DIR="$root/target" cargo build --release --locked)
PRESSROOM_DAEMON="$root/target/release/pressroom" "$here/scripts/bundle.sh" "$out" >/dev/null
app="$out/Pressroom.app"
plutil -replace CFBundleShortVersionString -string "$version" "$app/Contents/Info.plist"
plutil -replace CFBundleVersion -string "$version" "$app/Contents/Info.plist"
"$here/scripts/sign.sh" "$app"
codesign -dv "$app" 2>&1 | grep -q "TeamIdentifier=J53CA7SFMK" || { echo "release.sh: not signed as LUFF LLC" >&2; exit 1; }

zip="$out/Pressroom-$version-macOS-arm64.zip"
ditto -c -k --keepParent "$app" "$out/submit.zip"
result=$(asc notarization submit --file "$out/submit.zip" --wait --timeout 1h --output json)
echo "$result" > "$out/notarization.json"
echo "$result" | grep -q '"Accepted"' || { echo "release.sh: notarization was not accepted: $result" >&2; exit 1; }
rm "$out/submit.zip"
xcrun stapler staple "$app"
xcrun stapler validate "$app"
spctl --assess --type execute --verbose "$app"
ditto -c -k --keepParent "$app" "$zip"
shasum -a 256 "$zip" | tee "$zip.sha256"
