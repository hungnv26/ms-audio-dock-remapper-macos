#!/bin/sh
# Builds the release binary and wraps it in a signed (ad-hoc) .app bundle at
# target/release/MS Audio Dock Remapper for macOS.app. Pass a signing identity as the
# first argument to sign with a Developer ID instead.
set -eu
cd "$(dirname "$0")"

IDENTITY="${1:--}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
APP="target/release/MS Audio Dock Remapper for macOS.app"

cargo build --release

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
# The executable carries the product name: macOS shows the process name in
# the menu bar when the app is activated from its status item.
cp target/release/ms-audio-dock-remapper "$APP/Contents/MacOS/MS Audio Dock Remapper for macOS"
sed "s/__VERSION__/$VERSION/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"
cp public/app-icon.icns "$APP/Contents/Resources/"
printf 'APPL????' > "$APP/Contents/PkgInfo"

codesign --force --sign "$IDENTITY" "$APP"
echo "Built $APP (version $VERSION, signed with '$IDENTITY')"
