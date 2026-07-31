//! zemacs-tramp — editing files on other machines as if they were here.
//!
//! `C-x C-f /ssh:user@host:/etc/nginx.conf` should open a buffer, `C-x C-s`
//! should write it back, and dired on `/ssh:host:/etc` should list the
//! directory. Two halves, deliberately kept apart the way `zemacs-dired` splits
//! filesystem from presentation:
//!
//! * **Naming** (`path`): [`parse`] turns a file name into a [`RemotePath`],
//!   or `None` for the local path that it almost always is. Nothing there
//!   touches the network.
//! * **Doing** (this module): [`read`], [`write()`], [`list`], [`stat`],
//!   [`mkdir`], [`delete`] and [`rename`], each one `ssh` invocation. A
//!   [`Worker`] runs the same operations off the UI thread.
//!
//! Everything shells out to the system `ssh`. No `libssh`, no `russh`: `ssh`
//! already reads the user's `~/.ssh/config`, talks to the agent, knows the
//! jump hosts and the per-host keys and ciphers, and is the thing the user has
//! already made work. A linked SSH library would reimplement all of that
//! badly. This is how Emacs' own TRAMP does it, for the same reason.
//!
//! # Two shells, and only one of them is ours
//!
//! Locally there is no shell at all: [`std::process::Command`] takes an
//! argument vector, so a host or a path is one `argv` element no matter what is
//! in it.
//!
//! Remotely there is no avoiding one — `ssh host cmd` joins its arguments with
//! spaces and hands the result to the *remote* login shell, which splits it
//! again. So every operation sends a small POSIX `sh` script, and **the script
//! is a fixed literal with quoted holes**: every value that came from outside
//! this crate goes through `quote`, which wraps it in single quotes, where
//! `sh` performs no expansion of any kind, and splices the one character single
//! quotes cannot hold as `'\''`. There is no string concatenation of data into
//! a command anywhere in this file, and `quote` is the only way data gets in.
//! `sh_quote_is_total` pins that down, and
//! `a_hostile_path_reaches_the_remote_as_one_inert_word` follows one all the
//! way out to the bytes ssh sends.
//!
//! The script is then quoted a second time by `sh_c`, which names the shell
//! that runs it instead of accepting whatever the login shell happens to be —
//! see there for the bug that costs.
//!
//! # Never hanging
//!
//! `BatchMode=yes` means ssh never prompts: no password, no passphrase, no
//! "are you sure you want to continue connecting". A host that would have asked
//! fails immediately with [`Error::NeedsAuth`], which says to use the agent,
//! rather than blocking the editor on a prompt that has no terminal to appear
//! on. `ConnectTimeout` bounds the handshake, `ServerAlive*` bounds a
//! connection that dies mid-transfer, and [`TIMEOUT`] is a wall clock over the
//! whole thing for everything else.
//!
//! ponytail: the remote login shell still has to *unquote* the way POSIX says
//! — sh, dash, bash, zsh and ksh all do; csh and fish do not, and would need
//! their own escaping in `sh_c`. Remote file names are assumed to be UTF-8.
//! And there is no `sudo:`, no `docker:`, no multi-hop `|`: one method, which
//! is the one that opens a config file on a server.

mod path;

pub use path::{parse, RemotePath};

use std::fmt;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Wall clock ceiling on one operation, connection included.
///
/// ponytail: one constant rather than a knob. Generous enough for a large file
/// over a bad link, short enough that a wedged host is a message and not a
/// hung editor. A per-operation budget only matters once there is a progress
/// indicator to cancel from.
pub const TIMEOUT: Duration = Duration::from_secs(60);

/// Seconds ssh will spend on the TCP handshake before giving up.
const CONNECT_TIMEOUT: u32 = 10;

/// Seconds the multiplexing master stays alive after the last operation.
const PERSIST: u32 = 600;

// ------------------------------------------------------------------- errors

/// Why an operation did not happen.
///
/// The three cases that matter are told apart because their fixes are
/// completely different, and the message is the whole user interface here:
/// [`NeedsAuth`](Error::NeedsAuth) is the user's ssh agent,
/// [`HostKey`](Error::HostKey) is one manual connection in a terminal, and
/// [`Unreachable`](Error::Unreachable) is the network or the host name. Getting
/// these confused produces the classic "it just doesn't work" that makes remote
/// editing miserable.
#[derive(Debug)]
pub enum Error {
    /// ssh would have prompted for a password or a key passphrase. It never
    /// gets to, because we run it in batch mode — an editor has nowhere to
    /// draw a prompt the ssh client would read.
    NeedsAuth { host: String, detail: String },
    /// The host key is unknown or has changed. Deliberately not
    /// [`NeedsAuth`](Error::NeedsAuth): silently accepting a new key is a
    /// decision for the user, in a terminal, once.
    HostKey { host: String, detail: String },
    /// Name did not resolve, connection refused, no route, network down.
    Unreachable { host: String, detail: String },
    /// Connected, but nothing came back inside [`TIMEOUT`].
    Timeout { host: String, seconds: u64 },
    /// The remote command ran and failed: no such file, permission denied,
    /// disk full. `stderr` is the remote's own message, which is better than
    /// anything we could write.
    Remote {
        what: String,
        status: i32,
        stderr: String,
    },
    /// The local `ssh` binary could not be started at all.
    Spawn(std::io::Error),
    /// Refused before touching the network — deleting a root, renaming across
    /// hosts, decoding bytes that are not text.
    Refused(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NeedsAuth { host, detail } => write!(
                f,
                "{host} would need a password, and zemacs cannot prompt for one — \
                 add your key to the ssh agent (`ssh-add`) or set up key authentication \
                 for this host, then try again: {detail}"
            ),
            Error::HostKey { host, detail } => write!(
                f,
                "the host key for {host} is not trusted — connect once with `ssh {host}` \
                 in a terminal and accept it: {detail}"
            ),
            Error::Unreachable { host, detail } => write!(f, "cannot reach {host}: {detail}"),
            Error::Timeout { host, seconds } => {
                write!(f, "{host} did not answer within {seconds}s")
            }
            Error::Remote {
                what,
                status,
                stderr,
            } => {
                if stderr.is_empty() {
                    write!(f, "{what} failed (exit {status})")
                } else {
                    write!(f, "{what}: {stderr}")
                }
            }
            Error::Spawn(e) => write!(f, "cannot run ssh: {e}"),
            Error::Refused(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Spawn(e) => Some(e),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

// ------------------------------------------------------------------ entries

/// One remote directory entry, or one remote file: everything a dired line
/// needs and nothing that would cost another round trip to learn.
///
/// The metadata is the entry's *own* (`lstat`), matching what `ls -l` shows and
/// what `zemacs-dired` reports locally: a symlink's `len` is the length of its
/// target's name, not of the file. [`is_dir`](RemoteEntry::is_dir) is the one
/// field that follows the link, because it answers "does `RET` here open a
/// directory", which is false for a broken link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    /// File name only. `.` and `..` are never listed; dired synthesises `..`.
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub len: u64,
    /// Permission and setuid/sticky bits (`mode & 0o7777`).
    pub mode: u32,
    /// `None` when the remote `stat` said nothing useful.
    pub modified: Option<SystemTime>,
}

// ------------------------------------------------------------- the operations

/// Read a remote file's bytes.
///
/// Bytes, not text: a `.png` fetched into a buffer and written back must come
/// out byte-identical, and half the point of doing this over `cat` rather than
/// over a text protocol is that nothing re-encodes on the way.
pub fn read(p: &RemotePath) -> Result<Vec<u8>> {
    // `--` so that a file called `-n` is a file. Every `cat`, GNU and BSD,
    // takes it.
    ssh(p, &format!("cat -- {}", quote(p.path())), &[], "reading")
}

/// Read a remote file as text.
///
/// Errors rather than replacing undecodable bytes, because the buffer this
/// fills is one the user will save: silently turning a latin-1 file into
/// U+FFFD and writing it back destroys it. [`read`] is the answer for anything
/// that is not text.
pub fn read_to_string(p: &RemotePath) -> Result<String> {
    String::from_utf8(read(p)?)
        .map_err(|_| Error::Refused(format!("{p} is not UTF-8 text; read it as bytes")))
}

/// Write `bytes` to a remote file. **The dangerous one.**
///
/// The content goes to a temporary file *beside* the target and is `mv`'d over
/// it, so the target is either the old file or the new one and never a
/// half-transferred prefix. A connection dropped mid-write leaves the user's
/// file untouched and the temporary swept up by the shell's `EXIT` trap.
/// Same directory, so the `mv` is a `rename(2)` on one filesystem, which is
/// what makes it atomic.
///
/// The mode bits survive because the temporary is made with `cp -p` from the
/// existing file and then truncated in place: a redirection onto an existing
/// file changes its contents, never its permissions. That also carries the
/// group and, where permitted, the owner across, which a `chmod` would not.
///
/// ponytail: `cp -p` copies the old contents before throwing them away, so a
/// write costs one extra local-to-the-remote copy of the file. Invisible for
/// anything a human edits; the upgrade for huge files is to `stat` the mode and
/// `chmod` the temporary instead.
pub fn write(p: &RemotePath, bytes: &[u8]) -> Result<()> {
    ssh(p, &write_script(p), bytes, "writing").map(drop)
}

/// List a remote directory, `.` and `..` excluded, in the remote shell's glob
/// order (sorted). Presentation — hiding dotfiles, sorting, `..` — is the
/// caller's, exactly as it is for a local listing.
pub fn list(p: &RemotePath) -> Result<Vec<RemoteEntry>> {
    let out = ssh(p, &list_script(p), &[], "listing")?;
    let text = String::from_utf8_lossy(&out);
    Ok(text.lines().filter_map(entry).collect())
}

/// Stat one remote path. `Ok(None)` is "not there", which is what `find-file`
/// needs to tell an existing file from one the user is about to create.
///
/// A path inside a directory the login cannot search also reads as absent —
/// the remote `test` cannot tell us apart from the file not existing.
pub fn stat(p: &RemotePath) -> Result<Option<RemoteEntry>> {
    let out = match ssh(p, &stat_script(p), &[], "stat of") {
        Ok(out) => out,
        // The script's own code for "no such path", kept away from every other
        // reason a remote command can fail.
        Err(Error::Remote { status: 9, .. }) => return Ok(None),
        Err(e) => return Err(e),
    };
    let text = String::from_utf8_lossy(&out);
    // The record carries a placeholder name: the remote was asked about one
    // path, and only the caller knows how that path was written.
    Ok(text.lines().find_map(entry).map(|e| RemoteEntry {
        name: p.file_name().to_string(),
        ..e
    }))
}

/// Create one remote directory. Not recursive, and an existing name is an
/// error, matching `zemacs-dired`'s local `create_dir`.
pub fn mkdir(p: &RemotePath) -> Result<()> {
    ssh(p, &format!("mkdir -- {}", quote(p.path())), &[], "creating").map(drop)
}

/// Delete a remote path, recursively when `recursive`.
///
/// Refuses a path with no final component — `/`, `~`, anything ending in `..` —
/// because those are the shapes where `rm -rf` deletes something other than
/// what the user is looking at. This is the same guard `zemacs-dired::delete`
/// applies locally, and it matters far more here: there is no undo and no
/// trash on the other end.
pub fn delete(p: &RemotePath, recursive: bool) -> Result<()> {
    let name = p.file_name();
    if name.is_empty() || name == "." || name == ".." || name == "~" {
        return Err(Error::Refused(format!(
            "refusing to delete {p}: not a named file"
        )));
    }
    let flags = if recursive { "-rf" } else { "-f" };
    // `rm -f` on a missing file succeeds, which is the right answer for a
    // dired that has just been told to delete something already gone.
    ssh(
        p,
        &format!("rm {flags} -- {}", quote(p.path())),
        &[],
        "deleting",
    )
    .map(drop)
}

/// Rename a remote path. Both ends must be on the same host — a cross-host
/// move is a read plus a write plus a delete, and that is the caller's
/// decision to make, not something to do silently.
///
/// Refuses an existing destination rather than clobbering it, because `mv`
/// would delete it without a word and the caller has a user to ask.
pub fn rename(from: &RemotePath, to: &RemotePath) -> Result<()> {
    if (&from.host, &from.user, from.port) != (&to.host, &to.user, to.port) {
        return Err(Error::Refused(format!(
            "cannot rename {from} to {to}: different hosts"
        )));
    }
    ssh(from, &rename_script(from, to), &[], "renaming").map(drop)
}

/// Drop the multiplexed connection to this host, if there is one.
///
/// The escape hatch multiplexing needs: when a master survives a laptop
/// suspend or a VPN change it can hang every later operation until it times
/// out, and this is the "turn it off and on again" for that.
pub fn disconnect(p: &RemotePath) -> Result<()> {
    let mut argv = ssh_argv(p, control_path(p).as_deref(), None);
    argv.splice(0..0, ["-O".to_string(), "exit".to_string()]);
    // Nothing to report: no master means nothing to close, which is success.
    let _ = Command::new("ssh").args(&argv).output();
    Ok(())
}

// -------------------------------------------------------------- the scripts
//
// Each one is a `format!` over a *literal* skeleton whose only holes are
// [`quote`]d words — see the module docs. They live together, and apart from
// the operations that send them, so that `every_script_is_valid_shell` can
// parse all of them with `sh -n` without a host to run them against.

/// Content to a temporary beside the target, then one `mv`. See [`write`].
fn write_script(p: &RemotePath) -> String {
    format!(
        "set -e\n\
         f={}\n\
         t=\"$f.zemacs-tmp.$$\"\n\
         trap 'rm -f -- \"$t\"' EXIT\n\
         if [ -e \"$f\" ]; then cp -p -- \"$f\" \"$t\"; fi\n\
         cat > \"$t\"\n\
         mv -f -- \"$t\" \"$f\"\n",
        quote(p.path())
    )
}

/// One record per entry, `..` and `.` skipped, junk from the login shell's rc
/// files left for [`entry`] to drop. `./$e` rather than `--` because a couple
/// of `stat`s do not take `--` and every one of them takes a relative path.
fn list_script(p: &RemotePath) -> String {
    format!(
        "cd -- {} || exit 1\n\
         {STAT_FN}\
         for e in * .*; do\n\
         case \"$e\" in .|..) continue;; esac\n\
         {{ [ -e \"$e\" ] || [ -L \"$e\" ]; }} || continue\n\
         if [ -d \"$e\" ]; then d=d; else d=-; fi\n\
         if [ -L \"$e\" ]; then l=l; else l=-; fi\n\
         printf '%s %s %s\\n' \"$d$l\" \"$(zst \"./$e\")\" \"$e\"\n\
         done\n",
        quote(p.path())
    )
}

/// Exit 9 is this script's own "no such path", so that [`stat`] can answer
/// `Ok(None)` without having to read an error message to find out.
fn stat_script(p: &RemotePath) -> String {
    format!(
        "{STAT_FN}\
         f={}\n\
         {{ [ -e \"$f\" ] || [ -L \"$f\" ]; }} || exit 9\n\
         if [ -d \"$f\" ]; then d=d; else d=-; fi\n\
         if [ -L \"$f\" ]; then l=l; else l=-; fi\n\
         printf '%s %s %s\\n' \"$d$l\" \"$(zst \"$f\")\" x\n",
        quote(p.path())
    )
}

/// Exit 4 is "the destination is already there"; `mv` would have eaten it.
fn rename_script(from: &RemotePath, to: &RemotePath) -> String {
    format!(
        "set -e\n\
         a={}\n\
         b={}\n\
         if [ -e \"$b\" ] || [ -L \"$b\" ]; then printf '%s\\n' 'destination exists' >&2; exit 4; fi\n\
         mv -- \"$a\" \"$b\"\n",
        quote(from.path()),
        quote(to.path())
    )
}

/// Ask the remote for `mode size mtime` in one line, from either flavour of
/// `stat`. GNU takes `-c`, BSD takes `-f`, and neither knows the other's
/// letters, so the flavour is probed once per invocation — cheaper than a
/// round trip to find out, and it cannot go stale.
///
/// `%Mp%Lp` is BSD's setuid/sticky digit followed by the permission digits,
/// which is exactly what GNU's `%a` prints. Both `stat`s report the link
/// itself rather than its target, which is the `lstat` semantics dired wants.
const STAT_FN: &str = "if stat -c %s . >/dev/null 2>&1\n\
                       then zst() { stat -c '%a %s %Y' \"$1\" 2>/dev/null || echo '0 0 0'; }\n\
                       else zst() { stat -f '%Mp%Lp %z %m' \"$1\" 2>/dev/null || echo '0 0 0'; }\n\
                       fi\n";

/// One `printf`ed record back into a [`RemoteEntry`].
///
/// Fields are separated by exactly one space and the name comes last, so a
/// name containing spaces survives; splitting into five is what keeps it whole.
///
/// ponytail: a file name containing a newline is lost, because the records are
/// newline separated. `sh` cannot emit NUL, so the fix is a length-prefixed or
/// hex-encoded name and a parser for it — for a name nobody has ever typed.
fn entry(line: &str) -> Option<RemoteEntry> {
    let mut f = line.splitn(5, ' ');
    let flags = f.next()?;
    let mode = u32::from_str_radix(f.next()?, 8).unwrap_or(0);
    let len = f.next()?.parse().unwrap_or(0);
    let mtime: i64 = f.next()?.parse().unwrap_or(0);
    let name = f.next()?;
    if name.is_empty() || flags.len() != 2 {
        return None; // a shell that printed something we did not ask for
    }
    Some(RemoteEntry {
        name: name.to_string(),
        is_dir: flags.starts_with('d'),
        is_symlink: flags.ends_with('l'),
        len,
        mode: mode & 0o7777,
        // 0 is `stat` having failed, not 1970. Pre-epoch times are not worth a
        // signed-duration dance for a column that renders as a date.
        modified: (mtime > 0).then(|| UNIX_EPOCH + Duration::from_secs(mtime as u64)),
    })
}

// -------------------------------------------------------------------- shell

/// Quote `s` as a single POSIX shell word.
///
/// **The whole injection story lives here.** Single quotes are the one context
/// `sh` does nothing inside — no variable, command, arithmetic, tilde,
/// backslash or glob expansion — so a single-quoted word arrives at the remote
/// utility as exactly the bytes it contains. The only character that cannot
/// appear inside them is `'`, which is spliced as `'\''`: close, escaped quote,
/// reopen. There is no input for which this produces more than one word or any
/// expansion at all.
///
/// The exception is a leading `~`, which has to stay *outside* the quotes or
/// the remote home directory never expands — and POSIX says a tilde-prefix with
/// any quoted character in it is taken literally, so the `/` that ends the
/// prefix must be outside too. That leaves `~user` unquoted, which is only safe
/// because it is checked to be a plausible login name first; anything else
/// falls back to quoting the lot and losing the expansion, which is the failure
/// we want.
fn quote(s: &str) -> String {
    match tilde_prefix(s) {
        // `~` and `~user` on their own, with nothing left to quote.
        Some((prefix, "")) => prefix.to_string(),
        Some((prefix, rest)) => format!("{prefix}/{}", single(rest)),
        None => single(s),
    }
}

/// Split `~`/`~user` off the front, with the rest of the path after its `/`.
/// `None` when there is no tilde, or when what follows it is not a login name.
fn tilde_prefix(s: &str) -> Option<(&str, &str)> {
    if !s.starts_with('~') {
        return None;
    }
    let end = s.find('/').unwrap_or(s.len());
    let (prefix, rest) = s.split_at(end);
    // This part is handed to the shell unquoted, so it must be inert.
    if !prefix[1..]
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return None;
    }
    Some((prefix, rest.strip_prefix('/').unwrap_or(rest)))
}

fn single(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("'\\''"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

// ---------------------------------------------------------------- running ssh

/// The argument vector handed to `ssh`.
///
/// Everything is a separate element, so nothing here is ever re-parsed by a
/// local shell. `script` is one element too: `ssh` joins its trailing arguments
/// with spaces before sending them, so splitting the script would silently
/// collapse its whitespace.
///
/// `control` is `None` where connection multiplexing does not exist (Windows),
/// and in tests, where the socket path must not depend on `$HOME`.
/// Wrap a script so that POSIX `sh` runs it, whatever the login shell is.
///
/// Not a nicety. `ssh host <words>` hands the words to the *login* shell, and a
/// login shell of zsh — which is the default on macOS and common everywhere —
/// treats a glob that matches nothing as a fatal error rather than as itself,
/// so `for e in * .*` aborts [`list`] in an empty directory. Every such
/// difference disappears by naming the shell instead of taking what we are
/// given, and one more [`quote`] is a cheap way to buy that.
fn sh_c(script: &str) -> String {
    format!("exec /bin/sh -c {}", quote(script))
}

fn ssh_argv(p: &RemotePath, control: Option<&str>, script: Option<&str>) -> Vec<String> {
    let mut argv: Vec<String> = Vec::with_capacity(20);
    let mut opt = |o: String| {
        argv.push("-o".into());
        argv.push(o);
    };
    // No prompts of any kind: an editor has no terminal for ssh to ask on, so
    // a host that wants a password must fail now rather than hang forever.
    opt("BatchMode=yes".into());
    opt(format!("ConnectTimeout={CONNECT_TIMEOUT}"));
    // A connection that dies mid-transfer (laptop lid, VPN drop) is noticed in
    // ~45s instead of sitting in a TCP read until the kernel gives up.
    opt("ServerAliveInterval=15".into());
    opt("ServerAliveCountMax=3".into());
    if let Some(control) = control {
        // The one thing that makes this feel transparent. The first operation
        // pays for a handshake and leaves a master listening on a unix socket;
        // every later one to the same host is a new channel on it, which is a
        // few milliseconds rather than a few hundred.
        opt("ControlMaster=auto".into());
        opt(format!("ControlPath={control}"));
        opt(format!("ControlPersist={PERSIST}"));
    }
    if let Some(port) = p.port {
        argv.push("-p".into());
        argv.push(port.to_string());
    }
    if let Some(user) = &p.user {
        // `-l user` rather than `user@host`, so a host or a login containing an
        // `@` cannot be re-split into the wrong pair.
        argv.push("-l".into());
        argv.push(user.clone());
    }
    argv.push(p.ssh_host().to_string());
    argv.extend(script.map(str::to_string));
    argv
}

/// The socket one connection to `p`'s host multiplexes through: one per
/// (login, host, port), so two buffers on the same server share a connection
/// and two servers do not.
///
/// The name is a hash rather than ssh's own `%C` — or anything readable —
/// purely for length. A unix socket path gets 104 bytes, ssh appends 17 more
/// while creating one, and `%C` alone is 40 hex characters: under a macOS cache
/// directory that overflows and multiplexing silently stops happening.
/// `the_control_socket_stays_inside_the_unix_socket_path_limit` holds the line.
///
/// `None` where multiplexing is unavailable, which is the point of the
/// [`Option`]: Windows' OpenSSH rejects `ControlPath` outright, and one
/// unsupported option fails the whole call.
#[cfg(unix)]
fn control_path(p: &RemotePath) -> Option<String> {
    use std::hash::{Hash as _, Hasher as _};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    (&p.user, p.ssh_host(), p.port).hash(&mut hash);
    // A toolchain upgrade may change this hasher, which costs one extra
    // handshake once. Nothing depends on the name meaning anything.
    Some(format!(
        "{}/{:016x}",
        control_dir()?.display(),
        hash.finish()
    ))
}

#[cfg(not(unix))]
fn control_path(_p: &RemotePath) -> Option<String> {
    None
}

/// The user's cache directory: multiplexing sockets are regenerable state and
/// no business of `~/.ssh`. Created once, on first use.
#[cfg(unix)]
fn control_dir() -> Option<&'static std::path::Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let home = std::env::var_os("HOME")?;
        let dir = match std::env::var_os("XDG_CACHE_HOME") {
            Some(cache) => PathBuf::from(cache),
            // Where macOS keeps caches; elsewhere this is the XDG default.
            None if cfg!(target_os = "macos") => PathBuf::from(&home).join("Library/Caches"),
            None => PathBuf::from(&home).join(".cache"),
        }
        .join("zemacs/ssh");
        std::fs::create_dir_all(&dir).ok()?;
        // A socket in a directory anyone can write to is a way onto the
        // connection; ssh does not check the directory, so we do.
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        Some(dir)
    })
    .as_deref()
}

/// Run one script on `p`'s host, feeding it `stdin`, and hand back its stdout.
fn ssh(p: &RemotePath, script: &str, stdin: &[u8], what: &str) -> Result<Vec<u8>> {
    let argv = ssh_argv(p, control_path(p).as_deref(), Some(&sh_c(script)));
    let mut child = Command::new("ssh")
        .args(&argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(Error::Spawn)?;
    let pid = child.id();

    // Written from another thread: a file bigger than the pipe buffer would
    // otherwise deadlock, us blocked writing and ssh blocked on a stdout
    // nobody is draining. Dropping the handle is what sends EOF, so the remote
    // `cat` finishes.
    if let Some(mut sink) = child.stdin.take() {
        let bytes = stdin.to_vec();
        std::thread::spawn(move || {
            let _ = sink.write_all(&bytes);
        });
    }
    // ...and waited from a third, so the timeout below is a real deadline and
    // not a suggestion. `recv_timeout` needs someone else holding the child.
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    let out = match rx.recv_timeout(TIMEOUT) {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(Error::Spawn(e)),
        Err(_) => {
            // No libc, so `kill(1)`. Killing the local client is enough: it
            // closes the multiplexed channel and frees the editor, and the
            // remote side of the master notices and reaps its own command.
            let _ = Command::new("kill")
                .args(["-9", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            return Err(Error::Timeout {
                host: p.host.clone(),
                seconds: TIMEOUT.as_secs(),
            });
        }
    };
    if out.status.success() {
        return Ok(out.stdout);
    }
    Err(classify(p, &out, &format!("{what} {p}")))
}

/// Turn a failed `ssh` into the error whose fix is the right one.
///
/// Exit 255 is ssh's own: the connection never happened, or never got as far as
/// running anything. Any other status came from the remote command, and its
/// `stderr` already says what went wrong far better than we could.
fn classify(p: &RemotePath, out: &Output, what: &str) -> Error {
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let status = out.status.code().unwrap_or(-1);
    if status != 255 {
        return Error::Remote {
            what: what.to_string(),
            status,
            stderr,
        };
    }
    let host = p.host.clone();
    let detail = stderr.lines().next_back().unwrap_or_default().to_string();
    let said = stderr.to_ascii_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|n| said.contains(n));

    // Host key first: it is a refusal to authenticate that never mentions a
    // password, so the auth test below would not catch it.
    if any(&[
        "host key verification failed",
        "host identification has changed",
    ]) {
        Error::HostKey { host, detail }
    } else if any(&[
        "permission denied",
        "authentication",
        "no such identity",
        "passphrase",
        "password",
        "batch mode",
    ]) {
        Error::NeedsAuth { host, detail }
    } else if any(&[
        "could not resolve",
        "name or service not known",
        "connection refused",
        "connection timed out",
        "operation timed out",
        "no route to host",
        "network is unreachable",
        "connection closed",
        "connection reset",
    ]) {
        Error::Unreachable { host, detail }
    } else {
        // Something ssh said that we have no better name for. Still its own
        // words, still actionable, just not sorted.
        Error::Unreachable { host, detail }
    }
}

// ------------------------------------------------------------------- worker

/// One remote operation, as a message.
#[derive(Debug, Clone)]
pub enum Op {
    Read(RemotePath),
    Write(RemotePath, Vec<u8>),
    List(RemotePath),
    Stat(RemotePath),
    Mkdir(RemotePath),
    Delete { path: RemotePath, recursive: bool },
    Rename { from: RemotePath, to: RemotePath },
}

/// What an [`Op`] produced. `Done` covers everything whose only answer is
/// "it worked".
#[derive(Debug, Clone)]
pub enum Reply {
    Read(Vec<u8>),
    List(Vec<RemoteEntry>),
    /// `None` is "no such file", not an error — see [`stat`].
    Stat(Option<RemoteEntry>),
    Done,
}

/// Perform an [`Op`] on this thread. What [`Worker`] runs, exposed because
/// blocking is the honest thing in a test or a batch script.
pub fn execute(op: Op) -> Result<Reply> {
    match op {
        Op::Read(p) => read(&p).map(Reply::Read),
        Op::Write(p, bytes) => write(&p, &bytes).map(|()| Reply::Done),
        Op::List(p) => list(&p).map(Reply::List),
        Op::Stat(p) => stat(&p).map(Reply::Stat),
        Op::Mkdir(p) => mkdir(&p).map(|()| Reply::Done),
        Op::Delete { path, recursive } => delete(&path, recursive).map(|()| Reply::Done),
        Op::Rename { from, to } => rename(&from, &to).map(|()| Reply::Done),
    }
}

/// A thread that does the remote IO.
///
/// Same shape as `zemacs-syntax`'s highlighting worker — [`Worker::request`]
/// hands over a job and returns, [`Worker::poll`] picks up whatever has
/// finished — with one deliberate difference: **nothing is coalesced or
/// dropped**. A stale highlight is worth throwing away; a save is not.
/// Every request produces exactly one reply, tagged with the caller's own id.
pub struct Worker {
    jobs: crossbeam_channel::Sender<(u64, Op)>,
    done: crossbeam_channel::Receiver<(u64, Result<Reply>)>,
}

/// Spawn the remote IO thread. It exits when the [`Worker`] is dropped.
///
/// ponytail: one thread, so operations run strictly in order and a slow read
/// delays the save queued behind it. The ceiling is one host at a time; the
/// upgrade is a thread per host, keyed the same way the ssh control socket is,
/// which only starts to matter with two remote buffers open at once.
pub fn spawn_worker() -> Worker {
    let (job_tx, job_rx) = crossbeam_channel::unbounded::<(u64, Op)>();
    let (done_tx, done_rx) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("zemacs-tramp".into())
        .spawn(move || {
            while let Ok((id, op)) = job_rx.recv() {
                if done_tx.send((id, execute(op))).is_err() {
                    break;
                }
            }
        })
        .expect("failed to spawn tramp thread");
    Worker {
        jobs: job_tx,
        done: done_rx,
    }
}

impl Worker {
    /// Queue an operation. Never blocks. `id` comes back with the reply and is
    /// the caller's to interpret — a buffer number, a request counter.
    pub fn request(&self, id: u64, op: Op) {
        let _ = self.jobs.send((id, op));
    }

    /// One finished operation, or `None`. Call it until it returns `None`:
    /// unlike the highlighter's, every reply here matters.
    pub fn poll(&self) -> Option<(u64, Result<Reply>)> {
        self.done.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str) -> RemotePath {
        parse(name).unwrap()
    }

    /// The property the whole crate rests on: whatever goes in, the remote `sh`
    /// sees one word, and it is the original bytes.
    #[test]
    fn sh_quote_is_total() {
        assert_eq!(quote("/etc/nginx.conf"), "'/etc/nginx.conf'");
        assert_eq!(quote("/tmp/my file"), "'/tmp/my file'");
        assert_eq!(quote("/tmp/$HOME"), "'/tmp/$HOME'");
        assert_eq!(quote("/tmp/`id`"), "'/tmp/`id`'");
        assert_eq!(quote("/tmp/a\"b"), "'/tmp/a\"b'");
        assert_eq!(quote("/tmp/a\\b"), "'/tmp/a\\b'");
        assert_eq!(quote("/tmp/*"), "'/tmp/*'");
        assert_eq!(quote("/tmp/it's"), "'/tmp/it'\\''s'");
        // the shapes an attacker would try
        assert_eq!(quote("; rm -rf ~"), "'; rm -rf ~'");
        assert_eq!(quote("' ; rm -rf ~ ; '"), "''\\'' ; rm -rf ~ ; '\\'''");
        assert_eq!(quote("$(id)"), "'$(id)'");
        assert_eq!(quote(""), "''");
    }

    /// `~` has to survive as an expansion, and everything after it must not.
    #[test]
    fn a_tilde_expands_and_the_rest_of_the_path_does_not() {
        assert_eq!(quote("~"), "~");
        assert_eq!(quote("~/notes.org"), "~/'notes.org'");
        assert_eq!(quote("~/my notes/a'b"), "~/'my notes/a'\\''b'");
        assert_eq!(quote("~deploy/app"), "~deploy/'app'");
        assert_eq!(quote("~/"), "~");
        // a tilde-prefix that is not a login name is quoted whole, losing the
        // expansion, which is the safe direction to fail in
        assert_eq!(quote("~$(id)/x"), "'~$(id)/x'");
        assert_eq!(quote("~a;b/x"), "'~a;b/x'");
        // and a tilde anywhere but the front is just a character
        assert_eq!(quote("/tmp/~x"), "'/tmp/~x'");
    }

    /// Pinned exactly, because a stray or reordered option is how a batch-mode
    /// editor grows an invisible password prompt.
    #[test]
    #[rustfmt::skip] // the option pairs read as pairs
    fn the_argument_vector_is_exactly_this() {
        let argv = ssh_argv(&p("/ssh:host:/etc/x"), None, Some(&sh_c("cat -- '/etc/x'")));
        assert_eq!(
            argv,
            [
                "-o", "BatchMode=yes",
                "-o", "ConnectTimeout=10",
                "-o", "ServerAliveInterval=15",
                "-o", "ServerAliveCountMax=3",
                "host",
                r#"exec /bin/sh -c 'cat -- '\''/etc/x'\'''"#,
            ]
        );
    }

    #[test]
    #[rustfmt::skip] // ...here too
    fn a_user_a_port_and_a_control_socket_become_their_own_arguments() {
        let argv = ssh_argv(
            &p("/ssh:deploy@[fe80::1]#2222:/etc/x"),
            Some("/cache/zemacs/ssh/9d2c"),
            Some("true"),
        );
        assert_eq!(
            argv,
            [
                "-o", "BatchMode=yes",
                "-o", "ConnectTimeout=10",
                "-o", "ServerAliveInterval=15",
                "-o", "ServerAliveCountMax=3",
                "-o", "ControlMaster=auto",
                "-o", "ControlPath=/cache/zemacs/ssh/9d2c",
                "-o", "ControlPersist=600",
                "-p", "2222",
                "-l", "deploy",
                // brackets are a filename syntax, not an ssh one
                "fe80::1",
                "true",
            ]
        );
    }

    /// The failure this guards against is invisible: an over-long path makes
    /// ssh give up on multiplexing with a warning nobody reads, and every
    /// operation quietly pays for a fresh handshake again.
    #[test]
    #[cfg(unix)]
    fn the_control_socket_stays_inside_the_unix_socket_path_limit() {
        let long = "/ssh:deploy@a-really-quite-long-host-name.example.com#2222:";
        let Some(socket) = control_path(&p(&format!("{long}/etc/x"))) else {
            return; // no HOME, so no multiplexing to size
        };
        // 104 bytes of `sun_path`, minus the `.` and 16 random characters ssh
        // appends to the name while it is creating the socket.
        assert!(
            socket.len() + 17 < 104,
            "{socket} is {} bytes",
            socket.len()
        );

        // One socket per connection, not per file...
        assert_eq!(socket, control_path(&p(&format!("{long}/other"))).unwrap());
        // ...and a different login or port is a different connection.
        for other in [
            "/ssh:root@a-really-quite-long-host-name.example.com#2222:/etc/x",
            "/ssh:deploy@a-really-quite-long-host-name.example.com:/etc/x",
            "/ssh:deploy@elsewhere#2222:/etc/x",
        ] {
            assert_ne!(socket, control_path(&p(other)).unwrap(), "{other}");
        }
    }

    /// The script is one argv element, however much whitespace it contains:
    /// ssh joins its trailing arguments with single spaces before sending them.
    #[test]
    fn the_script_is_a_single_argument() {
        let argv = ssh_argv(&p("/ssh:host:/x"), None, Some(&sh_c("a\nb  c")));
        assert_eq!(argv.last().unwrap(), "exec /bin/sh -c 'a\nb  c'");
        assert_eq!(argv.iter().filter(|a| a.contains('\n')).count(), 1);
    }

    /// A path with a space, a quote and a `$` in it, all the way to the bytes
    /// ssh sends, through *both* layers of quoting. Nothing here may be able to
    /// run anything, and the last assertion is the proof: the login shell has
    /// to unwrap that word back into exactly the script we wrote.
    #[test]
    fn a_hostile_path_reaches_the_remote_as_one_inert_word() {
        let path = p("/ssh:host:/tmp/it's a $(id) file");
        let script = format!("cat -- {}", quote(path.path()));
        assert_eq!(script, "cat -- '/tmp/it'\\''s a $(id) file'");

        let argv = ssh_argv(&path, None, Some(&sh_c(&script)));
        let sent = argv.last().unwrap();
        // What the login shell will hand to `/bin/sh -c`, with `printf` standing
        // in for the `sh` so that nothing actually runs.
        let word = sent.strip_prefix("exec /bin/sh -c ").expect("{sent}");
        let out = std::process::Command::new("sh")
            .args(["-c", &format!("printf '%s' {word}")])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), script);
    }

    /// The guard that stops `rm -rf` from running on a root. No network is
    /// touched: these must fail before ssh is spawned.
    #[test]
    fn delete_refuses_paths_with_no_final_component() {
        for bad in [
            "/ssh:host:/",
            "/ssh:host:",
            "/ssh:host:~",
            "/ssh:host:/tmp/..",
        ] {
            let err = delete(&p(bad), true).unwrap_err().to_string();
            assert!(err.contains("not a named file"), "{bad}: {err}");
        }
    }

    #[test]
    fn renaming_across_hosts_is_refused_rather_than_half_done() {
        let err = rename(&p("/ssh:a:/x"), &p("/ssh:b:/x"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("different hosts"), "{err}");
        // the same host under a different login is a different host too
        assert!(rename(&p("/ssh:a:/x"), &p("/ssh:root@a:/x")).is_err());
        assert!(rename(&p("/ssh:a#22:/x"), &p("/ssh:a:/y")).is_err());
    }

    #[test]
    fn a_listing_record_parses_back_into_an_entry() {
        let e = entry("-- 0644 213 1761700864 hosts").unwrap();
        assert_eq!(e.name, "hosts");
        assert_eq!((e.is_dir, e.is_symlink), (false, false));
        assert_eq!((e.mode, e.len), (0o644, 213));
        assert_eq!(
            e.modified,
            Some(UNIX_EPOCH + Duration::from_secs(1761700864))
        );

        // a name with spaces is not five fields
        assert_eq!(
            entry("d- 1777 64 1 my dir with spaces").unwrap().name,
            "my dir with spaces"
        );
        assert!(entry("dl 0755 7 1 link to a dir").unwrap().is_dir);
        assert!(entry("-l 0755 7 1 broken").unwrap().is_symlink);
        // stat failed: no mtime rather than 1970
        assert_eq!(entry("-- 0 0 0 gone").unwrap().modified, None);
        // and noise from someone's .bashrc is dropped rather than listed
        for junk in ["", "Welcome to Ubuntu 24.04", "-- 0644 1 1 ", "d"] {
            assert!(entry(junk).is_none(), "{junk:?}");
        }
    }

    /// Every failure ssh reports has to land on the variant whose message tells
    /// the user the right thing to do.
    #[test]
    fn ssh_failures_are_sorted_by_what_would_fix_them() {
        let sorted = |stderr: &str| {
            let out = std::process::Command::new("sh")
                .args([
                    "-c",
                    &format!("printf '%s' {} >&2; exit 255", quote(stderr)),
                ])
                .output()
                .unwrap();
            classify(&p("/ssh:host:/x"), &out, "reading")
        };
        assert!(matches!(
            sorted("root@host: Permission denied (publickey,password)."),
            Error::NeedsAuth { .. }
        ));
        assert!(matches!(
            sorted("Host key verification failed."),
            Error::HostKey { .. }
        ));
        assert!(matches!(
            sorted("ssh: Could not resolve hostname host: nodename nor servname provided"),
            Error::Unreachable { .. }
        ));
        assert!(matches!(
            sorted("ssh: connect to host host port 22: Connection refused"),
            Error::Unreachable { .. }
        ));
        // and the message says what to do about it
        assert!(sorted("Permission denied (publickey).")
            .to_string()
            .contains("ssh-add"));
    }

    /// A non-255 exit came from the remote command, not from ssh, so it keeps
    /// the remote's own words.
    #[test]
    fn a_failing_remote_command_reports_what_the_remote_said() {
        let out = std::process::Command::new("sh")
            .args([
                "-c",
                "printf 'cat: /nope: No such file or directory' >&2; exit 1",
            ])
            .output()
            .unwrap();
        let err = classify(&p("/ssh:host:/nope"), &out, "reading /ssh:host:/nope");
        assert!(matches!(err, Error::Remote { status: 1, .. }));
        assert!(
            err.to_string().contains("No such file or directory"),
            "{err}"
        );
    }

    /// The scripts are shell, and a syntax error in one would only ever show up
    /// against a live host. `sh -n` parses without running, so every script
    /// gets checked with the nastiest paths we can hand it.
    #[test]
    fn every_script_is_valid_shell() {
        let ugly = p("/ssh:host:/tmp/it's a $(id) \"file\"");
        let home = p("/ssh:host:~/my notes");
        let mut scripts = Vec::new();
        for target in [&ugly, &home] {
            scripts.push(format!("cat -- {}", quote(target.path())));
            scripts.push(format!("mkdir -- {}", quote(target.path())));
            scripts.push(format!("rm -rf -- {}", quote(target.path())));
            scripts.push(write_script(target));
            scripts.push(list_script(target));
            scripts.push(stat_script(target));
        }
        scripts.push(rename_script(&ugly, &home));
        for script in scripts {
            let out = std::process::Command::new("sh")
                .args(["-n", "-c", &script])
                .output()
                .unwrap();
            assert!(out.status.success(), "not shell: {script}\n{out:?}");
        }
    }

    /// The scripts against a real filesystem, which is the closest we get to a
    /// remote without one: `sh -c` here runs exactly the text ssh would send.
    /// A path built to look like an injection has to come out as a *file* with
    /// that name — the proof that [`quote`] holds end to end.
    #[test]
    #[cfg(unix)]
    fn the_scripts_do_what_they_say_when_run_locally() {
        let dir = std::env::temp_dir().join(format!("zemacs-tramp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let name = "it's a $(touch pwned) \"file\"";
        let file = p(&format!("/ssh:host:{}/{name}", dir.display()));

        run(&write_script(&file), b"hello");
        assert_eq!(std::fs::read(dir.join(name)).unwrap(), b"hello");
        // ...and the command substitution really was inert.
        assert!(!dir.join("pwned").exists() && !std::path::Path::new("pwned").exists());

        // A second write goes through `cp -p`, so the mode must survive it.
        std::fs::set_permissions(dir.join(name), perms(0o600)).unwrap();
        run(&write_script(&file), b"again");
        assert_eq!(std::fs::read(dir.join(name)).unwrap(), b"again");

        let listed = parse_records(&run(
            &list_script(&p(&format!("/ssh:host:{}", dir.display()))),
            b"",
        ));
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].name, name);
        assert_eq!(
            (listed[0].mode, listed[0].len, listed[0].is_dir),
            (0o600, 5, false)
        );
        assert!(listed[0].modified.is_some());

        let stated = parse_records(&run(&stat_script(&file), b""));
        assert_eq!(stated[0].mode, 0o600);

        let moved = file.parent().unwrap().join("moved");
        run(&rename_script(&file, &moved), b"");
        assert!(dir.join("moved").exists() && !dir.join(name).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn run(script: &str, stdin: &[u8]) -> Vec<u8> {
        let mut child = std::process::Command::new("sh")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{script}\n{out:?}");
        out.stdout
    }

    fn parse_records(out: &[u8]) -> Vec<RemoteEntry> {
        String::from_utf8_lossy(out)
            .lines()
            .filter_map(entry)
            .collect()
    }

    #[cfg(unix)]
    fn perms(mode: u32) -> std::fs::Permissions {
        use std::os::unix::fs::PermissionsExt;
        std::fs::Permissions::from_mode(mode)
    }
}
