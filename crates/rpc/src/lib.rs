//! zemacs-rpc — talking to a long-lived child over stdio, in JSON-RPC 2.0.
//!
//! The terminal gave zemacs PTYs, which is a different thing: a PTY is a
//! *screen* a program draws on, and what a language server, an agent or a
//! typesetting daemon wants is a pipe it can be asked questions down. This
//! crate is that pipe. LSP is the first user and deliberately not the only one
//! — nothing below knows what `initialize` is.
//!
//! # What is here and what is not
//!
//! Here: spawning, Content-Length framing, id allocation, draining stderr so a
//! chatty child never blocks on a full pipe, and noticing the child die.
//!
//! Not here: what any of the messages *mean*. Responses are not matched to
//! their requests either — the thing that wants the answer is a Lisp closure,
//! so the pending table lives beside the closures, in `runtime/rpc.lisp`.
//! Matching in Rust would mean handing the answer to a channel and matching it
//! again on the other side.
//!
//! # Threads, and why there are three per child
//!
//! `docs/threading.org` is the contract: nothing may block the editor, and
//! nothing may call into ECL from a foreign thread. So per connection:
//!
//! - a **reader** thread parses frames and pushes [`Event`]s onto one process-wide
//!   crossbeam channel that the main loop drains once per frame;
//! - a **writer** thread owns the child's stdin and takes messages off a channel,
//!   so that [`send`] never blocks — a wedged server whose stdin pipe has filled
//!   would otherwise park the Lisp thread inside a primitive, holding the
//!   registry lock, and the main loop's drain behind it;
//! - a **stderr** thread that reads and keeps the last few lines, because a
//!   child whose stderr pipe fills stops running, and because "the server
//!   exited" is useless without the words it printed on the way out.
//!
//! None of them touches the editor or the image. The reader's output becomes a
//! Lisp form on the *main* thread, which queues it for the Lisp thread the same
//! way a mode hook is queued.
//!
//! # JSON
//!
//! `serde_json`, which the tree was already building. Hand-rolling a parser for
//! a *trust boundary* — a child process's output is untrusted input — is how
//! you get a stream desynchronisation that only reproduces on someone else's
//! machine. Encoding is the easy half and stays in Lisp; see [`lisp`].
//!
//! # The registry
//!
//! Connections live in one process-global table, because both the Lisp thread
//! (which starts them and sends on them) and the main thread (which drains
//! them) need to reach the same connection, and neither owns the other. That is
//! the same shape the renderer's leaked TTF context and the single `Editor`
//! already have: one editor per process.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};

use crossbeam_channel::{Receiver, Sender};
use serde_json::value::RawValue;
use serde_json::Value;

pub mod frame;
pub mod lisp;

use frame::Frame;

/// How many stderr lines to keep. Enough for a Python traceback, which is the
/// thing you actually want when `pylsp` refuses to start.
const STDERR_TAIL: usize = 40;

/// A connection handle. Small integers, handed to Lisp as fixnums the way a
/// marker handle is — no new Lisp type, and `PRINT` and `READ` already agree
/// about what one looks like.
pub type Conn = u32;

/// Something the child did.
#[derive(Debug, Clone)]
pub enum Event {
    /// A complete JSON-RPC message. Request, response and notification alike —
    /// telling them apart is reading `id` and `method`, which is protocol, and
    /// protocol is Lisp's.
    Message(Value),
    /// The child said something that is not a message: unframed output, a body
    /// that is not JSON, a length we will not honour. The connection survives a
    /// bad *body* and not a bad *frame*, and an [`Event::Exited`] follows if it
    /// did not.
    Protocol(String),
    /// The child is gone, with whatever it left on stderr. Always the last
    /// event for a connection.
    Exited(String),
}

struct Conns {
    /// Outgoing messages, already framed, on their way to the writer thread.
    /// Dropping this sender is what closes the child's stdin.
    out: Sender<Vec<u8>>,
    child: Child,
    /// JSON-RPC ids, monotonic per connection and starting at 1. Under the
    /// registry lock rather than an atomic: every path already takes it.
    next_id: i64,
}

struct Rpc {
    conns: Mutex<HashMap<Conn, Conns>>,
    next: Mutex<Conn>,
    tx: Sender<(Conn, Event)>,
    rx: Receiver<(Conn, Event)>,
}

fn rpc() -> &'static Rpc {
    static RPC: OnceLock<Rpc> = OnceLock::new();
    RPC.get_or_init(|| {
        let (tx, rx) = crossbeam_channel::unbounded();
        Rpc {
            conns: Mutex::new(HashMap::new()),
            next: Mutex::new(1),
            tx,
            rx,
        }
    })
}

/// A poisoned lock is stepped over rather than propagated, for the reason
/// `zemacs_lisp::with_editor` gives: one thread having panicked is not a reason
/// for every later keystroke to panic too.
fn conns() -> std::sync::MutexGuard<'static, HashMap<Conn, Conns>> {
    rpc().conns.lock().unwrap_or_else(|e| e.into_inner())
}

/// Spawn `program` and start talking to it.
///
/// `cwd` matters more than it looks: a language server decides what project it
/// is looking at from where it was started, and clangd finds
/// `compile_commands.json` relative to it.
pub fn start(program: &str, args: &[String], cwd: Option<&std::path::Path>) -> std::io::Result<Conn> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let mut child = command.spawn()?;

    // `take` rather than `as_mut`: each half goes to a thread that owns it for
    // the life of the connection, and the writer dropping stdin is the signal
    // that hangs up on the child.
    let stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    let id = {
        let mut next = rpc().next.lock().unwrap_or_else(|e| e.into_inner());
        let id = *next;
        *next += 1;
        id
    };

    // Shared with the reader so the exit event can say *why*.
    let tail: std::sync::Arc<Mutex<VecDeque<String>>> = Default::default();

    {
        let tail = tail.clone();
        std::thread::Builder::new()
            .name(format!("zemacs-rpc-err-{id}"))
            .spawn(move || {
                // Read it whether or not anybody wants it: a child whose stderr
                // pipe fills stops running, and a language server that has been
                // wedged by its own logging is a bug report nobody can read.
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut tail = tail.lock().unwrap_or_else(|e| e.into_inner());
                    if tail.len() == STDERR_TAIL {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            })
            .expect("failed to spawn the rpc stderr thread");
    }

    {
        let tx = rpc().tx.clone();
        std::thread::Builder::new()
            .name(format!("zemacs-rpc-in-{id}"))
            .spawn(move || {
                let mut r = BufReader::new(stdout);
                loop {
                    match frame::read_frame(&mut r) {
                        Frame::Body(body) => match serde_json::from_slice::<Value>(&body) {
                            Ok(v) => {
                                if tx.send((id, Event::Message(v))).is_err() {
                                    return; // the editor is gone
                                }
                            }
                            // The frame was well-formed, so the stream is still
                            // synchronised — report the message and keep going.
                            Err(e) => {
                                let head = String::from_utf8_lossy(&body);
                                let head: String = head.chars().take(120).collect();
                                let _ = tx.send((
                                    id,
                                    Event::Protocol(format!("not JSON ({e}): {head}")),
                                ));
                            }
                        },
                        Frame::Broken(why) => {
                            let _ = tx.send((id, Event::Protocol(why)));
                            let _ = tx.send((id, Event::Exited(exit_report(&tail))));
                            return;
                        }
                        Frame::Eof => {
                            let _ = tx.send((id, Event::Exited(exit_report(&tail))));
                            return;
                        }
                    }
                }
            })
            .expect("failed to spawn the rpc reader thread");
    }

    let (out_tx, out_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    std::thread::Builder::new()
        .name(format!("zemacs-rpc-out-{id}"))
        .spawn(move || {
            let mut stdin = stdin;
            for body in out_rx {
                // A write failing means the child is gone; the reader thread is
                // about to say so, so this one just stops.
                if frame::write_frame(&mut stdin, &body).is_err() {
                    return;
                }
            }
            // The channel closed: drop stdin, which is the polite hang-up.
        })
        .expect("failed to spawn the rpc writer thread");

    conns().insert(
        id,
        Conns {
            out: out_tx,
            child,
            next_id: 1,
        },
    );
    Ok(id)
}

fn exit_report(tail: &Mutex<VecDeque<String>>) -> String {
    let tail = tail.lock().unwrap_or_else(|e| e.into_inner());
    if tail.is_empty() {
        "exited".into()
    } else {
        format!("exited: {}", tail.iter().cloned().collect::<Vec<_>>().join("\n"))
    }
}

/// Send `method` with `params`, as a request when `want_reply` and as a
/// notification otherwise. Answers the id a request was given, so the caller
/// can park a continuation under it.
///
/// `params` is raw JSON text, built by whoever knows the protocol — validated
/// here, and spliced in verbatim rather than parsed into a tree and printed
/// back out. A `textDocument/didChange` carries the whole buffer, so the
/// difference between validating and re-serialising it is felt.
pub fn send(conn: Conn, method: &str, params: Option<&str>, want_reply: bool) -> Result<Option<i64>, String> {
    let mut map = conns();
    let entry = map.get_mut(&conn).ok_or("no such connection")?;
    let id = want_reply.then(|| {
        let id = entry.next_id;
        entry.next_id += 1;
        id
    });
    let body = envelope(method, params, id)?;
    entry
        .out
        .send(body)
        .map_err(|_| "the connection is closed".to_string())?;
    Ok(id)
}

/// Answer a request the *child* made. `id` and `result` are raw JSON — the id
/// especially, because JSON-RPC allows a string there and the only honest way
/// to echo one back is not to interpret it.
pub fn respond(conn: Conn, id: &str, result: Option<&str>, error: Option<&str>) -> Result<(), String> {
    let map = conns();
    let entry = map.get(&conn).ok_or("no such connection")?;
    let id = check(id, "id")?;
    let mut body = String::from(r#"{"jsonrpc":"2.0","id":"#);
    body.push_str(id.get());
    match (result, error) {
        (_, Some(e)) => {
            body.push_str(r#","error":"#);
            body.push_str(check(e, "error")?.get());
        }
        (result, None) => {
            body.push_str(r#","result":"#);
            body.push_str(match result {
                Some(r) => check(r, "result")?.get(),
                None => "null",
            });
        }
    }
    body.push('}');
    entry
        .out
        .send(body.into_bytes())
        .map_err(|_| "the connection is closed".to_string())
}

/// Build the outgoing message by hand, because the only part that needs
/// escaping is the method name and the params are already JSON.
fn envelope(method: &str, params: Option<&str>, id: Option<i64>) -> Result<Vec<u8>, String> {
    let params = params.filter(|p| !p.trim().is_empty());
    let params = params.map(|p| check(p, "params")).transpose()?;
    let mut body = String::with_capacity(params.map_or(0, |p| p.get().len()) + 96);
    body.push_str(r#"{"jsonrpc":"2.0""#);
    if let Some(id) = id {
        body.push_str(&format!(r#","id":{id}"#));
    }
    body.push_str(r#","method":"#);
    body.push_str(&serde_json::to_string(method).map_err(|e| e.to_string())?);
    if let Some(params) = params {
        body.push_str(r#","params":"#);
        body.push_str(params.get());
    }
    body.push('}');
    Ok(body.into_bytes())
}

/// Validate JSON without building a tree for it. A malformed payload from Lisp
/// is a message in the status line; letting it through would desynchronise the
/// server instead, and the symptom would surface three requests later.
fn check<'a>(json: &'a str, what: &str) -> Result<&'a RawValue, String> {
    serde_json::from_str::<&RawValue>(json).map_err(|e| format!("malformed {what}: {e}"))
}

/// `argv`, spelled as a JSON array of strings.
///
/// A single space-separated string would be shorter and wrong: `clangd
/// --compile-commands-dir=/Users/me/My Project` is one argument with a space in
/// it, and there is no separator a path cannot contain. Lisp already has to
/// build JSON for the params, so it costs nothing to spell this the same way.
/// An empty string means no arguments.
pub fn parse_args(json: &str) -> Result<Vec<String>, String> {
    if json.trim().is_empty() {
        return Ok(Vec::new());
    }
    let v: Value = serde_json::from_str(json).map_err(|e| format!("malformed args: {e}"))?;
    let Value::Array(items) = v else {
        return Err("args must be a JSON array of strings".into());
    };
    items
        .iter()
        .map(|i| match i {
            Value::String(s) => Ok(s.clone()),
            other => Err(format!("args must be strings, got {other}")),
        })
        .collect()
}

/// `s` as a JSON string literal, quotes included.
///
/// Exposed because the Lisp encoder is built on it: escaping is the one part of
/// producing JSON that is fiddly enough to be worth a primitive, and the string
/// it is asked to escape most often is a whole buffer.
pub fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Hang up. Queued messages are written first — a polite `exit` notification
/// sent immediately before this still reaches the server — and then the child
/// is killed.
///
/// ponytail: killed rather than waited for. A language server that ignores its
/// own shutdown sequence would otherwise outlive the editor, and there is no
/// timeout facility here to give it a grace period with. The upgrade is a
/// reaper thread that waits a second before the signal; nothing has needed one.
pub fn stop(conn: Conn) {
    if let Some(mut entry) = conns().remove(&conn) {
        drop(entry.out); // flushes what is queued, then closes stdin
        let _ = entry.child.kill();
        let _ = entry.child.wait(); // reap, so nothing is left as a zombie
    }
}

/// Every connection, on the way out of the editor. Without it a quit leaves
/// orphaned language servers behind, one per session.
pub fn stop_all() {
    let ids: Vec<Conn> = conns().keys().copied().collect();
    for id in ids {
        stop(id);
    }
}

pub fn is_live(conn: Conn) -> bool {
    conns().contains_key(&conn)
}

/// The next thing a child said, or `None`. Never blocks — this is called from
/// the main loop, once per frame, and the main loop may not wait on a child.
pub fn poll() -> Option<(Conn, Event)> {
    rpc().rx.try_recv().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// [`poll`] is one queue for every connection, because the editor has one
    /// drain — so two tests running at once would eat each other's events.
    /// Taking this and emptying the queue is what makes them independent.
    fn alone() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        while poll().is_some() {}
        guard
    }

    /// `cat` is the fake child: it copies stdin to stdout byte for byte, so
    /// every frame we write comes back as a frame to parse. That exercises the
    /// whole path — spawn, writer thread, framing out, framing in, reader
    /// thread, channel — without any of it depending on a language server being
    /// installed.
    fn echo() -> Option<Conn> {
        start("cat", &[], None).ok()
    }

    fn next(conn: Conn) -> Event {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some((c, e)) = poll() {
                if c == conn {
                    return e;
                }
            }
            assert!(Instant::now() < deadline, "timed out waiting for the child");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn field<'a>(e: &'a Event, key: &str) -> &'a Value {
        match e {
            Event::Message(v) => &v[key],
            other => panic!("expected a message, got {other:?}"),
        }
    }

    #[test]
    fn a_request_is_framed_and_comes_back_with_its_id() {
        let _alone = alone();
        let Some(conn) = echo() else {
            return; // no `cat`; nothing here is worth failing a build over
        };
        // Ids are per connection and start at 1, which is what lets the Lisp
        // side park a continuation under one.
        assert_eq!(send(conn, "ping", Some(r#"{"n":1}"#), true), Ok(Some(1)));
        assert_eq!(send(conn, "ping", None, true), Ok(Some(2)));
        // A notification takes no id at all.
        assert_eq!(send(conn, "note", Some("[1,2]"), false), Ok(None));

        let first = next(conn);
        assert_eq!(field(&first, "id"), &Value::from(1));
        assert_eq!(field(&first, "method"), &Value::from("ping"));
        assert_eq!(field(&first, "params"), &serde_json::json!({"n": 1}));
        assert_eq!(field(&first, "jsonrpc"), &Value::from("2.0"));

        let second = next(conn);
        assert_eq!(field(&second, "id"), &Value::from(2));
        // Absent params stay absent rather than becoming null: a server that
        // checks arity will reject `"params": null` on a method that takes none.
        assert_eq!(field(&second, "params"), &Value::Null);

        let third = next(conn);
        assert_eq!(field(&third, "id"), &Value::Null);
        assert_eq!(field(&third, "params"), &serde_json::json!([1, 2]));

        stop(conn);
        assert!(!is_live(conn));
    }

    /// A reply to a request the child made. The id is echoed as raw JSON so a
    /// string id survives, which is the half of JSON-RPC everyone forgets.
    #[test]
    fn a_response_echoes_the_id_it_was_given() {
        let _alone = alone();
        let Some(conn) = echo() else { return };
        respond(conn, r#""abc""#, Some("null"), None).unwrap();
        respond(conn, "7", None, Some(r#"{"code":-32601,"message":"nope"}"#)).unwrap();

        let first = next(conn);
        assert_eq!(field(&first, "id"), &Value::from("abc"));
        assert_eq!(field(&first, "result"), &Value::Null);

        let second = next(conn);
        assert_eq!(field(&second, "id"), &Value::from(7));
        assert_eq!(field(&second, "error")["code"], Value::from(-32601));

        stop(conn);
    }

    /// The trust boundary in the other direction: a params string built badly
    /// in Lisp must not reach the child, because a server that has been fed
    /// half a message stays desynchronised for the rest of the session.
    #[test]
    fn malformed_params_are_refused_rather_than_written() {
        let _alone = alone();
        let Some(conn) = echo() else { return };
        let bad = send(conn, "m", Some(r#"{"unterminated": "#), true);
        assert!(bad.unwrap_err().starts_with("malformed params"), "must not be written");
        // ...and the connection is still usable afterwards.
        assert_eq!(send(conn, "m", Some("{}"), true), Ok(Some(2)));
        assert_eq!(field(&next(conn), "id"), &Value::from(2));
        stop(conn);
    }

    /// A child that goes away has to say so exactly once, or Lisp never learns
    /// that its server is dead.
    #[test]
    fn the_child_dying_produces_one_exit_event() {
        let _alone = alone();
        let Ok(conn) = start("sh", &["-c".into(), "echo boom >&2; exit 3".into()], None) else {
            return;
        };
        match next(conn) {
            Event::Exited(why) => assert!(why.contains("boom"), "stderr belongs in the report: {why}"),
            other => panic!("expected an exit, got {other:?}"),
        }
        stop(conn);
    }

    /// Unframed output is not a reason to hang up on a whole session, but it is
    /// a reason to say something.
    #[test]
    fn a_body_that_is_not_json_is_reported_and_the_stream_survives() {
        let _alone = alone();
        // A well-framed body that is not JSON, then a good one — the frame
        // boundary is known either way, so only the bad message is lost.
        let script = r#"printf 'Content-Length: 5\r\n\r\nnot{ '; printf 'Content-Length: 8\r\n\r\n{"ok":1}'"#;
        let Ok(conn) = start("sh", &["-c".into(), script.into()], None) else {
            return;
        };
        match next(conn) {
            Event::Protocol(why) => assert!(why.contains("not JSON"), "{why}"),
            other => panic!("expected a protocol complaint, got {other:?}"),
        }
        assert_eq!(field(&next(conn), "ok"), &Value::from(1));
        stop(conn);
    }

    /// A path with a space in it is the case a space-separated argv gets wrong,
    /// and it is not a rare one on macOS.
    #[test]
    fn argv_arrives_as_json_so_a_space_in_a_path_survives() {
        assert_eq!(parse_args(""), Ok(Vec::new()));
        assert_eq!(
            parse_args(r#"["--dir=/Users/me/My Project", "-x"]"#),
            Ok(vec!["--dir=/Users/me/My Project".into(), "-x".into()])
        );
        assert!(parse_args("[1]").is_err());
        assert!(parse_args("{}").is_err());
        assert!(parse_args("[").is_err());
    }

    #[test]
    fn a_json_string_literal_escapes_what_json_insists_on() {
        assert_eq!(json_string("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(json_string("a\nb"), r#""a\nb""#);
    }

    #[test]
    fn sending_on_a_connection_that_never_existed_is_an_error_not_a_panic() {
        let _alone = alone();
        assert!(send(9999, "m", None, true).is_err());
        assert!(respond(9999, "1", None, None).is_err());
        stop(9999); // and stopping one twice is a no-op
    }
}
