//! argv handling: sources from the Dolphin service menu (`%F`).

use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct SrcInfo {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

/// Raw argv (after our own flags). Empty -> show the "select in Dolphin" hint.
pub fn raw_args() -> Vec<PathBuf> {
    std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with("--nascopy-"))
        .map(PathBuf::from)
        .collect()
}

/// Validate + stat all sources. Off the UI thread (sources may live on the NAS).
/// First error (missing/unreadable path) aborts with a message.
pub fn collect(raw: &[PathBuf]) -> Result<Vec<SrcInfo>, String> {
    let mut out = Vec::new();
    for a in raw {
        let p = if a.is_absolute() {
            a.clone()
        } else {
            std::env::current_dir()
                .map_err(|e| format!("Aktuelles Verzeichnis unbekannt: {e}"))?
                .join(a)
        };
        let md = std::fs::metadata(&p)
            .map_err(|e| format!("Quelle nicht zugreifbar:\n{}\n{}", p.display(), crate::fsutil::ioerr(&e)))?;
        out.push(SrcInfo {
            name: p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string_lossy().into_owned()),
            size: if md.is_file() { md.len() } else { 0 },
            is_dir: md.is_dir(),
            path: p,
        });
    }
    Ok(out)
}

/// True if `dest` lies inside `src` or vice versa (self-copy loop guard).
pub fn overlaps(src: &Path, dest: &Path) -> bool {
    match (src.canonicalize(), dest.canonicalize()) {
        (Ok(s), Ok(d)) => s == d || d.starts_with(&s) || s.starts_with(&d),
        _ => false,
    }
}

pub fn total_size(sources: &[SrcInfo]) -> u64 {
    sources.iter().map(|s| s.size).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_detects_nested() {
        let tmp = std::env::temp_dir();
        let a = tmp.join("nascopy-selftest-src");
        std::fs::create_dir_all(&a).unwrap();
        let b = a.join("sub");
        std::fs::create_dir_all(&b).unwrap();
        assert!(overlaps(&a, &b));
        assert!(overlaps(&b, &a));
        let c = tmp.join("nascopy-selftest-other");
        std::fs::create_dir_all(&c).unwrap();
        assert!(!overlaps(&a, &c));
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&c);
    }
}
