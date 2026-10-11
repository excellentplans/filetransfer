//! Filesystem helpers: mount probing, FAT detection, human-readable formatting.

use std::ffi::CString;
use std::io;
use std::path::{Path, PathBuf};

/// Destination filesystem classification (what matters for copy strategy).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsKind {
    /// FAT (vfat/msdos) — 4 GiB per-file limit, no permissions/symlinks.
    Fat,
    /// exFAT — no 4 GiB limit, but no permissions/symlinks either.
    ExFat,
    /// Everything POSIX-ish (ext4, btrfs, CIFS, NTFS, …).
    Posix,
}

/// CIFS / SMB superblock magic — how we detect a mounted NAS share.
pub const CIFS_MAGIC: u32 = 0xFF53_4D42;
/// MSDOS superblock magic (FAT12/16/32 via vfat driver).
pub const FAT_MAGIC: u32 = 0x4D44;
/// exFAT superblock magic.
pub const EXFAT_MAGIC: u32 = 0x6573_5546;

/// Largest file FAT32 can hold: 4 GiB − 1 byte.
pub const FAT_MAX_FILE: u64 = 4 * 1024 * 1024 * 1024 - 1;
/// Chunk size used when splitting on FAT: 4095 MiB, safely below the limit.
pub const FAT_CHUNK: u64 = 4095 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct Probe {
    /// None when statfs failed (e.g. target offline / not mounted).
    pub fs: Option<FsKind>,
    /// Free bytes available to unprivileged users.
    pub free: Option<u64>,
    /// True when the path sits on a CIFS mount (i.e. the NAS share).
    pub is_cifs: bool,
    /// True when the path itself exists (stat() succeeded).
    pub exists: bool,
}

/// statfs(2) with classification. May block for a long time on a dead CIFS
/// mount — always call it from a worker thread, never on the UI thread.
pub fn probe_path(path: &Path) -> Probe {
    let mut p = Probe::default();
    p.exists = path.exists() || std::fs::symlink_metadata(path).is_ok();
    let c = match CString::new(path.as_os_str().as_encoded_bytes()) {
        Ok(c) => c,
        Err(_) => return p,
    };
    unsafe {
        let mut s: libc::statfs = std::mem::zeroed();
        if libc::statfs(c.as_ptr(), &mut s) == 0 {
            let magic = s.f_type as u32;
            p.fs = Some(match magic {
                FAT_MAGIC => FsKind::Fat,
                EXFAT_MAGIC => FsKind::ExFat,
                _ => FsKind::Posix,
            });
            p.is_cifs = magic == CIFS_MAGIC;
            p.free = Some(s.f_bavail as u64 * s.f_bsize as u64);
        }
    }
    p
}

pub struct Mount {
    pub mountpoint: PathBuf,
    pub fstype: String,
}

/// Parse /proc/self/mountinfo (local file, always fast).
pub fn mounts() -> Vec<Mount> {
    let mut out = Vec::new();
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return out;
    };
    for line in text.lines() {
        let sep = match line.find(" - ") {
            Some(i) => i,
            None => continue,
        };
        let (left, right) = line.split_at(sep);
        let right = &right[3..];
        let mut lf = left.split_whitespace();
        let mp = match lf.nth(3) {
            Some(m) => unescape_mount(m),
            None => continue,
        };
        let mut rf = right.split_whitespace();
        let fstype = rf.next().unwrap_or("?").to_string();
        out.push(Mount {
            mountpoint: PathBuf::from(mp),
            fstype,
        });
    }
    out
}

/// /proc/self/mountinfo escapes space as \040, tab \011, newline \012, backslash \134.
fn unescape_mount(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let oct = &s[i + 1..i + 4];
            if let Ok(v) = u8::from_str_radix(oct, 8) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// "4293918720" -> "4,0 GiB" (German decimal comma).
pub fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    let dec = if u <= 1 { 0 } else { 1 };
    fmt_dec(v, dec, UNITS[u])
}

fn fmt_dec(v: f64, dec: usize, unit: &str) -> String {
    if dec == 0 {
        format!("{} {}", v.round() as u64, unit)
    } else {
        format!("{:.1$} {2}", v, dec, unit).replace('.', ",")
    }
}

/// Speed in bytes/s -> "38,5 MB/s".
pub fn fmt_speed(bps: f64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bps;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{} {}/s", v.round() as u64, UNITS[0])
    } else {
        format!("{:.1} {}/s", v, UNITS[u]).replace('.', ",")
    }
}

pub fn ioerr(e: &io::Error) -> String {
    match e.raw_os_error() {
        Some(code) => format!("{e} (Fehler {code})"),
        None => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_german() {
        assert_eq!(fmt_size(0), "0 B");
        assert_eq!(fmt_size(1024), "1 KiB");
        assert_eq!(fmt_size(4 * 1024 * 1024 * 1024), "4,0 GiB");
        assert_eq!(fmt_size(1536 * 1024 * 1024), "1,5 GiB");
    }

    #[test]
    fn speeds_german() {
        assert_eq!(fmt_speed(38_500_000.0), "38,5 MB/s");
        assert_eq!(fmt_speed(512.0), "512 B/s");
    }

    #[test]
    fn tmpdir_probe_is_posix() {
        let p = probe_path(&std::env::temp_dir());
        assert!(p.exists && p.fs.is_some());
        assert_eq!(p.fs, Some(FsKind::Posix));
        assert!(!p.is_cifs);
    }

    #[test]
    fn missing_probe() {
        let p = probe_path(&PathBuf::from("/nonexistent-nope-4711"));
        assert!(!p.exists);
    }

    #[test]
    fn mountinfo_parses() {
        let m = mounts();
        assert!(!m.is_empty());
        assert!(m.iter().any(|x| x.mountpoint == PathBuf::from("/")));
    }
}
