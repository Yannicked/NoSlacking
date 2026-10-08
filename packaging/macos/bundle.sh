#!/bin/sh
# Wraps a built noslacking binary, and its video helper, in
# NoSlacking.app. Runs on macOS (it needs sips and iconutil for the icon,
# and codesign for an ad-hoc signature).
#
#   packaging/macos/bundle.sh <binary> <version> <output dir> <helper>
#
# For example, after `cargo build --release -p noslacking -p noslacking-video`:
#
#   packaging/macos/bundle.sh target/release/noslacking 0.1.0 dist target/release/noslacking-video
#
# The helper goes beside the app in Contents/MacOS, where the app looks;
# huddle video is decoded only there, and the camera and a shared
# screen captured there (covered by the app's permissions).
#
# The bundle is signed ad hoc, not with a Developer ID, so Gatekeeper asks
# before the first launch of a downloaded copy.
set -eu

if [ "$#" -ne 4 ]; then
    echo "usage: $0 <binary> <version> <output dir> <helper>" >&2
    exit 2
fi
binary=$1
version=$2
out=$3
helper=$4
here=$(cd "$(dirname "$0")" && pwd)
icons="$here/../icons/hicolor"

app="$out/NoSlacking.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/noslacking"
cp "$helper" "$app/Contents/MacOS/noslacking-video"
sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"

# iconutil wants every size, and @2x, in an .iconset directory.
iconset=$(mktemp -d)/NoSlacking.iconset
mkdir -p "$iconset"
source_png="$icons/512x512/apps/cloud.yannick.NoSlacking.png"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$source_png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    if [ "$double" -le 512 ]; then
        sips -z "$double" "$double" "$source_png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
    fi
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/NoSlacking.icns"
rm -rf "$(dirname "$iconset")"

codesign --force --deep --sign - "$app"
echo "$app"
