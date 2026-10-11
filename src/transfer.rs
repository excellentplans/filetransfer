//! Transfer: rsync process management, --info=progress2 parsing, cancel,
//! plus the in-app copy engine for FAT32 chunk-splitting and renamed files.

use crate::conflicts::{chunk_resume_state, EngineKind, Item, Plan, Policy};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RSYNC: &str = "/usr/bin/rsync";
pub const RSYNC_PATH: &str = RSYNC;
const BUF: usize = 1024 * 1024;

// ---------- progress ----------

#[derive(Clone, Debug, Default)]
pub struct Prog {
    pub done: u64,
    pub pct: f32,
    pub speed: String,
    pub eta: String,
    pub xfr: u64,
    pub tochk: (u64, u64),
}

/// Parse one --info=progress2 update:
/// `  2.37G  46%   41.39MB/s    0:00:47 (xfr#12, to-chk=4/17)`
pub fn parse_progress2(line: &str) -> Option<Prog> {
    let line = line.trim();
    if line.is_empty() || !line.contains('%') {
        return None;
    }
    let paren = line.find("(xfr");
    let head = match paren {
        Some(i) => &line[..i],
        None => line,
    };
    let toks: Vec<&str> = head.split_whitespace().collect();
    if toks.len() < 2 {
        return None;
    }
    let mut p = Prog::default();
    p.done = human_to_bytes(toks[0]).unwrap_or(0);
    for t in &toks[1..] {
        if let Some(num) = t.strip_suffix('%') {
            p.pct = num.replace(',', ".").parse().unwrap_or(0.0);
        } else if t.contains("B/s") {
            p.speed = (*t).to_string();
        } else if t.split(':').count() == 3 && t.chars().all(|c| c.is_ascii_digit() || c == ':') {
            p.eta = (*t).to_string();
        }
    }
    if let Some(i) = paren {
        let tail = &line[i..];
        if let Some(x) = tail.find("xfr#") {
            let digits: String = tail[x + 4..].chars().take_while(|c| c.is_ascii_digit()).collect();
            p.xfr = digits.parse().unwrap_or(0);
        }
        if let Some(c) = tail.find("to-chk=") {
            let rest = &tail[c + 7..];
            let mut it = rest.splitn(2, '/');
            let i = it.next().unwrap_or("0").parse().unwrap_or(0);
            let j = it
                .next()
                .unwrap_or("0")
                .trim_end_matches(|c: char| !c.is_ascii_digit())
                .parse()
                .unwrap_or(0);
            p.tochk = (i, j);
        }
    }
    Some(p)
}

/// "2.37G" -> bytes (rsync -h uses powers of 1024); plain numbers = bytes.
pub fn human_to_bytes(tok: &str) -> Option<u64> {
    let idx = tok.find(|c: char| c.is_ascii_alphabetic());
    let (num, unit) = match idx {
        Some(i) => tok.split_at(i),
        None => (tok, ""),
    };
    let mult: f64 = match unit.to_ascii_uppercase().trim_end_matches('B') {
        "" => 1.0,
        "K" => 1024.0,
        "M" => 1024.0 * 1024.0,
        "G" => 1024.0 * 1024.0 * 1024.0,
        "T" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((num.replace(',', ".").parse::<f64>().ok()? * mult) as u64)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------- messages ----------

pub enum Msg {
    /// Current step label + "Schritt k/n".
    Phase(String, usize, usize),
    RsyncProg(Prog),
    EngineProg {
        name: String,
        part: usize,
        parts: usize,
        done: u64,
        total: u64,
    },
    RunStats(String),
    ItemDone {
        idx: usize,
        res: Result<(), String>,
        /// SHA-256 of what this run wrote ("" for dirs; partial for resumes).
        sha: String,
    },
    Finished(Outcome),
}

#[derive(Clone, Debug)]
pub enum Outcome {
    Ok,
    Cancelled,
    Failed(String),
}

enum XErr {
    Cancelled,
    Io(String),
}

// ---------- rsync ----------

fn base_flags(fat: bool) -> Vec<&'static str> {
    let mut v: Vec<&'static str> = if fat {
        vec!["-r", "-l", "-t", "--modify-window=1", "--no-perms", "--no-owner", "--no-group"]
    } else {
        vec!["-a"]
    };
    v.extend(["--partial", "--append-verify", "--info=progress2", "--stats"]);
    v
}

/// Build the full argv for one rsync run.
pub fn rsync_argv(
    fat: bool,
    checksum: bool,
    excludes: &[String],
    srcs: &[PathBuf],
    dest: &Path,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = Vec::new();
    for f in base_flags(fat) {
        if checksum && f == "--append-verify" {
            continue;
        }
        args.push(f.into());
    }
    if checksum {
        args.push("--checksum".into());
    }
    for e in excludes {
        args.push(format!("--exclude={e}").into());
    }
    args.push("--".into());
    for s in srcs {
        args.push(s.into());
    }
    args.push(dest.into());
    args
}

struct RunOut {
    stats: String,
    cancelled: bool,
}

fn run_rsync(args: Vec<std::ffi::OsString>, cancel: &AtomicBool, tx: &Sender<Msg>) -> Result<RunOut, String> {
    let mut cmd = Command::new(RSYNC);
    cmd.args(&args).env("LC_ALL", "C");
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("rsync konnte nicht gestartet werden: {e}"))?;
    let pid = child.id() as i32;

    let out_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let err_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    // stdout: parse progress2 updates + keep everything for the --stats block.
    {
        let mut out = child.stdout.take().unwrap();
        let buf = out_buf.clone();
        let tx2 = tx.clone();
        std::thread::spawn(move || {
            let mut raw = [0u8; 8192];
            let mut pending = String::new();
            loop {
                match out.read(&mut raw) {
                    Ok(0) => break,
                    Ok(n) => {
                        let s = String::from_utf8_lossy(&raw[..n]).into_owned();
                        buf.lock().unwrap().push_str(&s);
                        pending.push_str(&s);
                        while let Some(pos) = pending.find(['\r', '\n']) {
                            let seg: String = pending.drain(..=pos).collect();
                            let seg = seg.trim_end_matches(['\r', '\n']);
                            if let Some(p) = parse_progress2(seg) {
                                let _ = tx2.send(Msg::RsyncProg(p));
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            if let Some(p) = parse_progress2(&pending) {
                let _ = tx2.send(Msg::RsyncProg(p));
            }
        });
    }
    // stderr: keep the tail for error dialogs.
    {
        let mut err = child.stderr.take().unwrap();
        let buf = err_buf.clone();
        std::thread::spawn(move || {
            let mut raw = [0u8; 8192];
            loop {
                match err.read(&mut raw) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut b = buf.lock().unwrap();
                        b.push_str(&String::from_utf8_lossy(&raw[..n]));
                        let len = b.len();
                        if len > 8000 {
                            *b = b[len - 8000..].to_string();
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    let mut killed = false;
    let mut kill_at: Option<Instant> = None;
    let status = loop {
        if cancel.load(Ordering::Relaxed) && !killed {
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            killed = true;
            kill_at = Some(Instant::now());
        }
        if killed {
            if let Some(t) = kill_at {
                if t.elapsed() > Duration::from_secs(5) {
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        }
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("rsync-Status nicht lesbar: {e}")),
        }
    };

    let code = status.code().unwrap_or(-1);
    let out = out_buf.lock().unwrap().clone();
    let stats = extract_stats(&out).unwrap_or_default();
    if killed {
        return Ok(RunOut { stats, cancelled: true });
    }
    if code != 0 {
        let err_text = err_buf.lock().unwrap().clone();
        let tail: Vec<String> = err_text
            .lines()
            .rev()
            .take(12)
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return Err(format!("rsync brach mit Fehlercode {code} ab.\n{}", tail.join("\n")));
    }
    Ok(RunOut { stats, cancelled: false })
}

/// Extract the --stats block from the captured stdout.
fn extract_stats(out: &str) -> Option<String> {
    let idx = out.find("Number of files")?;
    let start = out[..idx].rfind('\n').map(|i| i + 1).unwrap_or(0);
    Some(out[start..].trim().to_string())
}

// ---------- engine copy ----------

fn io_err(p: &Path, e: std::io::Error) -> XErr {
    XErr::Io(format!("{}: {}", p.display(), crate::fsutil::ioerr(&e)))
}

/// Copy one engine item. Returns the SHA-256 of what this run wrote
/// ("" for directory items; a partial-file hash for resumed copies).
fn copy_item(item: &Item, cancel: &AtomicBool, tx: &Sender<Msg>) -> Result<String, XErr> {
    if item.is_dir {
        return std::fs::create_dir_all(item.dst_path())
            .map(|_| String::new())
            .map_err(|e| io_err(&item.dst_path(), e));
    }
    std::fs::create_dir_all(&item.dst_dir).map_err(|e| io_err(&item.dst_dir, e))?;

    let short = if item.rel.is_empty() {
        item.dst_name.clone()
    } else {
        format!("{} → {}", item.rel, item.dst_name)
    };
    match item.engine {
        EngineKind::Copy => {
            let dst = item.dst_path();
            let mut src = std::fs::File::open(&item.src).map_err(|e| io_err(&item.src, e))?;
            let mut dstf = std::fs::File::create(&dst).map_err(|e| io_err(&dst, e))?;
            let sha = stream(&mut src, &mut dstf, item.size, cancel, tx, &short, 0, 1, 0)?;
            let _ = dstf.sync_all();
            Ok(sha)
        }
        EngineKind::Chunks => copy_chunks(item, cancel, tx, &short),
        EngineKind::Rsync => Ok(String::new()), // never reached
    }
}

/// Stream src into dst with hashing + progress + cancel checks.
fn stream(
    src: &mut dyn Read,
    dst: &mut dyn Write,
    total: u64,
    cancel: &AtomicBool,
    tx: &Sender<Msg>,
    short: &str,
    base_done: u64,
    parts: usize,
    part: usize,
) -> Result<String, XErr> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; BUF];
    let mut done: u64 = 0;
    let mut last_send: u64 = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(XErr::Cancelled);
        }
        let n = src.read(&mut buf).map_err(|e| XErr::Io(crate::fsutil::ioerr(&e)))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        dst.write_all(&buf[..n])
            .map_err(|e| XErr::Io(format!("Schreibfehler am Ziel: {}", crate::fsutil::ioerr(&e))))?;
        done += n as u64;
        if done - last_send >= (8 * BUF) as u64 {
            last_send = done;
            let _ = tx.send(Msg::EngineProg {
                name: short.to_string(),
                part,
                parts,
                done: base_done + done,
                total: total.max(base_done + done),
            });
        }
    }
    let _ = tx.send(Msg::EngineProg {
        name: short.to_string(),
        part: parts,
        parts,
        done: base_done + done,
        total,
    });
    Ok(hex(&hasher.finalize()))
}

/// FAT32 chunked copy: parts named `<name>.001`, `<name>.002`, …
/// Resume re-validates existing parts against the source, then continues
/// at the first incomplete part.
fn copy_chunks(item: &Item, cancel: &AtomicBool, tx: &Sender<Msg>, short: &str) -> Result<String, XErr> {
    let parts = item.size.div_ceil(item.chunk) as usize;
    let mut src = std::fs::File::open(&item.src).map_err(|e| io_err(&item.src, e))?;
    let mut hasher = Sha256::new();
    let mut base_done: u64 = 0;

    let mut start = 0usize;
    if item.policy == Policy::Resume {
        if let Some(k) = chunk_resume_state(&item.dst_dir, &item.dst_name, item.size, item.chunk) {
            start = k;
            for idx in 0..k {
                if cancel.load(Ordering::Relaxed) {
                    return Err(XErr::Cancelled);
                }
                let expected = (item.size - idx as u64 * item.chunk).min(item.chunk);
                let part_path = item
                    .dst_dir
                    .join(format!("{}.{:03}", item.dst_name, idx + 1));
                let dst_h = hash_file(&part_path)?;
                let src_h = hash_source_range(&mut src, idx as u64 * item.chunk, expected)?;
                if dst_h.is_none() || dst_h.as_deref() != Some(src_h.as_str()) {
                    start = idx; // corrupt → rewrite from here
                    break;
                }
                base_done += expected;
                let _ = tx.send(Msg::EngineProg {
                    name: short.to_string(),
                    part: idx + 1,
                    parts,
                    done: base_done,
                    total: item.size,
                });
            }
        }
    }

    src.seek(SeekFrom::Start(start as u64 * item.chunk))
        .map_err(|e| io_err(&item.src, e))?;

    for k in start..parts {
        if cancel.load(Ordering::Relaxed) {
            return Err(XErr::Cancelled);
        }
        let expected = (item.size - k as u64 * item.chunk).min(item.chunk);
        let part_path = item
            .dst_dir
            .join(format!("{}.{:03}", item.dst_name, k + 1));
        let mut dstf = std::fs::File::create(&part_path).map_err(|e| io_err(&part_path, e))?;
        let mut written: u64 = 0;
        while written < expected {
            if cancel.load(Ordering::Relaxed) {
                // Partial part file stays — that's the resume point.
                return Err(XErr::Cancelled);
            }
            let want = (expected - written).min(BUF as u64) as usize;
            let mut chunk = vec![0u8; want];
            src.read_exact(&mut chunk)
                .map_err(|e| XErr::Io(crate::fsutil::ioerr(&e)))?;
            hasher.update(&chunk);
            dstf.write_all(&chunk)
                .map_err(|e| XErr::Io(format!("Schreibfehler am Ziel: {}", crate::fsutil::ioerr(&e))))?;
            written += want as u64;
        }
        let _ = dstf.sync_all();
        drop(dstf);
        base_done += written;
        let _ = tx.send(Msg::EngineProg {
            name: short.to_string(),
            part: k + 1,
            parts,
            done: base_done,
            total: item.size,
        });
    }
    Ok(hex(&hasher.finalize()))
}

fn hash_file(p: &Path) -> Result<Option<String>, XErr> {
    match std::fs::File::open(p) {
        Ok(mut f) => {
            let mut h = Sha256::new();
            let mut buf = vec![0u8; BUF];
            loop {
                let n = f.read(&mut buf).map_err(|e| XErr::Io(crate::fsutil::ioerr(&e)))?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            Ok(Some(hex(&h.finalize())))
        }
        Err(_) => Ok(None),
    }
}

/// Hash `len` bytes starting at `offset` of src (restores the file position).
fn hash_source_range(src: &mut std::fs::File, offset: u64, len: u64) -> Result<String, XErr> {
    let pos = src.stream_position().map_err(|e| XErr::Io(crate::fsutil::ioerr(&e)))?;
    src.seek(SeekFrom::Start(offset))
        .map_err(|e| XErr::Io(crate::fsutil::ioerr(&e)))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; BUF];
    let mut left = len;
    while left > 0 {
        let want = left.min(BUF as u64) as usize;
        src.read_exact(&mut buf[..want])
            .map_err(|e| XErr::Io(crate::fsutil::ioerr(&e)))?;
        h.update(&buf[..want]);
        left -= want as u64;
    }
    src.seek(SeekFrom::Start(pos))
        .map_err(|e| XErr::Io(crate::fsutil::ioerr(&e)))?;
    Ok(hex(&h.finalize()))
}

// ---------- orchestration ----------

/// Run the whole plan on a worker thread. `cancel` is polled throughout.
pub fn spawn(plan: Plan, cancel: Arc<AtomicBool>) -> Receiver<Msg> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let fat = plan.is_fat();
        let steps = plan.needs_run_a() as usize
            + plan.needs_run_b() as usize
            + plan.rsync_rename.len()
            + plan.engine.len();
        let mut step = 0usize;
        let mut any_failed = false;

        macro_rules! phase {
            ($label:expr) => {{
                step += 1;
                let _ = tx.send(Msg::Phase($label, step, steps));
            }};
        }

        if plan.needs_run_a() {
            phase!("Kopiervorgang (rsync)".to_string());
            let args = rsync_argv(fat, false, &plan.excludes, &plan.rsync_a_srcs, &plan.dest);
            crate::log::append(&format!(
                "rsync_start run=A items={} args={}",
                plan.rsync_a_srcs.len(),
                args.iter().map(|a| crate::log::q(&a.to_string_lossy())).collect::<Vec<_>>().join(" ")
            ));
            match run_rsync(args, &cancel, &tx) {
                Ok(out) => {
                    crate::log::append("rsync_exit run=A ok");
                    if !out.stats.is_empty() {
                        let _ = tx.send(Msg::RunStats(out.stats));
                    }
                    if out.cancelled {
                        crate::log::append("transfer cancelled (run A)");
                        let _ = tx.send(Msg::Finished(Outcome::Cancelled));
                        return;
                    }
                }
                Err(e) => {
                    crate::log::append(&format!("rsync_exit run=A FAILED: {e}"));
                    let _ = tx.send(Msg::Finished(Outcome::Failed(e)));
                    return;
                }
            }
        }

        if plan.needs_run_b() {
            phase!("Überschreiben (--checksum)".to_string());
            let args = rsync_argv(fat, true, &plan.excludes, &plan.rsync_b_srcs, &plan.dest);
            crate::log::append(&format!("rsync_start run=B items={}", plan.rsync_b_srcs.len()));
            match run_rsync(args, &cancel, &tx) {
                Ok(out) => {
                    crate::log::append("rsync_exit run=B ok");
                    if !out.stats.is_empty() {
                        let _ = tx.send(Msg::RunStats(out.stats));
                    }
                    if out.cancelled {
                        crate::log::append("transfer cancelled (run B)");
                        let _ = tx.send(Msg::Finished(Outcome::Cancelled));
                        return;
                    }
                }
                Err(e) => {
                    crate::log::append(&format!("rsync_exit run=B FAILED: {e}"));
                    let _ = tx.send(Msg::Finished(Outcome::Failed(e)));
                    return;
                }
            }
        }

        for &ridx in &plan.rsync_rename {
            let item = &plan.items[ridx];
            if item.policy == Policy::Skip {
                continue;
            }
            phase!(format!("Umbenennen: {}", item.dst_name));
            let args = rsync_argv(fat, false, &plan.excludes, &[item.src.clone()], &item.dst_path());
            crate::log::append(&format!(
                "rsync_start rename idx={ridx} dst={}",
                item.dst_path().display()
            ));
            match run_rsync(args, &cancel, &tx) {
                Ok(out) => {
                    crate::log::append("rsync_exit rename ok");
                    if !out.stats.is_empty() {
                        let _ = tx.send(Msg::RunStats(out.stats));
                    }
                    if out.cancelled {
                        crate::log::append("transfer cancelled (rename run)");
                        let _ = tx.send(Msg::Finished(Outcome::Cancelled));
                        return;
                    }
                }
                Err(e) => {
                    crate::log::append(&format!("rsync_exit rename FAILED: {e}"));
                    let _ = tx.send(Msg::Finished(Outcome::Failed(e)));
                    return;
                }
            }
        }

        for &idx in &plan.engine {
            let item = &plan.items[idx];
            if item.policy == Policy::Skip {
                continue;
            }
            let label = if item.is_dir {
                format!("Ordner: {}", item.dst_name)
            } else if item.engine == EngineKind::Chunks {
                format!("FAT32-Zerlegung: {}", item.dst_name)
            } else {
                format!("Direktkopie: {}", item.dst_name)
            };
            phase!(label);
            crate::log::append(&format!(
                "engine_start idx={} src={} dst={} engine={:?}",
                idx,
                item.src.display(),
                item.dst_path().display(),
                item.engine
            ));
            match copy_item(item, &cancel, &tx) {
                Ok(sha) => {
                    crate::log::append(&format!("engine_exit idx={idx} ok"));
                    let _ = tx.send(Msg::ItemDone {
                        idx,
                        res: Ok(()),
                        sha,
                    });
                }
                Err(XErr::Cancelled) => {
                    crate::log::append("engine_exit cancelled");
                    let _ = tx.send(Msg::Finished(Outcome::Cancelled));
                    return;
                }
                Err(XErr::Io(e)) => {
                    crate::log::append(&format!("engine_exit idx={idx} FAILED: {e}"));
                    any_failed = true;
                    let _ = tx.send(Msg::ItemDone {
                        idx,
                        res: Err(e),
                        sha: String::new(),
                    });
                }
            }
        }

        if steps == 0 {
            crate::log::append("nothing to transfer (all skipped)");
        }
        let outcome = if any_failed {
            Outcome::Failed("Einzelne Dateien konnten nicht kopiert werden — siehe Zusammenfassung.".into())
        } else {
            Outcome::Ok
        };
        let _ = tx.send(Msg::Finished(outcome));
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rsync_progress2() {
        let p = parse_progress2("          2.37G  46%   41.39MB/s    0:00:47 (xfr#12, to-chk=4/17)").unwrap();
        assert!((p.done as f64 - 2.37 * 1024.0 * 1024.0 * 1024.0).abs() < 1024.0 * 1024.0);
        assert_eq!(p.pct, 46.0);
        assert_eq!(p.speed, "41.39MB/s");
        assert_eq!(p.eta, "0:00:47");
        assert_eq!(p.xfr, 12);
        assert_eq!(p.tochk, (4, 17));
    }

    #[test]
    fn parses_progress_without_paren() {
        let p = parse_progress2("512B   0%    0.00kB/s    0:00:00").unwrap();
        assert_eq!(p.done, 512);
        assert_eq!(p.pct, 0.0);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_progress2("hello world").is_none());
        assert!(parse_progress2("").is_none());
    }

    #[test]
    fn human_bytes() {
        assert_eq!(human_to_bytes("512"), Some(512));
        assert_eq!(human_to_bytes("1.00K"), Some(1024));
        assert_eq!(human_to_bytes("2.37G"), Some((2.37 * 1024.0 * 1024.0 * 1024.0) as u64));
        assert_eq!(human_to_bytes("abc"), None);
    }

    #[test]
    fn argv_build() {
        let args = rsync_argv(
            false,
            false,
            &["/bad name.txt".to_string()],
            &[PathBuf::from("/src/a b.txt")],
            Path::new("/mnt/nas"),
        );
        let s: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(s.contains(&"--exclude=/bad name.txt".to_string()));
        assert!(s.contains(&"--append-verify".to_string()));
        assert_eq!(s.last().unwrap(), "/mnt/nas");

        let b = rsync_argv(true, true, &[], &[], Path::new("/tmp"));
        assert!(b.contains(&"--checksum".into()));
        assert!(!b.contains(&"--append-verify".into()));
        assert!(b.contains(&"--modify-window=1".into()));
    }

    #[test]
    fn stats_extraction() {
        let out = "\r  1.00G  99%   40MB/s    0:00:01 (xfr#1, to-chk=0/1)\nNumber of files: 1 (reg: 1)\nTotal transferred file size: 1.00G bytes\n";
        let s = extract_stats(out).unwrap();
        assert!(s.contains("Number of files"));
        assert!(s.contains("Total transferred"));
    }
}
