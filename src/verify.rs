//! Verification pass: every copied item is re-read on both sides and
//! compared via SHA-256 — streamed, byte-accurate progress, cancel-safe.
//! (No rsync here: rsync's checksum dry-run only reports per-file progress,
//! so the verify bar would stand still for single big files.)

use crate::conflicts::{EngineKind, Item, Plan, Policy};
use crate::fsutil::{fmt_size, fmt_speed, ioerr};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Instant;

const BUF: usize = 1024 * 1024;
/// Send a progress update at least this often while hashing.
const UPDATE: u64 = 4 * 1024 * 1024;
/// Cap the per-file failure list so the summary stays readable.
const MAX_FAILS: usize = 50;

#[derive(Debug)]
pub struct VerifyReport {
    pub ok: usize,
    pub fails: Vec<(String, String)>,
    pub total: usize,
}

/// Byte-granular verification progress.
#[derive(Clone, Default)]
pub struct VProg {
    pub done: u64,
    pub total: u64,
    pub speed: String,
}

pub enum VMsg {
    Prog(VProg),
    Done(Result<VerifyReport, String>),
}

pub fn spawn(plan: Plan, cancel: Arc<AtomicBool>) -> Receiver<VMsg> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let res = run(&plan, &cancel, &tx);
        let _ = tx.send(VMsg::Done(res));
    });
    rx
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

const CANCEL: &str = "Abgebrochen";

/// Progress accumulator: knows the total byte weight up front (2 reads per
/// file) and reports `done`/`total` plus the average speed.
struct Meter {
    tx: Sender<VMsg>,
    total: u64,
    done: u64,
    sent_at: u64,
    start: Instant,
}

impl Meter {
    fn new(tx: Sender<VMsg>, total: u64) -> Self {
        Meter {
            tx,
            total,
            done: 0,
            sent_at: 0,
            start: Instant::now(),
        }
    }
    fn tick(&mut self, n: u64) {
        self.done += n;
        if self.done - self.sent_at >= UPDATE {
            self.send();
        }
    }
    fn send(&mut self) {
        self.sent_at = self.done;
        let secs = self.start.elapsed().as_secs_f64().max(0.2);
        let speed = fmt_speed(self.done as f64 / secs);
        let _ = self.tx.send(VMsg::Prog(VProg {
            done: self.done,
            total: self.total,
            speed,
        }));
    }
    fn finish(&mut self) {
        self.done = self.total;
        self.send();
    }
}

/// Stream-hash one file, ticking the meter. `Err("Abgebrochen")` on cancel.
fn hash_stream(r: &mut dyn Read, cancel: &AtomicBool, m: &mut Meter) -> Result<String, String> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; BUF];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCEL.into());
        }
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                h.update(&buf[..n]);
                m.tick(n as u64);
            }
            Err(e) => return Err(ioerr(&e)),
        }
    }
    Ok(hex(&h.finalize()))
}

fn hash_file(p: &Path, cancel: &AtomicBool, m: &mut Meter) -> Result<String, String> {
    let mut f = std::fs::File::open(p).map_err(|e| ioerr(&e))?;
    hash_stream(&mut f, cancel, m)
}

/// Regular files below `root` as (relative path, size). Symlinks are skipped
/// (they were never copied by rsync either). The root must be listable.
fn collect_files(root: &Path, out: &mut Vec<(String, u64)>) -> std::io::Result<()> {
    fn rec(dir: &Path, rel: &str, out: &mut Vec<(String, u64)>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            let Ok(md) = std::fs::symlink_metadata(entry.path()) else { continue };
            let ft = md.file_type();
            let name = entry.file_name().to_string_lossy().into_owned();
            let r = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if ft.is_dir() {
                rec(&entry.path(), &r, out);
            } else if ft.is_file() {
                out.push((r, md.len()));
            }
        }
    }
    std::fs::read_dir(root)?;
    rec(root, "", out);
    Ok(())
}

/// Is `rel` protected by an anchored exclude (engine-owned path that must
/// not be compared — rsync never copied it)?
fn is_excluded(rel: &str, excludes: &[String]) -> bool {
    for pat in excludes {
        let p = pat.strip_prefix('/').unwrap_or(pat);
        let plain: String = {
            let mut s = String::with_capacity(p.len());
            let mut esc = false;
            for c in p.chars() {
                if esc {
                    s.push(c);
                    esc = false;
                } else if c == '\\' {
                    esc = true;
                } else {
                    s.push(c);
                }
            }
            s
        };
        if rel == plain || rel.starts_with(&format!("{plain}/")) {
            return true;
        }
    }
    false
}

/// Compare one file pair, ticking the meter for the byte work done.
/// `Err` = failure reason (or CANCEL).
fn verify_pair(src: &Path, dst: &Path, size: u64, cancel: &AtomicBool, m: &mut Meter) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err(CANCEL.into());
    }
    let dmeta = match std::fs::symlink_metadata(dst) {
        Ok(md) => md,
        Err(_) => {
            m.tick(size * 2);
            return Err("fehlt am Ziel".into());
        }
    };
    if !dmeta.is_file() {
        m.tick(size * 2);
        return Err("am Ziel liegt keine Datei".into());
    }
    if dmeta.len() != size {
        m.tick(size * 2);
        return Err(format!(
            "Größe weicht ab (Quelle {}, Ziel {})",
            fmt_size(size),
            fmt_size(dmeta.len())
        ));
    }
    let src_sha = match hash_file(src, cancel, m) {
        Ok(s) => s,
        Err(e) => {
            if cancel.load(Ordering::Relaxed) {
                return Err(CANCEL.into());
            }
            return Err(format!("Quelle nicht lesbar: {e}"));
        }
    };
    let dst_sha = match hash_file(dst, cancel, m) {
        Ok(s) => s,
        Err(e) => {
            if cancel.load(Ordering::Relaxed) {
                return Err(CANCEL.into());
            }
            return Err(format!("Ziel nicht lesbar: {e}"));
        }
    };
    if src_sha == dst_sha {
        Ok(())
    } else {
        Err("Inhalt weicht ab (Prüfsumme)".into())
    }
}

/// Compare a whole directory pair file by file. Fails are appended to
/// `fails` (capped); the item counts as ok only if every file matches.
/// `files` is the pre-collected, exclude-filtered source listing.
fn verify_tree(
    src_root: &Path,
    dst_root: &Path,
    display: &str,
    files: &[(String, u64)],
    excludes: &[String],
    cancel: &AtomicBool,
    m: &mut Meter,
    fails: &mut Vec<(String, String)>,
) -> Result<bool, String> {
    if !dst_root.is_dir() {
        let rest: u64 = files.iter().map(|(_, s)| s * 2).sum();
        m.tick(rest);
        fails.push((display.to_string(), "fehlt am Ziel".into()));
        return Ok(false);
    }
    let mut item_ok = true;
    for (rel, size) in files {
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCEL.into());
        }
        if is_excluded(rel, excludes) {
            continue;
        }
        let name = if rel.is_empty() {
            display.to_string()
        } else {
            format!("{display}/{rel}")
        };
        if let Err(reason) = verify_pair(&src_root.join(rel), &dst_root.join(rel), *size, cancel, m) {
            if reason == CANCEL {
                return Err(CANCEL.into());
            }
            item_ok = false;
            if fails.len() < MAX_FAILS {
                fails.push((name, reason));
            }
        }
    }
    Ok(item_ok)
}

/// SHA-256 over all FAT32 parts in order (missing part -> failure).
fn verify_chunks(item: &Item, cancel: &AtomicBool, m: &mut Meter) -> Result<(), String> {
    let src_sha = hash_file(&item.src, cancel, m)?;
    let parts = item.size.div_ceil(item.chunk) as usize;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; BUF];
    for k in 0..parts {
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCEL.into());
        }
        let p = item.dst_dir.join(format!("{}.{:03}", item.dst_name, k + 1));
        let expected = (item.size - k as u64 * item.chunk).min(item.chunk);
        let mut f = match std::fs::File::open(&p) {
            Ok(f) => f,
            Err(_) => {
                m.tick(item.size - k as u64 * item.chunk);
                return Err(format!("Teil {} fehlt am Ziel", k + 1));
            }
        };
        let mut got: u64 = 0;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(CANCEL.into());
            }
            match f.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    h.update(&buf[..n]);
                    got += n as u64;
                    m.tick(n as u64);
                }
                Err(e) => return Err(format!("Ziel nicht lesbar: {}", ioerr(&e))),
            }
        }
        if got != expected {
            return Err(format!(
                "Teil {} hat {} statt {}",
                k + 1,
                fmt_size(got),
                fmt_size(expected)
            ));
        }
    }
    let dst_sha = hex(&h.finalize());
    if dst_sha == src_sha {
        Ok(())
    } else {
        Err("Inhalt weicht ab (Prüfsumme)".into())
    }
}

fn run(plan: &Plan, cancel: &AtomicBool, tx: &Sender<VMsg>) -> Result<VerifyReport, String> {
    // Which items are verified how?
    let tree_items: Vec<usize> = (0..plan.items.len())
        .filter(|&i| {
            plan.items[i].engine == EngineKind::Rsync
                && plan.items[i].policy != Policy::Skip
                && !plan.rsync_rename.contains(&i)
        })
        .collect();
    let rename_items: Vec<usize> = plan
        .rsync_rename
        .iter()
        .copied()
        .filter(|&i| plan.items[i].policy != Policy::Skip)
        .collect();
    let engine_items: Vec<usize> = plan
        .engine
        .iter()
        .copied()
        .filter(|&i| !plan.items[i].is_dir && plan.items[i].policy != Policy::Skip)
        .collect();

    // Pre-pass: file lists for directory items + total byte weight
    // (every file is read twice: source and destination).
    let mut trees: Vec<(usize, Option<Vec<(String, u64)>>)> = Vec::new();
    let mut total: u64 = 0;
    for &i in tree_items.iter().chain(rename_items.iter()) {
        let item = &plan.items[i];
        if !item.is_dir {
            total += item.size * 2;
            continue;
        }
        let mut files = Vec::new();
        match collect_files(&item.src, &mut files) {
            Ok(()) => {
                files.retain(|(rel, _)| !is_excluded(rel, &plan.excludes));
                total += files.iter().map(|(_, s)| s * 2).sum::<u64>();
                trees.push((i, Some(files)));
            }
            Err(_) => trees.push((i, None)),
        }
    }
    for &i in &engine_items {
        total += plan.items[i].size * 2;
    }

    let mut m = Meter::new(tx.clone(), total);
    let mut ok = 0usize;
    let mut fails: Vec<(String, String)> = Vec::new();

    // ---- rsync-covered items: tree walk / single-file compare ----
    for &i in tree_items.iter().chain(rename_items.iter()) {
        let item = &plan.items[i];
        let display = if item.rel.is_empty() {
            item.dst_name.clone()
        } else {
            item.rel.clone()
        };
        let good = if item.is_dir {
            let dst_root = if plan.rsync_rename.contains(&i) {
                item.dst_path()
            } else {
                plan.dest.join(&item.dst_name)
            };
            let files = trees
                .iter()
                .find(|(j, _)| *j == i)
                .and_then(|(_, f)| f.clone());
            match files {
                None => {
                    fails.push((display.clone(), "Quelle nicht lesbar".into()));
                    false
                }
                Some(files) => match verify_tree(
                    &item.src,
                    &dst_root,
                    &display,
                    &files,
                    &plan.excludes,
                    cancel,
                    &mut m,
                    &mut fails,
                ) {
                    Ok(g) => g,
                    Err(e) => {
                        if e == CANCEL {
                            return Err(e);
                        }
                        false
                    }
                },
            }
        } else {
            match verify_pair(&item.src, &item.dst_path(), item.size, cancel, &mut m) {
                Ok(()) => true,
                Err(e) => {
                    if e == CANCEL {
                        return Err(e);
                    }
                    if fails.len() < MAX_FAILS {
                        fails.push((display.clone(), e));
                    }
                    false
                }
            }
        };
        if good {
            ok += 1;
        }
        m.send();
    }

    // ---- engine items ----
    for &i in &engine_items {
        let item = &plan.items[i];
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCEL.into());
        }
        let display = if item.rel.is_empty() {
            item.dst_name.clone()
        } else {
            item.rel.clone()
        };
        let res = match item.engine {
            EngineKind::Copy => verify_pair(&item.src, &item.dst_path(), item.size, cancel, &mut m),
            EngineKind::Chunks => verify_chunks(item, cancel, &mut m),
            EngineKind::Rsync => Ok(()), // never in engine list
        };
        match res {
            Ok(()) => ok += 1,
            Err(e) => {
                if e == CANCEL {
                    return Err(CANCEL.into());
                }
                crate::log::append(&format!("verify FAILED item={display}: {e}"));
                if fails.len() < MAX_FAILS {
                    fails.push((display, e));
                }
            }
        }
        m.send();
    }

    m.finish();
    Ok(VerifyReport { ok, fails, total: tree_items.len() + rename_items.len() + engine_items.len() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concat_sha_matches_full_file() {
        let tmp = std::env::temp_dir().join("ft-verify-t");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(tmp.join("f.bin"), &data).unwrap();
        // Fake three parts: 4096 + 4096 + 1808 = 10000
        std::fs::write(tmp.join("f.bin.001"), &data[..4096]).unwrap();
        std::fs::write(tmp.join("f.bin.002"), &data[4096..8192]).unwrap();
        std::fs::write(tmp.join("f.bin.003"), &data[8192..]).unwrap();
        let want = hex(&{
            let mut h = Sha256::new();
            h.update(&data);
            h.finalize()
        });
        // Build a minimal Chunks item and verify it end-to-end.
        let item = Item {
            src: tmp.join("f.bin"),
            rel: String::new(),
            owner: 0,
            dst_dir: tmp.clone(),
            dst_name: "f.bin".into(),
            size: data.len() as u64,
            is_dir: false,
            engine: EngineKind::Chunks,
            chunk: 4096,
            policy: Policy::New,
        };
        let (tx, rx) = channel();
        let mut meter = Meter::new(tx, item.size * 2);
        assert!(verify_chunks(&item, &AtomicBool::new(false), &mut meter).is_ok());
        while let Ok(msg) = rx.try_recv() {
            if let VMsg::Prog(p) = msg {
                assert!(p.done <= p.total + BUF as u64);
            }
        }
        assert_eq!(concat_hash_for_test(&tmp, "f.bin", data.len() as u64, 4096), Some(want));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Reference: hash over all parts in order (missing part -> None).
    fn concat_hash_for_test(dst_dir: &Path, name: &str, size: u64, chunk: u64) -> Option<String> {
        let parts = size.div_ceil(chunk) as usize;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; BUF];
        for k in 0..parts {
            let p = dst_dir.join(format!("{name}.{:03}", k + 1));
            let mut f = std::fs::File::open(p).ok()?;
            loop {
                let n = f.read(&mut buf).ok()?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
        }
        Some(hex(&h.finalize()))
    }

    #[test]
    fn exclude_matching() {
        let ex = ["/große datei.bin".to_string(), "/sub".to_string()];
        assert!(is_excluded("große datei.bin", &ex));
        assert!(is_excluded("sub", &ex));
        assert!(is_excluded("sub/inhalt.txt", &ex));
        assert!(!is_excluded("subdatei", &ex));
        assert!(!is_excluded("andere.txt", &ex));
        // rsync-style wildcard escaping is unescaped before comparing
        let ex2 = ["/pfad\\ mit\\ stern\\*.bin".to_string()];
        assert!(is_excluded("pfad mit stern*.bin", &ex2));
    }

    #[test]
    fn tree_verify_detects_missing_and_differing() {
        let tmp = std::env::temp_dir().join("ft-verify-tree");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src/u")).unwrap();
        std::fs::create_dir_all(tmp.join("dst/u")).unwrap();
        std::fs::write(tmp.join("src/a.txt"), b"AAAA").unwrap();
        std::fs::write(tmp.join("src/u/b.txt"), b"BBBB").unwrap();
        std::fs::write(tmp.join("dst/a.txt"), b"AAAA").unwrap();
        std::fs::write(tmp.join("dst/u/b.txt"), b"XXXX").unwrap();

        let (tx, _rx) = channel();
        let mut m = Meter::new(tx, 0);
        let mut fails = Vec::new();
        let mut files = Vec::new();
        collect_files(&tmp.join("src"), &mut files).unwrap();
        let good = verify_tree(
            &tmp.join("src"),
            &tmp.join("dst"),
            "mein Ordner",
            &files,
            &[],
            &AtomicBool::new(false),
            &mut m,
            &mut fails,
        )
        .unwrap();
        assert!(!good);
        assert_eq!(fails.len(), 1);
        assert_eq!(fails[0].0, "mein Ordner/u/b.txt");
        assert!(fails[0].1.contains("Prüfsumme") || fails[0].1.contains("Größe"));

        // Missing destination root
        let mut fails2 = Vec::new();
        let good2 = verify_tree(
            &tmp.join("src"),
            &tmp.join("does-not-exist"),
            "mein Ordner",
            &files,
            &[],
            &AtomicBool::new(false),
            &mut m,
            &mut fails2,
        )
        .unwrap();
        assert!(!good2);
        assert_eq!(fails2[0].1, "fehlt am Ziel");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
