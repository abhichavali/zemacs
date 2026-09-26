//! A repository on another machine, driven through the one seam every git call
//! takes — with `$ZEMACS_SSH` pointing at a stand-in that does what the far
//! login shell would (run the last argument with `sh -c`), so the whole path is
//! exercised except the network.
//!
//! Its own file, and so its own process, because the variable is process-wide:
//! `tests/repo.rs` runs local git in parallel and must never see it.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git").current_dir(dir).args(args).output().unwrap();
    assert!(out.status.success(), "setup git {args:?}: {out:?}");
}

#[test]
#[cfg(unix)]
fn a_remote_repository_is_found_read_and_staged_over_ssh() {
    use std::os::unix::fs::PermissionsExt;
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let base = std::env::temp_dir()
        .join(format!("zemacs-git-remote-{}", std::process::id()))
        .join("it's a $(touch pwned) repo");
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(base.join("src")).unwrap();
    let repo = base.canonicalize().unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "t"]);
    fs::write(repo.join("src/a.rs"), "fn a() {}\n").unwrap();

    let fake = repo.parent().unwrap().join("ssh");
    fs::write(&fake, "#!/bin/sh\nfor a; do last=$a; done\nexec /bin/sh -c \"$last\"\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("ZEMACS_SSH", &fake);

    // From a file two levels down, the way `magit-status` asks: the top level
    // comes back as a name on that host, not as a path on this one.
    let file = PathBuf::from(format!("/ssh:me@box:{}/src/a.rs", repo.display()));
    let root = zemacs_git::repo_root(&file).expect("found over ssh");
    assert_eq!(root, PathBuf::from(format!("/ssh:me@box:{}", repo.display())));

    let status = zemacs_git::status(&root).unwrap();
    assert_eq!(status.untracked, [PathBuf::from("src/a.rs")]);
    // No marker files can be read over ssh, so nothing is claimed in flight.
    assert_eq!(status.in_progress, None);

    zemacs_git::stage(&root, std::path::Path::new("src/a.rs")).unwrap();
    let status = zemacs_git::status(&root).unwrap();
    assert!(status.untracked.is_empty(), "{status:?}");
    assert_eq!(status.staged.len(), 1, "{status:?}");
    zemacs_git::commit(&root, "first").unwrap();
    assert_eq!(zemacs_git::status(&root).unwrap().recent.len(), 1);

    // ...and the hostile directory name ran nothing on the way.
    assert!(!repo.join("pwned").exists() && !PathBuf::from("pwned").exists());
    std::env::remove_var("ZEMACS_SSH");
    let _ = fs::remove_dir_all(repo.parent().unwrap());
}
