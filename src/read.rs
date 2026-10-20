//! Reading logs back: parse lines of either format, filter, and walk a
//! stream's files (archives incl. `.gz`, then the active file) in time order.

use std::borrow::Cow;
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::de::{self, Deserializer as _, MapAccess, Visitor};
use serde_json::value::RawValue;

use crate::compress::open_maybe_gz;
use crate::level::Level;
use crate::rotation::Paths;
use crate::time::parse_rfc3339;

/// A parsed log line. Lines that match neither format keep the whole text in `msg`.
#[derive(Clone, Debug, Default)]
pub struct Record {
    pub ts: Option<i64>,
    pub level: Option<Level>,
    pub logger: String,
    pub msg: String,
    /// Flattened fields; JSON strings unescaped, other JSON values as raw text.
    pub fields: Vec<(String, String)>,
}

impl Record {
    pub fn parse(line: &str) -> Record {
        let line = line.trim_end_matches(['\n', '\r']);
        if line.starts_with('{') {
            if let Some(r) = parse_json_line(line) {
                return r;
            }
        } else if let Some(r) = parse_text_line(line) {
            return r;
        }
        Record { msg: line.to_string(), ..Record::default() }
    }

    pub fn field(&self, key: &str) -> Option<&str> {
        match key {
            "level" => self.level.map(Level::as_str),
            "logger" => Some(&self.logger),
            "msg" | "message" => Some(&self.msg),
            _ => self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()),
        }
    }

    /// Normalized JSON object (`ts`, `level`, `logger`, `msg`, then fields).
    pub fn to_json(&self, tz: crate::time::Tz) -> String {
        let mut out = Vec::with_capacity(256);
        out.push(b'{');
        let mut first = true;
        let mut kv = |out: &mut Vec<u8>, k: &str, v: &str, raw: bool| {
            if !first {
                out.push(b',');
            }
            first = false;
            crate::format::write_json_str(out, k);
            out.push(b':');
            if raw && serde_json::from_str::<de::IgnoredAny>(v).is_ok() {
                out.extend_from_slice(v.as_bytes());
            } else {
                crate::format::write_json_str(out, v);
            }
        };
        if let Some(ts) = self.ts {
            kv(&mut out, "ts", &crate::time::format_rfc3339(ts, tz), false);
        }
        if let Some(l) = self.level {
            kv(&mut out, "level", l.as_str(), false);
        }
        if !self.logger.is_empty() {
            kv(&mut out, "logger", &self.logger, false);
        }
        kv(&mut out, "msg", &self.msg, false);
        for (k, v) in &self.fields {
            let numeric = v.parse::<f64>().is_ok()
                || matches!(v.as_str(), "true" | "false" | "null")
                || v.starts_with(['[', '{']);
            kv(&mut out, k, v, numeric);
        }
        out.push(b'}');
        String::from_utf8(out).unwrap_or_default()
    }
}

fn parse_text_line(line: &str) -> Option<Record> {
    let (ts, rest) = line.split_once(' ')?;
    let ts = parse_rfc3339(ts)?;
    let (lvl, rest) = rest.split_once(' ')?;
    let level = Level::parse(lvl);
    let rest = rest.trim_start_matches(' ');
    let (logger, rest) = rest.split_once(": ").or_else(|| rest.strip_suffix(':').map(|l| (l, "")))?;
    let (msg, fields_part) = rest.split_once('\t').unwrap_or((rest, ""));
    let mut fields = Vec::new();
    parse_logfmt(fields_part, &mut fields);
    Some(Record { ts: Some(ts), level, logger: logger.to_string(), msg: unescape_msg(msg).into_owned(), fields })
}

fn unescape_msg(s: &str) -> Cow<'_, str> {
    if !s.contains('\\') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.peek() {
            Some('n') => {
                it.next();
                out.push('\n');
            }
            Some('r') => {
                it.next();
                out.push('\r');
            }
            Some('t') => {
                it.next();
                out.push('\t');
            }
            _ => out.push('\\'),
        }
    }
    Cow::Owned(out)
}

/// Parses `key=value key2="quoted value"` pairs.
pub fn parse_logfmt(s: &str, out: &mut Vec<(String, String)>) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && b[i] == b' ' {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let ks = i;
        while i < b.len() && b[i] != b'=' && b[i] != b' ' {
            i += 1;
        }
        let key = &s[ks..i];
        if i >= b.len() || b[i] == b' ' {
            out.push((key.to_string(), "true".into()));
            continue;
        }
        i += 1; // '='
        if i < b.len() && b[i] == b'"' {
            i += 1;
            let mut v = Vec::new();
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' && i + 1 < b.len() {
                    i += 1;
                    match b[i] {
                        b'n' => v.push(b'\n'),
                        b'r' => v.push(b'\r'),
                        b't' => v.push(b'\t'),
                        b'x' if i + 2 < b.len() => {
                            let h =
                                std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                            match h {
                                Some(h) => {
                                    v.push(h);
                                    i += 2;
                                }
                                None => v.extend_from_slice(b"\\x"),
                            }
                        }
                        c => v.push(c),
                    }
                } else {
                    v.push(b[i]);
                }
                i += 1;
            }
            i += 1; // closing quote
            out.push((key.to_string(), String::from_utf8_lossy(&v).into_owned()));
        } else {
            let vs = i;
            while i < b.len() && b[i] != b' ' {
                i += 1;
            }
            out.push((key.to_string(), s[vs..i].to_string()));
        }
    }
}

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
        }
        d.deserialize_str(V)
    }
}

struct FlattenVisitor<'o> {
    out: &'o mut Vec<(String, String)>,
    prefix: String,
}

impl<'de> Visitor<'de> for FlattenVisitor<'_> {
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
                    let v =
                        serde_json::from_str::<JStr>(raw).map(|s| s.0.into_owned()).unwrap_or_else(|_| raw.to_string());
                    self.out.push((format!("{}{key}", self.prefix), v));
                }
                Some(b'{') if raw.len() > 2 => {
                    let nested = FlattenVisitor { out: &mut *self.out, prefix: format!("{}{key}.", self.prefix) };
                    serde_json::Deserializer::from_str(raw).deserialize_map(nested).map_err(de::Error::custom)?;
                }
                _ => self.out.push((format!("{}{key}", self.prefix), raw.to_string())),
            }
        }
        Ok(())
    }
}

/// Flattens a JSON object into ordered `(dotted.key, value)` pairs.
pub fn flatten_json(json: &str) -> Option<Vec<(String, String)>> {
    let mut out = Vec::new();
    let v = FlattenVisitor { out: &mut out, prefix: String::new() };
    serde_json::Deserializer::from_str(json).deserialize_map(v).ok()?;
    Some(out)
}

fn parse_json_line(line: &str) -> Option<Record> {
    let mut fields = flatten_json(line)?;
    let mut r = Record::default();
    let mut take = |names: &[&str]| -> Option<String> {
        let i = fields.iter().position(|(k, _)| names.contains(&k.as_str()))?;
        Some(fields.remove(i).1)
    };
    r.ts = take(&["ts", "time", "timestamp", "@timestamp"]).and_then(|t| {
        parse_rfc3339(&t).or_else(|| {
            // Epoch milliseconds (pino, Date.now()).
            t.parse::<f64>().ok().map(|ms| (ms * 1000.0) as i64)
        })
    });
    r.level = take(&["level", "severity", "lvl"]).and_then(|l| Level::parse(&l));
    r.logger = take(&["logger", "name", "target"]).unwrap_or_default();
    r.msg = take(&["msg", "message"]).unwrap_or_default();
    r.fields = fields;
    Some(r)
}

/// Comparison in a `--where` condition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    Contains,
}

#[derive(Clone, Debug)]
pub struct Cond {
    pub key: String,
    pub op: Op,
    pub value: String,
}

impl Cond {
    /// Parses `key=value`, `key!=v`, `key>=n`, `key<n`, `key~substring`.
    pub fn parse(s: &str) -> Result<Cond, String> {
        let i =
            s.find(['=', '!', '<', '>', '~']).ok_or_else(|| format!("invalid condition '{s}' (expected key=value)"))?;
        let (key, rest) = s.split_at(i);
        let (op, value) = if let Some(v) = rest.strip_prefix("!=") {
            (Op::Ne, v)
        } else if let Some(v) = rest.strip_prefix(">=") {
            (Op::Ge, v)
        } else if let Some(v) = rest.strip_prefix("<=") {
            (Op::Le, v)
        } else if let Some(v) = rest.strip_prefix("==") {
            (Op::Eq, v)
        } else if let Some(v) = rest.strip_prefix('=') {
            (Op::Eq, v)
        } else if let Some(v) = rest.strip_prefix('>') {
            (Op::Gt, v)
        } else if let Some(v) = rest.strip_prefix('<') {
            (Op::Lt, v)
        } else if let Some(v) = rest.strip_prefix('~') {
            (Op::Contains, v)
        } else {
            return Err(format!("invalid condition '{s}'"));
        };
        if key.is_empty() {
            return Err(format!("invalid condition '{s}' (missing key)"));
        }
        Ok(Cond { key: key.to_string(), op, value: value.to_string() })
    }

    pub fn matches(&self, r: &Record) -> bool {
        let Some(v) = r.field(&self.key) else { return self.op == Op::Ne };
        let pattern = self.value.as_bytes();
        // `status=5xx` style class match.
        if pattern.len() == 3 && pattern[1..].eq_ignore_ascii_case(b"xx") && matches!(self.op, Op::Eq | Op::Ne) {
            let hit = v.len() == 3 && v.as_bytes()[0] == pattern[0];
            return hit == (self.op == Op::Eq);
        }
        let ord = match (v.parse::<f64>(), self.value.parse::<f64>()) {
            (Ok(a), Ok(b)) => a.partial_cmp(&b),
            _ => Some(v.cmp(self.value.as_str())),
        };
        use std::cmp::Ordering::*;
        match self.op {
            Op::Eq => ord == Some(Equal),
            Op::Ne => ord != Some(Equal),
            Op::Gt => ord == Some(Greater),
            Op::Ge => matches!(ord, Some(Greater | Equal)),
            Op::Lt => ord == Some(Less),
            Op::Le => matches!(ord, Some(Less | Equal)),
            Op::Contains => v.to_lowercase().contains(&self.value.to_lowercase()),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub min_level: Option<Level>,
    pub logger: Option<String>,
    /// Case-insensitive substring of the raw line.
    pub grep: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub conds: Vec<Cond>,
}

impl Filter {
    pub fn is_empty(&self) -> bool {
        self.min_level.is_none()
            && self.logger.is_none()
            && self.grep.is_none()
            && self.since.is_none()
            && self.until.is_none()
            && self.conds.is_empty()
    }

    /// Cheap pre-check on the raw line before parsing.
    pub fn line_may_match(&self, line: &str) -> bool {
        match &self.grep {
            Some(g) => contains_ci(line, g),
            None => true,
        }
    }

    pub fn matches(&self, r: &Record) -> bool {
        if let Some(min) = self.min_level
            && r.level.is_none_or(|l| l < min)
        {
            return false;
        }
        if let Some(l) = &self.logger
            && &r.logger != l
        {
            return false;
        }
        if let Some(s) = self.since
            && r.ts.is_none_or(|t| t < s)
        {
            return false;
        }
        if let Some(u) = self.until
            && r.ts.is_none_or(|t| t > u)
        {
            return false;
        }
        self.conds.iter().all(|c| c.matches(r))
    }
}

fn contains_ci(hay: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    let h = hay.as_bytes();
    let n = needle_lower.as_bytes();
    h.windows(n.len()).any(|w| w.iter().zip(n).all(|(a, b)| a.to_ascii_lowercase() == *b))
}

/// Files of a stream that may hold records in `[since, until]`, oldest first,
/// ending with the active file.
pub fn stream_files(paths: &Paths, since: Option<i64>, until: Option<i64>) -> Vec<PathBuf> {
    let archives = paths.archives();
    let starts: Vec<Option<i64>> = archives.iter().map(|a| a.start_micros(paths.tz)).collect();
    let mut out = Vec::new();
    for (i, a) in archives.iter().enumerate() {
        let start = starts[i];
        let next = starts.get(i + 1).copied().flatten();
        if let (Some(s), Some(n)) = (since, next)
            && n < s
        {
            continue;
        }
        if let (Some(u), Some(st)) = (until, start)
            && st > u
        {
            continue;
        }
        out.push(a.path.clone());
    }
    let active = paths.active();
    if active.exists() {
        out.push(active);
    }
    out
}

/// Calls `f` for every line of `path` (plain or gzip). Stops early when `f` returns false.
pub fn for_each_line(path: &Path, f: &mut dyn FnMut(&str) -> bool) -> io::Result<bool> {
    let mut r = open_maybe_gz(path)?;
    let mut buf = Vec::with_capacity(1024);
    loop {
        buf.clear();
        if r.read_until(b'\n', &mut buf)? == 0 {
            return Ok(true);
        }
        let line = String::from_utf8_lossy(&buf);
        if !f(line.trim_end_matches(['\n', '\r'])) {
            return Ok(false);
        }
    }
}

/// Last `n` lines of a plain file accepted by `keep`, read backwards in chunks.
pub fn tail_lines(path: &Path, n: usize, keep: &dyn Fn(&str) -> bool) -> io::Result<(Vec<String>, u64)> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut out: Vec<String> = Vec::new();
    if n == 0 {
        return Ok((out, len));
    }
    const CHUNK: u64 = 256 * 1024;
    let mut end = len;
    let mut carry: Vec<u8> = Vec::new();
    while end > 0 && out.len() < n {
        let start = end.saturating_sub(CHUNK);
        let mut chunk = vec![0u8; (end - start) as usize];
        f.seek(SeekFrom::Start(start))?;
        f.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&carry);
        let mut lines: Vec<&[u8]> = chunk.split(|&b| b == b'\n').collect();
        // The first piece may be partial unless we reached the file start.
        carry = if start > 0 { lines.remove(0).to_vec() } else { Vec::new() };
        for l in lines.into_iter().rev() {
            if l.is_empty() {
                continue;
            }
            let s = String::from_utf8_lossy(l);
            if keep(&s) {
                out.push(s.into_owned());
                if out.len() >= n {
                    break;
                }
            }
        }
        end = start;
    }
    if end == 0 && out.len() < n && !carry.is_empty() {
        let s = String::from_utf8_lossy(&carry);
        if keep(&s) {
            out.push(s.into_owned());
        }
    }
    out.reverse();
    Ok((out, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_lines() {
        let r = Record::parse(
            "2025-10-15T12:34:56.000001Z ERROR api:db: boom\\nline2\tmethod=GET status=500 q=\"a \\\"b\\\"\" flag",
        );
        assert_eq!(r.level, Some(Level::Error));
        assert_eq!(r.logger, "api:db");
        assert_eq!(r.msg, "boom\nline2");
        assert_eq!(r.field("status"), Some("500"));
        assert_eq!(r.field("q"), Some("a \"b\""));
        assert_eq!(r.field("flag"), Some("true"));
        assert_eq!(r.ts, Some(1_760_531_696_000_001));
    }

    #[test]
    fn parses_json_lines() {
        let r = Record::parse(
            r#"{"ts":"2025-10-15T12:34:56.000001Z","level":"warn","logger":"api","msg":"slow","user":{"id":7},"dur_ms":812.5}"#,
        );
        assert_eq!(r.level, Some(Level::Warn));
        assert_eq!(r.field("user.id"), Some("7"));
        assert_eq!(r.field("dur_ms"), Some("812.5"));
        let pino = Record::parse(r#"{"level":50,"time":1760531696000,"msg":"x"}"#);
        assert_eq!(pino.level, Some(Level::Error));
        assert_eq!(pino.ts, Some(1_760_531_696_000_000));
    }

    #[test]
    fn foreign_lines_survive() {
        let r = Record::parse("plain old line");
        assert_eq!(r.msg, "plain old line");
        assert_eq!(r.level, None);
    }

    #[test]
    fn conditions() {
        let r = Record::parse("2025-10-15T12:34:56Z WARN  api: x\tstatus=404 dur_ms=12.5 user=alice");
        assert!(Cond::parse("status=4xx").unwrap().matches(&r));
        assert!(!Cond::parse("status=5xx").unwrap().matches(&r));
        assert!(Cond::parse("dur_ms>10").unwrap().matches(&r));
        assert!(!Cond::parse("dur_ms>=13").unwrap().matches(&r));
        assert!(Cond::parse("user~ALI").unwrap().matches(&r));
        assert!(Cond::parse("user!=bob").unwrap().matches(&r));
        assert!(Cond::parse("missing!=x").unwrap().matches(&r));
        assert!(Cond::parse("level=warn").unwrap().matches(&r));
    }
}
