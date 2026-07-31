#!/bin/sh
# Wrap the built binary in a macOS .app bundle.
#
# Not cosmetic. A bare Unix executable has no application identity: macOS gives
# it no dock entry, `[NSApp activateIgnoringOtherApps:]` does not reliably take,
# and windows belonging to a process in that state are second-class — they do
# not take keyboard focus properly, and the close/minimise/zoom buttons can come
# up inert. An Info.plist and this directory layout are the whole difference.
#
# Usage:  scripts/bundle.sh [--release]      then: open target/zemacs.app
set -eu

profile=debug
[ "${1:-}" = "--release" ] && profile=release

root=$(cd "$(dirname "$0")/.." && pwd)
bin="$root/target/$profile/zemacs"
app="$root/target/zemacs.app"

[ -x "$bin" ] || {
    echo "no binary at $bin — run: cargo build${1:+ $1}" >&2
    exit 1
}

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/zemacs"

# LSMinimumSystemVersion and the bundle identifier are what make this a real
# application rather than a directory that happens to hold a binary.
# NSHighResolutionCapable is what stops a Retina display handing back a 1x
# drawable and upscaling every glyph.
cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>              <string>zemacs</string>
  <key>CFBundleDisplayName</key>       <string>zemacs</string>
  <key>CFBundleExecutable</key>        <string>zemacs</string>
  <key>CFBundleIdentifier</key>        <string>org.zemacs.zemacs</string>
  <key>CFBundlePackageType</key>       <string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key> <string>6.0</string>
  <key>CFBundleShortVersionString</key> <string>0.0.0</string>
  <key>CFBundleVersion</key>           <string>0.0.0</string>
  <key>LSMinimumSystemVersion</key>    <string>11.0</string>
  <key>NSHighResolutionCapable</key>   <true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key> <true/>
</dict>
</plist>
PLIST

echo "built $app"
echo "run:  open $app"
