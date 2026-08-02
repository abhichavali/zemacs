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

# The icon. macOS will not read a PNG here: the Finder, the dock and ⌘-Tab all
# want an `.icns`, which is a container holding the *same* artwork rasterised at
# every size they ask for — 16pt in a list view, 512@2x on a Retina desktop.
# Handing it one size and letting it scale is what makes an icon look soft next
# to its neighbours.
#
# `sips` and `iconutil` both ship with macOS, so this adds no dependency. The
# whole block is optional: a bundle with no icon is a working bundle with a
# blank tile, which is a much better outcome than a build that fails because
# somebody moved a PNG.
icon_src="$root/assets/Lisp_logo.svg.png"
if [ -f "$icon_src" ] && command -v iconutil >/dev/null 2>&1; then
    iconset=$(mktemp -d)/zemacs.iconset
    mkdir -p "$iconset"
    # Every size the format defines. `@2x` is the same pixel count as the next
    # size up and is *not* redundant: macOS picks by point size and scale
    # factor, and a missing @2x means a Retina display falls back to upscaling.
    for size in 16 32 128 256 512; do
        sips -z $size $size "$icon_src" \
             --out "$iconset/icon_${size}x${size}.png" >/dev/null 2>&1
        sips -z $((size * 2)) $((size * 2)) "$icon_src" \
             --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null 2>&1
    done
    if iconutil -c icns "$iconset" -o "$app/Contents/Resources/zemacs.icns" 2>/dev/null
    then
        icon_plist='  <key>CFBundleIconFile</key>          <string>zemacs</string>'
    else
        echo "warning: iconutil failed; bundling without an icon" >&2
        icon_plist=''
    fi
    rm -rf "$(dirname "$iconset")"
else
    echo "warning: no $icon_src (or no iconutil); bundling without an icon" >&2
    icon_plist=''
fi

# LSMinimumSystemVersion and the bundle identifier are what make this a real
# application rather than a directory that happens to hold a binary.
# NSHighResolutionCapable is what stops a Retina display handing back a 1x
# drawable and upscaling every glyph.
# Unquoted heredoc, so `$icon_plist` above is substituted. Safe because there is
# no other `$` or backtick in the document — check that before adding one.
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>              <string>zemacs</string>
  <key>CFBundleDisplayName</key>       <string>zemacs</string>
  <key>CFBundleExecutable</key>        <string>zemacs</string>
$icon_plist
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
