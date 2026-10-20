//! Talking to the process that owns a stream. The owner holds an flock on
//! `.<name>.lock`; requests are dropped as `.<name>.ctl`, served by the writer
//! thread within ~250 ms, and answered through `.<name>.ctl.resp`.

use std::fs;
use std::time::{Duration, Instant};

use crate::rotation::Paths;
use crate::sys;

/// Pid of the process currently writing the stream, if any.
pub fn owner(paths: &Paths) -> Option<u32> {
    let path = paths.lock();
    if !path.exists() {
        return None;
    }
    let f = sys::open_lock_file(&path).ok()?;
    match sys::try_lock(&f) {
        Ok(false) => fs::read_to_string(&path).ok()?.trim().parse().ok().or(Some(0)),
        _ => None, // we got the lock, so nobody holds it; dropping `f` releases it
    }
}

/// Sends `cmd` (`rotate`, `flush`, `maintain`, `status`, `level [lvl]`) and returns the reply text.
pub fn send(paths: &Paths, cmd: &str, timeout: Duration) -> Result<String, String> {
    let Some(pid) = owner(paths) else {
        return Err(format!("no running process is writing stream '{}' in {}", paths.name, paths.dir.display()));
    };
    let ctl = paths.ctl();
    let resp = paths.ctl_response();
    let deadline = Instant::now() + timeout;
    // One request at a time per stream.
    while ctl.exists() {
        if Instant::now() > deadline {
            return Err("another control request is pending".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = fs::remove_file(&resp);
    let tmp = ctl.with_extension("ctl.tmp");
    fs::write(&tmp, cmd).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &ctl).map_err(|e| e.to_string())?;
    loop {
        if let Ok(reply) = fs::read_to_string(&resp) {
            let _ = fs::remove_file(&resp);
            return match reply.split_once(' ').unwrap_or((reply.as_str(), "")) {
                ("ok", rest) => Ok(rest.to_string()),
                (_, rest) => Err(rest.to_string()),
            };
        }
        if Instant::now() > deadline {
            let _ = fs::remove_file(&ctl);
            return Err(format!("pid {pid} did not answer within {}s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
