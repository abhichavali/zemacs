#!/bin/sh
# Build zemacs and install both of the ways it gets launched: `zemacs` on $PATH,
# and the .app the Dock and Spotlight open.
#
# Usage:  scripts/install.sh              -> /usr/local/bin, /Applications
#         PREFIX=~/.local scripts/install.sh
#         APPDIR=~/Applications scripts/install.sh
#
# The bundle is written to *both* target/zemacs.app and $APPDIR/zemacs.app, so
# whichever of the two a Dock tile was made from is the one just rebuilt. A tile
# points at a path, not at an application, and there is no way from here to ask
# the Dock which path it kept.
#
# The config is not copied anywhere. `resolve_init_path` looks at $ZEMACS_INIT,
# then ~/.config/zemacs/init.lisp, then the runtime/ directory of the tree this
# binary was built from — an absolute path baked in at compile time. So an
# installed binary keeps reading this checkout's config, which is what you want
# on the machine you develop on and the one thing that breaks if you later move
# the checkout. The last line says which file it will read.
set -eu

prefix=${PREFIX:-/usr/local}
appdir=${APPDIR:-/Applications}
root=$(cd "$(dirname "$0")/.." && pwd)
bindir=$prefix/bin

# Checked before the build rather than after, so a missing permission costs a
# second instead of a full release compile.
for dir in "$bindir" "$appdir"; do
    mkdir -p "$dir" 2>/dev/null || :
    [ -w "$dir" ] || {
        echo "$dir is not writable — run: sudo $0" >&2
        echo "or pick somewhere of your own: PREFIX=\$HOME/.local APPDIR=\$HOME/Applications $0" >&2
        exit 1
    }
done

cargo build --release --manifest-path "$root/Cargo.toml"
install -m 755 "$root/target/release/zemacs" "$bindir/zemacs"
sh "$root/scripts/bundle.sh" --release >/dev/null

# Replaced rather than copied over: a stale Info.plist or an icon that was
# removed upstream would otherwise survive every future install. Deleting a
# bundle whose binary is *running* is safe — the process holds its own inodes —
# and the copy is what the next launch reads.
rm -rf "$appdir/zemacs.app"
cp -R "$root/target/zemacs.app" "$appdir/zemacs.app"
# Tell LaunchServices about the replacement, so Spotlight and the Dock do not
# spend a while pointing at the bundle that is no longer there.
lsregister=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
[ -x "$lsregister" ] && "$lsregister" -f "$appdir/zemacs.app" || :

echo "installed $bindir/zemacs"
echo "installed $appdir/zemacs.app (and refreshed $root/target/zemacs.app)"
case ":$PATH:" in
    *":$bindir:"*) ;;
    *) echo "warning: $bindir is not on \$PATH" >&2 ;;
esac
# `-q` is a Linux pgrep flag and not a BSD one, so the redirect does that job.
if pgrep -x zemacs >/dev/null 2>&1; then
    echo "note:     a zemacs is still running — quit it to pick this build up"
fi
if [ -f "$HOME/.config/zemacs/init.lisp" ]; then
    echo "config:   $HOME/.config/zemacs/init.lisp"
else
    echo "config:   $root/runtime/init.lisp (set ZEMACS_INIT to override)"
fi
