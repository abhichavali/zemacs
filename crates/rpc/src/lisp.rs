//! JSON as Common Lisp source.
//!
//! The same trick `zemacs_core::query` plays, for the same reason: rather
//! than marshal a tagged tree through C, print the value as *source* and let
//! ECL's READ build it. One C signature carries every shape, and the image gets
//! a real cons tree instead of a handle it has to keep asking questions about.
//!
//! It also means **no JSON parser in Lisp**. Decoding is the half that is a bug
//! farm; `serde_json` does it here, and the image only ever has to walk conses.
//! Encoding — the easy half — stays in Lisp, in `runtime/rpc.lisp`, so that the
//! *shape* of a request is written where the protocol is written.
//!
//! The mapping, which `runtime/rpc.lisp` documents from the other side:
//!
//! | JSON        | Lisp                          |
//! |-------------|-------------------------------|
//! | `null`      | `NIL`                         |
//! | `true`      | `T`                           |
//! | `false`     | `NIL`                         |
//! | number      | integer or double             |
//! | string      | string                        |
//! | `[a, b]`    | `(a b)`                       |
//! | `{"k": v}`  | `(("k" . v))` — an alist      |
//!
//! Object keys come out sorted, because `serde_json` keeps them in a `BTreeMap`
//! unless asked otherwise. Nothing should depend on the order — an alist is
//! searched, not indexed — but it does mean the printed form of a message is
//! stable, which is what makes it assertable in a test.
//!
//! ponytail: `false`, `null`, `[]` and `{}` all arrive as `NIL`, so a reader
//! cannot tell "absent" from "explicitly false". That is the mapping that makes
//! `(when (jget d "deprecated") ...)` read the way a Lisp author expects, and
//! for LSP nothing depends on the distinction. The upgrade, if something ever
//! does, is `:false` / `:null` keywords — one arm of the match below and one
//! line in the Lisp encoder.

use serde_json::Value;

/// A Lisp form for `v`, ready to be READ.
pub fn to_lisp(v: &Value) -> String {
    let mut out = String::new();
    write(&mut out, v);
    out
}

fn write(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("nil"),
        Value::Bool(true) => out.push('t'),
        Value::Bool(false) => out.push_str("nil"),
        Value::Number(n) => out.push_str(&number(n)),
        Value::String(s) => out.push_str(&string(s)),
        Value::Array(items) => {
            out.push('(');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                write(out, item);
            }
            out.push(')');
        }
        // An alist, so `(cdr (assoc "uri" o :test #'string=))` is the accessor —
        // and note `("k" . (1 2))` prints and reads as `("k" 1 2)`, which is the
        // same cons and the same `cdr`. Nothing has to special-case it.
        Value::Object(map) => {
            out.push('(');
            for (i, (k, val)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                out.push('(');
                out.push_str(&string(k));
                out.push_str(" . ");
                write(out, val);
                out.push(')');
            }
            out.push(')');
        }
    }
}

/// A Lisp string literal. Only `\` and `"` are special inside one.
///
/// A NUL is replaced rather than escaped: the form crosses into ECL as a
/// NUL-terminated C string, so an interior NUL would truncate the message and
/// leave the reader staring at an unbalanced paren. Common Lisp has no `\x`
/// escape to spell it with, and JSON strings with embedded NULs are pathology
/// rather than data.
pub fn string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            '\0' => out.push('\u{fffd}'),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A number CL's reader will accept.
///
/// The exponent marker is `d`, not `e`: `1e300` is read as a *single* float
/// under the default `*read-default-float-format*` and overflows, which would
/// turn one absurd number in a server's response into a read error that eats
/// the whole message.
fn number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    match n.as_f64() {
        Some(f) if f.is_finite() => format!("{f:?}").replace(['e', 'E'], "d"),
        // Not expressible in JSON in the first place; NIL beats a read error.
        _ => "nil".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lisp(v: Value) -> String {
        to_lisp(&v)
    }

    #[test]
    fn scalars_read_as_themselves() {
        assert_eq!(lisp(json!(null)), "nil");
        assert_eq!(lisp(json!(true)), "t");
        assert_eq!(lisp(json!(false)), "nil");
        assert_eq!(lisp(json!(42)), "42");
        assert_eq!(lisp(json!(-1)), "-1");
        assert_eq!(lisp(json!(1.5)), "1.5");
        assert_eq!(lisp(json!("hi")), "\"hi\"");
    }

    /// The whole point of emitting source: it has to survive READ.
    #[test]
    fn strings_are_escaped_for_the_reader() {
        assert_eq!(lisp(json!(r#"say "hi\there""#)), r#""say \"hi\\there\"""#);
        // A newline stands for itself inside a CL string literal.
        assert_eq!(lisp(json!("a\nb")), "\"a\nb\"");
        // ...and a NUL would truncate the C string carrying the whole form.
        assert_eq!(lisp(json!("a\0b")), "\"a\u{fffd}b\"");
    }

    /// `1e300` under `*read-default-float-format*` is a single float and
    /// overflows, which would cost the whole message rather than one field.
    #[test]
    fn a_huge_float_uses_the_double_exponent_marker() {
        assert_eq!(lisp(json!(1e300)), "1d300");
        assert_eq!(lisp(json!(1e-7)), "1d-7");
    }

    #[test]
    fn an_object_is_an_alist_and_an_array_is_a_list() {
        assert_eq!(lisp(json!([1, 2, 3])), "(1 2 3)");
        assert_eq!(lisp(json!({"a": 1})), r#"(("a" . 1))"#);
        // Keys sorted, as a `BTreeMap` hands them over.
        assert_eq!(
            lisp(json!({"range": {"start": {"line": 3, "character": 5}}})),
            r#"(("range" . (("start" . (("character" . 5) ("line" . 3))))))"#
        );
        // `()` and `nil` are the same object; an empty array, an empty object,
        // `false` and `null` all land on it. See the ponytail note above.
        assert_eq!(lisp(json!({"items": []})), r#"(("items" . ()))"#);
        assert_eq!(lisp(json!({"items": {}})), r#"(("items" . ()))"#);
    }

    /// A real `publishDiagnostics` payload, in the shape the Lisp handler walks.
    #[test]
    fn a_diagnostics_notification_converts_whole() {
        let v = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///tmp/x.py",
                "diagnostics": [{
                    "range": {"start": {"line": 0, "character": 0},
                              "end": {"line": 0, "character": 3}},
                    "severity": 1,
                    "source": "pyflakes",
                    "message": "undefined name 'foo'"
                }]
            }
        });
        let src = to_lisp(&v);
        assert!(src.starts_with(r#"(("jsonrpc" . "2.0")"#), "{src}");
        assert!(src.contains(r#"("severity" . 1)"#), "{src}");
        assert!(src.contains(r#"("message" . "undefined name 'foo'")"#), "{src}");
        // Balanced, which is the only thing READ actually insists on.
        assert_eq!(src.matches('(').count(), src.matches(')').count());
    }
}
