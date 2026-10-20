//! Clock, civil-date math and RFC 3339 formatting/parsing without external crates.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const MICROS: i64 = 1_000_000;

#[cfg(target_vendor = "apple")]
unsafe extern "C" {
    fn clock_gettime_nsec_np(clock_id: libc::clockid_t) -> u64;
}

/// Current wall-clock time in microseconds since the Unix epoch.
/// Calls the OS real-time clock directly (about twice as fast as `SystemTime::now` on macOS).
#[cfg(target_vendor = "apple")]
#[inline]
pub fn now_micros() -> i64 {
    // SAFETY: plain libSystem call without pointers.
    (unsafe { clock_gettime_nsec_np(libc::CLOCK_REALTIME) } / 1000) as i64
}

#[cfg(all(unix, not(target_vendor = "apple")))]
#[inline]
pub fn now_micros() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime writes into the provided timespec (vDSO on Linux).
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    ts.tv_sec as i64 * MICROS + ts.tv_nsec as i64 / 1000
}

#[cfg(not(unix))]
#[inline]
pub fn now_micros() -> i64 {
    system_time_micros(SystemTime::now())
}

pub fn system_time_micros(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64 * MICROS + d.subsec_micros() as i64,
        Err(e) => -(e.duration().as_micros() as i64),
    }
}

/// Time zone used for timestamps, rotation boundaries and archive names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tz {
    #[default]
    Local,
    Utc,
}

impl Tz {
    /// UTC offset in seconds in effect at `secs` (unix time).
    #[inline]
    pub fn offset(self, secs: i64) -> i32 {
        match self {
            Tz::Utc => 0,
            Tz::Local => local_offset(secs),
        }
    }

    /// Converts a wall-clock time in this zone to unix seconds.
    pub fn to_unix(self, c: Civil) -> i64 {
        match self {
            Tz::Utc => c.naive_secs(),
            Tz::Local => local_to_unix(c),
        }
    }

    /// Wall-clock fields of `secs` (unix time) in this zone.
    pub fn civil(self, secs: i64) -> Civil {
        Civil::from_naive_secs(secs + self.offset(secs) as i64)
    }
}

#[cfg(unix)]
fn local_offset(secs: i64) -> i32 {
    // SAFETY: localtime_r only writes into the provided, zeroed tm struct.
    unsafe {
        let t = secs as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff as i32
    }
}

#[cfg(not(unix))]
fn local_offset(_secs: i64) -> i32 {
    0
}

#[cfg(unix)]
fn local_to_unix(c: Civil) -> i64 {
    // SAFETY: mktime reads and normalizes the provided tm struct only.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tm.tm_year = (c.year - 1900) as libc::c_int;
        tm.tm_mon = c.month as libc::c_int - 1;
        tm.tm_mday = c.day as libc::c_int;
        tm.tm_hour = c.hour as libc::c_int;
        tm.tm_min = c.minute as libc::c_int;
        tm.tm_sec = c.second as libc::c_int;
        tm.tm_isdst = -1;
        libc::mktime(&mut tm) as i64
    }
}

#[cfg(not(unix))]
fn local_to_unix(c: Civil) -> i64 {
    c.naive_secs()
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

/// Inverse of [`days_from_civil`].
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Broken-down wall-clock time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Civil {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl Civil {
    pub fn from_naive_secs(s: i64) -> Civil {
        let days = s.div_euclid(86400);
        let rem = s.rem_euclid(86400) as u32;
        let (year, month, day) = civil_from_days(days);
        Civil { year, month, day, hour: rem / 3600, minute: rem / 60 % 60, second: rem % 60 }
    }

    /// Seconds since epoch treating the fields as UTC.
    pub fn naive_secs(self) -> i64 {
        days_from_civil(self.year, self.month, self.day) * 86400
            + (self.hour * 3600 + self.minute * 60 + self.second) as i64
    }

    /// `YYYY-MM-DD_HH-MM-SS`, the timestamp used in archive file names.
    pub fn file_label(self) -> String {
        format!(
            "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// Parses a [`Civil::file_label`].
    pub fn parse_file_label(s: &str) -> Option<Civil> {
        let b = s.as_bytes();
        if b.len() != 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'_' || b[13] != b'-' || b[16] != b'-' {
            return None;
        }
        Some(Civil {
            year: num(&b[0..4])? as i64,
            month: num(&b[5..7])?,
            day: num(&b[8..10])?,
            hour: num(&b[11..13])?,
            minute: num(&b[14..16])?,
            second: num(&b[17..19])?,
        })
    }
}

fn num(b: &[u8]) -> Option<u32> {
    let mut n = 0u32;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n * 10 + (c - b'0') as u32;
    }
    Some(n)
}

#[inline]
fn push2(out: &mut [u8], v: u32) {
    out[0] = b'0' + (v / 10 % 10) as u8;
    out[1] = b'0' + (v % 10) as u8;
}

/// Formats RFC 3339 timestamps with microsecond precision
/// (`2026-10-15T12:34:56.123456+02:00`), caching everything but the fraction
/// for the current second so the hot path is a couple of memcpys.
pub struct TsFormatter {
    tz: Tz,
    sec: i64,
    prefix: [u8; 20],
    suffix: [u8; 6],
    suffix_len: usize,
}

impl TsFormatter {
    pub fn new(tz: Tz) -> TsFormatter {
        TsFormatter { tz, sec: i64::MIN, prefix: [0; 20], suffix: [0; 6], suffix_len: 0 }
    }

    fn refresh(&mut self, sec: i64) {
        let off = self.tz.offset(sec);
        let c = Civil::from_naive_secs(sec + off as i64);
        let p = &mut self.prefix;
        let y = c.year.clamp(0, 9999) as u32;
        push2(&mut p[0..2], y / 100);
        push2(&mut p[2..4], y % 100);
        p[4] = b'-';
        push2(&mut p[5..7], c.month);
        p[7] = b'-';
        push2(&mut p[8..10], c.day);
        p[10] = b'T';
        push2(&mut p[11..13], c.hour);
        p[13] = b':';
        push2(&mut p[14..16], c.minute);
        p[16] = b':';
        push2(&mut p[17..19], c.second);
        p[19] = b'.';
        if self.tz == Tz::Utc {
            self.suffix[0] = b'Z';
            self.suffix_len = 1;
        } else {
            let (sign, a) = if off < 0 { (b'-', -off) } else { (b'+', off) };
            let a = a as u32;
            self.suffix[0] = sign;
            push2(&mut self.suffix[1..3], a / 3600);
            self.suffix[3] = b':';
            push2(&mut self.suffix[4..6], a / 60 % 60);
            self.suffix_len = 6;
        }
        self.sec = sec;
    }

    #[inline]
    pub fn write(&mut self, micros: i64, out: &mut Vec<u8>) {
        let sec = micros.div_euclid(MICROS);
        if sec != self.sec {
            self.refresh(sec);
        }
        let us = micros.rem_euclid(MICROS) as u32;
        let mut frac = [0u8; 6];
        push2(&mut frac[0..2], us / 10_000);
        push2(&mut frac[2..4], us / 100 % 100);
        push2(&mut frac[4..6], us % 100);
        out.extend_from_slice(&self.prefix);
        out.extend_from_slice(&frac);
        out.extend_from_slice(&self.suffix[..self.suffix_len]);
    }
}

/// One-off RFC 3339 formatting.
pub fn format_rfc3339(micros: i64, tz: Tz) -> String {
    let mut f = TsFormatter::new(tz);
    let mut out = Vec::with_capacity(32);
    f.write(micros, &mut out);
    String::from_utf8(out).unwrap_or_default()
}

/// Parses `YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)` (a space may replace the `T`)
/// into microseconds since the epoch.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't' | b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let c = Civil {
        year: num(&b[0..4])? as i64,
        month: num(&b[5..7])?,
        day: num(&b[8..10])?,
        hour: num(&b[11..13])?,
        minute: num(&b[14..16])?,
        second: num(&b[17..19])?,
    };
    let mut i = 19;
    let mut us = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            if i - start < 6 {
                us = us * 10 + (b[i] - b'0') as i64;
            }
            i += 1;
        }
        for _ in (i - start).min(6)..6 {
            us *= 10;
        }
    }
    let off = match b.get(i)? {
        b'Z' | b'z' => 0,
        sign @ (b'+' | b'-') => {
            if b.len() < i + 6 {
                return None;
            }
            let h = num(&b[i + 1..i + 3])? as i64;
            let m = num(&b[i + 4..i + 6])? as i64;
            let o = h * 3600 + m * 60;
            if *sign == b'-' { -o } else { o }
        }
        _ => return None,
    };
    Some((c.naive_secs() - off) * MICROS + us)
}

/// Parses a user-supplied point in time: RFC 3339, `YYYY-MM-DD[ HH:MM[:SS]]`
/// (interpreted in `tz`), or a relative duration meaning "that long ago" (`15m`, `2h`, `3d`).
pub fn parse_when(s: &str, tz: Tz, now: i64) -> Option<i64> {
    let s = s.trim();
    if let Some(t) = parse_rfc3339(s) {
        return Some(t);
    }
    if let Ok(d) = crate::config::parse_duration(s) {
        return Some(now - d.as_micros() as i64);
    }
    let b = s.as_bytes();
    if b.len() >= 10 && b[4] == b'-' && b[7] == b'-' {
        let mut c = Civil {
            year: num(&b[0..4])? as i64,
            month: num(&b[5..7])?,
            day: num(&b[8..10])?,
            hour: 0,
            minute: 0,
            second: 0,
        };
        if b.len() > 10 {
            if !matches!(b[10], b'T' | b' ') || b.len() < 16 || b[13] != b':' {
                return None;
            }
            c.hour = num(&b[11..13])?;
            c.minute = num(&b[14..16])?;
            if b.len() >= 19 && b[16] == b':' {
                c.second = num(&b[17..19])?;
            }
        }
        return Some(tz.to_unix(c) * MICROS);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_roundtrip() {
        for days in [-1_000_000, -1, 0, 1, 19_000, 20_376, 2_000_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2026, 10, 15), 20_741);
    }

    #[test]
    fn format_and_parse_utc() {
        let t = 1_760_531_696_123_456; // 2025-10-15T12:34:56.123456Z
        let s = format_rfc3339(t, Tz::Utc);
        assert_eq!(s, "2025-10-15T12:34:56.123456Z");
        assert_eq!(parse_rfc3339(&s), Some(t));
        assert_eq!(parse_rfc3339("2025-10-15T14:34:56.123456+02:00"), Some(t));
        assert_eq!(parse_rfc3339("2025-10-15T12:34:56Z"), Some(t - 123_456));
        assert_eq!(parse_rfc3339("2025-10-15T12:34:56.1Z"), Some(t - 23_456));
    }

    #[test]
    fn fast_clock_matches_system_time() {
        let a = system_time_micros(SystemTime::now());
        let b = now_micros();
        let c = system_time_micros(SystemTime::now());
        assert!(a <= b && b <= c, "{a} {b} {c}");
    }

    #[test]
    fn local_roundtrip() {
        let t = now_micros();
        let s = format_rfc3339(t, Tz::Local);
        assert_eq!(parse_rfc3339(&s), Some(t));
    }

    #[test]
    fn file_label_roundtrip() {
        let c = Civil::from_naive_secs(1_760_531_696);
        let l = c.file_label();
        assert_eq!(l, "2025-10-15_12-34-56");
        assert_eq!(Civil::parse_file_label(&l), Some(c));
    }
}
