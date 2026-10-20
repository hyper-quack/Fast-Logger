//! Configuration: one struct shared by the library, the CLI (TOML files + flags)
//! and the Bun binding (JSON).

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::level::Level;
use crate::rotation::Every;
use crate::time::Tz;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Directory holding the active file and its archives.
    pub dir: PathBuf,
    /// Stream name: the active file is `<dir>/<name>.<ext>`.
    pub name: String,
    pub ext: String,
    /// Minimum level written.
    pub level: Level,
    pub format: Format,
    pub timezone: Tz,
    /// Also echo every line to the console.
    pub console: Console,
    /// Longest time a record waits in memory before being written. `0` writes immediately.
    pub flush_interval: Dur,
    /// Pending bytes that trigger an early write.
    pub buffer_size: Size,
    /// Pending bytes at which producers block (or drop, see `overflow`) until the disk catches up.
    pub max_buffer: Size,
    pub overflow: Overflow,
    pub fsync: Fsync,
    /// Write `error`/`fatal` records immediately instead of waiting for `flush_interval`.
    pub flush_on_error: bool,
    /// How often compression, upload and retention run in the background.
    pub maintenance_interval: Dur,
    pub rotation: RotationConfig,
    pub compress: CompressConfig,
    pub upload: UploadConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            dir: PathBuf::from("logs"),
            name: "app".into(),
            ext: "log".into(),
            level: Level::Info,
            format: Format::Text,
            timezone: Tz::Local,
            console: Console::Off,
            flush_interval: Dur(Duration::from_millis(100)),
            buffer_size: Size(256 * 1024),
            max_buffer: Size(64 * 1024 * 1024),
            overflow: Overflow::Block,
            fsync: Fsync::Rotate,
            flush_on_error: true,
            maintenance_interval: Dur(Duration::from_secs(60)),
            rotation: RotationConfig::default(),
            compress: CompressConfig::default(),
            upload: UploadConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RotationConfig {
    /// `never`, `minutely`, `hourly`, `daily`, `weekly[:mon..sun]`, or an aligned period like `15m`, `6h`, `2d`.
    pub every: Every,
    /// Also rotate when the active file would exceed this size. `0` disables.
    pub max_size: Size,
    /// Archives kept locally (newest first). `0` keeps all.
    pub keep: u32,
    /// Archives older than this are deleted locally. `0` disables.
    pub max_age: Dur,
}

impl Default for RotationConfig {
    fn default() -> Self {
        RotationConfig { every: Every::Daily, max_size: Size(0), keep: 30, max_age: Dur(Duration::ZERO) }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompressConfig {
    /// Gzip archives once they are older than `after`.
    pub enabled: bool,
    /// Age (since rotation) after which an archive gets compressed. `0` compresses right away.
    pub after: Dur,
    /// Gzip level 1 (fastest) to 9 (smallest).
    pub level: u32,
}

impl Default for CompressConfig {
    fn default() -> Self {
        CompressConfig { enabled: true, after: Dur(Duration::ZERO), level: 6 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UploadConfig {
    pub provider: Provider,
    /// Destination bucket. Upload is active only when this is set.
    pub bucket: String,
    /// Prepended to object names (available as `{prefix}` in `object`).
    pub prefix: String,
    /// Object name template. Variables: `{prefix} {host} {name} {file} {yyyy} {mm} {dd} {hh}`.
    pub object: String,
    /// Wait this long after rotation (or compression) before uploading.
    pub after: Dur,
    /// Delete the local archive once it is safely uploaded.
    pub delete_local: bool,
    /// Service-account or authorized-user JSON. Defaults to Application Default Credentials,
    /// then the GCE/Cloud Run metadata server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials: Option<PathBuf>,
    /// Static OAuth access token (testing / short-lived jobs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    /// API endpoint; override for emulators such as fake-gcs-server.
    pub endpoint: String,
    /// Per-request timeout.
    pub timeout: Dur,
}

impl Default for UploadConfig {
    fn default() -> Self {
        UploadConfig {
            provider: Provider::Gcs,
            bucket: String::new(),
            prefix: String::new(),
            object: "{prefix}{host}/{name}/{yyyy}/{mm}/{dd}/{file}".into(),
            after: Dur(Duration::ZERO),
            delete_local: false,
            credentials: None,
            access_token: None,
            endpoint: "https://storage.googleapis.com".into(),
            timeout: Dur(Duration::from_secs(120)),
        }
    }
}

impl UploadConfig {
    pub fn active(&self) -> bool {
        !self.bucket.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Gcs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// `2026-10-15T12:00:00.000123+02:00 INFO  api: message<TAB>key=value ...`
    #[default]
    Text,
    /// One JSON object per line.
    Json,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Console {
    #[default]
    Off,
    Stdout,
    Stderr,
}

impl<'de> Deserialize<'de> for Console {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            B(bool),
            S(String),
        }
        Ok(match Raw::deserialize(d)? {
            Raw::B(true) => Console::Stderr,
            Raw::B(false) => Console::Off,
            Raw::S(s) => match s.to_ascii_lowercase().as_str() {
                "off" | "none" | "false" => Console::Off,
                "stdout" => Console::Stdout,
                "stderr" | "true" => Console::Stderr,
                _ => return Err(serde::de::Error::custom(format!("invalid console '{s}' (off, stdout, stderr)"))),
            },
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Overflow {
    /// Producers wait for the writer: nothing is ever lost.
    #[default]
    Block,
    /// Records are dropped (and counted) while the buffer is full.
    Drop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fsync {
    Never,
    /// fsync each file before it is archived.
    #[default]
    Rotate,
    /// fsync after every write batch.
    Always,
}

/// A duration accepting `"250ms"`, `"30s"`, `"15m"`, `"6h"`, `"7d"`, `"2w"` or plain seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Dur(pub Duration);

/// A byte size accepting `"512k"`, `"100MB"`, `"1GiB"` or plain bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Size(pub u64);

pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: f64 = n.parse().map_err(|_| format!("invalid duration '{s}'"))?;
    let mult = match unit.trim() {
        "" | "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
        "ms" => 0.001,
        "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hour" | "hours" => 3600.0,
        "d" | "day" | "days" => 86400.0,
        "w" | "week" | "weeks" => 604800.0,
        _ => return Err(format!("invalid duration unit in '{s}' (ms, s, m, h, d, w)")),
    };
    Ok(Duration::from_secs_f64(n * mult))
}

pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: f64 = n.parse().map_err(|_| format!("invalid size '{s}'"))?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        _ => return Err(format!("invalid size unit in '{s}' (b, kb, mb, gb)")),
    };
    Ok((n * mult as f64) as u64)
}

impl fmt::Display for Dur {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = self.0.as_millis();
        let units = [(604_800_000, "w"), (86_400_000, "d"), (3_600_000, "h"), (60_000, "m"), (1_000, "s")];
        if ms == 0 {
            return f.write_str("0s");
        }
        for (div, u) in units {
            if ms.is_multiple_of(div) {
                return write!(f, "{}{u}", ms / div);
            }
        }
        write!(f, "{ms}ms")
    }
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let units = [(1u64 << 40, "TB"), (1 << 30, "GB"), (1 << 20, "MB"), (1 << 10, "KB")];
        for (div, u) in units {
            if self.0 >= div && self.0.is_multiple_of(div) {
                return write!(f, "{}{u}", self.0 / div);
            }
        }
        write!(f, "{}B", self.0)
    }
}

/// Human-readable size with one decimal (`1.5MB`).
pub fn human_size(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n}B") } else { format!("{v:.1}{}", units[i]) }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum NumOrStr {
    N(f64),
    S(String),
}

impl<'de> Deserialize<'de> for Dur {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match NumOrStr::deserialize(d)? {
            NumOrStr::N(n) if n >= 0.0 => Ok(Dur(Duration::from_secs_f64(n))),
            NumOrStr::N(_) => Err(serde::de::Error::custom("duration must be positive")),
            NumOrStr::S(s) => parse_duration(&s).map(Dur).map_err(serde::de::Error::custom),
        }
    }
}

impl Serialize for Dur {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Size {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match NumOrStr::deserialize(d)? {
            NumOrStr::N(n) if n >= 0.0 => Ok(Size(n as u64)),
            NumOrStr::N(_) => Err(serde::de::Error::custom("size must be positive")),
            NumOrStr::S(s) => parse_size(&s).map(Size).map_err(serde::de::Error::custom),
        }
    }
}

impl Serialize for Size {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl Config {
    /// Loads a `.toml` or `.json` file (by extension; TOML otherwise).
    pub fn from_file(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if path.extension().is_some_and(|e| e == "json") {
            Config::from_json(&text)
        } else {
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
        }
    }

    pub fn from_json(text: &str) -> Result<Config, String> {
        serde_json::from_str(text).map_err(|e| format!("invalid config: {e}"))
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// One-line summary of rotation, retention, compression and upload.
    pub fn policy_summary(&self) -> String {
        let r = &self.rotation;
        format!(
            "rotate {}{}, keep {}, compress {}, upload {}",
            r.every,
            if r.max_size.0 > 0 { format!(" or {}", r.max_size) } else { String::new() },
            if r.keep == 0 { "all".to_string() } else { r.keep.to_string() },
            if self.compress.enabled { format!("after {}", self.compress.after) } else { "off".into() },
            if self.upload.active() {
                format!("gs://{}/{}", self.upload.bucket, self.upload.prefix)
            } else {
                "off".into()
            },
        )
    }

    /// Checks invariants the writer relies on.
    pub fn validate(&self) -> Result<(), String> {
        let bad = |s: &str| s.is_empty() || s.contains(['/', '\\', '\0']) || s.starts_with('.');
        if bad(&self.name) {
            return Err(format!("invalid stream name '{}'", self.name));
        }
        if bad(&self.ext) {
            return Err(format!("invalid extension '{}'", self.ext));
        }
        if !(1..=9).contains(&self.compress.level) {
            return Err("compress.level must be between 1 and 9".into());
        }
        if self.max_buffer.0 < self.buffer_size.0 {
            return Err("max_buffer must be >= buffer_size".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_sizes() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("6h").unwrap(), Duration::from_secs(21600));
        assert_eq!(parse_duration("1.5m").unwrap(), Duration::from_secs(90));
        assert!(parse_duration("5x").is_err());
        assert_eq!(parse_size("100MB").unwrap(), 100 << 20);
        assert_eq!(parse_size("512k").unwrap(), 512 << 10);
        assert_eq!(Dur(Duration::from_secs(7200)).to_string(), "2h");
        assert_eq!(Size(64 << 20).to_string(), "64MB");
    }

    #[test]
    fn toml_roundtrip() {
        let c: Config = toml::from_str(
            r#"
            dir = "/var/log/api"
            name = "api"
            level = "debug"
            format = "json"
            console = true
            [rotation]
            every = "6h"
            max_size = "100MB"
            [compress]
            after = "1d"
            [upload]
            bucket = "b"
            "#,
        )
        .unwrap();
        assert_eq!(c.level, Level::Debug);
        assert_eq!(c.console, Console::Stderr);
        assert_eq!(c.rotation.every, Every::Period(21600));
        assert_eq!(c.rotation.max_size, Size(100 << 20));
        let back: Config = toml::from_str(&c.to_toml()).unwrap();
        assert_eq!(back.rotation.every, c.rotation.every);
        assert_eq!(back.compress.after, c.compress.after);
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(Config::from_json(r#"{"nmae":"x"}"#).is_err());
    }
}
