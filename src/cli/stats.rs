//! `rotlog stats`: aggregates over parsed records.

use std::collections::HashMap;
use std::io::Write;

use rotlog::level::Level;
use rotlog::read::Record;
use rotlog::time::Tz;
use serde_json::json;

use super::print::{self, thousands};

#[derive(Default)]
struct PathAgg {
    count: u64,
    c4xx: u64,
    c5xx: u64,
    durs: Vec<f64>,
}

#[derive(Default)]
struct UserAgg {
    count: u64,
    errors: u64,
}

#[derive(Default)]
pub struct Agg {
    total: u64,
    levels: [u64; 7],
    no_level: u64,
    loggers: HashMap<String, u64>,
    requests: u64,
    classes: [u64; 6],
    durs: Vec<f64>,
    paths: HashMap<String, PathAgg>,
    users: HashMap<String, UserAgg>,
    errors: HashMap<String, u64>,
    first: Option<i64>,
    last: Option<i64>,
}

/// Groups `/users/42/orders/9f1c…` as `/users/:id/orders/:id`.
pub fn route_of(path: &str) -> String {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let mut out = String::with_capacity(path.len());
    for (i, seg) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        let is_id = !seg.is_empty()
            && (seg.bytes().all(|b| b.is_ascii_digit())
                || (seg.len() >= 16 && seg.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')));
        out.push_str(if is_id { ":id" } else { seg });
    }
    out
}

fn pct(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let i = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    Some(sorted[i.min(sorted.len() - 1)])
}

fn ms(v: Option<f64>) -> String {
    match v {
        None => "-".into(),
        Some(v) if v >= 1000.0 => format!("{:.2}s", v / 1000.0),
        Some(v) => format!("{v:.1}ms"),
    }
}

impl Agg {
    pub fn add(&mut self, r: &Record) {
        self.total += 1;
        if let Some(t) = r.ts {
            self.first = Some(self.first.map_or(t, |f| f.min(t)));
            self.last = Some(self.last.map_or(t, |l| l.max(t)));
        }
        match r.level {
            Some(l) => self.levels[l as usize] += 1,
            None => self.no_level += 1,
        }
        if !r.logger.is_empty() {
            *self.loggers.entry(r.logger.clone()).or_default() += 1;
        }
        let status: Option<u16> = r.field("status").and_then(|s| s.parse().ok());
        if let Some(st) = status {
            self.requests += 1;
            self.classes[(st / 100).min(5) as usize] += 1;
            let dur = r.field("dur_ms").and_then(|d| d.parse::<f64>().ok());
            if let Some(d) = dur {
                self.durs.push(d);
            }
            if let Some(p) = r.field("path") {
                let key = match r.field("method") {
                    Some(m) => format!("{m} {}", route_of(p)),
                    None => route_of(p),
                };
                let e = self.paths.entry(key).or_default();
                e.count += 1;
                match st {
                    400..=499 => e.c4xx += 1,
                    500.. => e.c5xx += 1,
                    _ => {}
                }
                if let Some(d) = dur {
                    e.durs.push(d);
                }
            }
        }
        let is_err = matches!(r.level, Some(Level::Error | Level::Fatal));
        if let Some(u) = r.field("user") {
            let e = self.users.entry(u.to_string()).or_default();
            e.count += 1;
            e.errors += is_err as u64;
        }
        if is_err {
            let mut m: String = r.msg.chars().take(120).collect();
            if let Some(err) = r.field("err.message").or_else(|| r.field("error"))
                && err != r.msg
            {
                m = format!("{m}: {}", err.chars().take(100).collect::<String>());
            }
            *self.errors.entry(m).or_default() += 1;
        }
    }

    fn top<T>(map: &HashMap<String, T>, n: usize, key: impl Fn(&T) -> u64) -> Vec<(&String, &T)> {
        let mut v: Vec<_> = map.iter().collect();
        v.sort_by(|a, b| key(b.1).cmp(&key(a.1)).then(a.0.cmp(b.0)));
        v.truncate(n);
        v
    }

    pub fn print(mut self, top: usize, tz: Tz, color: bool, out: &mut impl Write) -> std::io::Result<()> {
        let b = |s: &'static str| if color { s } else { "" };
        let (bold, reset, dim) = (b(print::BOLD), b(print::RESET), b(print::DIM));
        let span = match (self.first, self.last) {
            (Some(f), Some(l)) => format!("  {dim}{} → {}{reset}", print::short_ts(f, tz), print::short_ts(l, tz)),
            _ => String::new(),
        };
        writeln!(out, "{bold}Records{reset}   {}{span}", thousands(self.total))?;
        write!(out, "{bold}Levels{reset}   ")?;
        for l in Level::ALL {
            let n = self.levels[l as usize];
            if n > 0 {
                write!(out, "  {}{}{reset} {}", b(print::level_color(l)), l.as_str(), thousands(n))?;
            }
        }
        if self.no_level > 0 {
            write!(out, "  unparsed {}", thousands(self.no_level))?;
        }
        writeln!(out)?;
        if self.requests > 0 {
            let r = self.requests as f64;
            write!(out, "{bold}Requests{reset}  {}", thousands(self.requests))?;
            for (i, n) in self.classes.iter().enumerate().skip(1) {
                if *n > 0 {
                    write!(out, "   {i}xx {} ({:.1}%)", thousands(*n), *n as f64 * 100.0 / r)?;
                }
            }
            writeln!(out)?;
            self.durs.sort_by(f64::total_cmp);
            if !self.durs.is_empty() {
                let d = &self.durs;
                writeln!(
                    out,
                    "{bold}Latency{reset}   p50 {}   p90 {}   p95 {}   p99 {}   max {}",
                    ms(pct(d, 50.0)),
                    ms(pct(d, 90.0)),
                    ms(pct(d, 95.0)),
                    ms(pct(d, 99.0)),
                    ms(d.last().copied())
                )?;
            }
        }
        if !self.paths.is_empty() {
            writeln!(out, "\n{bold}Top routes{reset}")?;
            writeln!(out, "{dim}{:>9} {:>7} {:>7} {:>9} {:>9}  ROUTE{reset}", "COUNT", "4XX", "5XX", "P50", "P95")?;
            let mut paths = std::mem::take(&mut self.paths);
            for v in paths.values_mut() {
                v.durs.sort_by(f64::total_cmp);
            }
            for (k, v) in Self::top(&paths, top, |p| p.count) {
                writeln!(
                    out,
                    "{:>9} {:>7} {:>7} {:>9} {:>9}  {k}",
                    thousands(v.count),
                    thousands(v.c4xx),
                    thousands(v.c5xx),
                    ms(pct(&v.durs, 50.0)),
                    ms(pct(&v.durs, 95.0))
                )?;
            }
        }
        if !self.users.is_empty() {
            writeln!(out, "\n{bold}Top users{reset}")?;
            writeln!(out, "{dim}{:>9} {:>7}  USER{reset}", "RECORDS", "ERRORS")?;
            for (k, v) in Self::top(&self.users, top, |u| u.count) {
                writeln!(out, "{:>9} {:>7}  {k}", thousands(v.count), thousands(v.errors))?;
            }
        }
        if !self.errors.is_empty() {
            writeln!(out, "\n{bold}Top errors{reset}")?;
            for (k, v) in Self::top(&self.errors, top, |n| *n) {
                writeln!(out, "{:>9}  {k}", thousands(*v))?;
            }
        }
        if self.loggers.len() > 1 {
            writeln!(out, "\n{bold}Loggers{reset}")?;
            for (k, v) in Self::top(&self.loggers, top, |n| *n) {
                writeln!(out, "{:>9}  {k}", thousands(*v))?;
            }
        }
        Ok(())
    }

    pub fn into_json(mut self, top: usize, tz: Tz) -> serde_json::Value {
        self.durs.sort_by(f64::total_cmp);
        let d = &self.durs;
        let levels: serde_json::Map<String, serde_json::Value> =
            Level::ALL.iter().map(|l| (l.as_str().to_string(), self.levels[*l as usize].into())).collect();
        let mut paths = std::mem::take(&mut self.paths);
        for v in paths.values_mut() {
            v.durs.sort_by(f64::total_cmp);
        }
        json!({
            "records": self.total,
            "first": self.first.map(|t| rotlog::time::format_rfc3339(t, tz)),
            "last": self.last.map(|t| rotlog::time::format_rfc3339(t, tz)),
            "levels": levels,
            "unparsed": self.no_level,
            "requests": {
                "total": self.requests,
                "1xx": self.classes[1], "2xx": self.classes[2], "3xx": self.classes[3],
                "4xx": self.classes[4], "5xx": self.classes[5],
                "latency_ms": { "p50": pct(d, 50.0), "p90": pct(d, 90.0), "p95": pct(d, 95.0), "p99": pct(d, 99.0), "max": d.last() },
            },
            "routes": Self::top(&paths, top, |p| p.count).iter().map(|(k, v)| json!({
                "route": k, "count": v.count, "4xx": v.c4xx, "5xx": v.c5xx,
                "p50_ms": pct(&v.durs, 50.0), "p95_ms": pct(&v.durs, 95.0),
            })).collect::<Vec<_>>(),
            "users": Self::top(&self.users, top, |u| u.count).iter().map(|(k, v)| json!({
                "user": k, "records": v.count, "errors": v.errors,
            })).collect::<Vec<_>>(),
            "errors": Self::top(&self.errors, top, |n| *n).iter().map(|(k, v)| json!({ "message": k, "count": v })).collect::<Vec<_>>(),
            "loggers": self.loggers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes() {
        assert_eq!(route_of("/users/42/orders?x=1"), "/users/:id/orders");
        assert_eq!(route_of("/o/3f2b8c1e-9a4d-4c2b-8f1e-1234567890ab"), "/o/:id");
        assert_eq!(route_of("/health"), "/health");
    }
}
