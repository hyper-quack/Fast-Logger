//! Archive lifecycle: compress → upload → retention. Runs on the logger's
//! maintenance thread and from `rotlog maintain`; a per-stream lock keeps
//! concurrent runs (several processes, cron + app) from stepping on each other.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::compress;
use crate::config::{Config, Format};
use crate::rotation::{Archive, Paths};
use crate::sys;
use crate::upload::Uploader;

pub enum Event<'a> {
    Compressed { file: &'a str, before: u64, after: u64 },
    Uploaded { file: &'a str, target: &'a str, bytes: u64, existed: bool },
    UploadFailed { file: &'a str, error: &'a str },
    Deleted { file: &'a str, reason: &'static str },
    Error { msg: &'a str },
}

#[derive(Clone, Debug)]
pub struct Options {
    pub compress: bool,
    pub upload: bool,
    pub prune: bool,
    /// Ignore `compress.after` / `upload.after`.
    pub force: bool,
    /// Report what would happen without touching anything.
    pub dry_run: bool,
    /// Wait for a concurrent run instead of skipping.
    pub wait_lock: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { compress: true, upload: true, prune: true, force: false, dry_run: false, wait_lock: false }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub compressed: u32,
    pub uploaded: u32,
    pub deleted: u32,
    pub errors: u32,
    /// Another process was running maintenance for this stream.
    pub busy: bool,
    /// Planned actions (dry run).
    pub planned: Vec<String>,
}

const MAX_CONSECUTIVE_UPLOAD_FAILURES: u32 = 3;

pub fn run(cfg: &Config, uploader: Option<&dyn Uploader>, opts: &Options, on: &mut dyn FnMut(Event)) -> Report {
    let paths = Paths::new(cfg);
    let mut report = Report::default();
    if !paths.dir.is_dir() {
        return report;
    }
    let lock = match sys::open_lock_file(&paths.maint_lock()) {
        Ok(f) => f,
        Err(e) => {
            on(Event::Error { msg: &format!("open {}: {e}", paths.maint_lock().display()) });
            report.errors += 1;
            return report;
        }
    };
    let locked = if opts.wait_lock { sys::lock(&lock).is_ok() } else { sys::try_lock(&lock).unwrap_or(false) };
    if !locked {
        report.busy = true;
        return report;
    }
    if !opts.dry_run {
        remove_stale_tmp(&paths);
    }

    let now = SystemTime::now();
    let age = |p: &Path| -> Duration {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .map(|t| now.duration_since(t).unwrap_or_default())
            .unwrap_or_default()
    };

    if opts.compress && cfg.compress.enabled {
        for a in paths.archives().into_iter().filter(|a| !a.compressed) {
            if !opts.force && age(&a.path) < cfg.compress.after.0 {
                continue;
            }
            if opts.dry_run {
                report.planned.push(format!("compress {}", a.file_name));
                continue;
            }
            match compress::gzip_file(&a.path, cfg.compress.level) {
                Ok((before, after, _)) => {
                    report.compressed += 1;
                    on(Event::Compressed { file: &a.file_name, before, after });
                }
                // Another process got there first.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists || e.kind() == std::io::ErrorKind::NotFound => {
                }
                Err(e) => {
                    report.errors += 1;
                    on(Event::Error { msg: &format!("compress {}: {e}", a.file_name) });
                }
            }
        }
    }

    let mut uploaded = read_manifest(&paths);
    if opts.upload && cfg.upload.active() {
        match uploader {
            None => {
                report.errors += 1;
                on(Event::Error { msg: "upload is configured but no uploader is available" });
            }
            Some(up) => {
                let host = sys::hostname();
                let mut failures = 0;
                for a in paths.archives() {
                    if uploaded.contains(&a.file_name) {
                        continue;
                    }
                    // Wait for compression so each archive is uploaded once, compressed.
                    if cfg.compress.enabled && !a.compressed {
                        continue;
                    }
                    if !opts.force && age(&a.path) < cfg.upload.after.0 {
                        continue;
                    }
                    let object = object_name(cfg, &host, &a);
                    if opts.dry_run {
                        report.planned.push(format!("upload {} -> {}", a.file_name, up.url(&object)));
                        continue;
                    }
                    let ct = content_type(cfg.format, a.compressed);
                    match up.upload(&a.path, &object, ct) {
                        Ok(u) => {
                            failures = 0;
                            if let Err(e) = append_manifest(&paths, &a.file_name) {
                                on(Event::Error { msg: &format!("record upload of {}: {e}", a.file_name) });
                            }
                            uploaded.insert(a.file_name.clone());
                            report.uploaded += 1;
                            on(Event::Uploaded {
                                file: &a.file_name,
                                target: &up.url(&object),
                                bytes: u.bytes,
                                existed: u.existed,
                            });
                            if cfg.upload.delete_local && fs::remove_file(&a.path).is_ok() {
                                report.deleted += 1;
                                on(Event::Deleted { file: &a.file_name, reason: "uploaded" });
                            }
                        }
                        Err(e) => {
                            report.errors += 1;
                            on(Event::UploadFailed { file: &a.file_name, error: &e });
                            failures += 1;
                            if failures >= MAX_CONSECUTIVE_UPLOAD_FAILURES {
                                break; // likely an outage; retry on the next run
                            }
                        }
                    }
                }
            }
        }
    }

    if opts.prune {
        let archives = paths.archives();
        let keep = cfg.rotation.keep as usize;
        let max_age = cfg.rotation.max_age.0;
        let n = archives.len();
        for (i, a) in archives.iter().enumerate() {
            let newer = n - i - 1;
            let too_many = keep > 0 && newer >= keep;
            let too_old = !max_age.is_zero() && age(&a.path) > max_age;
            if !(too_many || too_old) {
                continue;
            }
            // Never delete what has not reached remote storage yet.
            if cfg.upload.active() && !uploaded.contains(&a.file_name) {
                continue;
            }
            let reason = if too_old { "max_age" } else { "keep" };
            if opts.dry_run {
                report.planned.push(format!("delete {} ({reason})", a.file_name));
                continue;
            }
            if fs::remove_file(&a.path).is_ok() {
                report.deleted += 1;
                on(Event::Deleted { file: &a.file_name, reason });
            }
        }
        if !opts.dry_run {
            compact_manifest(&paths, &uploaded);
        }
    }
    report
}

/// Expands the `upload.object` template for an archive.
pub fn object_name(cfg: &Config, host: &str, a: &Archive) -> String {
    let l = &a.label; // YYYY-MM-DD_HH-MM-SS
    cfg.upload
        .object
        .replace("{prefix}", &cfg.upload.prefix)
        .replace("{host}", host)
        .replace("{name}", &cfg.name)
        .replace("{file}", &a.file_name)
        .replace("{yyyy}", &l[0..4])
        .replace("{mm}", &l[5..7])
        .replace("{dd}", &l[8..10])
        .replace("{hh}", &l[11..13])
}

fn content_type(fmt: Format, gz: bool) -> &'static str {
    match (gz, fmt) {
        (true, _) => "application/gzip",
        (false, Format::Json) => "application/x-ndjson",
        (false, Format::Text) => "text/plain; charset=utf-8",
    }
}

/// Leftover `.gz.tmp` files are from interrupted runs (we hold the maintenance lock).
fn remove_stale_tmp(paths: &Paths) {
    let Ok(rd) = fs::read_dir(&paths.dir) else { return };
    let prefix = format!("{}.", paths.name);
    for e in rd.flatten() {
        let n = e.file_name();
        let Some(n) = n.to_str() else { continue };
        if n.starts_with(&prefix) && n.ends_with(".gz.tmp") {
            let _ = fs::remove_file(e.path());
        }
    }
}

pub fn read_manifest(paths: &Paths) -> HashSet<String> {
    fs::read_to_string(paths.manifest())
        .map(|s| s.lines().filter(|l| !l.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

fn append_manifest(paths: &Paths, file: &str) -> std::io::Result<()> {
    let mut f = OpenOptions::new().create(true).append(true).open(paths.manifest())?;
    f.write_all(format!("{file}\n").as_bytes())?;
    f.sync_data()
}

/// Drops manifest entries for archives that no longer exist locally.
fn compact_manifest(paths: &Paths, uploaded: &HashSet<String>) {
    if uploaded.is_empty() {
        return;
    }
    let present: HashSet<String> = paths.archives().into_iter().map(|a| a.file_name).collect();
    if uploaded.iter().all(|f| present.contains(f)) {
        return;
    }
    let mut keep: Vec<&String> = uploaded.iter().filter(|f| present.contains(*f)).collect();
    keep.sort();
    let body: String = keep.iter().map(|f| format!("{f}\n")).collect();
    let tmp = paths.manifest().with_extension("uploaded.tmp");
    if fs::write(&tmp, body).is_ok() {
        let _ = fs::rename(&tmp, paths.manifest());
    }
}
