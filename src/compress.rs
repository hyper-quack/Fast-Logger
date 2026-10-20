//! Gzip archiving. Writes `<file>.gz.tmp`, fsyncs, renames, then removes the
//! original, so a crash at any point leaves either the original or a complete `.gz`.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use flate2::{Compression, GzBuilder};

const BUF: usize = 256 * 1024;

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s: OsString = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

pub fn gz_path(src: &Path) -> PathBuf {
    with_suffix(src, ".gz")
}

/// Compresses `src` to `src.gz` and deletes `src`. Returns (original size, compressed size, gz path).
pub fn gzip_file(src: &Path, level: u32) -> io::Result<(u64, u64, PathBuf)> {
    let gz = gz_path(src);
    if let Ok(m) = fs::metadata(&gz) {
        // A previous run finished the .gz but died before removing the original.
        let before = fs::metadata(src).map(|m| m.len()).unwrap_or(0);
        fs::remove_file(src)?;
        return Ok((before, m.len(), gz));
    }
    let tmp = with_suffix(&gz, ".tmp");
    let input = File::open(src)?;
    let meta = input.metadata()?;
    let out = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let result = (|| {
        let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as u32);
        let mut enc = GzBuilder::new()
            .filename(name)
            .mtime(mtime)
            .write(BufWriter::with_capacity(BUF, out), Compression::new(level));
        let mut reader = BufReader::with_capacity(BUF, input);
        let mut chunk = vec![0u8; BUF];
        loop {
            let n = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            enc.write_all(&chunk[..n])?;
        }
        let out = enc.finish()?.into_inner().map_err(|e| e.into_error())?;
        out.sync_all()?;
        if let Ok(t) = meta.modified() {
            let _ = out.set_modified(t);
        }
        Ok(out.metadata()?.len())
    })();
    match result {
        Ok(after) => {
            fs::rename(&tmp, &gz)?;
            fs::remove_file(src)?;
            Ok((meta.len(), after, gz))
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Opens a log file for reading, transparently decompressing `.gz` files.
pub fn open_maybe_gz(path: &Path) -> io::Result<Box<dyn io::BufRead + Send>> {
    let f = File::open(path)?;
    if path.extension().is_some_and(|e| e == "gz") {
        Ok(Box::new(BufReader::with_capacity(BUF, flate2::read::MultiGzDecoder::new(BufReader::with_capacity(BUF, f)))))
    } else {
        Ok(Box::new(BufReader::with_capacity(BUF, f)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gzip_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rotlog-gz-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.log");
        let body = "line\n".repeat(10_000);
        fs::write(&src, &body).unwrap();
        let (before, after, gz) = gzip_file(&src, 6).unwrap();
        assert_eq!(before, body.len() as u64);
        assert!(after < before / 10);
        assert!(!src.exists());
        let mut s = String::new();
        open_maybe_gz(&gz).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, body);
        fs::remove_dir_all(&dir).unwrap();
    }
}
