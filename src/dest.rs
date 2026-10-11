//! Destination picker model: locations (NAS / USB sticks / home),
//! threaded directory listing (CIFS can block for a long time!), new folders.

use crate::fsutil::{self, Probe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

#[derive(Clone)]
pub struct Location {
    pub root: PathBuf,
    pub label: String,
    pub detail: String,
}

/// Build the location sidebar. The NAS entry is always shown (probe tells
/// whether it's actually reachable). USB sticks come from /proc/self/mountinfo.
pub fn build_locations() -> Vec<Location> {
    let mut out = Vec::new();
    let nas = PathBuf::from("/mnt/nas");
    out.push(Location {
        root: nas.clone(),
        label: "NAS".into(),
        detail: "/mnt/nas".into(),
    });

    let user = std::env::var("USER").unwrap_or_default();
    let usb_bases = [
        PathBuf::from("/media").join(&user),
        PathBuf::from("/run/media").join(&user),
    ];
    let mut seen = vec![nas];
    for m in fsutil::mounts() {
        if !usb_bases.iter().any(|b| m.mountpoint.starts_with(b)) {
            continue;
        }
        if seen.contains(&m.mountpoint) {
            continue;
        }
        seen.push(m.mountpoint.clone());
        let name = m
            .mountpoint
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| m.mountpoint.display().to_string());
        out.push(Location {
            root: m.mountpoint,
            label: name,
            detail: match m.fstype.as_str() {
                "vfat" => "FAT32".into(),
                "exfat" => "exFAT".into(),
                f => f.to_string(),
            },
        });
    }

    if let Some(home) = dirs_home() {
        out.push(Location {
            root: home,
            label: "Persönlicher Ordner".into(),
            detail: "lokale Festplatte".into(),
        });
    }
    out
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Clone)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub probe: Probe,
}

/// Spawn a background listing of `dir` (probe + read_dir). Returns a receiver;
/// the UI shows a spinner and can decide after 3 s that the target is stalling.
pub fn spawn_list(dir: PathBuf) -> Receiver<std::io::Result<Listing>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let res = (|| {
            let probe = fsutil::probe_path(&dir);
            let mut entries = Vec::new();
            for e in std::fs::read_dir(&dir)? {
                let Ok(e) = e else { continue };
                let Ok(md) = e.metadata() else { continue };
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                entries.push(Entry {
                    is_dir: md.is_dir(),
                    size: if md.is_file() { md.len() } else { 0 },
                    name,
                });
            }
            entries.sort_by(|a, b| match (b.is_dir, a.is_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            });
            Ok::<_, std::io::Error>(Listing { entries, probe })
        })();
        let _ = tx.send(res);
    });
    rx
}

/// Create a folder at `parent/name` in the background (may block on CIFS).
pub fn spawn_mkdir(parent: &Path, name: &str) -> Receiver<Result<PathBuf, String>> {
    let (tx, rx) = channel();
    let target = parent.join(name);
    std::thread::spawn(move || {
        let res = std::fs::create_dir(&target)
            .map(|_| target.clone())
            .map_err(|e| format!("Ordner konnte nicht erstellt werden:\n{}\n{}", target.display(), fsutil::ioerr(&e)));
        let _ = tx.send(res);
    });
    rx
}

/// Components of `p` relative to `root` (empty when p == root).
pub fn rel_components(root: &Path, p: &Path) -> Vec<String> {
    p.strip_prefix(root)
        .map(|r| {
            r.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_components_works() {
        let root = PathBuf::from("/a/b");
        let p = PathBuf::from("/a/b/c/d.txt");
        assert_eq!(rel_components(&root, &p), vec!["c", "d.txt"]);
        assert!(rel_components(&root, &root).is_empty());
    }

    #[test]
    fn locations_have_nas() {
        let locs = build_locations();
        assert!(locs.iter().any(|l| l.label == "NAS" && l.root == PathBuf::from("/mnt/nas")));
    }
}
