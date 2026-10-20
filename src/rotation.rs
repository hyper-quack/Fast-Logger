//! Rotation schedule and on-disk naming of a stream's files.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::{Config, parse_duration};
use crate::time::{Civil, MICROS, Tz};

/// When the active file is rotated. Boundaries are aligned to the wall clock
/// of the configured time zone (`6h` rotates at 00:00, 06:00, 12:00, 18:00).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Every {
    Never,
    /// Aligned period in seconds.
    Period(u32),
    /// Local midnight.
    #[default]
    Daily,
    /// Midnight starting the given weekday (0 = Monday … 6 = Sunday).
    Weekly(u8),
}

const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

impl Every {
    pub fn parse(s: &str) -> Result<Every, String> {
        let l = s.trim().to_ascii_lowercase();
        Ok(match l.as_str() {
            "never" | "none" | "off" | "0" => Every::Never,
            "secondly" => Every::Period(1),
            "minutely" => Every::Period(60),
            "hourly" => Every::Period(3600),
            "daily" | "midnight" | "1d" | "24h" => Every::Daily,
            "weekly" => Every::Weekly(0),
            _ => {
                if let Some(day) = l.strip_prefix("weekly:") {
                    let i = WEEKDAYS
                        .iter()
                        .position(|d| day.starts_with(d))
                        .ok_or_else(|| format!("invalid weekday in '{s}'"))?;
                    Every::Weekly(i as u8)
                } else if let Some(d) = l.strip_prefix('w').and_then(|d| d.parse::<u8>().ok()).filter(|d| *d < 7) {
                    // Python TimedRotatingFileHandler compatibility: W0 = Monday.
                    Every::Weekly(d)
                } else {
                    let secs = parse_duration(&l)?.as_secs();
                    if secs == 0 || secs > u32::MAX as u64 {
                        return Err(format!("invalid rotation period '{s}'"));
                    }
                    match secs {
                        86400 => Every::Daily,
                        604800 => Every::Weekly(0),
                        _ => Every::Period(secs as u32),
                    }
                }
            }
        })
    }

    /// Start of the period containing `micros` and the start of the next one.
    pub fn bounds(self, micros: i64, tz: Tz) -> Option<(i64, i64)> {
        let secs = micros.div_euclid(MICROS);
        let off = tz.offset(secs) as i64;
        let local = secs + off;
        let (start_local, next_local) = match self {
            Every::Never => return None,
            Every::Period(p) => {
                let p = p as i64;
                let s = local.div_euclid(p) * p;
                (s, s + p)
            }
            Every::Daily => {
                let s = local.div_euclid(86400) * 86400;
                (s, s + 86400)
            }
            Every::Weekly(wd) => {
                let day = local.div_euclid(86400);
                let cur = (day + 3).rem_euclid(7); // 1970-01-01 was a Thursday
                let back = (cur - wd as i64).rem_euclid(7);
                let s = (day - back) * 86400;
                (s, s + 7 * 86400)
            }
        };
        Some((to_utc(start_local, off, tz) * MICROS, to_utc(next_local, off, tz) * MICROS))
    }
}

/// Converts a local wall-clock second back to UTC, correcting once for a
/// DST transition between "now" and the boundary.
fn to_utc(local: i64, off: i64, tz: Tz) -> i64 {
    let guess = local - off;
    let off2 = tz.offset(guess) as i64;
    if off2 == off { guess } else { local - off2 }
}

impl fmt::Display for Every {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Every::Never => f.write_str("never"),
            Every::Daily => f.write_str("daily"),
            Every::Weekly(d) => write!(f, "weekly:{}", WEEKDAYS[d as usize % 7]),
            Every::Period(60) => f.write_str("minutely"),
            Every::Period(3600) => f.write_str("hourly"),
            Every::Period(p) => write!(f, "{}", crate::config::Dur(std::time::Duration::from_secs(p as u64))),
        }
    }
}

impl Serialize for Every {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Every {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Every::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Where a stream's files live.
#[derive(Clone, Debug)]
pub struct Paths {
    pub dir: PathBuf,
    pub name: String,
    pub ext: String,
    pub tz: Tz,
}

/// A rotated file belonging to a stream.
#[derive(Clone, Debug)]
pub struct Archive {
    pub path: PathBuf,
    pub file_name: String,
    /// `YYYY-MM-DD_HH-MM-SS` from the file name.
    pub label: String,
    pub seq: u32,
    pub compressed: bool,
}

impl Archive {
    /// Period start encoded in the name, in microseconds.
    pub fn start_micros(&self, tz: Tz) -> Option<i64> {
        Civil::parse_file_label(&self.label).map(|c| tz.to_unix(c) * MICROS)
    }
}

impl Paths {
    pub fn new(cfg: &Config) -> Paths {
        Paths { dir: cfg.dir.clone(), name: cfg.name.clone(), ext: cfg.ext.clone(), tz: cfg.timezone }
    }

    pub fn active(&self) -> PathBuf {
        self.dir.join(format!("{}.{}", self.name, self.ext))
    }

    fn hidden(&self, suffix: &str) -> PathBuf {
        self.dir.join(format!(".{}.{suffix}", self.name))
    }

    /// Held (flock) by the process writing the stream; contains its pid.
    pub fn lock(&self) -> PathBuf {
        self.hidden("lock")
    }

    /// Serializes compression/upload/retention between processes.
    pub fn maint_lock(&self) -> PathBuf {
        self.hidden("maint.lock")
    }

    /// Control request dropped by `rotlog ctl` for the owning process.
    pub fn ctl(&self) -> PathBuf {
        self.hidden("ctl")
    }

    pub fn ctl_response(&self) -> PathBuf {
        self.hidden("ctl.resp")
    }

    /// Names of archives already uploaded, one per line.
    pub fn manifest(&self) -> PathBuf {
        self.hidden("uploaded")
    }

    /// A fresh archive path for a file whose period started at `start_micros`.
    pub fn new_archive(&self, start_micros: i64) -> PathBuf {
        let label = self.tz.civil(start_micros.div_euclid(MICROS)).file_label();
        for seq in 0u32.. {
            let base = if seq == 0 {
                format!("{}.{label}.{}", self.name, self.ext)
            } else {
                format!("{}.{label}.{seq}.{}", self.name, self.ext)
            };
            let p = self.dir.join(&base);
            if !p.exists() && !self.dir.join(format!("{base}.gz")).exists() {
                return p;
            }
        }
        unreachable!()
    }

    /// Recognizes `<name>.<label>[.<seq>].<ext>[.gz]`.
    pub fn parse_archive(&self, file_name: &str) -> Option<Archive> {
        let rest = file_name.strip_prefix(&self.name)?.strip_prefix('.')?;
        let (rest, compressed) = match rest.strip_suffix(".gz") {
            Some(r) => (r, true),
            None => (rest, false),
        };
        let rest = rest.strip_suffix(&self.ext)?.strip_suffix('.')?;
        if rest.len() < 19 {
            return None;
        }
        let (label, seq) = rest.split_at(19);
        Civil::parse_file_label(label)?;
        let seq = match seq {
            "" => 0,
            s => s.strip_prefix('.')?.parse().ok()?,
        };
        Some(Archive {
            path: self.dir.join(file_name),
            file_name: file_name.to_string(),
            label: label.to_string(),
            seq,
            compressed,
        })
    }

    /// Archives sorted oldest first.
    pub fn archives(&self) -> Vec<Archive> {
        let Ok(rd) = fs::read_dir(&self.dir) else { return Vec::new() };
        let mut v: Vec<Archive> =
            rd.flatten().filter_map(|e| e.file_name().to_str().and_then(|n| self.parse_archive(n))).collect();
        v.sort_by(|a, b| (&a.label, a.seq, a.compressed).cmp(&(&b.label, b.seq, b.compressed)));
        v
    }

    /// Every stream (`<name>.<ext>` active file, archive or lock) found in `dir`.
    pub fn discover(dir: &Path) -> Vec<String> {
        let mut names = std::collections::BTreeSet::new();
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                let n = e.file_name();
                let Some(n) = n.to_str() else { continue };
                if let Some(s) = n.strip_prefix('.').and_then(|s| s.strip_suffix(".lock"))
                    && !s.ends_with(".maint")
                {
                    names.insert(s.to_string());
                }
            }
        }
        names.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_every() {
        assert_eq!(Every::parse("hourly").unwrap(), Every::Period(3600));
        assert_eq!(Every::parse("6h").unwrap(), Every::Period(21600));
        assert_eq!(Every::parse("1d").unwrap(), Every::Daily);
        assert_eq!(Every::parse("weekly:sunday").unwrap(), Every::Weekly(6));
        assert_eq!(Every::parse("W0").unwrap(), Every::Weekly(0));
        assert!(Every::parse("weekly:xyz").is_err());
        assert_eq!(Every::Period(21600).to_string(), "6h");
    }

    #[test]
    fn utc_bounds() {
        // 2025-10-15T12:34:56Z, a Wednesday
        let t = 1_760_531_696 * MICROS;
        let day = 1_760_486_400 * MICROS;
        assert_eq!(Every::Daily.bounds(t, Tz::Utc), Some((day, day + 86400 * MICROS)));
        assert_eq!(Every::Period(21600).bounds(t, Tz::Utc), Some((day + 43200 * MICROS, day + 64800 * MICROS)));
        let monday = day - 2 * 86400 * MICROS;
        assert_eq!(Every::Weekly(0).bounds(t, Tz::Utc), Some((monday, monday + 7 * 86400 * MICROS)));
        let sunday = day - 3 * 86400 * MICROS;
        assert_eq!(Every::Weekly(6).bounds(t, Tz::Utc).unwrap().0, sunday);
        assert_eq!(Every::Never.bounds(t, Tz::Utc), None);
    }

    #[test]
    fn archive_names() {
        let p = Paths { dir: "/x".into(), name: "api".into(), ext: "log".into(), tz: Tz::Utc };
        let a = p.parse_archive("api.2025-10-15_00-00-00.log.gz").unwrap();
        assert!(a.compressed);
        assert_eq!(a.seq, 0);
        let b = p.parse_archive("api.2025-10-15_00-00-00.3.log").unwrap();
        assert_eq!(b.seq, 3);
        assert!(p.parse_archive("api.log").is_none());
        assert!(p.parse_archive("api.db.2025-10-15_00-00-00.log").is_none());
        assert!(p.parse_archive("api.2025-10-15_00-00-00.log.gz.tmp").is_none());
        assert_eq!(a.start_micros(Tz::Utc), Some(1_760_486_400 * MICROS));
    }
}
