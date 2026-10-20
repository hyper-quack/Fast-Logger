//! C ABI used by the Bun binding (`bun:ffi`) and any other FFI host.
//!
//! All functions are panic-safe. Handles come from `rl_open` and must be
//! released exactly once with `rl_close`; strings are UTF-8 (pointer, length).

use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::slice;

use crate::config::Config;
use crate::level::Level;
use crate::logger::Logger;

thread_local! {
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

fn set_error(e: impl ToString) {
    let msg = e.to_string();
    let _ = LAST_ERROR.try_with(|l| *l.borrow_mut() = msg);
}

/// # Safety
/// `ptr` must be valid for `len` bytes (or null when `len` is 0).
unsafe fn bytes<'a>(ptr: *const u8, len: u32) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        // SAFETY: guaranteed by the caller.
        unsafe { slice::from_raw_parts(ptr, len as usize) }
    }
}

fn text(b: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(b)
}

fn copy_out(s: &str, buf: *mut u8, cap: u32) -> u32 {
    if !buf.is_null() && cap > 0 {
        let n = s.len().min(cap as usize);
        // SAFETY: the caller provides `cap` writable bytes at `buf`.
        unsafe { std::ptr::copy_nonoverlapping(s.as_ptr(), buf, n) };
    }
    s.len() as u32
}

/// Opens a stream from a JSON config. Returns null on error (see `rl_last_error`).
///
/// # Safety
/// `cfg` must be valid for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rl_open(cfg: *const u8, len: u32) -> *mut Logger {
    // SAFETY: forwarded caller guarantee.
    let raw = unsafe { bytes(cfg, len) };
    let r = catch_unwind(AssertUnwindSafe(|| {
        let json = text(raw);
        let cfg = if json.trim().is_empty() { Ok(Config::default()) } else { Config::from_json(&json) };
        match cfg.and_then(|c| Logger::open(c).map_err(|e| e.to_string())) {
            Ok(l) => Box::into_raw(Box::new(l)),
            Err(e) => {
                set_error(e);
                std::ptr::null_mut()
            }
        }
    }));
    r.unwrap_or_else(|_| {
        set_error("panic while opening logger");
        std::ptr::null_mut()
    })
}

/// Copies the last error of this thread into `buf`; returns its full length.
#[unsafe(no_mangle)]
pub extern "C" fn rl_last_error(buf: *mut u8, cap: u32) -> u32 {
    LAST_ERROR.try_with(|l| copy_out(&l.borrow(), buf, cap)).unwrap_or(0)
}

/// Logs one record. `buf` holds `name`, `msg` and `fields` (a JSON object,
/// possibly empty) back to back. An empty name uses the stream name.
///
/// # Safety
/// `h` must come from `rl_open` and not be closed; `buf` must be valid for
/// `name_len + msg_len + fields_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rl_log(
    h: *const Logger,
    level: u32,
    buf: *const u8,
    name_len: u32,
    msg_len: u32,
    fields_len: u32,
) -> i32 {
    // SAFETY: caller guarantees a live handle.
    let Some(l) = (unsafe { h.as_ref() }) else { return -1 };
    let level = Level::from_u8(level.min(6) as u8);
    if !l.enabled(level) {
        return 0;
    }
    let total = name_len as u64 + msg_len as u64 + fields_len as u64;
    if total > u32::MAX as u64 {
        return -1;
    }
    // SAFETY: caller guarantees `buf` covers all three segments.
    let all = unsafe { bytes(buf, total as u32) };
    let (name, rest) = all.split_at(name_len as usize);
    let (msg, fields) = rest.split_at(msg_len as usize);
    catch_unwind(AssertUnwindSafe(|| {
        let name = text(name);
        l.log_json(level, Some(&name), &text(msg), &text(fields));
        0
    }))
    .unwrap_or(-1)
}

/// Pointer to the live level threshold byte (0 = trace … 6 = off). Valid until `rl_close`.
///
/// # Safety
/// `h` must come from `rl_open` and not be closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rl_level_ptr(h: *const Logger) -> *const u8 {
    // SAFETY: caller guarantees a live handle.
    unsafe { h.as_ref() }.map_or(std::ptr::null(), Logger::level_ptr)
}

/// # Safety
/// `h` must come from `rl_open` and not be closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rl_set_level(h: *const Logger, level: u32) {
    // SAFETY: caller guarantees a live handle.
    if let Some(l) = unsafe { h.as_ref() } {
        l.set_level(Level::from_u8(level.min(6) as u8));
    }
}

/// Blocks until everything logged so far is on disk. 0 on success.
///
/// # Safety
/// `h` must come from `rl_open` and not be closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rl_flush(h: *const Logger) -> i32 {
    // SAFETY: caller guarantees a live handle.
    let Some(l) = (unsafe { h.as_ref() }) else { return -1 };
    match catch_unwind(AssertUnwindSafe(|| l.flush())) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => {
            set_error(e);
            -1
        }
        Err(_) => -1,
    }
}

/// Rotates now. 1 = rotated, 0 = active file was empty, -1 = error.
///
/// # Safety
/// `h` must come from `rl_open` and not be closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rl_rotate(h: *const Logger) -> i32 {
    // SAFETY: caller guarantees a live handle.
    let Some(l) = (unsafe { h.as_ref() }) else { return -1 };
    match catch_unwind(AssertUnwindSafe(|| l.rotate())) {
        Ok(Ok(r)) => r as i32,
        Ok(Err(e)) => {
            set_error(e);
            -1
        }
        Err(_) => -1,
    }
}

/// Triggers compression/upload/retention; with `wait != 0`, blocks until done.
/// Returns a ticket: done once `maintenance_completed` in `rl_stats` reaches it
/// (0 = already done).
///
/// # Safety
/// `h` must come from `rl_open` and not be closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rl_maintain(h: *const Logger, wait: u32) -> u32 {
    // SAFETY: caller guarantees a live handle.
    let Some(l) = (unsafe { h.as_ref() }) else { return 0 };
    catch_unwind(AssertUnwindSafe(|| l.maintain_now(wait != 0) as u32)).unwrap_or(0)
}

/// Writes stats as JSON into `buf`; returns the full length (call again with a bigger buffer if larger than `cap`).
///
/// # Safety
/// `h` must come from `rl_open` and not be closed; `buf` must be writable for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rl_stats(h: *const Logger, buf: *mut u8, cap: u32) -> u32 {
    // SAFETY: caller guarantees a live handle.
    let Some(l) = (unsafe { h.as_ref() }) else { return 0 };
    catch_unwind(AssertUnwindSafe(|| {
        let json = serde_json::to_string(&l.stats()).unwrap_or_default();
        copy_out(&json, buf, cap)
    }))
    .unwrap_or(0)
}

/// Flushes, stops background threads and frees the handle.
///
/// # Safety
/// `h` must come from `rl_open`; it must not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rl_close(h: *mut Logger) {
    if h.is_null() {
        return;
    }
    // SAFETY: the caller transfers ownership of a handle created by `rl_open`.
    let l = unsafe { Box::from_raw(h) };
    let _ = catch_unwind(AssertUnwindSafe(|| l.close()));
}

/// Library version, e.g. `0.1.0`.
#[unsafe(no_mangle)]
pub extern "C" fn rl_version(buf: *mut u8, cap: u32) -> u32 {
    copy_out(env!("CARGO_PKG_VERSION"), buf, cap)
}
