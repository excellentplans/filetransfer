//! Headless self-test: `filetransfer --selftest`
//!
//! Exercises the full pipeline without GUI: conflict scan, FAT32 chunked
//! copy (with a small test limit), resume, verify, and a real rsync run.

use crate::args::SrcInfo;
use crate::conflicts::{self, EngineKind, Policy};
use crate::fsutil::FsKind;
use crate::transfer::{self, Msg, Outcome};
use crate::verify;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

pub fn run() {
    println!("filetransfer --selftest");
    step("rsync vorhanden", test_rsync_present);
    step("progress2-Parser", test_progress);
    step("Namensregeln", test_names);
    step("Lock (eine Instanz)", test_lock);
    step("Rundtrip: FAT32-Zerlegung + rsync + Verify", test_roundtrip);
    step("Konflikt-Übernehmen", test_conflict_apply);
    println!("SELBSTTEST OK");
}

fn step(name: &str, f: fn()) {
    print!("  {name} … ");
    f();
    println!("ok");
}

fn test_rsync_present() {
    assert!(std::path::Path::new(transfer::RSYNC_PATH).exists(), "rsync fehlt");
}

fn test_progress() {
    let p = transfer::parse_progress2(
        "          2.37G  46%   41.39MB/s    0:00:47 (xfr#12, to-chk=4/17)",
    )
    .expect("parse");
    assert_eq!(p.pct, 46.0);
    assert_eq!(p.xfr, 12);
    assert_eq!(p.tochk, (4, 17));
    assert!(transfer::parse_progress2("kein fortschritt").is_none());
}

fn test_names() {
    use std::collections::HashSet;
    let mut t = HashSet::new();
    t.insert("video.mkv".to_string());
    assert_eq!(conflicts::unique_name("video.mkv", &t), "video (2).mkv");
    assert_eq!(conflicts::sanitize("a:b*c?"), "a_b_c_");
    assert!(!conflicts::name_is_fat_ok("x/y"));
}

fn test_lock() {
    let a = crate::log::InstanceLock::acquire().expect("first lock");
    assert!(crate::log::InstanceLock::acquire().is_err(), "second lock must fail");
    drop(a);
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ft-selftest-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::create_dir_all(d.join("dst")).unwrap();
    d
}

fn run_transfer(plan: crate::conflicts::Plan) -> Outcome {
    let rx = transfer::spawn(plan, Arc::new(AtomicBool::new(false)));
    loop {
        match rx.recv_timeout(Duration::from_secs(120)) {
            Ok(Msg::Finished(o)) => return o,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => panic!("transfer timeout"),
            Err(RecvTimeoutError::Disconnected) => panic!("transfer channel closed"),
        }
    }
}

fn run_verify(plan: crate::conflicts::Plan) -> verify::VerifyReport {
    let rx = verify::spawn(plan, Arc::new(AtomicBool::new(false)));
    loop {
        match rx.recv_timeout(Duration::from_secs(120)) {
            Ok(verify::VMsg::Done(Ok(r))) => return r,
            Ok(verify::VMsg::Done(Err(e))) => panic!("verify error: {e}"),
            Ok(_) => {}
            Err(_) => panic!("verify channel closed"),
        }
    }
}

fn test_roundtrip() {
    let tmp = tmpdir("round");
    let src_dir = tmp.join("src");
    let dst = tmp.join("dst");

    // 5 MiB pattern file → chunked (limit 1 MiB → 5 parts)
    let big: Vec<u8> = (0..5 * 1024 * 1024u32)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(7))
        .collect();
    std::fs::write(src_dir.join("big.bin"), &big).unwrap();
    std::fs::write(src_dir.join("small.txt"), b"hallo welt").unwrap();

    let sources = vec![SrcInfo {
        path: src_dir.clone(),
        name: "src".into(),
        size: 0,
        is_dir: true,
    }];
    let mut noop = |_n: usize| {};
    let plan = conflicts::scan(dst.clone(), FsKind::Fat, &sources, 1024 * 1024, &mut noop).unwrap();

    let chunk_idx = plan
        .items
        .iter()
        .position(|i| i.dst_name == "big.bin")
        .expect("big.bin item");
    assert_eq!(plan.items[chunk_idx].engine, EngineKind::Chunks);
    assert_eq!(plan.items[chunk_idx].chunk, 1024 * 1024);
    assert!(plan.excludes.iter().any(|e| e == "/big.bin"));
    assert!(plan.needs_run_a(), "rsync run A for small.txt");

    let outcome = run_transfer(plan.clone());
    assert!(matches!(outcome, Outcome::Ok), "transfer must succeed");

    // parts exist and reassemble to the original (tree layout: dst/src/…)
    let parts_dir = dst.join("src");
    let mut assembled = Vec::new();
    for k in 1..=5 {
        assembled.extend(std::fs::read(parts_dir.join(format!("big.bin.{k:03}"))).unwrap());
    }
    assert_eq!(assembled.len(), big.len());
    assert_eq!(assembled, big);
    assert_eq!(std::fs::read(parts_dir.join("small.txt")).unwrap(), b"hallo welt");

    // verify pass: both items must be OK
    let rep = run_verify(plan.clone());
    assert_eq!(rep.fails.len(), 0, "verify fails: {rep:?}");
    assert_eq!(rep.total, 2, "2 verified items (rsync item + chunk item)");

    // resume: remove last part, mark Resume, rerun
    std::fs::remove_file(parts_dir.join("big.bin.005")).unwrap();
    let mut plan2 = plan.clone();
    plan2.items[chunk_idx].policy = Policy::Resume;
    let outcome = run_transfer(plan2);
    assert!(matches!(outcome, Outcome::Ok), "resume must succeed");
    let mut assembled2 = Vec::new();
    for k in 1..=5 {
        assembled2.extend(std::fs::read(parts_dir.join(format!("big.bin.{k:03}"))).unwrap());
    }
    assert_eq!(assembled2, big, "resumed content must match");

    // cleanup
    let _ = std::fs::remove_dir_all(&tmp);
}

fn test_conflict_apply() {
    let tmp = tmpdir("conf");
    let src = tmp.join("src");
    let dst = tmp.join("dst");
    std::fs::write(src.join("a.txt"), b"AAA").unwrap();
    std::fs::write(dst.join("a.txt"), b"LONGER-EXISTING").unwrap();

    let sources = vec![SrcInfo {
        path: src.join("a.txt"),
        name: "a.txt".into(),
        size: 3,
        is_dir: false,
    }];
    let mut noop = |_n: usize| {};
    let mut plan = conflicts::scan(dst.clone(), FsKind::Posix, &sources, u64::MAX, &mut noop).unwrap();
    assert_eq!(plan.conflicts.len(), 1);
    let idx = plan.conflicts[0].item_idx;
    assert_eq!(plan.conflicts[0].suggestion, "a (2).txt");

    conflicts::apply_conflict(&mut plan, idx, Policy::Overwrite, None);
    assert!(plan.rsync_b_srcs.contains(&src.join("a.txt")));
    assert!(plan.rsync_a_srcs.is_empty());
    assert!(conflicts::plan_invariants(&plan));

    let outcome = run_transfer(plan.clone());
    assert!(matches!(outcome, Outcome::Ok));
    assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"AAA");

    let rep = run_verify(plan);
    assert!(rep.fails.is_empty());
    let _ = std::fs::remove_dir_all(&tmp);
}
