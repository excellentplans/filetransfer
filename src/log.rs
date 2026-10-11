//! State dir logging + single-instance lock (~/.local/state/filetransfer.*).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// The directory that actually exists on disk (state_dir() is a file stem).
pub fn state_base() -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let h = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(h).join(".local/state")
        })
}

pub fn state_dir() -> PathBuf {
    state_base().join("filetransfer")
}

/// Settings of the predecessor app (nascopy), for one-time migration.
pub fn legacy_last_path() -> Option<PathBuf> {
    let p = state_base().join("nascopy.last");
    p.exists().then_some(p)
}

pub fn log_path() -> PathBuf {
    state_dir().with_extension("log")
}

pub fn last_path() -> PathBuf {
    state_dir().with_extension("last")
}

pub fn lock_path() -> PathBuf {
    state_dir().with_extension("lock")
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// HH:MM:SS of the local clock, cheap version for the log.
fn clock() -> String {
    let secs = epoch_secs() % 86_400;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

static LOG: Mutex<()> = Mutex::new(());

pub fn append(msg: &str) {
    let _g = LOG.lock();
    let _ = std::fs::create_dir_all(state_base());
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = writeln!(f, "{} {}", clock(), msg);
    }
}

/// Quote an argv-style value for readable logs.
pub fn q(s: &str) -> String {
    if s.contains(' ') || s.contains('\'') || s.contains('(') || s.contains(')') || s.is_empty() {
        format!("'{s}'")
    } else {
        s.to_string()
    }
}

/// Single-instance lock: hold an flock on filetransfer.lock for the app lifetime.
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// Ok(lock) = we are the only instance. Err(()) = another instance holds it.
    pub fn acquire() -> Result<InstanceLock, ()> {
        let _ = std::fs::create_dir_all(state_base());
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(lock_path())
            .map_err(|_| ())?;
        let r = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if r != 0 {
            return Err(());
        }
        Ok(InstanceLock { _file: file })
    }
}
