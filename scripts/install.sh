#!/bin/sh
# Build zemacs and install both of the ways it gets launched: `zemacs` on $PATH,
# and the .app the Dock and Spotlight open.
#
# Usage:  scripts/install.sh              -> ~/.local/bin, ~/Applications
#         PREFIX=/usr/local sudo -E scripts/install.sh
#         APPDIR=/Applications scripts/install.sh
#
# Both defaults are inside $HOME, so this never needs sudo. `~/Applications` is
# a location macOS has always known about: Spotlight indexes it, the Dock takes
# a tile from it, and Launchpad lists it beside everything in /Applications. The
# system-wide directories are one env var away for a machine with more than one
# person on it, and are the only case where this wants a password.
#
# The bundle is written to *both* target/zemacs.app and $APPDIR/zemacs.app, so
# whichever of the two a Dock tile was made from is the one just rebuilt. A tile
# points at a path, not at an application, and there is no way from here to ask
# the Dock which path it kept.
#
# The config is not copied anywhere. `resolve_init_path` looks at $ZEMACS_INIT,
# then ~/.zemacs.d/init.lisp, then the runtime/ directory of the tree this
# binary was built from — an absolute path baked in at compile time. So an
# installed binary keeps reading this checkout's config, which is what you want
# on the machine you develop on and the one thing that breaks if you later move
# the checkout. The last line says which file it will read.
set -eu

prefix=${PREFIX:-$HOME/.local}
appdir=${APPDIR:-$HOME/Applications}
root=$(cd "$(dirname "$0")/.." && pwd)
bindir=$prefix/bin

# `sudo` used to be the way to run this and is now the way to install it into
# the wrong place: sudo resets $HOME to /var/root, so the defaults would put a
# bundle somewhere nobody will ever open it. Caught rather than corrected — a
# script that quietly rewrites what root asked for is worse than one that says
# what it wants.
if [ "$(id -u)" = 0 ] && [ -z "${PREFIX:-}${APPDIR:-}" ]; then
    echo "no sudo needed: this installs into \$HOME/.local/bin and \$HOME/Applications" >&2
    echo "for the system-wide directories: PREFIX=/usr/local APPDIR=/Applications sudo -E $0" >&2
    exit 1
fi

# Checked before the build rather than after, so a missing permission costs a
# second instead of a full release compile.
for dir in "$bindir" "$appdir"; do
    mkdir -p "$dir" 2>/dev/null || :
    [ -w "$dir" ] || {
        echo "$dir is not writable — run: sudo -E $0" >&2
        echo "or leave PREFIX/APPDIR unset, which installs under \$HOME and needs no password" >&2
        exit 1
    }
done

cargo build --release --manifest-path "$root/Cargo.toml"
install -m 755 "$root/target/release/zemacs" "$bindir/zemacs"
sh "$root/scripts/bundle.sh" --release >/dev/null

# /usr/local/bin usually means this script is run under sudo, which means cargo
# and bundle.sh just ran as root and everything they left in target/ is owned by
# root. The damage is not to this run but to the *next* one: an ordinary
# `scripts/bundle.sh` then dies on `rm: target/zemacs.app: Permission denied`,
# and a plain `cargo build` starts failing on artifacts it cannot replace. Hand
# the tree back to whoever owns the checkout.
#
# ponytail: a chown afterwards rather than dropping privileges for the build
# itself, which is the tidier fix and needs `sudo -u` to find a cargo that is on
# the *user's* PATH and not on root's `secure_path`. One line here beats that
# until someone runs this as a root that is not a sudo'd user.
if [ "$(id -u)" = 0 ] && [ -n "${SUDO_USER:-}" ]; then
    chown -R "$SUDO_USER" "$root/target" 2>/dev/null || :
fi

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
if [ -f "$HOME/.zemacs.d/init.lisp" ]; then
    echo "config:   $HOME/.zemacs.d/init.lisp"
else
    echo "config:   $root/runtime/init.lisp (set ZEMACS_INIT to override)"
fi
