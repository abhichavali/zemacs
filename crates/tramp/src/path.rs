//! The `/ssh:user@host#port:/path` filename syntax, parsed and printed.
//!
//! [`parse`] runs on every `find-file`, so the local case — which is almost
//! every case — is a five-byte prefix comparison that allocates nothing and
//! returns `None`. Anything that is not exactly this syntax *is* a local path:
//! a file really called `/ssh:notes.txt` opens as itself rather than as a
//! parse error, which is both the safe answer and the one that needs no code.

use std::fmt;

/// Every remote name starts with this. One `memcmp` against it is the whole
/// cost of [`parse`] for a local path.
const PREFIX: &str = "/ssh:";

/// A file on another machine.
///
/// The fields hold what was *written*, not what was resolved: `host` keeps the
/// brackets of an IPv6 literal, `path` keeps its `~` and any colons, and a
/// missing `user` or `port` stays missing rather than being filled in with a
/// guess. That is what makes [`Display`](fmt::Display) an exact inverse of
/// [`parse`], and it is also correct — the user's `~/.ssh/config` is what
/// decides the real login name and port, and we must not pre-empt it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemotePath {
    /// `None` means "whatever ssh would pick", not "the current user".
    pub user: Option<String>,
    /// Host as written, including the brackets of `[::1]`.
    pub host: String,
    pub port: Option<u16>,
    /// Path on the remote, verbatim: may be empty (`/ssh:host:`, the home
    /// directory), may start with `~`, and may contain colons.
    pub path: String,
}

/// Split a file name into a [`RemotePath`], or `None` if it is a local path.
///
/// **This is the function on the `find-file` path.** `None` means "use the
/// string you were given, untouched" — a Windows drive letter, a relative
/// name, a name that merely looks tramp-ish, and every ordinary path all land
/// there without being copied or normalised.
pub fn parse(name: &str) -> Option<RemotePath> {
    let rest = name.strip_prefix(PREFIX)?;
    let (spec, path) = split_spec(rest)?;

    // `user@host`: a login name cannot contain `@`, so the first one wins.
    let (user, host) = match spec.split_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, spec),
    };
    // `host#port`, after any `]`, so an IPv6 literal keeps its own colons.
    let (host, port) = match host.rsplit_once('#') {
        Some((h, p)) => (h, Some(p.parse().ok()?)),
        None => (host, None),
    };
    // A `/` here means the colons belonged to some local path that happened to
    // start with `/ssh:`. Nothing else can validate a hostname for us; ssh will
    // say so far better than we could.
    if host.is_empty() || host.contains('/') || user.is_some_and(|u| u.contains('/')) {
        return None;
    }

    Some(RemotePath {
        user: user.map(str::to_string),
        host: host.to_string(),
        port,
        path: path.to_string(),
    })
}

/// Split `user@host#port:path` at the colon that ends the host part.
///
/// The first colon is the right one *except* inside the brackets of an IPv6
/// literal, which is why this is not `split_once(':')`. A `[` only means an
/// address when it comes before that first colon — after it, it is a character
/// in somebody's file name and must not move the split.
fn split_spec(rest: &str) -> Option<(&str, &str)> {
    let colon = rest.find(':')?;
    let from = match rest.find('[') {
        Some(open) if open < colon => rest[open..].find(']')? + open,
        _ => 0,
    };
    let at = rest[from..].find(':')? + from;
    Some((&rest[..at], &rest[at + 1..]))
}

impl fmt::Display for RemotePath {
    /// Exactly what [`parse`] accepted, so a name survives a trip through a
    /// buffer's `default-directory` and back.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(PREFIX)?;
        if let Some(user) = &self.user {
            write!(f, "{user}@")?;
        }
        f.write_str(&self.host)?;
        if let Some(port) = self.port {
            write!(f, "#{port}")?;
        }
        write!(f, ":{}", self.path)
    }
}

impl RemotePath {
    /// The remote path with the empty case spelled out: `/ssh:host:` means the
    /// login home directory, which on the remote side is `~`.
    pub fn path(&self) -> &str {
        if self.path.is_empty() {
            "~"
        } else {
            &self.path
        }
    }

    /// Same host, same login, one component deeper. What dired presses `RET` on.
    pub fn join(&self, name: &str) -> RemotePath {
        let mut path = self.path().to_string();
        if !path.ends_with('/') {
            path.push('/');
        }
        path.push_str(name);
        RemotePath {
            path,
            ..self.clone()
        }
    }

    /// The directory above, or `None` at a root — `/`, `~`, or `/ssh:host:`,
    /// none of which have a parent we can name.
    pub fn parent(&self) -> Option<RemotePath> {
        let trimmed = self.path().trim_end_matches('/');
        let cut = trimmed.rfind('/')?;
        // `/etc` -> `/`, not the empty path, which would mean the home directory.
        let path = if cut == 0 { "/" } else { &trimmed[..cut] };
        Some(RemotePath {
            path: path.to_string(),
            ..self.clone()
        })
    }

    /// The last component, empty at a root. What dired keeps the cursor on.
    pub fn file_name(&self) -> &str {
        let trimmed = self.path().trim_end_matches('/');
        trimmed.rsplit('/').next().unwrap_or("")
    }

    /// Host as `ssh` wants it on the command line: an IPv6 literal loses the
    /// brackets, which are a *filename* syntax and not an ssh one.
    pub(crate) fn ssh_host(&self) -> &str {
        self.host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(&self.host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(name: &str) -> RemotePath {
        let parsed = parse(name).unwrap_or_else(|| panic!("{name} did not parse"));
        assert_eq!(parsed.to_string(), name, "round trip changed {name}");
        parsed
    }

    #[test]
    fn the_three_shapes_emacs_writes_round_trip() {
        let p = round_trip("/ssh:host:/etc/nginx.conf");
        assert_eq!((p.user, p.host, p.port), (None, "host".into(), None));

        let p = round_trip("/ssh:user@host:/etc/nginx.conf");
        assert_eq!(p.user.as_deref(), Some("user"));

        let p = round_trip("/ssh:user@host#2222:/etc/nginx.conf");
        assert_eq!(p.port, Some(2222));
        assert_eq!(p.path, "/etc/nginx.conf");
    }

    #[test]
    fn a_colon_in_the_remote_path_survives_a_round_trip() {
        let p = round_trip("/ssh:host:/var/log/2024:01:02.log");
        assert_eq!(p.path, "/var/log/2024:01:02.log");
        assert_eq!(p.host, "host");
    }

    #[test]
    fn an_empty_remote_path_means_the_home_directory() {
        let p = round_trip("/ssh:host:");
        assert_eq!(p.path, "");
        // ...but every operation asks for it by the name the remote knows.
        assert_eq!(p.path(), "~");
        assert_eq!(p.join("notes.org").path, "~/notes.org");
    }

    #[test]
    fn a_tilde_path_is_left_for_the_remote_shell_to_expand() {
        let p = round_trip("/ssh:user@host:~/notes.org");
        assert_eq!(p.path, "~/notes.org");
        assert_eq!(round_trip("/ssh:host:~other/x").path, "~other/x");
    }

    #[test]
    fn an_ipv6_literal_keeps_its_brackets_in_the_name_and_loses_them_for_ssh() {
        let p = round_trip("/ssh:root@[fe80::1]#22:/etc/hosts");
        assert_eq!(p.host, "[fe80::1]");
        assert_eq!(p.ssh_host(), "fe80::1");
        assert_eq!(p.path, "/etc/hosts");
        assert_eq!(round_trip("/ssh:[::1]:/etc/hosts").ssh_host(), "::1");
        // ...and a bracket in the *path* does not move the split.
        assert_eq!(round_trip("/ssh:host:/tmp/[a:b]").path, "/tmp/[a:b]");
    }

    /// Everything here is a file on this machine and must come back untouched.
    #[test]
    fn a_local_path_is_recognised_as_local() {
        for local in [
            "",
            "/",
            "/etc/passwd",
            "notes.txt",
            "./relative:with:colons",
            "~/src/zemacs",
            r"C:\Users\me\notes.txt",
            "c:/tmp/x",
            "/ssh",
            "/ssh:",            // no host, no second colon
            "/ssh:host",        // never closed
            "/ssh:/etc/x",      // empty host
            "/ssh:a/b:c",       // a directory that happens to be called `ssh:a`
            "/ssh:[fe80::1:/x", // a bracket that never closes
            "/ssh:host#no:",    // a port that is not a number
            "/SSH:host:/x",     // the method is spelled in lower case
            "/sudo:root@host:/etc/x",
        ] {
            assert!(parse(local).is_none(), "{local:?} should be local");
        }
    }

    #[test]
    fn join_and_parent_walk_the_remote_tree() {
        let root = parse("/ssh:host:/").unwrap();
        assert_eq!(root.join("etc").to_string(), "/ssh:host:/etc");
        assert_eq!(root.parent(), None);

        let file = parse("/ssh:user@host#22:/etc/nginx.conf").unwrap();
        assert_eq!(file.file_name(), "nginx.conf");
        let dir = file.parent().unwrap();
        // The login travels with the path; only the path part moves.
        assert_eq!(dir.to_string(), "/ssh:user@host#22:/etc");
        assert_eq!(dir.parent().unwrap().to_string(), "/ssh:user@host#22:/");
        assert_eq!(dir.parent().unwrap().parent(), None);

        // A trailing slash is not an extra, nameless component.
        let slash = parse("/ssh:host:/etc/").unwrap();
        assert_eq!(slash.file_name(), "etc");
        assert_eq!(slash.join("hosts").path, "/etc/hosts");

        // Home is a root too: `~/..` exists but has no name we can write.
        let home = parse("/ssh:host:~").unwrap();
        assert_eq!(home.parent(), None);
        assert_eq!(home.join("x").path, "~/x");
        assert_eq!(parse("/ssh:host:").unwrap().parent(), None);
    }

    /// Names with the awkward characters in them must still round trip, since
    /// the path part is copied rather than interpreted.
    #[test]
    fn odd_characters_in_the_remote_path_are_not_interpreted() {
        for name in [
            "/ssh:host:/tmp/my file",
            "/ssh:host:/tmp/it's",
            "/ssh:host:/tmp/$HOME",
            "/ssh:host:/tmp/a\"b",
            "/ssh:host:/tmp/;rm -rf ~",
            "/ssh:host:/tmp/café",
            "/ssh:host:/tmp/#hash#",
        ] {
            round_trip(name);
        }
    }
}
