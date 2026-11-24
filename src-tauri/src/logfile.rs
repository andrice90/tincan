//! Persistent diagnostics: every run writes `logs/tincan.log` in its profile folder, so a
//! problem seen on a test machine can be read afterwards. The previous run's log is kept as
//! `tincan.1.log` when the current one is over [`ROTATE_AT`]; a run stops writing at
//! [`CAP`], so a long session can't fill the disk.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Default filter when `TINCAN_LOG` is unset: warnings everywhere, plus the network,
/// app and audio crates' info lines (connections, host changes, voice, devices).
pub const DEFAULT_FILTER: &str = "warn,tincan=info,tincan_app=info,tincan_net=info,tincan_audio=info";
/// A log at least this big at startup becomes `tincan.1.log`.
pub const ROTATE_AT: u64 = 4 * 1024 * 1024;
/// Bytes one run writes at most.
pub const CAP: u64 = 16 * 1024 * 1024;

/// `<profile dir>/logs/tincan.log`.
pub fn log_path(profile_dir: &Path) -> PathBuf { profile_dir.join("logs").join("tincan.log") }

/// Opens the log for appending, rotating a big previous log first.
pub fn open(path: &Path) -> io::Result<CappedFile> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    if fs::metadata(path).is_ok_and(|m| m.len() >= ROTATE_AT) {
        let old = path.with_file_name("tincan.1.log");
        let _ = fs::remove_file(&old);
        fs::rename(path, &old)?;
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    Ok(CappedFile { file: Mutex::new(file), written: AtomicU64::new(0), cap: CAP })
}

/// An append-only file that drops writes once `cap` bytes were written by this run.
pub struct CappedFile {
    file: Mutex<File>,
    written: AtomicU64,
    cap: u64,
}

impl CappedFile {
    fn write_line(&self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len() as u64;
        if self.written.fetch_add(n, Ordering::Relaxed) + n > self.cap {
            return Ok(buf.len());
        }
        let mut f = self.file.lock().unwrap_or_else(|p| p.into_inner());
        f.write_all(buf)?;
        Ok(buf.len())
    }
}

impl Write for &CappedFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> { self.write_line(buf) }
    fn flush(&mut self) -> io::Result<()> { self.file.lock().unwrap_or_else(|p| p.into_inner()).flush() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_a_big_log_and_caps_a_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = log_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![b'x'; ROTATE_AT as usize]).unwrap();
        let f = open(&path).unwrap();
        assert_eq!(fs::metadata(path.with_file_name("tincan.1.log")).unwrap().len(), ROTATE_AT);
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        let capped = CappedFile { cap: 10, ..f };
        (&capped).write_all(b"12345678").unwrap();
        (&capped).write_all(b"abcdef").unwrap();
        (&capped).flush().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"12345678");
    }

    #[test]
    fn a_small_log_is_appended_to() {
        let dir = tempfile::tempdir().unwrap();
        let path = log_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"old\n").unwrap();
        let f = open(&path).unwrap();
        (&f).write_all(b"new\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"old\nnew\n");
        assert!(!path.with_file_name("tincan.1.log").exists());
    }
}
