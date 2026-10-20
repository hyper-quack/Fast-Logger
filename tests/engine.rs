use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use rotlog::config::{Format, Size};
use rotlog::read::{Record, for_each_line};
use rotlog::rotation::{Every, Paths};
use rotlog::time::{MICROS, Tz};
use rotlog::{Config, Level, Logger};

fn tmpdir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "rotlog-it-{}-{tag}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn cfg(dir: &Path) -> Config {
    let mut c = Config { dir: dir.to_path_buf(), name: "t".into(), timezone: Tz::Utc, ..Config::default() };
    c.compress.enabled = false;
    c.maintenance_interval = rotlog::config::Dur(Duration::ZERO);
    c
}

fn all_records(paths: &Paths) -> Vec<(PathBuf, Record)> {
    let mut out = Vec::new();
    for f in rotlog::read::stream_files(paths, None, None) {
        for_each_line(&f, &mut |l| {
            out.push((f.clone(), Record::parse(l)));
            true
        })
        .unwrap();
    }
    out
}

#[test]
fn concurrent_writers_lose_nothing_and_stay_ordered() {
    let dir = tmpdir("conc");
    let log = Logger::open(cfg(&dir)).unwrap();
    std::thread::scope(|s| {
        for t in 0..8 {
            let log = log.clone();
            s.spawn(move || {
                for i in 0..5000 {
                    log.info("msg", &[("t", t.into()), ("i", i.into())]);
                }
            });
        }
    });
    log.flush().unwrap();
    let paths = Paths::new(log.config());
    let recs = all_records(&paths);
    assert_eq!(recs.len(), 40_000);
    let mut last = 0;
    for (_, r) in &recs {
        let ts = r.ts.unwrap();
        assert!(ts >= last, "timestamps must be non-decreasing in file order");
        last = ts;
    }
    // Per-thread sequence preserved.
    let mut next = [0u32; 8];
    for (_, r) in &recs {
        let t: usize = r.field("t").unwrap().parse().unwrap();
        let i: u32 = r.field("i").unwrap().parse().unwrap();
        assert_eq!(i, next[t]);
        next[t] += 1;
    }
    log.close();
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn time_rotation_puts_each_record_in_its_period() {
    let dir = tmpdir("time");
    let mut c = cfg(&dir);
    c.rotation.every = Every::Period(1);
    c.rotation.keep = 0;
    let log = Logger::open(c).unwrap();
    let start = std::time::Instant::now();
    let mut n = 0;
    while start.elapsed() < Duration::from_millis(2600) {
        log.info("tick", &[("n", n.into())]);
        n += 1;
        std::thread::sleep(Duration::from_micros(300));
    }
    log.flush().unwrap();
    let paths = Paths::new(log.config());
    let archives = paths.archives();
    assert!(archives.len() >= 2, "expected rotations, got {}", archives.len());
    let mut total = 0;
    for a in &archives {
        let label_start = a.start_micros(Tz::Utc).unwrap();
        for_each_line(&a.path, &mut |l| {
            let ts = Record::parse(l).ts.unwrap();
            assert!(
                ts >= label_start && ts < label_start + MICROS,
                "{} holds a record at {ts} outside [{label_start}, +1s)",
                a.file_name
            );
            total += 1;
            true
        })
        .unwrap();
    }
    for_each_line(&paths.active(), &mut |_| {
        total += 1;
        true
    })
    .unwrap();
    assert_eq!(total, n);
    log.close();
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn size_rotation_caps_files() {
    let dir = tmpdir("size");
    let mut c = cfg(&dir);
    c.rotation.every = Every::Never;
    c.rotation.max_size = Size(10_000);
    c.rotation.keep = 0;
    let log = Logger::open(c).unwrap();
    for i in 0..2000 {
        log.warn("payload payload payload payload payload", &[("i", i.into())]);
    }
    log.flush().unwrap();
    let paths = Paths::new(log.config());
    let archives = paths.archives();
    assert!(archives.len() >= 10);
    for a in &archives {
        let len = fs::metadata(&a.path).unwrap().len();
        assert!(len <= 10_000 && len > 9_000, "{} is {len} bytes", a.file_name);
    }
    assert_eq!(all_records(&paths).len(), 2000);
    log.close();
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn stale_active_file_is_archived_on_open() {
    let dir = tmpdir("stale");
    let active = dir.join("t.log");
    fs::write(&active, "2020-01-01T10:00:00.000000Z INFO  t: old\n").unwrap();
    let yesterday = SystemTime::now() - Duration::from_secs(2 * 86400);
    fs::File::options().write(true).open(&active).unwrap().set_modified(yesterday).unwrap();
    let log = Logger::open(cfg(&dir)).unwrap();
    log.info("new", &[]);
    log.flush().unwrap();
    let paths = Paths::new(log.config());
    let archives = paths.archives();
    assert_eq!(archives.len(), 1);
    assert!(fs::read_to_string(&archives[0].path).unwrap().contains(" old"));
    let cur = fs::read_to_string(&active).unwrap();
    assert!(cur.contains(" new") && !cur.contains(" old"));
    log.close();
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn second_writer_is_refused() {
    let dir = tmpdir("lock");
    let log = Logger::open(cfg(&dir)).unwrap();
    let err = Logger::open(cfg(&dir)).err().expect("second open must fail");
    assert!(err.to_string().contains("already being written"), "{err}");
    log.close();
    // Lock released on close.
    Logger::open(cfg(&dir)).unwrap().close();
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn compress_and_retention() {
    let dir = tmpdir("maint");
    let mut c = cfg(&dir);
    c.compress.enabled = true;
    c.rotation.keep = 2;
    let log = Logger::open(c).unwrap();
    for i in 0..5 {
        log.info("x", &[("i", i.into())]);
        assert!(log.rotate().unwrap());
    }
    assert!(!log.rotate().unwrap(), "empty file is not rotated");
    log.maintain_now(true);
    let paths = Paths::new(log.config());
    let archives = paths.archives();
    assert_eq!(archives.len(), 2, "{archives:?}");
    assert!(archives.iter().all(|a| a.compressed));
    let recs = all_records(&paths);
    let is: Vec<&str> = recs.iter().filter_map(|(_, r)| r.field("i")).collect();
    assert_eq!(is, ["3", "4"]);
    log.close();
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn json_format_and_children() {
    let dir = tmpdir("json");
    let mut c = cfg(&dir);
    c.format = Format::Json;
    c.level = Level::Debug;
    let log = Logger::open(c).unwrap();
    let child = log.named("db").with(&[("pool", "main".into())]);
    child.debug("query", &[("ms", 1.25.into())]);
    log.trace("hidden", &[]);
    log.log_json(Level::Error, Some("http"), "boom", r#"{"status":500,"user":{"id":"u1"}}"#);
    log.request(
        &rotlog::Request {
            method: "GET",
            path: "/x",
            status: 404,
            duration: Duration::from_micros(1500),
            ..Default::default()
        },
        &[],
    );
    log.flush().unwrap();
    let text = fs::read_to_string(dir.join("t.log")).unwrap();
    let lines: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["logger"], "db");
    assert_eq!(lines[0]["pool"], "main");
    assert_eq!(lines[0]["ms"], 1.25);
    assert_eq!(lines[1]["level"], "error");
    assert_eq!(lines[1]["user"]["id"], "u1");
    assert_eq!(lines[2]["level"], "warn");
    assert_eq!(lines[2]["dur_ms"], 1.5);
    log.close();
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn control_channel() {
    let dir = tmpdir("ctl");
    let log = Logger::open(cfg(&dir)).unwrap();
    let paths = Paths::new(log.config());
    assert_eq!(rotlog::control::owner(&paths), Some(std::process::id()));
    let r = rotlog::control::send(&paths, "level debug", Duration::from_secs(5)).unwrap();
    assert_eq!(r, "level=debug");
    assert_eq!(log.level(), Level::Debug);
    log.info("a", &[]);
    let r = rotlog::control::send(&paths, "rotate", Duration::from_secs(5)).unwrap();
    assert_eq!(r, "rotated");
    assert_eq!(paths.archives().len(), 1);
    let status = rotlog::control::send(&paths, "status", Duration::from_secs(5)).unwrap();
    let v: serde_json::Value = serde_json::from_str(&status).unwrap();
    assert_eq!(v["records"], 1);
    assert!(rotlog::control::send(&paths, "bogus", Duration::from_secs(5)).is_err());
    log.close();
    assert_eq!(rotlog::control::owner(&paths), None);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn drop_overflow_counts_instead_of_blocking() {
    let dir = tmpdir("drop");
    let mut c = cfg(&dir);
    c.overflow = rotlog::config::Overflow::Drop;
    c.max_buffer = Size(4096);
    c.buffer_size = Size(4096);
    c.flush_interval = rotlog::config::Dur(Duration::from_secs(5));
    let log = Logger::open(c).unwrap();
    for _ in 0..10_000 {
        log.info("0123456789012345678901234567890123456789", &[]);
    }
    log.flush().unwrap();
    let s = log.stats();
    assert_eq!(s.records + s.dropped, 10_000);
    log.close();
    fs::remove_dir_all(&dir).unwrap();
}
