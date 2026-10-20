//! Thin OS helpers: advisory file locks and host name.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

pub fn open_lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)
}

/// Non-blocking exclusive `flock`. `Ok(false)` when another process holds it.
#[cfg(unix)]
pub fn try_lock(f: &File) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: flock on a valid descriptor owned by `f`.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    if e.kind() == io::ErrorKind::WouldBlock { Ok(false) } else { Err(e) }
}

/// Blocking exclusive `flock`.
#[cfg(unix)]
pub fn lock(f: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    loop {
        // SAFETY: flock on a valid descriptor owned by `f`.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

#[cfg(not(unix))]
pub fn try_lock(_f: &File) -> io::Result<bool> {
    Ok(true)
}

#[cfg(not(unix))]
pub fn lock(_f: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes into buf.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return "localhost".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let h = String::from_utf8_lossy(&buf[..end]).into_owned();
    if h.is_empty() { "localhost".into() } else { h }
}

#[cfg(not(unix))]
pub fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".into())
}

/// Whether a process with this pid exists.
#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 performs only the existence/permission check.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

#[cfg(not(unix))]
pub fn pid_alive(_pid: u32) -> bool {
    true
}
