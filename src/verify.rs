//! Verification pass: rsync dry-run checksum comparison for rsync items,
//! full SHA-256 comparison (both sides) for engine-copied items.

use crate::conflicts::{EngineKind, Plan, Policy};
use crate::transfer::sha256_file;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

const RSYNC: &str = "/usr/bin/rsync";

#[derive(Debug)]
pub struct VerifyReport {
    pub ok: usize,
    pub fails: Vec<(String, String)>,
    pub total: usize,
}

pub enum VMsg {
    Prog(usize, usize),
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

fn run(plan: &Plan, cancel: &AtomicBool, tx: &Sender<VMsg>) -> Result<VerifyReport, String> {
    let send = |tx: &Sender<VMsg>, done: usize, total: usize| {
        let _ = tx.send(VMsg::Prog(done, total));
    };

    let rsync_items: Vec<usize> = (0..plan.items.len())
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
    let total = rsync_items.len() + rename_items.len() + engine_items.len();
    let mut done = 0usize;
    let mut fails: Vec<(String, String)> = Vec::new();

    // ---- rsync batch: -r -c -n --out-format='DIFF: %n' ----
    let mut rsync_bad_items: Vec<usize> = Vec::new();
    if !rsync_items.is_empty() {
        if cancel.load(Ordering::Relaxed) {
            return Err("Abgebrochen".into());
        }
        let srcs: Vec<PathBuf> = rsync_items.iter().map(|&i| plan.items[i].src.clone()).collect();
        let mut args: Vec<std::ffi::OsString> = vec![
            "-r".into(),
            "-c".into(),
            "-n".into(),
            "--out-format=DIFF: %n".into(),
        ];
        // Engine-owned paths (chunked/renamed) are not rsync's business.
        for e in &plan.excludes {
            args.push(format!("--exclude={e}").into());
        }
        args.push("--".into());
        for s in &srcs {
            args.push(s.into());
        }
        args.push(plan.dest.clone().into());
        crate::log::append(&format!("verify_start rsync batch items={}", srcs.len()));

        let out = Command::new(RSYNC)
            .args(&args)
            .env("LC_ALL", "C")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("rsync (Prüfung) konnte nicht gestartet werden: {e}"))?;
        if !out.status.success() {
            let tail = String::from_utf8_lossy(&out.stderr);
            return Err(format!("Prüfung fehlgeschlagen (rsync): {tail}"));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let diffs: Vec<String> = text
            .lines()
            .filter_map(|l| l.strip_prefix("DIFF: ").map(|p| p.trim().to_string()))
            .collect();
        // A DIFF line implicates the top-level item it belongs to.
        for (di, path) in diffs.iter().enumerate() {
            let owner = rsync_items.iter().copied().find(|&i| {
                let n = &plan.items[i].dst_name;
                path == n || path.starts_with(&format!("{n}/")) || path.ends_with(&format!("/{n}"))
            });
            let display = match owner {
                Some(i) => {
                    if !rsync_bad_items.contains(&i) {
                        rsync_bad_items.push(i);
                    }
                    plan.items[i].dst_name.clone()
                }
                None => format!("item {di}"),
            };
            fails.push((display, format!("unterschiedlich/fehlt: {path}")));
        }
        crate::log::append(&format!("verify rsync batch: {} DIFF lines", diffs.len()));
        done += rsync_items.len();
        send(tx, done, total);
    }

    // ---- renamed items: one rsync dry-run against the explicit dest path ----
    for &idx in &rename_items {
        if cancel.load(Ordering::Relaxed) {
            return Err("Abgebrochen".into());
        }
        let item = &plan.items[idx];
        let args: Vec<std::ffi::OsString> = vec![
            "-r".into(),
            "-c".into(),
            "-n".into(),
            "--out-format=DIFF: %n".into(),
            "--".into(),
            item.src.clone().into(),
            item.dst_path().into(),
        ];
        let out = Command::new(RSYNC)
            .args(&args)
            .env("LC_ALL", "C")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("rsync (Prüfung) konnte nicht gestartet werden: {e}"))?;
        if !out.status.success() {
            fails.push((
                item.dst_name.clone(),
                format!("Prüfung fehlgeschlagen (rsync, Code {})", out.status.code().unwrap_or(-1)),
            ));
            rsync_bad_items.push(idx);
        } else {
            let text = String::from_utf8_lossy(&out.stdout);
            let diffs: Vec<String> = text
                .lines()
                .filter_map(|l| l.strip_prefix("DIFF: ").map(|p| p.trim().to_string()))
                .collect();
            if diffs.is_empty() {
                // counted as ok below
            } else {
                rsync_bad_items.push(idx);
                for d in diffs {
                    fails.push((item.dst_name.clone(), format!("unterschiedlich/fehlt: {d}")));
                }
            }
        }
        done += 1;
        send(tx, done, total);
    }

    // ---- engine items: read both sides fully, compare SHA-256 ----
    let mut ok = rsync_items.len() + rename_items.len() - rsync_bad_items.len();
    for &idx in &engine_items {
        if cancel.load(Ordering::Relaxed) {
            return Err("Abgebrochen".into());
        }
        let item = &plan.items[idx];
        let display = if item.rel.is_empty() {
            item.dst_name.clone()
        } else {
            item.rel.clone()
        };
        let res = (|| -> Result<(), String> {
            let src_sha = sha256_file(&item.src).map_err(|e| format!("Quelle nicht lesbar: {e}"))?;
            let dst_sha = match item.engine {
                EngineKind::Copy => sha256_file(&item.dst_path()).ok(),
                EngineKind::Chunks => concat_parts_sha(&item.dst_dir, &item.dst_name, item.size, item.chunk),
                _ => None,
            };
            match dst_sha {
                None => Err("fehlt am Ziel".to_string()),
                Some(d) if d == src_sha => Ok(()),
                Some(_) => Err("Inhalt weicht ab (Prüfsumme)".to_string()),
            }
        })();
        match res {
            Ok(()) => ok += 1,
            Err(reason) => {
                crate::log::append(&format!("verify FAILED item={display}: {reason}"));
                fails.push((display, reason));
            }
        }
        done += 1;
        send(tx, done, total);
    }

    Ok(VerifyReport { ok, fails, total })
}

/// SHA-256 over all FAT32 parts in order (missing part -> None).
fn concat_parts_sha(dst_dir: &std::path::Path, name: &str, size: u64, chunk: u64) -> Option<String> {
    let parts = size.div_ceil(chunk) as usize;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concat_sha_matches_full_file() {
        use std::path::PathBuf;
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
        assert_eq!(concat_parts_sha(&tmp, "f.bin", data.len() as u64, 4096), Some(want));
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = PathBuf::new();
    }
}
