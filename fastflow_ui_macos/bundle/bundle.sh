#!/bin/sh
# Usage: bundle.sh [--install]
# FASTFLOW_SIGN_IDENTITY selects the codesign identity, default "-" (ad-hoc).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
profile=${FASTFLOW_PROFILE:-release}
identity=${FASTFLOW_SIGN_IDENTITY:--}

cd "$root"
if [ "$profile" = release ]; then
    cargo build --release -p fastflow_ui_macos
else
    cargo build -p fastflow_ui_macos
fi

version=$(cargo metadata --no-deps --format-version 1 \
    | sed -n 's/.*"name":"fastflow_ui_macos","version":"\([^"]*\)".*/\1/p')

app="$root/target/$profile/fastflow.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$here/fastflow.icns" "$app/Contents/Resources/fastflow.icns"
sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"
cp "$root/target/$profile/fastflow-app" "$app/Contents/MacOS/fastflow-app"
codesign --force --sign "$identity" --identifier com.excsn.mac.fastflow "$app"
codesign --verify --verbose=1 "$app"

if [ "${1:-}" = --install ]; then
    pkill -x fastflow-app || true
    rm -rf /Applications/fastflow.app
    cp -R "$app" /Applications/fastflow.app
    echo "installed /Applications/fastflow.app"
else
    echo "built $app"
fi
