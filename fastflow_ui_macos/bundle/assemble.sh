#!/bin/sh
# Usage: assemble.sh <bin-dir> <app> <identity> [codesign flags...]
# Builds fastflow.app from the fastflow-app and fastflow binaries in <bin-dir> and signs it.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
bin=$1
app=$2
identity=$3
shift 3

version=$(cd "$root" && cargo metadata --no-deps --format-version 1 \
    | sed -n 's/.*"name":"fastflow_ui_macos","version":"\([^"]*\)".*/\1/p')

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$here/fastflow.icns" "$app/Contents/Resources/fastflow.icns"
sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"
cp "$bin/fastflow-app" "$app/Contents/MacOS/fastflow-app"
cp "$bin/fastflow" "$app/Contents/MacOS/fastflow"

# Nested code is signed before the bundle that contains it.
codesign --force --sign "$identity" --identifier com.excsn.mac.fastflow.cli "$@" \
    "$app/Contents/MacOS/fastflow"
codesign --force --sign "$identity" --identifier com.excsn.mac.fastflow "$@" "$app"
codesign --verify --strict --verbose=1 "$app"
