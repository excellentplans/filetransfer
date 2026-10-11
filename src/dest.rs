//! Destination picker model: locations (home, removable drives, network
//! shares, arbitrary folders), threaded directory listing (CIFS can block
//! for a long time!), new folders.

use crate::fsutil::{self, Probe};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LocationKind {
    /// The user's home directory.
    Home,
    /// Removable drive mounted below /media or /run/media.
    Removable,
    /// Network filesystem (CIFS, NFS, SSHFS, GVfs, …).
    Network,
    /// Folder the user picked by hand.
    Other,
}

#[derive(Clone)]
pub struct Location {
    pub root: PathBuf,
    pub label: String,
    pub detail: String,
    pub kind: LocationKind,
}

/// Build the location sidebar: home, removable drives and network shares —
/// all discovered from /proc/self/mountinfo, nothing hardcoded. Offline
/// destinations simply don't show up until they are mounted again.
pub fn build_locations() -> Vec<Location> {
    let mut out = Vec::new();
    if let Some(home) = dirs_home() {
        out.push(Location {
            root: home,
            label: "Persönlicher Ordner".into(),
            detail: "lokale Festplatte".into(),
            kind: LocationKind::Home,
        });
    }
    let user = std::env::var("USER").unwrap_or_default();
    let gvfs_base = gvfs_dir();
    out.extend(locations_from_mounts(&fsutil::mounts(), &user, &gvfs_base));
    out.extend(gvfs_shares(&gvfs_base));
    out
}

/// ~/… GVfs mount point of this login session (smb:// etc. from Dolphin).
fn gvfs_dir() -> PathBuf {
    // SAFETY: getuid is always safe to call.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/run/user/{uid}/gvfs"))
}

/// Human-readable name for a filesystem type.
fn fstype_label(fstype: &str) -> &str {
    match fstype {
        "vfat" | "msdos" => "FAT32",
        "exfat" => "exFAT",
        "ntfs" | "ntfs3" | "fuse.ntfs-3g" | "fuse.ntfs" => "NTFS",
        "cifs" | "smbfs" | "smb2" => "CIFS",
        "nfs" | "nfs4" => "NFS",
        "sshfs" | "fuse.sshfs" => "SSHFS",
        "davfs2" | "fuse.davfs2" => "WebDAV",
        "fuse.rclone" | "fuse.rclonefs" => "rclone",
        other => other,
    }
}

fn is_network_fstype(fstype: &str) -> bool {
    matches!(
        fstype,
        "cifs" | "smbfs" | "smb2" | "nfs" | "nfs4" | "sshfs" | "fuse.sshfs" | "davfs2"
            | "fuse.davfs2" | "fuse.rclone" | "fuse.rclonefs" | "ncpfs" | "9p"
    )
}

/// Classify mounts into removable drives and network shares.
/// Pure function so it can be tested without real mounts.
pub fn locations_from_mounts(
    mounts: &[fsutil::Mount],
    user: &str,
    gvfs_base: &Path,
) -> Vec<Location> {
    let usb_bases = [
        PathBuf::from("/media").join(user),
        PathBuf::from("/run/media").join(user),
    ];
    let mut out = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for m in mounts {
        if seen.contains(&m.mountpoint) || m.mountpoint == gvfs_base {
            continue;
        }
        let kind = if usb_bases.iter().any(|b| m.mountpoint.starts_with(b)) {
            LocationKind::Removable
        } else if is_network_fstype(&m.fstype) {
            LocationKind::Network
        } else {
            continue;
        };
        seen.insert(m.mountpoint.clone());
        let label = m            .mountpoint
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| m.mountpoint.display().to_string());
        let detail = match kind {
            // Device paths are noise on a stick; the filesystem is what matters.
            LocationKind::Removable => fstype_label(&m.fstype).to_string(),
            _ => match &m.source {
                Some(s) => format!("{}: {s}", fstype_label(&m.fstype)),
                None => fstype_label(&m.fstype).to_string(),
            },
        };
        out.push(Location {
            root: m.mountpoint.clone(),
            label,
            detail,
            kind,
        });
    }
    // Removable drives first (they are the common case), then network shares,
    // each alphabetical and case-insensitive.
    out.sort_by(|a, b| {
        let k = |l: &Location| match l.kind {
            LocationKind::Removable => 0,
            _ => 1,
        };
        k(a).cmp(&k(b)).then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
    });
    out
}

/// Individual shares inside the GVfs bridge, e.g. Dolphin's smb:// places.
fn gvfs_shares(base: &Path) -> Vec<Location> {
    let Ok(rd) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let raw = e.file_name().to_string_lossy().into_owned();
        out.push(Location {
            root: e.path(),
            label: gvfs_label(&raw),
            detail: "Netzfreigabe (GVfs)".into(),
            kind: LocationKind::Network,
        });
    }
    out.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
    out
}

/// "smb-share:server=nas,share=videos" -> "videos (nas)".
fn gvfs_label(raw: &str) -> String {
    let mut server: Option<&str> = None;
    let mut share: Option<&str> = None;
    for part in raw.split(':').skip(1) {
        for kv in part.split(',') {
            let mut it = kv.splitn(2, '=');
            let (Some(k), Some(v)) = (it.next(), it.next()) else { continue };
            match k {
                "share" | "path" => share = Some(v),
                "server" | "host" if server.is_none() => server = Some(v),
                _ => {}
            }
        }
    }
    match (share, server) {
        (Some(s), Some(h)) => format!("{s} ({h})"),
        (Some(s), None) => s.to_string(),
        (None, Some(h)) => h.to_string(),
        _ => raw.to_string(),
    }
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

/// Open a native file dialog (XDG portal) on a worker thread — used by the
/// intake screen when the app is started without Dolphin.
pub fn spawn_file_picker() -> Receiver<Option<Vec<PathBuf>>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let picked = rfd::FileDialog::new()
            .set_title("Dateien wählen")
            .pick_files();
        let _ = tx.send(picked);
    });
    rx
}

/// Open a native folder dialog (XDG portal) on a worker thread.
pub fn spawn_folder_picker() -> Receiver<Option<PathBuf>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let picked = rfd::FileDialog::new()
            .set_title("Zielordner wählen")
            .pick_folder();
        let _ = tx.send(picked);
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

    fn m(mountpoint: &str, fstype: &str, source: Option<&str>) -> fsutil::Mount {
        fsutil::Mount {
            mountpoint: PathBuf::from(mountpoint),
            fstype: fstype.into(),
            source: source.map(Into::into),
        }
    }

    #[test]
    fn mounts_classify_removable_and_network() {
        let gvfs = PathBuf::from("/run/user/1000/gvfs");
        let mounts = vec![
            m("/", "ext4", None),
            m("/mnt/nas", "cifs", Some("//192.168.1.199/nas1")),
            m("/media/erni/TOURO 1TB", "vfat", Some("/dev/sdb1")),
            m("/run/media/erni/KINGSTON", "exfat", Some("/dev/sdc1")),
            m("/sys", "sysfs", None),
            m(&gvfs.display().to_string(), "fuse.gvfsd-fuse", None),
        ];
        let locs = locations_from_mounts(&mounts, "erni", &gvfs);
        let roots: Vec<PathBuf> = locs.iter().map(|l| l.root.clone()).collect();
        assert!(roots.contains(&PathBuf::from("/mnt/nas")));
        assert!(roots.contains(&PathBuf::from("/media/erni/TOURO 1TB")));
        assert!(!roots.contains(&PathBuf::from("/sys")));
        assert!(!roots.contains(&gvfs));
        let nas = locs.iter().find(|l| l.root == PathBuf::from("/mnt/nas")).unwrap();
        assert_eq!(nas.kind, LocationKind::Network);
        assert_eq!(nas.detail, "CIFS: //192.168.1.199/nas1");
        let stick = locs.iter().filter(|l| l.kind == LocationKind::Removable).map(|l| l.label.as_str()).collect::<Vec<_>>();
        assert!(stick.contains(&"TOURO 1TB"));
        assert!(stick.contains(&"KINGSTON"));
        assert!(locs.iter().any(|l| l.label == "TOURO 1TB" && l.detail == "FAT32"));
        // Removables come before network shares.
        let i_stick = locs.iter().position(|l| l.kind == LocationKind::Removable).unwrap();
        let i_nas = locs.iter().position(|l| l.kind == LocationKind::Network).unwrap();
        assert!(i_stick < i_nas);
    }

    #[test]
    fn build_locations_has_home() {
        let locs = build_locations();
        assert!(locs.iter().any(|l| l.kind == LocationKind::Home));
    }

    #[test]
    fn gvfs_labels() {
        assert_eq!(gvfs_label("smb-share:server=nas,share=videos"), "videos (nas)");
        assert_eq!(gvfs_label("sftp:host=box,user=erni"), "box");
        assert_eq!(gvfs_label("unknown-thing"), "unknown-thing");
    }

    #[test]
    fn rel_components_works() {
        let root = PathBuf::from("/a/b");
        let p = PathBuf::from("/a/b/c/d.txt");
        assert_eq!(rel_components(&root, &p), vec!["c", "d.txt"]);
        assert!(rel_components(&root, &root).is_empty());
    }
}
