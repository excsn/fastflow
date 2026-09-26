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
    cargo build --release -p fastflow_ui_macos -p fastflow_cli
else
    cargo build -p fastflow_ui_macos -p fastflow_cli
fi

app="$root/target/$profile/fastflow.app"
"$here/assemble.sh" "$root/target/$profile" "$app" "$identity"

if [ "${1:-}" = --install ]; then
    pkill -x fastflow-app || true
    rm -rf /Applications/fastflow.app
    cp -R "$app" /Applications/fastflow.app
    echo "installed /Applications/fastflow.app"
else
    echo "built $app"
fi
