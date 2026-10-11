//! State dir logging + destination claims (~/.local/state/filetransfer.*).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
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

/// Destination claims: each running instance publishes the destination it is
/// writing to in `filetransfer.job.<pid>`. A claim counts as live while its
/// pid still exists in /proc — the kernel reaps the pid on crash/kill, so
/// stale files skip themselves in the listing and are removed on sight. No
/// flock needed: a pid is unique while its writer lives. Dropping the claim
/// releases the destination immediately.
pub struct DestClaim {
    file: PathBuf,
    dest: String,
}

fn claim_path(pid: u32) -> PathBuf {
    state_dir().with_extension(format!("job.{pid}"))
}

/// String-level normalization for claim comparison: trailing slashes off,
/// case-sensitive (POSIX). Two spellings of the same share via different
/// mountpoints stay distinct — acceptable for a warning-level guard.
fn norm_dest(p: &Path) -> String {
    let s = p.to_string_lossy();
    let t = s.trim_end_matches('/');
    if t.is_empty() { "/".into() } else { t.into() }
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

impl DestClaim {
    /// Publish `dest` for this instance (drop = release). Err lists the live
    /// claims of other instances on the same destination; nothing is claimed.
    pub fn acquire(dest: &Path) -> Result<DestClaim, Vec<String>> {
        let others = Self::conflicts(dest);
        if !others.is_empty() {
            return Err(others);
        }
        Ok(Self::acquire_unchecked(dest))
    }

    /// Publish without checking — used after the user overrides a warning,
    /// or in the rare race where a claim appeared between check and write.
    pub fn acquire_unchecked(dest: &Path) -> DestClaim {
        let path = claim_path(std::process::id());
        let _ = std::fs::remove_file(&path); // our own leftover, if any
        let _ = std::fs::create_dir_all(state_base());
        if let Ok(mut f) = File::create(&path) {
            let _ = writeln!(f, "{}", dest.display());
        }
        let dest = dest.to_string_lossy().into_owned();
        append(&format!("dest claimed: {dest}"));
        DestClaim { file: path, dest }
    }

    /// Live claims of other instances on `dest`, as "pid <N>: <path>" lines.
    /// Runs on the local state dir only — no network filesystem involved.
    pub fn conflicts(dest: &Path) -> Vec<String> {
        let want = norm_dest(dest);
        let me = std::process::id();
        let mut out = Vec::new();
        let mut stale = Vec::new();
        if let Ok(dir) = std::fs::read_dir(state_base()) {
            for e in dir.flatten() {
                let name = e.file_name();
                let Some(name) = name.to_str() else { continue };
                let Some(pid) = name
                    .strip_prefix("filetransfer.job.")
                    .and_then(|s| s.parse::<u32>().ok())
                else {
                    continue;
                };
                if pid == me {
                    continue;
                }
                if !pid_alive(pid) {
                    stale.push(e.path());
                    continue;
                }
                let Ok(body) = std::fs::read_to_string(e.path()) else { continue };
                let claimed = body.lines().next().unwrap_or("");
                if norm_dest(Path::new(claimed)) == want {
                    out.push(format!("pid {pid}: {claimed}"));
                }
            }
        }
        for p in stale {
            let _ = std::fs::remove_file(p);
        }
        out
    }
}

impl Drop for DestClaim {
    fn drop(&mut self) {
        // Only unlink while the file still advertises our destination: the
        // same instance may have re-claimed (same pid → same file) with a
        // different dest between this claim's acquire and its drop.
        if let Ok(body) = std::fs::read_to_string(&self.file) {
            if body.lines().next().map_or(false, |l| l.trim() == self.dest) {
                let _ = std::fs::remove_file(&self.file);
            }
        }
        append(&format!("dest released: {}", self.dest));
    }
}
