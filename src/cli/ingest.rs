//! `rotlog pipe` and `rotlog run`: turn arbitrary output lines into records.

use std::borrow::Cow;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Instant;

use rotlog::level::Level;
use rotlog::{Logger, Value};

#[derive(Clone)]
pub struct IngestOpts {
    pub logger: Option<String>,
    pub detect: bool,
    pub keep_ansi: bool,
    pub tee: bool,
}

/// Finds a level in the first part of a line: an uppercase token (`ERROR`,
/// `[WARN]`, `W1015` is not matched), `level=debug`, or a lowercase token in brackets.
pub fn detect_level(line: &str) -> Option<Level> {
    let mut end = line.len().min(96);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    let head = &line[..end];
    for key in ["level=", "lvl=", "severity="] {
        if let Some(i) = head.find(key) {
            let v: String = head[i + key.len()..]
                .trim_start_matches('"')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if let Some(l) = Level::parse(&v) {
                return Some(l);
            }
        }
    }
    let b = head.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let s = i;
        while i < b.len() && b[i].is_ascii_alphanumeric() {
            i += 1;
        }
        let tok = &head[s..i];
        if !(3..=8).contains(&tok.len()) {
            continue;
        }
        let upper = tok.bytes().all(|c| c.is_ascii_uppercase());
        let bracketed = s > 0 && b[s - 1] == b'[' && b.get(i) == Some(&b']');
        if (upper || bracketed)
            && let Some(l) = Level::parse(tok)
            && l != Level::Off
            && !tok.eq_ignore_ascii_case("log")
            && !tok.eq_ignore_ascii_case("none")
        {
            return Some(l);
        }
    }
    None
}

/// Removes ANSI escape sequences (colors, cursor moves, OSC titles/links).
pub fn strip_ansi(s: &str) -> Cow<'_, str> {
    if !s.contains('\x1b') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match it.peek() {
            Some('[') => {
                it.next();
                for c in it.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                it.next();
                while let Some(c) = it.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' && it.peek() == Some(&'\\') {
                        it.next();
                        break;
                    }
                }
            }
            Some(_) => {
                it.next();
            }
            None => {}
        }
    }
    Cow::Owned(out)
}

/// Logs one input line, honoring JSON lines (level/msg/fields) and level detection.
pub fn ingest_line(log: &Logger, opts: &IngestOpts, default: Level, raw: &str, extra: &[(&str, Value)]) {
    let line = if opts.keep_ansi { Cow::Borrowed(raw) } else { strip_ansi(raw) };
    let line = line.trim_end_matches(['\n', '\r']);
    if line.is_empty() {
        return;
    }
    let name = opts.logger.as_deref();
    if line.starts_with('{')
        && let Ok(mut map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(line)
    {
        let level = ["level", "severity", "lvl"]
            .iter()
            .find_map(|k| map.remove(*k))
            .and_then(|v| match v {
                serde_json::Value::String(s) => Level::parse(&s),
                serde_json::Value::Number(n) => n.as_u64().map(|n| Level::from_number(n as u32)),
                _ => None,
            })
            .unwrap_or(default);
        let msg = ["msg", "message"]
            .iter()
            .find_map(|k| map.remove(*k))
            .map(|v| match v {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            })
            .unwrap_or_default();
        if let Some(t) = ["time", "ts", "timestamp"].iter().find_map(|k| map.remove(*k)) {
            map.insert("src_time".into(), t);
        }
        let logger =
            ["logger", "name"].iter().find_map(|k| map.remove(*k)).and_then(|v| v.as_str().map(str::to_string));
        for (k, v) in extra {
            map.insert((*k).into(), value_to_json(*v));
        }
        let fields = serde_json::to_string(&map).unwrap_or_default();
        log.log_json(level, name.or(logger.as_deref()), &msg, &fields);
        return;
    }
    let level = if opts.detect { detect_level(line).unwrap_or(default) } else { default };
    match name {
        Some(n) => log.named(n).log(level, line, extra),
        None => log.log(level, line, extra),
    }
}

fn value_to_json(v: Value) -> serde_json::Value {
    match v {
        Value::Str(s) => s.into(),
        Value::I64(n) => n.into(),
        Value::U64(n) => n.into(),
        Value::F64(f) => f.into(),
        Value::Bool(b) => b.into(),
        Value::Null => serde_json::Value::Null,
        Value::Json(j) => serde_json::from_str(j).unwrap_or(serde_json::Value::Null),
    }
}

fn read_lines(mut r: impl BufRead, mut f: impl FnMut(&str)) -> io::Result<()> {
    let mut buf = Vec::with_capacity(4096);
    loop {
        buf.clear();
        if r.read_until(b'\n', &mut buf)? == 0 {
            return Ok(());
        }
        f(&String::from_utf8_lossy(&buf));
    }
}

/// `app | rotlog pipe`: returns when stdin closes.
pub fn pipe(log: Logger, opts: IngestOpts, default: Level) -> i32 {
    let interrupted = Arc::new(AtomicBool::new(false));
    {
        let log = log.clone();
        let interrupted = Arc::clone(&interrupted);
        signals::spawn(move |sig| match sig {
            signals::HUP => {
                let _ = log.rotate();
            }
            signals::INT if !interrupted.swap(true, Ordering::SeqCst) => {
                // The upstream process got the same Ctrl-C; drain until EOF.
            }
            s => {
                let _ = log.flush();
                std::process::exit(128 + s);
            }
        });
    }
    let stdin = io::stdin();
    let mut out = io::stdout();
    let r = read_lines(BufReader::with_capacity(1 << 16, stdin.lock()), |line| {
        if opts.tee {
            let _ = out.write_all(line.as_bytes());
        }
        ingest_line(&log, &opts, default, line, &[]);
    });
    if let Err(e) = r {
        eprintln!("rotlog: reading stdin: {e}");
    }
    log.close();
    0
}

/// `rotlog run -- cmd args…`: logs both streams, forwards signals, returns the child's exit code.
pub fn run(log: Logger, opts: IngestOpts, cmd: &[String], out_level: Level, err_level: Level) -> i32 {
    let mut command = Command::new(&cmd[0]);
    command.args(&cmd[1..]).stdin(Stdio::inherit()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group: a terminal Ctrl-C reaches only us and is forwarded once.
        command.process_group(0);
    }
    let started = Instant::now();
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            log.error(
                "failed to start process",
                &[("cmd", cmd.join(" ").as_str().into()), ("error", e.to_string().as_str().into())],
            );
            log.close();
            eprintln!("rotlog: {}: {e}", cmd[0]);
            return 127;
        }
    };
    let pid = child.id();
    let cmdline = cmd.join(" ");
    log.named("rotlog").info("process started", &[("pid", pid.into()), ("cmd", cmdline.as_str().into())]);

    let child_pid = Arc::new(AtomicI32::new(pid as i32));
    {
        let log = log.clone();
        let child_pid = Arc::clone(&child_pid);
        signals::spawn(move |sig| {
            if sig == signals::HUP {
                let _ = log.rotate();
            } else {
                signals::forward(child_pid.load(Ordering::SeqCst), sig);
            }
        });
    }

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let readers = [(Box::new(stdout) as Box<dyn Read + Send>, out_level, false), (Box::new(stderr), err_level, true)]
        .map(|(src, level, is_err)| {
            let log = log.clone();
            let opts = opts.clone();
            std::thread::spawn(move || {
                let mut echo: Box<dyn Write> = if is_err { Box::new(io::stderr()) } else { Box::new(io::stdout()) };
                let _ = read_lines(BufReader::with_capacity(1 << 16, src), |line| {
                    if opts.tee {
                        let _ = echo.write_all(line.as_bytes());
                    }
                    ingest_line(&log, &opts, level, line, &[]);
                });
            })
        });
    let status = child.wait();
    for r in readers {
        let _ = r.join();
    }
    child_pid.store(0, Ordering::SeqCst);
    let secs = started.elapsed().as_secs_f64();
    let code = match status {
        Ok(s) => {
            #[cfg(unix)]
            let sig = std::os::unix::process::ExitStatusExt::signal(&s);
            #[cfg(not(unix))]
            let sig: Option<i32> = None;
            let code = s.code().unwrap_or_else(|| 128 + sig.unwrap_or(0));
            let level = if s.success() { Level::Info } else { Level::Error };
            let uptime = (secs * 1000.0).round() / 1000.0;
            let mut fields: Vec<(&str, Value)> = vec![("pid", pid.into()), ("code", code.into())];
            if let Some(sig) = sig {
                fields.push(("signal", sig.into()));
            }
            fields.push(("uptime_s", uptime.into()));
            log.named("rotlog").log(level, "process exited", &fields);
            code
        }
        Err(e) => {
            log.error("wait failed", &[("error", e.to_string().as_str().into())]);
            1
        }
    };
    log.close();
    code
}

/// Signal handling via a dedicated `sigwait` thread (no async-signal-safety pitfalls).
pub mod signals {
    pub const HUP: i32 = libc::SIGHUP;
    pub const INT: i32 = libc::SIGINT;
    const SIGNALS: [i32; 4] = [libc::SIGHUP, libc::SIGINT, libc::SIGTERM, libc::SIGQUIT];

    /// Blocks the handled signals in the calling thread. Call before spawning
    /// any thread so they all inherit the mask.
    pub fn block() {
        // SAFETY: plain sigset manipulation on a local set.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for s in SIGNALS {
                libc::sigaddset(&mut set, s);
            }
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        }
    }

    pub fn spawn(handler: impl Fn(i32) + Send + 'static) {
        std::thread::Builder::new()
            .name("rotlog-signals".into())
            .spawn(move || {
                // SAFETY: waits on a locally built set of blocked signals.
                unsafe {
                    let mut set: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut set);
                    for s in SIGNALS {
                        libc::sigaddset(&mut set, s);
                    }
                    loop {
                        let mut sig = 0;
                        if libc::sigwait(&set, &mut sig) == 0 {
                            handler(sig);
                        }
                    }
                }
            })
            .expect("spawn signal thread");
    }

    pub fn forward(pid: i32, sig: i32) {
        if pid > 0 {
            // SAFETY: sending a signal to our own child.
            unsafe {
                libc::kill(pid, sig);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_levels() {
        assert_eq!(detect_level("2026-01-01 ERROR db down"), Some(Level::Error));
        assert_eq!(detect_level("[warn] disk 91%"), Some(Level::Warn));
        assert_eq!(detect_level("time=x level=debug msg=hi"), Some(Level::Debug));
        assert_eq!(detect_level("no error here"), None);
        assert_eq!(detect_level("INFORMATION only"), None);
        assert_eq!(detect_level("CRITICAL: boom"), Some(Level::Fatal));
    }

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m ok"), "red ok");
        assert_eq!(strip_ansi("\x1b]8;;http://x\x07link\x1b]8;;\x07"), "link");
    }
}
