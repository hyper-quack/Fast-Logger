//! Terminal rendering of records.

use std::io::{self, BufWriter, Stdout, Write};

use rotlog::level::Level;
use rotlog::read::Record;
use rotlog::time::{MICROS, Tz};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Pretty,
    Raw,
    Json,
}

pub struct Printer {
    pub mode: Mode,
    pub color: bool,
    pub tz: Tz,
    out: BufWriter<Stdout>,
}

pub const RESET: &str = "\x1b[0m";
pub const DIM: &str = "\x1b[2m";
pub const BOLD: &str = "\x1b[1m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const BLUE: &str = "\x1b[34m";
pub const CYAN: &str = "\x1b[36m";

pub fn level_color(l: Level) -> &'static str {
    match l {
        Level::Trace => DIM,
        Level::Debug => BLUE,
        Level::Info => GREEN,
        Level::Warn => YELLOW,
        Level::Error => RED,
        Level::Fatal => "\x1b[1;41;97m",
        Level::Off => "",
    }
}

fn status_color(v: &str) -> &'static str {
    match v.as_bytes().first() {
        Some(b'5') => RED,
        Some(b'4') => YELLOW,
        Some(b'3') => CYAN,
        Some(b'2') => GREEN,
        _ => "",
    }
}

/// `2026-10-15 12:00:00.123` in `tz`.
pub fn short_ts(micros: i64, tz: Tz) -> String {
    let secs = micros.div_euclid(MICROS);
    let c = tz.civil(secs);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        c.year,
        c.month,
        c.day,
        c.hour,
        c.minute,
        c.second,
        micros.rem_euclid(MICROS) / 1000
    )
}

impl Printer {
    pub fn new(mode: Mode, color: bool, tz: Tz) -> Printer {
        Printer { mode, color, tz, out: BufWriter::with_capacity(64 * 1024, io::stdout()) }
    }

    fn c(&self, code: &'static str) -> &'static str {
        if self.color { code } else { "" }
    }

    pub fn print(&mut self, raw: &str, r: &Record) -> io::Result<()> {
        match self.mode {
            Mode::Raw => writeln!(self.out, "{raw}"),
            Mode::Json => writeln!(self.out, "{}", r.to_json(self.tz)),
            Mode::Pretty => self.pretty(raw, r),
        }
    }

    fn pretty(&mut self, raw: &str, r: &Record) -> io::Result<()> {
        let Some(ts) = r.ts else {
            return writeln!(self.out, "{raw}");
        };
        let (dim, reset) = (self.c(DIM), self.c(RESET));
        let mut line = String::with_capacity(raw.len() + 64);
        line.push_str(dim);
        line.push_str(&short_ts(ts, self.tz));
        line.push_str(reset);
        line.push(' ');
        match r.level {
            Some(l) => {
                line.push_str(self.c(level_color(l)));
                line.push_str(l.padded());
                line.push_str(reset);
            }
            None => line.push_str("     "),
        }
        if !r.logger.is_empty() {
            line.push(' ');
            line.push_str(self.c(CYAN));
            line.push_str(&r.logger);
            line.push_str(reset);
        }
        line.push_str("  ");
        let bold = matches!(r.level, Some(Level::Error | Level::Fatal));
        if bold {
            line.push_str(self.c(BOLD));
        }
        line.push_str(&r.msg);
        if bold {
            line.push_str(reset);
        }
        let mut stacks: Vec<&str> = Vec::new();
        for (k, v) in &r.fields {
            if k == "stack" || k.ends_with(".stack") {
                stacks.push(v);
                continue;
            }
            line.push_str("  ");
            line.push_str(dim);
            line.push_str(k);
            line.push('=');
            line.push_str(reset);
            let color = if k == "status" { self.c(status_color(v)) } else { "" };
            line.push_str(color);
            let mut buf = Vec::new();
            rotlog::format::write_logfmt_value(&mut buf, v);
            line.push_str(&String::from_utf8_lossy(&buf));
            if !color.is_empty() {
                line.push_str(reset);
            }
        }
        writeln!(self.out, "{line}")?;
        for s in stacks {
            for l in s.lines() {
                writeln!(self.out, "    {dim}{l}{reset}")?;
            }
        }
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    pub fn writer(&mut self) -> &mut BufWriter<Stdout> {
        &mut self.out
    }
}

/// Exits quietly when stdout is closed (`rotlog query | head`).
pub fn exit_on_broken_pipe(r: io::Result<()>) {
    if let Err(e) = r {
        if e.kind() == io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        eprintln!("rotlog: {e}");
        std::process::exit(1);
    }
}

/// `12345` -> `12,345`.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
