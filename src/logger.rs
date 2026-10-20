//! The writer engine.
//!
//! Callers encode the record body on their own thread, then take one mutex to
//! stamp the time and append to a shared buffer. Taking the timestamp under the
//! lock makes file order equal time order, so a record always lands in the file
//! of the period it was stamped in. A background thread swaps the buffer out and
//! writes it in large batches, performing rotations at the exact byte offsets
//! recorded by producers. A second thread runs compression, upload and retention.

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::{Config, Console, Fsync, Overflow};
use crate::format::{self, Fields, Value};
use crate::level::Level;
use crate::maintain::{self, Event};
use crate::rotation::{Every, Paths};
use crate::sys;
use crate::time::{TsFormatter, now_micros, system_time_micros};
use crate::upload::{self, Uploader};

/// How often the writer looks for control requests and a moved/deleted active file.
const CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// How long `close` waits for an in-flight compression/upload run.
const MAINT_GRACE: Duration = Duration::from_secs(3);

/// A handle to a log stream. Cloning is cheap; the stream is flushed and closed
/// when the last clone is dropped (or on [`Logger::close`]).
#[derive(Clone)]
pub struct Logger {
    core: Arc<Core>,
    name: Arc<str>,
    bound: Arc<[u8]>,
}

/// Closes the stream when the last user handle goes away.
struct Core {
    inner: Arc<Inner>,
}

impl Drop for Core {
    fn drop(&mut self) {
        self.inner.shutdown();
    }
}

pub(crate) struct Inner {
    pub(crate) cfg: Config,
    pub(crate) paths: Paths,
    pub(crate) level: AtomicU8,
    state: Mutex<State>,
    writer_cv: Condvar,
    space_cv: Condvar,
    flushed_cv: Condvar,
    stats: Counters,
    maint: Arc<MaintSignal>,
    /// Background maintenance runs (off when `maintenance_interval = 0`).
    maint_thread: bool,
    writer: Mutex<Option<JoinHandle<()>>>,
    lock_file: Mutex<Option<File>>,
    every: Every,
    max_size: u64,
    threshold: usize,
    max_buffer: usize,
    immediate: bool,
    started: Instant,
}

/// Rotation point inside a pending buffer.
struct Cut {
    /// Byte offset where the new file starts.
    at: usize,
    /// Start time of the file being closed (names the archive).
    old_start: i64,
}

struct State {
    buf: Vec<u8>,
    cuts: Vec<Cut>,
    ts: TsFormatter,
    /// Size of the active file including pending bytes.
    file_bytes: u64,
    file_start: i64,
    next_boundary: i64,
    records: u64,
    bytes: u64,
    writer_sleeping: bool,
    space_waiters: u32,
    flush_req: u64,
    flush_done: u64,
    closing: bool,
    writer_gone: bool,
}

#[derive(Default)]
struct Counters {
    dropped: AtomicU64,
    rotations: AtomicU64,
    write_errors: AtomicU64,
    compressed: AtomicU64,
    uploaded: AtomicU64,
    upload_errors: AtomicU64,
    deleted: AtomicU64,
    last_error: Mutex<Option<String>>,
}

/// Point-in-time counters of a stream.
#[derive(Clone, Debug, Serialize)]
pub struct Stats {
    pub pid: u32,
    pub name: String,
    pub active: PathBuf,
    pub level: Level,
    pub uptime_secs: u64,
    pub records: u64,
    pub bytes: u64,
    pub dropped: u64,
    pub pending_bytes: u64,
    pub file_bytes: u64,
    pub rotations: u64,
    pub write_errors: u64,
    pub compressed: u64,
    pub uploaded: u64,
    pub upload_errors: u64,
    pub deleted: u64,
    /// Highest maintenance ticket served (see [`Logger::maintain_now`]).
    pub maintenance_completed: u64,
    pub last_error: Option<String>,
    /// The writer's own rotation/compression/upload settings.
    pub policy: String,
}

/// An HTTP request summary for [`Logger::request`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub status: u16,
    pub duration: Duration,
    pub ip: Option<&'a str>,
    pub user: Option<&'a str>,
    pub req_id: Option<&'a str>,
}

thread_local! {
    static BODY: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(512));
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

impl Logger {
    /// Opens (or creates) the stream described by `cfg` and starts its writer.
    /// Fails if another process is already writing the same stream.
    pub fn open(cfg: Config) -> io::Result<Logger> {
        cfg.validate().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        fs::create_dir_all(&cfg.dir)?;
        let paths = Paths::new(&cfg);
        let lock_file = acquire_stream_lock(&paths)?;

        let uploader = if cfg.upload.active() {
            Some(upload::from_config(&cfg.upload).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?)
        } else {
            None
        };

        let active = paths.active();
        let file = open_append(&active)?;
        let meta = file.metadata()?;
        let now = now_micros();
        let tz = cfg.timezone;
        let every = cfg.rotation.every;
        let (period_start, next_boundary) = every.bounds(now, tz).unwrap_or((i64::MIN, i64::MAX));
        let period_of = |t: i64| every.bounds(t, tz).map_or(t, |b| b.0);

        let mut cuts = Vec::new();
        let mut file_bytes = meta.len();
        let mut file_start = if period_start == i64::MIN { now } else { period_start };
        if file_bytes > 0 {
            let modified = meta.modified().map(system_time_micros).unwrap_or(now);
            let created = meta.created().map(system_time_micros).unwrap_or(modified).min(modified);
            let existing_start = period_of(created);
            let max_size = cfg.rotation.max_size.0;
            if modified < period_start || (max_size > 0 && file_bytes >= max_size) {
                // Left over from an earlier period (or already full): archive it first.
                cuts.push(Cut { at: 0, old_start: existing_start });
                if period_start == i64::MIN || modified >= period_start {
                    file_start = now;
                }
                file_bytes = 0;
            } else {
                file_start = existing_start;
            }
        }

        let max_buffer = cfg.max_buffer.0.max(1024) as usize;
        let inner = Arc::new(Inner {
            level: AtomicU8::new(cfg.level as u8),
            state: Mutex::new(State {
                buf: Vec::with_capacity((cfg.buffer_size.0 as usize).clamp(4096, 1 << 22)),
                cuts,
                ts: TsFormatter::new(tz),
                file_bytes,
                file_start,
                next_boundary,
                records: 0,
                bytes: 0,
                writer_sleeping: false,
                space_waiters: 0,
                flush_req: 0,
                flush_done: 0,
                closing: false,
                writer_gone: false,
            }),
            writer_cv: Condvar::new(),
            space_cv: Condvar::new(),
            flushed_cv: Condvar::new(),
            stats: Counters::default(),
            maint: Arc::new(MaintSignal::default()),
            maint_thread: !cfg.maintenance_interval.0.is_zero(),
            writer: Mutex::new(None),
            lock_file: Mutex::new(Some(lock_file)),
            every,
            max_size: cfg.rotation.max_size.0,
            threshold: (cfg.buffer_size.0 as usize).max(1),
            max_buffer,
            immediate: cfg.flush_interval.0.is_zero(),
            started: Instant::now(),
            paths,
            cfg,
        });

        let w = Arc::clone(&inner);
        let handle = thread::Builder::new().name("rotlog-writer".into()).spawn(move || w.writer_main(file))?;
        *lock(&inner.writer) = Some(handle);

        if inner.maint_thread {
            let weak = Arc::downgrade(&inner);
            let sig = Arc::clone(&inner.maint);
            let interval = inner.cfg.maintenance_interval.0.max(Duration::from_secs(1));
            thread::Builder::new()
                .name("rotlog-maint".into())
                .spawn(move || maint_main(weak, sig, interval, uploader))?;
        }

        let name: Arc<str> = Arc::from(inner.cfg.name.as_str());
        Ok(Logger { core: Arc::new(Core { inner }), name, bound: Arc::from(Vec::new()) })
    }

    #[inline]
    fn inner(&self) -> &Inner {
        &self.core.inner
    }

    pub fn config(&self) -> &Config {
        &self.inner().cfg
    }

    /// Logger name written with each record (defaults to the stream name).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// A handle writing to the same stream under another logger name.
    pub fn named(&self, name: &str) -> Logger {
        Logger { core: Arc::clone(&self.core), name: Arc::from(name), bound: Arc::clone(&self.bound) }
    }

    /// A handle that adds `fields` to every record.
    pub fn with(&self, fields: &[(&str, Value)]) -> Logger {
        let fmt = self.inner().cfg.format;
        let extra = format::encode_bound(fmt, fields);
        let bound = format::join_bound(fmt, &self.bound, &extra);
        Logger { core: Arc::clone(&self.core), name: Arc::clone(&self.name), bound: Arc::from(bound) }
    }

    #[inline]
    pub fn enabled(&self, level: Level) -> bool {
        level != Level::Off && level as u8 >= self.inner().level.load(Relaxed)
    }

    pub fn level(&self) -> Level {
        Level::from_u8(self.inner().level.load(Relaxed))
    }

    /// Changes the threshold at runtime (also possible with `rotlog level`).
    pub fn set_level(&self, level: Level) {
        self.inner().level.store(level as u8, Relaxed);
    }

    /// Address of the live level byte; lets FFI callers filter without a call.
    pub fn level_ptr(&self) -> *const u8 {
        self.inner().level.as_ptr() as *const u8
    }

    #[inline]
    pub fn log(&self, level: Level, msg: &str, fields: &[(&str, Value)]) {
        if self.enabled(level) {
            self.inner().emit(level, &self.name, msg, &self.bound, Fields::Kv(fields));
        }
    }

    /// Logs with fields given as a JSON object (`{"user":"u1","status":200}`).
    /// In JSON output the object is spliced verbatim, so it must be valid JSON.
    #[inline]
    pub fn log_json(&self, level: Level, name: Option<&str>, msg: &str, fields_json: &str) {
        if self.enabled(level) {
            let name = name.filter(|n| !n.is_empty()).unwrap_or(&self.name);
            self.inner().emit(level, name, msg, &self.bound, Fields::Json(fields_json));
        }
    }

    #[inline]
    pub fn trace(&self, msg: &str, fields: &[(&str, Value)]) {
        self.log(Level::Trace, msg, fields)
    }
    #[inline]
    pub fn debug(&self, msg: &str, fields: &[(&str, Value)]) {
        self.log(Level::Debug, msg, fields)
    }
    #[inline]
    pub fn info(&self, msg: &str, fields: &[(&str, Value)]) {
        self.log(Level::Info, msg, fields)
    }
    #[inline]
    pub fn warn(&self, msg: &str, fields: &[(&str, Value)]) {
        self.log(Level::Warn, msg, fields)
    }
    #[inline]
    pub fn error(&self, msg: &str, fields: &[(&str, Value)]) {
        self.log(Level::Error, msg, fields)
    }
    #[inline]
    pub fn fatal(&self, msg: &str, fields: &[(&str, Value)]) {
        self.log(Level::Fatal, msg, fields)
    }

    /// Logs an HTTP request: `error` for 5xx, `warn` for 4xx, `info` otherwise.
    pub fn request(&self, r: &Request, extra: &[(&str, Value)]) {
        let level = level_for_status(r.status);
        if !self.enabled(level) {
            return;
        }
        let msg = format!("{} {} {}", r.method, r.path, r.status);
        let dur_ms = (r.duration.as_micros() as f64 / 1000.0 * 1000.0).round() / 1000.0;
        let mut fields: Vec<(&str, Value)> = Vec::with_capacity(7 + extra.len());
        fields.push(("method", r.method.into()));
        fields.push(("path", r.path.into()));
        fields.push(("status", r.status.into()));
        fields.push(("dur_ms", dur_ms.into()));
        if let Some(v) = r.ip {
            fields.push(("ip", v.into()));
        }
        if let Some(v) = r.user {
            fields.push(("user", v.into()));
        }
        if let Some(v) = r.req_id {
            fields.push(("req_id", v.into()));
        }
        fields.extend_from_slice(extra);
        self.inner().emit(level, &self.name, &msg, &self.bound, Fields::Kv(&fields));
    }

    /// Blocks until every record logged before this call is written to disk
    /// (and fsynced when `fsync = "always"`).
    pub fn flush(&self) -> io::Result<()> {
        self.inner().flush()
    }

    /// Archives the active file now. Returns `false` if it was empty.
    pub fn rotate(&self) -> io::Result<bool> {
        let rotated = self.inner().cut_now();
        self.inner().flush()?;
        Ok(rotated)
    }

    /// Runs compression, upload and retention now; optionally waits for completion.
    /// Without a background thread (`maintenance_interval = 0`) this always runs inline.
    ///
    /// Returns a ticket: the run is complete once [`Stats::maintenance_completed`]
    /// reaches it (0 when it already ran inline).
    pub fn maintain_now(&self, wait: bool) -> u64 {
        let inner = self.inner();
        if !inner.maint_thread {
            let up = if inner.cfg.upload.active() { upload::from_config(&inner.cfg.upload).ok() } else { None };
            maintain::run(&inner.cfg, up.as_deref(), &maintain::Options::default(), &mut |ev| inner.on_maint_event(ev));
            return 0;
        }
        let target = inner.maint.request();
        if wait {
            inner.maint.wait(target);
        }
        target
    }

    pub fn stats(&self) -> Stats {
        self.inner().stats()
    }

    /// Flushes and stops the writer even if other clones are alive; they drop records afterwards.
    pub fn close(self) {
        self.inner().shutdown();
    }
}

pub fn level_for_status(status: u16) -> Level {
    match status {
        500.. => Level::Error,
        400..=499 => Level::Warn,
        _ => Level::Info,
    }
}

fn acquire_stream_lock(paths: &Paths) -> io::Result<File> {
    let path = paths.lock();
    let mut f = sys::open_lock_file(&path)?;
    if !sys::try_lock(&f)? {
        let pid = fs::read_to_string(&path).unwrap_or_default();
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "stream '{}' in {} is already being written by pid {} (use a different name per process)",
                paths.name,
                paths.dir.display(),
                pid.trim()
            ),
        ));
    }
    f.set_len(0)?;
    write!(f, "{}", std::process::id())?;
    Ok(f)
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    pub(crate) fn emit(&self, level: Level, name: &str, msg: &str, bound: &[u8], fields: Fields) {
        let fmt = self.cfg.format;
        let done = BODY.try_with(|cell| match cell.try_borrow_mut() {
            Ok(mut body) => {
                body.clear();
                format::encode_body(fmt, &mut body, level, name, msg, bound, fields);
                self.append(level, &body);
                if body.capacity() > 1 << 16 {
                    *body = Vec::with_capacity(512);
                }
                true
            }
            Err(_) => false,
        });
        if done != Ok(true) {
            // Re-entrant call or thread teardown: use a one-off buffer.
            let mut body = Vec::with_capacity(256);
            format::encode_body(fmt, &mut body, level, name, msg, bound, fields);
            self.append(level, &body);
        }
    }

    fn append(&self, level: Level, body: &[u8]) {
        let mut g = self.state();
        if g.buf.len() >= self.max_buffer && !g.closing {
            if self.cfg.overflow == Overflow::Drop {
                drop(g);
                self.stats.dropped.fetch_add(1, Relaxed);
                return;
            }
            while g.buf.len() >= self.max_buffer && !g.closing {
                g.space_waiters += 1;
                if g.writer_sleeping {
                    g.writer_sleeping = false;
                    self.writer_cv.notify_one();
                }
                g = self.space_cv.wait(g).unwrap_or_else(PoisonError::into_inner);
                g.space_waiters -= 1;
            }
        }
        if g.closing {
            drop(g);
            self.stats.dropped.fetch_add(1, Relaxed);
            return;
        }
        let st = &mut *g;
        let now = now_micros();
        if now >= st.next_boundary {
            self.cut_time(st, now);
        }
        let start = st.buf.len();
        format::write_header(self.cfg.format, &mut st.ts, now, &mut st.buf);
        st.buf.extend_from_slice(body);
        let n = (st.buf.len() - start) as u64;
        if self.max_size > 0 && st.file_bytes > 0 && st.file_bytes + n > self.max_size {
            st.cuts.push(Cut { at: start, old_start: st.file_start });
            st.file_start = now;
            st.file_bytes = 0;
        }
        st.file_bytes += n;
        st.records += 1;
        st.bytes += n;
        let urgent =
            self.immediate || st.buf.len() >= self.threshold || (self.cfg.flush_on_error && level >= Level::Error);
        if urgent && st.writer_sleeping {
            st.writer_sleeping = false;
            self.writer_cv.notify_one();
        }
    }

    /// Called under the state lock once `now` passed the scheduled boundary.
    fn cut_time(&self, st: &mut State, now: i64) {
        match self.every.bounds(now, self.cfg.timezone) {
            Some((start, next)) => {
                if st.file_bytes > 0 {
                    st.cuts.push(Cut { at: st.buf.len(), old_start: st.file_start });
                    st.file_bytes = 0;
                }
                st.file_start = start;
                st.next_boundary = next;
            }
            None => st.next_boundary = i64::MAX,
        }
    }

    /// Requests a rotation at the current end of the buffer.
    fn cut_now(&self) -> bool {
        let mut g = self.state();
        if g.file_bytes == 0 {
            return false;
        }
        let at = g.buf.len();
        let old_start = g.file_start;
        g.cuts.push(Cut { at, old_start });
        g.file_start = now_micros();
        g.file_bytes = 0;
        true
    }

    fn flush(&self) -> io::Result<()> {
        let mut g = self.state();
        if g.writer_gone {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "logger is closed"));
        }
        g.flush_req += 1;
        let target = g.flush_req;
        if g.writer_sleeping {
            g.writer_sleeping = false;
            self.writer_cv.notify_one();
        }
        while g.flush_done < target && !g.writer_gone {
            g = self.flushed_cv.wait(g).unwrap_or_else(PoisonError::into_inner);
        }
        Ok(())
    }

    fn shutdown(&self) {
        {
            let mut g = self.state();
            if !g.closing {
                g.closing = true;
                g.writer_sleeping = false;
                self.writer_cv.notify_one();
                self.space_cv.notify_all();
            }
        }
        if let Some(h) = lock(&self.writer).take() {
            let _ = h.join();
        }
        self.maint.stop(MAINT_GRACE);
        // Releases the flock so another process may take over the stream.
        lock(&self.lock_file).take();
    }

    fn note_error(&self, msg: String) {
        let mut last = lock(&self.stats.last_error);
        if last.as_deref() != Some(&msg) {
            eprintln!("rotlog: {msg}");
        }
        *last = Some(msg);
    }

    fn stats(&self) -> Stats {
        let (records, bytes, pending, file_bytes) = {
            let g = self.state();
            (g.records, g.bytes, g.buf.len() as u64, g.file_bytes)
        };
        let s = &self.stats;
        Stats {
            pid: std::process::id(),
            name: self.cfg.name.clone(),
            active: self.paths.active(),
            level: Level::from_u8(self.level.load(Relaxed)),
            uptime_secs: self.started.elapsed().as_secs(),
            records,
            bytes,
            dropped: s.dropped.load(Relaxed),
            pending_bytes: pending,
            file_bytes,
            rotations: s.rotations.load(Relaxed),
            write_errors: s.write_errors.load(Relaxed),
            compressed: s.compressed.load(Relaxed),
            uploaded: s.uploaded.load(Relaxed),
            upload_errors: s.upload_errors.load(Relaxed),
            deleted: s.deleted.load(Relaxed),
            maintenance_completed: lock(&self.maint.m).completed,
            last_error: lock(&s.last_error).clone(),
            policy: self.cfg.policy_summary(),
        }
    }

    // ---- writer thread -------------------------------------------------

    fn writer_main(&self, mut file: File) {
        let mut spare: Vec<u8> = Vec::with_capacity(self.threshold.clamp(4096, 1 << 22));
        let mut cuts: Vec<Cut> = Vec::new();
        let mut last_check = Instant::now();
        let base_wait = if self.immediate { CHECK_INTERVAL } else { self.cfg.flush_interval.0.min(CHECK_INTERVAL * 4) };
        loop {
            let closing = self.cycle(&mut file, &mut spare, &mut cuts, Some(base_wait));
            if closing {
                break;
            }
            if last_check.elapsed() >= CHECK_INTERVAL {
                last_check = Instant::now();
                self.check_active_file(&mut file);
                self.check_control(&mut file, &mut spare, &mut cuts);
            }
        }
        if self.cfg.fsync != Fsync::Never {
            let _ = file.sync_all();
        }
        let mut g = self.state();
        g.writer_gone = true;
        g.flush_done = g.flush_req;
        self.flushed_cv.notify_all();
        self.space_cv.notify_all();
    }

    /// Waits (unless work is pending), swaps the buffer out, writes it and
    /// acknowledges flush requests. Returns whether the stream is closing.
    fn cycle(&self, file: &mut File, spare: &mut Vec<u8>, cuts: &mut Vec<Cut>, wait: Option<Duration>) -> bool {
        let (flush_gen, closing) = {
            let mut g = self.state();
            let busy = |s: &State| {
                s.closing || s.flush_req != s.flush_done || s.buf.len() >= self.threshold || s.space_waiters > 0
            };
            if let Some(wait) = wait
                && !busy(&g)
            {
                let until_boundary = (g.next_boundary.saturating_sub(now_micros())).max(1000) as u64;
                let wait = wait.min(Duration::from_micros(until_boundary));
                g.writer_sleeping = true;
                g = self.writer_cv.wait_timeout(g, wait).unwrap_or_else(PoisonError::into_inner).0;
                g.writer_sleeping = false;
            }
            let now = now_micros();
            if now >= g.next_boundary {
                self.cut_time(&mut g, now);
            }
            std::mem::swap(&mut g.buf, spare);
            std::mem::swap(&mut g.cuts, cuts);
            if g.space_waiters > 0 {
                self.space_cv.notify_all();
            }
            (g.flush_req, g.closing)
        };
        if !spare.is_empty() || !cuts.is_empty() {
            self.write_batch(file, spare, cuts);
            spare.clear();
            cuts.clear();
            if spare.capacity() > (self.threshold * 4).max(1 << 22) {
                *spare = Vec::with_capacity(self.threshold.clamp(4096, 1 << 22));
            }
        }
        let mut g = self.state();
        if g.flush_done < flush_gen {
            g.flush_done = flush_gen;
            self.flushed_cv.notify_all();
        }
        closing
    }

    fn write_batch(&self, file: &mut File, buf: &[u8], cuts: &[Cut]) {
        let mut pos = 0;
        for c in cuts {
            self.write_all(file, &buf[pos..c.at]);
            self.rotate_file(file, c.old_start);
            pos = c.at;
        }
        self.write_all(file, &buf[pos..]);
        if self.cfg.fsync == Fsync::Always {
            let _ = file.sync_data();
        }
        match self.cfg.console {
            Console::Off => {}
            Console::Stdout => {
                let _ = io::stdout().lock().write_all(buf);
            }
            Console::Stderr => {
                let _ = io::stderr().lock().write_all(buf);
            }
        }
    }

    /// Writes everything, retrying transient failures (disk full, EIO) for ~10s
    /// before giving up on the batch.
    fn write_all(&self, file: &mut File, mut data: &[u8]) {
        let mut failures = 0;
        while !data.is_empty() {
            match file.write(data) {
                Ok(0) => failures = self.write_failed(failures, "write returned 0 bytes".into(), data.len()),
                Ok(n) => data = &data[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => failures = self.write_failed(failures, e.to_string(), data.len()),
            }
            if failures == usize::MAX {
                return;
            }
        }
    }

    fn write_failed(&self, failures: usize, err: String, remaining: usize) -> usize {
        self.stats.write_errors.fetch_add(1, Relaxed);
        self.note_error(format!("write {}: {err}", self.paths.active().display()));
        if failures >= 100 {
            self.note_error(format!("giving up on {remaining} bytes after repeated write errors"));
            self.stats.dropped.fetch_add(1, Relaxed);
            return usize::MAX;
        }
        thread::sleep(Duration::from_millis(100));
        failures + 1
    }

    fn rotate_file(&self, file: &mut File, old_start: i64) {
        if self.cfg.fsync != Fsync::Never {
            let _ = file.sync_all();
        }
        let active = self.paths.active();
        let archive = self.paths.new_archive(old_start);
        match fs::rename(&active, &archive) {
            Ok(()) => {
                self.stats.rotations.fetch_add(1, Relaxed);
            }
            Err(e) => self.note_error(format!("rotate {} -> {}: {e}", active.display(), archive.display())),
        }
        match open_append(&active) {
            Ok(f) => *file = f,
            Err(e) => self.note_error(format!("reopen {}: {e}", active.display())),
        }
        if self.maint_thread {
            self.maint.request();
        }
    }

    /// Reopens the active file if it was moved or deleted behind our back.
    fn check_active_file(&self, file: &mut File) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let active = self.paths.active();
            let reopen = match (fs::metadata(&active), file.metadata()) {
                (Ok(a), Ok(f)) => a.ino() != f.ino() || a.dev() != f.dev(),
                (Err(e), _) => e.kind() == io::ErrorKind::NotFound,
                _ => false,
            };
            if reopen && let Ok(f) = open_append(&active) {
                let len = f.metadata().map(|m| m.len()).unwrap_or(0);
                *file = f;
                let mut g = self.state();
                g.file_bytes = len + g.buf.len() as u64;
            }
        }
    }

    /// Serves a request dropped by `rotlog ctl` (see [`crate::control`]).
    fn check_control(&self, file: &mut File, spare: &mut Vec<u8>, cuts: &mut Vec<Cut>) {
        let ctl = self.paths.ctl();
        let Ok(cmd) = fs::read_to_string(&ctl) else { return };
        let _ = fs::remove_file(&ctl);
        let mut parts = cmd.split_whitespace();
        let reply = match (parts.next().unwrap_or(""), parts.next()) {
            ("rotate", _) => {
                let rotated = self.cut_now();
                self.cycle(file, spare, cuts, None);
                if rotated {
                    "ok rotated".to_string()
                } else {
                    "ok nothing to rotate (active file is empty)".to_string()
                }
            }
            ("flush", _) => {
                self.cycle(file, spare, cuts, None);
                "ok flushed".to_string()
            }
            ("maintain", _) if self.maint_thread => {
                self.maint.request();
                "ok maintenance started".to_string()
            }
            ("maintain", _) => "error background maintenance is disabled; run `rotlog maintain`".to_string(),
            ("level", Some(l)) => match Level::parse(l) {
                Some(l) => {
                    self.level.store(l as u8, Relaxed);
                    format!("ok level={l}")
                }
                None => format!("error invalid level '{l}'"),
            },
            ("level", None) => format!("ok level={}", Level::from_u8(self.level.load(Relaxed))),
            ("status", _) => format!("ok {}", serde_json::to_string(&self.stats()).unwrap_or_default()),
            (other, _) => format!("error unknown command '{other}'"),
        };
        let resp = self.paths.ctl_response();
        let tmp = resp.with_extension("resp.tmp");
        if fs::write(&tmp, reply).is_ok() {
            let _ = fs::rename(&tmp, &resp);
        }
    }

    fn on_maint_event(&self, ev: Event) {
        match ev {
            Event::Compressed { file, before, after } => {
                self.stats.compressed.fetch_add(1, Relaxed);
                self.emit_internal(
                    Level::Debug,
                    "archive compressed",
                    &[("file", file.into()), ("bytes", before.into()), ("gz_bytes", after.into())],
                );
            }
            Event::Uploaded { file, target, bytes, existed } => {
                self.stats.uploaded.fetch_add(1, Relaxed);
                self.emit_internal(
                    Level::Info,
                    "archive uploaded",
                    &[
                        ("file", file.into()),
                        ("target", target.into()),
                        ("bytes", bytes.into()),
                        ("already_present", existed.into()),
                    ],
                );
            }
            Event::Deleted { file, reason } => {
                self.stats.deleted.fetch_add(1, Relaxed);
                self.emit_internal(
                    Level::Debug,
                    "archive deleted",
                    &[("file", file.into()), ("reason", reason.into())],
                );
            }
            Event::UploadFailed { file, error } => {
                self.stats.upload_errors.fetch_add(1, Relaxed);
                self.note_error(format!("upload {file}: {error}"));
                self.emit_internal(
                    Level::Error,
                    "archive upload failed",
                    &[("file", file.into()), ("error", error.into())],
                );
            }
            Event::Error { msg } => {
                self.note_error(msg.to_string());
                self.emit_internal(Level::Error, "maintenance error", &[("error", msg.into())]);
            }
        }
    }

    fn emit_internal(&self, level: Level, msg: &str, fields: &[(&str, Value)]) {
        if level as u8 >= self.level.load(Relaxed) {
            self.emit(level, "rotlog", msg, &[], Fields::Kv(fields));
        }
    }
}

// ---- maintenance thread ------------------------------------------------

#[derive(Default)]
struct MaintSignal {
    m: Mutex<MaintState>,
    cv: Condvar,
}

#[derive(Default)]
struct MaintState {
    requested: u64,
    completed: u64,
    stop: bool,
    running: bool,
}

impl MaintSignal {
    fn request(&self) -> u64 {
        let mut s = lock(&self.m);
        s.requested += 1;
        self.cv.notify_all();
        s.requested
    }

    fn wait(&self, target: u64) {
        let mut s = lock(&self.m);
        while s.completed < target && !s.stop {
            s = self.cv.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Stops the thread, giving an in-flight run up to `grace` to finish
    /// (interrupted runs are safe, but leave work for the next one).
    fn stop(&self, grace: Duration) {
        let deadline = Instant::now() + grace;
        let mut s = lock(&self.m);
        s.stop = true;
        self.cv.notify_all();
        while s.running {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            s = self.cv.wait_timeout(s, left).unwrap_or_else(PoisonError::into_inner).0;
        }
    }
}

fn maint_main(weak: Weak<Inner>, sig: Arc<MaintSignal>, interval: Duration, uploader: Option<Box<dyn Uploader>>) {
    let mut first = true;
    loop {
        let serving = {
            let mut s = lock(&sig.m);
            if !first && s.requested == s.completed && !s.stop {
                s = sig.cv.wait_timeout(s, interval).unwrap_or_else(PoisonError::into_inner).0;
            }
            if s.stop {
                return;
            }
            s.running = true;
            s.requested
        };
        first = false;
        let alive = match weak.upgrade() {
            Some(inner) => {
                let opts = maintain::Options::default();
                maintain::run(&inner.cfg, uploader.as_deref(), &opts, &mut |ev| inner.on_maint_event(ev));
                true
            }
            None => false,
        };
        let mut s = lock(&sig.m);
        s.running = false;
        s.completed = s.completed.max(serving);
        sig.cv.notify_all();
        if !alive {
            return;
        }
    }
}
