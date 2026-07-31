//! Content-Length framing — the wire format LSP borrowed from HTTP.
//!
//! A message is a few `Name: value` header lines, a blank line, and exactly
//! `Content-Length` **bytes** of body. Bytes, not characters: a header saying 42
//! over a body full of emoji is 42 bytes, and reading 42 *chars* is how you
//! desynchronise a stream permanently.
//!
//! Everything here treats the child's output as untrusted, because it is. A
//! framing error is a value, never a panic and never an `unwrap`.

use std::io::{BufRead, Write};

/// Refuse to allocate more than this for one message. A child that says
/// `Content-Length: 9999999999` gets an error rather than an OOM — the number
/// is attacker-controlled in exactly the same sense any parser's length field
/// is, and 32 MB is already an absurd JSON-RPC message.
pub const MAX_FRAME: usize = 32 * 1024 * 1024;

/// What one read off the child's stdout produced.
pub enum Frame {
    /// A complete body. Still unparsed — framing does not care what is inside.
    Body(Vec<u8>),
    /// The child closed stdout. Nothing more will ever arrive.
    Eof,
    /// The stream is not a Content-Length stream, or stopped being one. There
    /// is no way to resynchronise without guessing, so the caller hangs up.
    Broken(String),
}

/// Frame `body` and write it. One `write_all` for the header and one for the
/// body, then a flush — a pipe is not line-buffered and a half-written header
/// is a child that waits forever.
pub fn write_frame(w: &mut impl Write, body: &[u8]) -> std::io::Result<()> {
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(body)?;
    w.flush()
}

/// Read one message.
///
/// ponytail: unrecognised header lines are *skipped* rather than rejected.
/// Servers and their wrapper scripts print banners, deprecation notices and the
/// occasional traceback on stdout, and hanging up on the first stray line would
/// make the editor blame the protocol for the server's manners. A malformed
/// `Content-Length` is still fatal, because that is the one field we cannot
/// guess our way past. The upgrade, if a server ever needs it, is to scan
/// forward for the next `Content-Length:` instead of failing.
pub fn read_frame(r: &mut impl BufRead) -> Frame {
    let mut len: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) => return Frame::Eof,
            Ok(_) => {}
            // Non-UTF-8 in a header, or the pipe dying mid-read. Both mean this
            // is not a stream we can keep reading.
            Err(e) => return Frame::Broken(format!("reading headers: {e}")),
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            // A blank line ends the headers — but only once we have a length.
            // Before that it is just noise from a chatty child.
            if len.is_some() {
                break;
            }
            continue;
        }
        let Some((name, value)) = trimmed.split_once(':') else {
            continue; // not a header at all; noise
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            match value.trim().parse::<usize>() {
                Ok(n) if n <= MAX_FRAME => len = Some(n),
                Ok(n) => return Frame::Broken(format!("Content-Length {n} over the {MAX_FRAME} limit")),
                Err(e) => return Frame::Broken(format!("bad Content-Length {:?}: {e}", value.trim())),
            }
        }
    }

    let n = len.expect("the loop only breaks with a length");
    let mut body = vec![0u8; n];
    match r.read_exact(&mut body) {
        Ok(()) => Frame::Body(body),
        // A short body at EOF is a child that died mid-message. Reported rather
        // than silently treated as a clean exit, because the difference is the
        // difference between "the server finished" and "the server crashed".
        Err(e) => Frame::Broken(format!("truncated body of {n} bytes: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn frames(input: &[u8]) -> Vec<Frame> {
        let mut r = BufReader::new(input);
        let mut out = Vec::new();
        loop {
            let f = read_frame(&mut r);
            let stop = !matches!(f, Frame::Body(_));
            out.push(f);
            if stop {
                return out;
            }
        }
    }

    fn body(f: &Frame) -> &[u8] {
        match f {
            Frame::Body(b) => b,
            Frame::Eof => panic!("expected a body, got EOF"),
            Frame::Broken(e) => panic!("expected a body, got {e}"),
        }
    }

    #[test]
    fn a_frame_round_trips() {
        let mut buf = Vec::new();
        write_frame(&mut buf, br#"{"id":1}"#).unwrap();
        assert_eq!(buf, b"Content-Length: 8\r\n\r\n{\"id\":1}");
        let got = frames(&buf);
        assert_eq!(body(&got[0]), br#"{"id":1}"#);
        assert!(matches!(got[1], Frame::Eof));
    }

    /// Two messages arriving in one read is the normal case, not an edge one:
    /// a server answering a burst of notifications writes them back to back.
    #[test]
    fn several_frames_come_out_of_one_buffer() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"one").unwrap();
        write_frame(&mut buf, b"two").unwrap();
        let got = frames(&buf);
        assert_eq!(body(&got[0]), b"one");
        assert_eq!(body(&got[1]), b"two");
        assert!(matches!(got[2], Frame::Eof));
    }

    /// The whole reason the length is in bytes. "héllo" is five characters and
    /// six bytes, and counting the wrong one desynchronises the stream for good.
    #[test]
    fn the_length_is_bytes_and_not_characters() {
        let mut buf = Vec::new();
        write_frame(&mut buf, "héllo".as_bytes()).unwrap();
        write_frame(&mut buf, b"after").unwrap();
        let got = frames(&buf);
        assert_eq!(body(&got[0]), "héllo".as_bytes());
        assert_eq!(body(&got[1]), b"after");
    }

    #[test]
    fn extra_headers_and_a_chatty_child_are_tolerated() {
        let input = b"warming up\r\n\r\nContent-Type: application/vscode-jsonrpc\r\nContent-Length: 2\r\n\r\nhi";
        assert_eq!(body(&frames(input)[0]), b"hi");
    }

    /// The one header we cannot guess past.
    #[test]
    fn a_bad_length_is_fatal_rather_than_a_guess() {
        let got = frames(b"Content-Length: banana\r\n\r\n{}");
        assert!(matches!(&got[0], Frame::Broken(e) if e.contains("bad Content-Length")));

        let got = frames(b"Content-Length: 99999999999\r\n\r\n{}");
        assert!(matches!(&got[0], Frame::Broken(e) if e.contains("limit")));
    }

    /// A child that dies halfway through a message is a crash, and must not be
    /// reported as a clean exit.
    #[test]
    fn a_truncated_body_is_reported_rather_than_treated_as_eof() {
        let got = frames(b"Content-Length: 10\r\n\r\nshort");
        assert!(matches!(&got[0], Frame::Broken(e) if e.contains("truncated")));
    }

    /// Nothing at all is a clean exit, not an error: it is what a child that
    /// was asked to quit looks like.
    #[test]
    fn an_empty_stream_is_eof() {
        assert!(matches!(frames(b"")[0], Frame::Eof));
    }
}
