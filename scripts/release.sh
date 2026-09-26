#!/bin/sh
# Usage: release.sh [--no-notarize]
#
# Builds an Apple Silicon fastflow.app with the CLI inside, signs it for distribution, packs it into
# dist/fastflow-<version>.dmg, notarizes and staples the dmg and writes the Homebrew cask to
# dist/fastflow.rb.
#
# FASTFLOW_RELEASE_IDENTITY  a "Developer ID Application: ..." codesign identity (required)
# FASTFLOW_NOTARY_PROFILE    the notarytool keychain profile, default "fastflow"
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
identity=${FASTFLOW_RELEASE_IDENTITY:?set FASTFLOW_RELEASE_IDENTITY to a Developer ID Application identity}
profile=${FASTFLOW_NOTARY_PROFILE:-fastflow}
notarize=true
[ "${1:-}" = --no-notarize ] && notarize=false

cd "$root"
version=$(cargo metadata --no-deps --format-version 1 \
    | sed -n 's/.*"name":"fastflow_ui_macos","version":"\([^"]*\)".*/\1/p')
dist="$root/dist"
work="$dist/work"
rm -rf "$work"
mkdir -p "$work/bin" "$work/dmg"

target=aarch64-apple-darwin
cargo build --release --target "$target" -p fastflow_ui_macos -p fastflow_cli
cp "target/$target/release/fastflow-app" "target/$target/release/fastflow" "$work/bin/"

app="$work/dmg/fastflow.app"
"$root/fastflow_ui_macos/bundle/assemble.sh" "$work/bin" "$app" "$identity" \
    --options runtime --timestamp
ln -s /Applications "$work/dmg/Applications"

dmg="$dist/fastflow-$version.dmg"
rm -f "$dmg"
hdiutil create -quiet -volname fastflow -srcfolder "$work/dmg" -format UDZO "$dmg"
codesign --force --sign "$identity" --timestamp "$dmg"

if $notarize; then
    echo "notarizing $dmg"
    xcrun notarytool submit "$dmg" --keychain-profile "$profile" --wait
    xcrun stapler staple "$dmg"
    spctl -a -vv -t open --context context:primary-signature "$dmg"
    spctl -a -vv -t exec "$app"
else
    echo "skipped notarization; Gatekeeper will reject this dmg on other Macs"
fi

sha=$(shasum -a 256 "$dmg" | cut -d' ' -f1)
sed -e "s/@VERSION@/$version/" -e "s/@SHA256@/$sha/" \
    "$root/packaging/fastflow.rb" > "$dist/fastflow.rb"
rm -rf "$work"

echo "dmg   $dmg"
echo "sha   $sha"
echo "cask  $dist/fastflow.rb"
