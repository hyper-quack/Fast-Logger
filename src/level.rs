use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Severity of a record. Ordered: `Trace < Debug < Info < Warn < Error < Fatal < Off`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(u8)]
pub enum Level {
    Trace = 0,
    Debug = 1,
    #[default]
    Info = 2,
    Warn = 3,
    Error = 4,
    Fatal = 5,
    /// Only meaningful as a threshold: disables all output.
    Off = 6,
}

impl Level {
    pub const ALL: [Level; 6] = [Level::Trace, Level::Debug, Level::Info, Level::Warn, Level::Error, Level::Fatal];

    #[inline]
    pub fn from_u8(v: u8) -> Level {
        match v {
            0 => Level::Trace,
            1 => Level::Debug,
            2 => Level::Info,
            3 => Level::Warn,
            4 => Level::Error,
            5 => Level::Fatal,
            _ => Level::Off,
        }
    }

    /// Lowercase name, used in JSON output.
    #[inline]
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "trace",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
            Level::Fatal => "fatal",
            Level::Off => "off",
        }
    }

    /// Uppercase name padded to 5 columns, used in text output.
    #[inline]
    pub fn padded(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO ",
            Level::Warn => "WARN ",
            Level::Error => "ERROR",
            Level::Fatal => "FATAL",
            Level::Off => "OFF  ",
        }
    }

    /// Parses a level name case-insensitively, accepting common aliases
    /// (`warning`, `err`, `critical`, `crit`, `panic`, `emerg`, `notice`, `verbose`)
    /// and numeric severities: 0-6 as rotlog levels, 10-60 as pino/bunyan levels.
    pub fn parse(s: &str) -> Option<Level> {
        let s = s.trim();
        if let Ok(n) = s.parse::<u32>() {
            return Some(Self::from_number(n));
        }
        let mut lower = [0u8; 8];
        if s.len() > lower.len() {
            return None;
        }
        for (d, b) in lower.iter_mut().zip(s.bytes()) {
            *d = b.to_ascii_lowercase();
        }
        Some(match &lower[..s.len()] {
            b"trace" | b"verbose" => Level::Trace,
            b"debug" | b"dbg" => Level::Debug,
            b"info" | b"notice" | b"log" => Level::Info,
            b"warn" | b"warning" => Level::Warn,
            b"error" | b"err" => Level::Error,
            b"fatal" | b"critical" | b"crit" | b"panic" | b"emerg" | b"alert" => Level::Fatal,
            b"off" | b"none" | b"silent" => Level::Off,
            _ => return None,
        })
    }

    /// Numeric severities: 0-6 map directly, 10-60 follow pino/bunyan conventions.
    pub fn from_number(n: u32) -> Level {
        match n {
            0..=6 => Level::from_u8(n as u8),
            7..=10 => Level::Trace,
            11..=20 => Level::Debug,
            21..=30 => Level::Info,
            31..=40 => Level::Warn,
            41..=50 => Level::Error,
            _ => Level::Fatal,
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Level {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Level::parse(s)
            .ok_or_else(|| format!("invalid level '{s}' (expected trace, debug, info, warn, error, fatal or off)"))
    }
}

impl Serialize for Level {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Level {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            N(u32),
            S(String),
        }
        match Raw::deserialize(d)? {
            Raw::N(n) => Ok(Level::from_number(n)),
            Raw::S(s) => s.parse().map_err(serde::de::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_aliases() {
        assert_eq!(Level::parse("WARNING"), Some(Level::Warn));
        assert_eq!(Level::parse("critical"), Some(Level::Fatal));
        assert_eq!(Level::parse("50"), Some(Level::Error));
        assert_eq!(Level::parse("3"), Some(Level::Warn));
        assert_eq!(Level::parse("nope"), None);
        assert!(Level::Debug < Level::Error);
    }
}
