#!/bin/bash
# Builds "dist/MediaFiles Check.app" as a universal binary (Apple Silicon + Intel)
# and "dist/MediaFiles-Check-<version>.zip" (+ .sig, updater) and ".dmg" (downloads). Usage: scripts/build-mac-app.sh
# Requires: rustup target add x86_64-apple-darwin; create-dmg (Homebrew)
set -euo pipefail
cd "$(dirname "$0")/.."
NAME="MediaFiles Check"
RELEASE=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)   # e.g. 1.0.0-beta.1
VERSION=${RELEASE%%-*}                                                  # numbers only for macOS
BUILD=8                                                                 # bump with every release
APP="dist/$NAME.app"

cargo build --release --bin mediafiles-check --target aarch64-apple-darwin
cargo build --release --bin mediafiles-check --target x86_64-apple-darwin
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create target/aarch64-apple-darwin/release/mediafiles-check target/x86_64-apple-darwin/release/mediafiles-check \
  -output "$APP/Contents/MacOS/$NAME"

# every size drawn for itself (scripts/make-icon.swift → assets/AppIcon.iconset), not scaled down
iconutil -c icns assets/AppIcon.iconset -o "$APP/Contents/Resources/AppIcon.icns"
# license texts ship with the app (GPL for the app, OFL for the embedded font)
cp LICENSE "$APP/Contents/Resources/LICENSE.txt"
cp ui/fonts/OFL.txt "$APP/Contents/Resources/OFL.txt"
# screenshots of the manual (docs/help/images/<lang>/…)
mkdir -p "$APP/Contents/Resources/help"
cp -R docs/help/images "$APP/Contents/Resources/help/"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$NAME</string>
  <key>CFBundleDisplayName</key><string>$NAME</string>
  <key>CFBundleExecutable</key><string>$NAME</string>
  <key>CFBundleIdentifier</key><string>io.github.moritz-poth.mediafiles-check</string>
  <key>CFBundleVersion</key><string>$BUILD</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>MediaFilesCheckRelease</key><string>$RELEASE</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSHumanReadableCopyright</key><string>© 2026 Moritz Poth. GPL-3.0-or-later.</string>
</dict>
</plist>
PLIST
# ad-hoc signed (no Apple Developer account): macOS needs "Open Anyway" once, see README
codesign --force --deep --sign - "$APP"
ZIP="dist/MediaFiles-Check-$RELEASE.zip"
rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$ZIP"
# download image: the shared moe-apps DMG window (660 × 400, split background, motion trail); the updater keeps
# using the zip. create-dmg arranges the window through the Finder (needs Finder automation once).
DMG="dist/MediaFiles-Check-$RELEASE.dmg"
# never touch someone else's volume: the Finder's disk "MediaFiles Check" below must be ours
if ls /Volumes | grep -Eq "^$NAME( [0-9]+)?\$"; then echo "a volume \"$NAME\" is mounted – eject it first" >&2; exit 1; fi
STAGE=target/dmg-stage
rm -rf "$STAGE" "$DMG"; mkdir -p "$STAGE"
ditto "$APP" "$STAGE/$NAME.app"
swift scripts/make-dmg-background.swift assets/AppIcon.iconset/icon_512x512@2x.png target/dmg-bg.png 1
swift scripts/make-dmg-background.swift assets/AppIcon.iconset/icon_512x512@2x.png target/dmg-bg@2x.png 2
tiffutil -cathidpicheck target/dmg-bg.png target/dmg-bg@2x.png -out target/dmg-background.tiff 2>/dev/null
create-dmg --volname "$NAME" --window-size 660 400 --icon-size 128 \
  --icon "$NAME.app" 170 188 --app-drop-link 490 188 --hide-extension "$NAME.app" \
  --background target/dmg-background.tiff --no-internet-enable "$DMG" "$STAGE" >/dev/null
rm -rf "$STAGE"
# update signature: installed apps only accept zips signed with the key in this Mac's Keychain
rm -f "$ZIP.sig"
if PUB=$(swift scripts/update-key.swift public 2>/dev/null) && grep -q "\"$PUB\"" src/lib.rs \
   && swift scripts/update-key.swift sign "$ZIP"; then
  echo "Done: $APP, $DMG, $ZIP (+ .sig – upload all three)"
else
  echo "Done: $APP, $ZIP – NOT SIGNED (no update key, or it differs from UPDATE_APP.public_key): not releasable" >&2
fi
