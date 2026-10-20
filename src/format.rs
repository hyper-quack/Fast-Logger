//! Record encoding. A line is `header + body`: the header (timestamp) is written
//! under the writer lock so file order equals time order; the body is encoded
//! beforehand on the caller's thread.
//!
//! Text: `2026-10-15T12:00:00.000123+02:00 INFO  api: message<TAB>key=value key2="a b"`
//! JSON: `{"ts":"2026-10-15T12:00:00.000123+02:00","level":"info","logger":"api","msg":"message","key":"value"}`

use std::borrow::Cow;
use std::fmt;

use serde::de::{self, Deserializer as _, MapAccess, Visitor};
use serde_json::value::RawValue;

use crate::config::Format;
use crate::level::Level;
use crate::time::TsFormatter;

/// A structured field value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value<'a> {
    Str(&'a str),
    I64(i64),
    U64(u64),
    F64(f64),
    Bool(bool),
    Null,
    /// Pre-serialized JSON (object, array, …), spliced verbatim in JSON output.
    Json(&'a str),
}

macro_rules! from_int {
    ($($t:ty => $v:ident),*) => {$(
        impl From<$t> for Value<'_> {
            #[inline]
            fn from(v: $t) -> Self { Value::$v(v as _) }
        }
    )*};
}
from_int!(i8 => I64, i16 => I64, i32 => I64, i64 => I64, isize => I64, u8 => U64, u16 => U64, u32 => U64, u64 => U64, usize => U64, f32 => F64, f64 => F64);

impl<'a> From<&'a str> for Value<'a> {
    #[inline]
    fn from(v: &'a str) -> Self {
        Value::Str(v)
    }
}

impl<'a> From<&'a String> for Value<'a> {
    #[inline]
    fn from(v: &'a String) -> Self {
        Value::Str(v)
    }
}

impl From<bool> for Value<'_> {
    #[inline]
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

impl<'a, T: Into<Value<'a>>> From<Option<T>> for Value<'a> {
    #[inline]
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}

/// Fields attached to one record.
#[derive(Clone, Copy)]
pub enum Fields<'a> {
    Kv(&'a [(&'a str, Value<'a>)]),
    /// A JSON object as text (`{"k":1}`); spliced as-is in JSON output.
    Json(&'a str),
}

/// Writes the part of the line that must be produced under the writer lock.
#[inline]
pub fn write_header(fmt: Format, ts: &mut TsFormatter, micros: i64, out: &mut Vec<u8>) {
    match fmt {
        Format::Text => {
            ts.write(micros, out);
            out.push(b' ');
        }
        Format::Json => {
            out.extend_from_slice(b"{\"ts\":\"");
            ts.write(micros, out);
            out.extend_from_slice(b"\",");
        }
    }
}

/// Encodes everything after the header, including the trailing newline.
pub fn encode_body(fmt: Format, out: &mut Vec<u8>, level: Level, name: &str, msg: &str, bound: &[u8], fields: Fields) {
    match fmt {
        Format::Text => {
            out.extend_from_slice(level.padded().as_bytes());
            out.push(b' ');
            write_name(out, name);
            out.extend_from_slice(b": ");
            write_text_msg(out, msg);
            let tab = out.len();
            out.push(b'\t');
            out.extend_from_slice(bound);
            match fields {
                Fields::Kv(kv) => {
                    for (k, v) in kv {
                        logfmt_kv(out, tab, "", k, *v);
                    }
                }
                Fields::Json(j) => json_to_logfmt(out, tab, j),
            }
            if out.len() == tab + 1 {
                out.truncate(tab);
            }
            out.push(b'\n');
        }
        Format::Json => {
            out.extend_from_slice(b"\"level\":\"");
            out.extend_from_slice(level.as_str().as_bytes());
            out.extend_from_slice(b"\",\"logger\":");
            write_json_str(out, name);
            out.extend_from_slice(b",\"msg\":");
            write_json_str(out, msg);
            out.extend_from_slice(bound);
            match fields {
                Fields::Kv(kv) => {
                    for (k, v) in kv {
                        out.push(b',');
                        write_json_str(out, k);
                        out.push(b':');
                        write_json_value(out, *v);
                    }
                }
                Fields::Json(j) => {
                    let inner = j.trim();
                    let inner = inner.strip_prefix('{').and_then(|s| s.strip_suffix('}')).map(str::trim);
                    match inner {
                        Some("") => {}
                        Some(inner) => {
                            out.push(b',');
                            out.extend_from_slice(inner.as_bytes());
                        }
                        // Not an object: keep it, but under its own key.
                        None if !j.trim().is_empty() => {
                            out.extend_from_slice(b",\"fields\":");
                            write_json_str(out, j);
                        }
                        None => {}
                    }
                }
            }
            out.extend_from_slice(b"}\n");
        }
    }
}

/// Pre-encodes fields bound to a child logger, ready to be spliced by [`encode_body`].
pub fn encode_bound(fmt: Format, fields: &[(&str, Value)]) -> Vec<u8> {
    let mut out = Vec::new();
    match fmt {
        Format::Text => {
            // `logfmt_kv` separates with a space unless right after the TAB marker.
            out.push(b'\t');
            for (k, v) in fields {
                logfmt_kv(&mut out, 0, "", k, *v);
            }
            out.remove(0);
        }
        Format::Json => {
            for (k, v) in fields {
                out.push(b',');
                write_json_str(&mut out, k);
                out.push(b':');
                write_json_value(&mut out, *v);
            }
        }
    }
    out
}

/// Joins two pre-encoded bound-field blocks.
pub fn join_bound(fmt: Format, a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut v = a.to_vec();
    if fmt == Format::Text && !a.is_empty() && !b.is_empty() {
        v.push(b' ');
    }
    v.extend_from_slice(b);
    v
}

fn write_name(out: &mut Vec<u8>, name: &str) {
    if name.is_empty() {
        out.push(b'-');
        return;
    }
    for &b in name.as_bytes() {
        out.push(if b <= b' ' || b == 0x7f { b'_' } else { b });
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Message text: keeps the line single-line by escaping control characters.
fn write_text_msg(out: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    let mut start = 0;
    for (i, &c) in b.iter().enumerate() {
        if c >= 0x20 && c != 0x7f {
            continue;
        }
        out.extend_from_slice(&b[start..i]);
        match c {
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            _ => out.extend_from_slice(&[b'\\', b'x', HEX[(c >> 4) as usize], HEX[(c & 15) as usize]]),
        }
        start = i + 1;
    }
    out.extend_from_slice(&b[start..]);
}

/// Writes `s` as a JSON string literal.
pub fn write_json_str(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    let b = s.as_bytes();
    let mut start = 0;
    for (i, &c) in b.iter().enumerate() {
        let esc: &[u8] = match c {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0..=0x1f => b"",
            _ => continue,
        };
        out.extend_from_slice(&b[start..i]);
        if esc.is_empty() {
            out.extend_from_slice(&[b'\\', b'u', b'0', b'0', HEX[(c >> 4) as usize], HEX[(c & 15) as usize]]);
        } else {
            out.extend_from_slice(esc);
        }
        start = i + 1;
    }
    out.extend_from_slice(&b[start..]);
    out.push(b'"');
}

#[inline]
fn write_u64(out: &mut Vec<u8>, mut n: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[i..]);
}

#[inline]
fn write_i64(out: &mut Vec<u8>, n: i64) {
    if n < 0 {
        out.push(b'-');
    }
    write_u64(out, n.unsigned_abs());
}

struct VecWriter<'a>(&'a mut Vec<u8>);

impl fmt::Write for VecWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

fn write_f64(out: &mut Vec<u8>, f: f64) {
    if !f.is_finite() {
        out.extend_from_slice(b"null");
    } else if f.fract() == 0.0 && f.abs() < 1e15 {
        write_i64(out, f as i64);
    } else {
        let _ = fmt::write(&mut VecWriter(out), format_args!("{f}"));
    }
}

fn write_json_value(out: &mut Vec<u8>, v: Value) {
    match v {
        Value::Str(s) => write_json_str(out, s),
        Value::I64(n) => write_i64(out, n),
        Value::U64(n) => write_u64(out, n),
        Value::F64(f) => write_f64(out, f),
        Value::Bool(b) => out.extend_from_slice(if b { b"true" } else { b"false" }),
        Value::Null => out.extend_from_slice(b"null"),
        Value::Json(j) if j.trim().is_empty() => out.extend_from_slice(b"null"),
        Value::Json(j) => out.extend_from_slice(j.trim().as_bytes()),
    }
}

fn logfmt_needs_quotes(s: &str) -> bool {
    s.is_empty() || s.bytes().any(|c| c <= b' ' || c == b'=' || c == b'"' || c == 0x7f)
}

/// Writes a logfmt value, quoting and escaping when required.
pub fn write_logfmt_value(out: &mut Vec<u8>, s: &str) {
    if !logfmt_needs_quotes(s) {
        out.extend_from_slice(s.as_bytes());
        return;
    }
    out.push(b'"');
    let b = s.as_bytes();
    let mut start = 0;
    for (i, &c) in b.iter().enumerate() {
        let esc: &[u8] = match c {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0..=0x1f | 0x7f => b"",
            _ => continue,
        };
        out.extend_from_slice(&b[start..i]);
        if esc.is_empty() {
            out.extend_from_slice(&[b'\\', b'x', HEX[(c >> 4) as usize], HEX[(c & 15) as usize]]);
        } else {
            out.extend_from_slice(esc);
        }
        start = i + 1;
    }
    out.extend_from_slice(&b[start..]);
    out.push(b'"');
}

fn write_logfmt_key(out: &mut Vec<u8>, prefix: &str, key: &str) {
    for &b in prefix.as_bytes().iter().chain(key.as_bytes()) {
        out.push(if b <= b' ' || b == b'=' || b == b'"' || b == 0x7f { b'_' } else { b });
    }
    if prefix.is_empty() && key.is_empty() {
        out.push(b'_');
    }
}

/// `tab` is the offset of the TAB that starts the field list; no separator is
/// written for the first field.
fn logfmt_kv(out: &mut Vec<u8>, tab: usize, prefix: &str, key: &str, v: Value) {
    if out.len() > tab + 1 {
        out.push(b' ');
    }
    write_logfmt_key(out, prefix, key);
    out.push(b'=');
    match v {
        Value::Str(s) => write_logfmt_value(out, s),
        Value::I64(n) => write_i64(out, n),
        Value::U64(n) => write_u64(out, n),
        Value::F64(f) => write_f64(out, f),
        Value::Bool(b) => out.extend_from_slice(if b { b"true" } else { b"false" }),
        Value::Null => out.extend_from_slice(b"null"),
        Value::Json(j) => write_logfmt_value(out, j.trim()),
    }
}

/// A JSON string that borrows from the input when it has no escapes.
struct JStr<'de>(Cow<'de, str>);

impl<'de> de::Deserialize<'de> for JStr<'de> {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = JStr<'de>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string")
            }
            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<JStr<'de>, E> {
                Ok(JStr(Cow::Borrowed(v)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<JStr<'de>, E> {
                Ok(JStr(Cow::Owned(v.to_owned())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<JStr<'de>, E> {
                Ok(JStr(Cow::Owned(v)))
            }
        }
        d.deserialize_str(V)
    }
}

/// Streams a JSON object into logfmt pairs, preserving key order and
/// flattening nested objects into dotted keys (`user.id=7`).
struct LogfmtVisitor<'o> {
    out: &'o mut Vec<u8>,
    tab: usize,
    prefix: String,
}

impl<'de> Visitor<'de> for LogfmtVisitor<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(JStr(key)) = map.next_key::<JStr<'de>>()? {
            let raw: &'de RawValue = map.next_value()?;
            let raw = raw.get();
            match raw.as_bytes().first() {
                Some(b'"') => {
                    let s = serde_json::from_str::<JStr>(raw).map(|s| s.0).unwrap_or(Cow::Borrowed(raw));
                    logfmt_kv(self.out, self.tab, &self.prefix, &key, Value::Str(&s));
                }
                Some(b'{') => {
                    let prefix = format!("{}{}.", self.prefix, key);
                    let nested = LogfmtVisitor { out: &mut *self.out, tab: self.tab, prefix };
                    serde_json::Deserializer::from_str(raw).deserialize_map(nested).map_err(de::Error::custom)?;
                }
                _ => logfmt_kv(self.out, self.tab, &self.prefix, &key, Value::Json(raw)),
            }
        }
        Ok(())
    }
}

fn json_to_logfmt(out: &mut Vec<u8>, tab: usize, json: &str) {
    let json = json.trim();
    if json.is_empty() || json == "{}" {
        return;
    }
    let mark = out.len();
    if fast_object(out, tab, "", json).is_some() {
        return;
    }
    out.truncate(mark);
    let v = LogfmtVisitor { out: &mut *out, tab, prefix: String::new() };
    if serde_json::Deserializer::from_str(json).deserialize_map(v).is_err() {
        out.truncate(mark);
        logfmt_kv(out, tab, "", "fields", Value::Str(json));
    }
}

/// Single-pass scanner over a JSON object (as produced by `JSON.stringify`),
/// borrowing every key and value. Returns `None` on anything unexpected; the
/// caller then falls back to serde.
fn fast_object(out: &mut Vec<u8>, tab: usize, prefix: &str, json: &str) -> Option<()> {
    let b = json.as_bytes();
    let mut s = Scan { b, i: 0 };
    s.ws();
    s.eat(b'{')?;
    s.ws();
    if s.peek()? == b'}' {
        s.i += 1;
    } else {
        loop {
            s.ws();
            let (k, k_esc) = s.string()?;
            s.ws();
            s.eat(b':')?;
            s.ws();
            let (vs, ve) = s.value()?;
            let key = if k_esc { Cow::Owned(unescape_json(&json[k.0..k.1])?) } else { Cow::Borrowed(&json[k.0..k.1]) };
            let raw = &json[vs..ve];
            match b[vs] {
                b'"' => {
                    let inner = &raw[1..raw.len() - 1];
                    let v = if inner.contains('\\') { Cow::Owned(unescape_json(inner)?) } else { Cow::Borrowed(inner) };
                    logfmt_kv(out, tab, prefix, &key, Value::Str(&v));
                }
                b'{' => fast_object(out, tab, &format!("{prefix}{key}."), raw)?,
                _ => logfmt_kv(out, tab, prefix, &key, Value::Json(raw)),
            }
            s.ws();
            match s.peek()? {
                b',' => s.i += 1,
                b'}' => {
                    s.i += 1;
                    break;
                }
                _ => return None,
            }
        }
    }
    s.ws();
    (s.i == b.len()).then_some(())
}

struct Scan<'a> {
    b: &'a [u8],
    i: usize,
}

impl Scan<'_> {
    #[inline]
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\n' | b'\r' | b'\t') {
            self.i += 1;
        }
    }

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    #[inline]
    fn eat(&mut self, c: u8) -> Option<()> {
        (self.peek()? == c).then(|| self.i += 1)
    }

    /// At a quote: returns the content span and whether it has escapes.
    fn string(&mut self) -> Option<((usize, usize), bool)> {
        self.eat(b'"')?;
        let start = self.i;
        let mut esc = false;
        loop {
            match *self.b.get(self.i)? {
                b'"' => {
                    self.i += 1;
                    return Some(((start, self.i - 1), esc));
                }
                b'\\' => {
                    esc = true;
                    self.i += 2;
                }
                _ => self.i += 1,
            }
        }
    }

    /// Skips one value and returns its span.
    fn value(&mut self) -> Option<(usize, usize)> {
        let start = self.i;
        match self.peek()? {
            b'"' => {
                self.string()?;
            }
            b'{' | b'[' => {
                let mut depth = 0usize;
                loop {
                    match *self.b.get(self.i)? {
                        b'"' => {
                            self.string()?;
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                self.i += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    self.i += 1;
                }
            }
            _ => {
                while self.i < self.b.len()
                    && !matches!(self.b[self.i], b',' | b'}' | b']' | b' ' | b'\n' | b'\r' | b'\t')
                {
                    self.i += 1;
                }
                if self.i == start {
                    return None;
                }
            }
        }
        Some((start, self.i))
    }
}

fn unescape_json(inner: &str) -> Option<String> {
    let mut q = String::with_capacity(inner.len() + 2);
    q.push('"');
    q.push_str(inner);
    q.push('"');
    serde_json::from_str(&q).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::Tz;

    fn line(fmt: Format, level: Level, msg: &str, bound: &[u8], fields: Fields) -> String {
        let mut ts = TsFormatter::new(Tz::Utc);
        let mut out = Vec::new();
        write_header(fmt, &mut ts, 1_760_531_696_000_001, &mut out);
        encode_body(fmt, &mut out, level, "api", msg, bound, fields);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn text_line() {
        let l = line(
            Format::Text,
            Level::Info,
            "hello\nworld",
            b"",
            Fields::Kv(&[("method", "GET".into()), ("status", 200.into()), ("q", "a b".into()), ("ok", true.into())]),
        );
        assert_eq!(
            l,
            "2025-10-15T12:34:56.000001Z INFO  api: hello\\nworld\tmethod=GET status=200 q=\"a b\" ok=true\n"
        );
        let bare = line(Format::Text, Level::Warn, "x", b"", Fields::Kv(&[]));
        assert_eq!(bare, "2025-10-15T12:34:56.000001Z WARN  api: x\n");
    }

    #[test]
    fn text_from_json_fields() {
        let bound = encode_bound(Format::Text, &[("svc", "billing".into())]);
        let l = line(
            Format::Text,
            Level::Error,
            "boom",
            &bound,
            Fields::Json(r#"{"user":{"id":7,"name":"Al \"B\""},"tags":["a","b"],"dur_ms":1.5,"z":null}"#),
        );
        assert_eq!(
            l,
            "2025-10-15T12:34:56.000001Z ERROR api: boom\tsvc=billing user.id=7 user.name=\"Al \\\"B\\\"\" tags=\"[\\\"a\\\",\\\"b\\\"]\" dur_ms=1.5 z=null\n"
        );
    }

    #[test]
    fn json_line() {
        let bound = encode_bound(Format::Json, &[("svc", "billing".into())]);
        let l = line(Format::Json, Level::Error, "a \"q\"\n", &bound, Fields::Json(r#" {"status":500} "#));
        assert_eq!(
            l,
            "{\"ts\":\"2025-10-15T12:34:56.000001Z\",\"level\":\"error\",\"logger\":\"api\",\"msg\":\"a \\\"q\\\"\\n\",\"svc\":\"billing\",\"status\":500}\n"
        );
        let v: serde_json::Value = serde_json::from_str(&l).unwrap();
        assert_eq!(v["status"], 500);
        let kv = line(
            Format::Json,
            Level::Info,
            "m",
            b"",
            Fields::Kv(&[("f", 2.5.into()), ("n", (-3).into()), ("big", 10.0.into())]),
        );
        assert!(kv.ends_with(",\"f\":2.5,\"n\":-3,\"big\":10}\n"), "{kv}");
    }

    #[test]
    fn fast_path_matches_serde_path() {
        let cases = [
            r#"{"a":1,"b":"x y","c":true,"d":null,"e":[1,"two",{"x":3}],"f":{"g":{"h":"deep"}},"i":-1.5e3}"#,
            r#"{"esc":"line\nbreak \"q\" \u00e9\ud83d\ude00","k\"ey":"v","empty":"","eq":"a=b"}"#,
            r#" { "spaced" : 1 , "x" : { } , "arr" : [ ] } "#,
            r#"{"unicode":"✓ ü 日本","n":0}"#,
        ];
        for c in cases {
            let mut fast = vec![b'\t'];
            assert!(fast_object(&mut fast, 0, "", c).is_some(), "fast path rejected {c}");
            let mut slow = vec![b'\t'];
            let v = LogfmtVisitor { out: &mut slow, tab: 0, prefix: String::new() };
            serde_json::Deserializer::from_str(c).deserialize_map(v).unwrap();
            assert_eq!(String::from_utf8(fast).unwrap(), String::from_utf8(slow).unwrap(), "{c}");
        }
        for bad in [r#"{"a":}"#, r#"{"a":1"#, r#"{"a" 1}"#, r#"{"a":1}x"#, "[1]"] {
            assert!(fast_object(&mut Vec::new(), 0, "", bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn invalid_json_fields_are_kept() {
        let l = line(Format::Text, Level::Info, "m", b"", Fields::Json("{oops"));
        assert!(l.ends_with("\tfields={oops\n"), "{l}");
    }
}
