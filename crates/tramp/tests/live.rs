//! The half that needs a real host, and therefore skips itself by default.
//!
//! `cargo test` on a laptop with no server, on a machine with no `sshd`, and on
//! CI must all pass, so nothing here runs unless it is asked for:
//!
//! ```sh
//! ZEMACS_TRAMP_TEST_HOST=user@server cargo test -p zemacs-tramp
//! ZEMACS_TRAMP_TEST_LOCALHOST=1     cargo test -p zemacs-tramp   # needs sshd
//! ```
//!
//! Both want key authentication to this host already working — a host that
//! would ask for a password is *supposed* to fail here, since that is the whole
//! contract the crate offers.
//!
//! The last two tests need no host at all and always run.

use std::time::Instant;

use zemacs_tramp::{
    delete, list, mkdir, parse, read_to_string, rename, stat, write, Error, Op, RemotePath,
};

/// A scratch directory under the remote home, named after this process so two
/// runs cannot collide. `None` when the guard variable is unset, which is the
/// only reason any of this is allowed to be skipped.
fn scratch(var: &str, fallback: &str) -> Option<RemotePath> {
    let value = std::env::var(var).ok()?;
    // The host variable carries the login itself; the localhost one is a plain
    // on/off switch, so anything truthy in it means the fallback.
    let spec = match value.as_str() {
        "" | "1" | "yes" | "true" => fallback,
        spec => spec,
    };
    parse(&format!(
        "/ssh:{spec}:~/zemacs-tramp-test-{}",
        std::process::id()
    ))
}

/// Everything, against one host, in one test: each step depends on the last,
/// and splitting them would mean setting the directory up four times.
fn round_trip(dir: RemotePath) {
    mkdir(&dir).unwrap_or_else(|e| panic!("mkdir {dir}: {e}"));

    // The awkward name, end to end. If quoting leaks, this is where it shows.
    let file = dir.join("it's a $(touch pwned) \"file\".conf");
    let body = "server {\n  listen 80;\n}\n";
    write(&file, body.as_bytes()).unwrap_or_else(|e| panic!("write {file}: {e}"));
    assert_eq!(read_to_string(&file).unwrap(), body);
    assert!(
        stat(&dir.join("pwned")).unwrap().is_none(),
        "the substitution ran"
    );

    let meta = stat(&file).unwrap().expect("no metadata");
    assert_eq!(meta.len as usize, body.len());
    assert!(!meta.is_dir && !meta.is_symlink);
    assert!(meta.mode & 0o400 != 0, "unreadable: {:o}", meta.mode);
    assert_eq!(meta.name, file.file_name());

    // An overwrite must keep the mode bits and must not leave a temporary
    // behind for the listing below to trip over.
    write(&file, b"listen 443;\n").unwrap();
    assert_eq!(read_to_string(&file).unwrap(), "listen 443;\n");
    assert_eq!(stat(&file).unwrap().unwrap().mode, meta.mode);

    let entries = list(&dir).unwrap();
    assert_eq!(entries.len(), 1, "{entries:#?}");
    assert_eq!(entries[0].name, file.file_name());
    assert!(entries[0].modified.is_some());

    let moved = dir.join("nginx.conf");
    rename(&file, &moved).unwrap();
    assert!(stat(&file).unwrap().is_none());
    assert_eq!(list(&dir).unwrap()[0].name, "nginx.conf");
    // ...and a rename onto something that exists is refused, not silent.
    write(&file, b"x").unwrap();
    assert!(rename(&file, &moved).is_err());

    // Binary in, binary out: nothing on the way re-encodes.
    let bytes: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
    let blob = dir.join("blob.bin");
    write(&blob, &bytes).unwrap();
    assert_eq!(zemacs_tramp::read(&blob).unwrap(), bytes);
    assert!(
        read_to_string(&blob).is_err(),
        "should refuse to decode a blob"
    );

    delete(&dir, true).unwrap();
    assert!(stat(&dir).unwrap().is_none());
}

/// The reason for the control socket: the second operation must not pay for a
/// handshake. Ten times the first one would still be generous; in practice it
/// is the difference between ~300ms and ~10ms.
fn reuses_the_connection(dir: RemotePath) {
    let first = Instant::now();
    stat(&dir).unwrap();
    let first = first.elapsed();
    let again = Instant::now();
    stat(&dir).unwrap();
    let again = again.elapsed();
    assert!(
        again * 2 < first || again.as_millis() < 100,
        "no connection reuse: {first:?} then {again:?}"
    );
}

#[test]
fn a_remote_host_round_trips_every_operation() {
    let Some(dir) = scratch("ZEMACS_TRAMP_TEST_HOST", "localhost") else {
        return; // no host configured, nothing to say
    };
    round_trip(dir.clone());
    reuses_the_connection(dir);
}

/// The same thing over `ssh localhost`, which needs nothing but this machine's
/// own sshd and an authorized key — the cheapest real host there is.
#[test]
fn localhost_round_trips_every_operation() {
    let Some(dir) = scratch("ZEMACS_TRAMP_TEST_LOCALHOST", "localhost") else {
        return;
    };
    round_trip(dir.clone());
    reuses_the_connection(dir);
}

/// No host needed, so this one always runs: port 1 on `0.0.0.0` refuses the
/// connection immediately, which pins down that a host we cannot talk to is a
/// message and not a frozen editor.
#[test]
fn an_unreachable_host_fails_quickly_rather_than_hanging() {
    let start = Instant::now();
    let err = read_to_string(&parse("/ssh:0.0.0.0#1:/etc/hosts").unwrap()).unwrap_err();
    assert!(
        matches!(err, Error::Unreachable { .. }),
        "sorted the wrong way: {err:?}"
    );
    assert!(
        start.elapsed() < zemacs_tramp::TIMEOUT,
        "took the full timeout"
    );
}

/// The worker's plumbing, with an operation that fails before it reaches the
/// network: what matters here is that a request comes back at all, tagged with
/// the id it went in with.
#[test]
fn the_worker_answers_every_request_exactly_once() {
    let worker = zemacs_tramp::spawn_worker();
    for id in 0..3 {
        worker.request(
            id,
            Op::Delete {
                path: parse("/ssh:host:/").unwrap(),
                recursive: true,
            },
        );
    }
    let mut seen = Vec::new();
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while seen.len() < 3 && Instant::now() < deadline {
        match worker.poll() {
            Some((id, reply)) => {
                assert!(reply.is_err(), "deleting a root should not have run");
                seen.push(id);
            }
            None => std::thread::yield_now(),
        }
    }
    seen.sort_unstable();
    assert_eq!(seen, [0, 1, 2], "nothing may be coalesced or dropped");
    assert!(worker.poll().is_none(), "and nothing answered twice");
}
