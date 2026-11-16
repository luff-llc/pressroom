#!/bin/sh
# Sign Pressroom.app. A setting with a default: PRESSROOM_SIGN_IDENTITY, else LUFF LLC's
# Developer ID if this Mac's keychain has it (hardened runtime, secure timestamp), else ad hoc
# with a warning. Software is signed as LUFF LLC, never as a person.
# Usage: scripts/sign.sh path/to/Pressroom.app
set -eu
app=$1
luff="Developer ID Application: LUFF LLC (J53CA7SFMK)"
identity=${PRESSROOM_SIGN_IDENTITY:-}
if [ -z "$identity" ] && security find-identity -v -p codesigning | grep -qF "\"$luff\""; then
    identity=$luff
fi
daemon="$app/Contents/MacOS/pressroom"
if [ -z "$identity" ] || [ "$identity" = "-" ]; then
    echo "sign.sh: no LUFF LLC Developer ID on this Mac; signing ad hoc (fine on this Mac only)" >&2
    [ -f "$daemon" ] && codesign --force --sign - --timestamp=none --identifier works.luff.pressroom "$daemon"
    codesign --force --sign - --timestamp=none "$app"
else
    # Nested code first, then the app around it.
    [ -f "$daemon" ] && codesign --force --options runtime --timestamp \
        --identifier works.luff.pressroom --sign "$identity" "$daemon"
    codesign --force --options runtime --timestamp --sign "$identity" "$app"
fi
codesign --verify --strict "$app"
