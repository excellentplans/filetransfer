//! Conflict detection and transfer planning.
//!
//! Produces a `Plan`: rsync runs (A = new/merge/resume, B = forced overwrite
//! with --checksum) plus "engine" items that bypass rsync — used for FAT32
//! chunk-splitting (>4 GiB), FAT-illegal names and user-chosen renames.

use crate::args::SrcInfo;
use crate::fsutil::{FsKind, FAT_CHUNK};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
// (dest:: helpers not needed here)

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Policy {
    New,
    Skip,
    Overwrite,
    Rename,
    Resume,
}

impl Policy {
    pub fn label(self) -> &'static str {
        match self {
            Policy::New => "Neu",
            Policy::Skip => "Überspringen",
            Policy::Overwrite => "Überschreiben",
            Policy::Rename => "Umbenennen",
            Policy::Resume => "Fortsetzen",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EngineKind {
    /// Normal rsync transfer.
    Rsync,
    /// Plain sequential copy (renamed files, FAT-illegal names).
    Copy,
    /// FAT32 chunked copy (file > 4 GiB).
    Chunks,
}

#[derive(Clone)]
pub struct Item {
    pub src: PathBuf,
    /// Relative path inside its top-level source arg ("" = the arg itself).
    pub rel: String,
    /// Index of the top-level source arg this belongs to.
    pub owner: usize,
    /// Destination parent directory.
    pub dst_dir: PathBuf,
    /// Destination file name (after rename policy).
    pub dst_name: String,
    pub size: u64,
    pub is_dir: bool,
    pub engine: EngineKind,
    /// FAT32 chunk size for Chunks items (0 otherwise).
    pub chunk: u64,
    pub policy: Policy,
}

impl Item {
    pub fn dst_path(&self) -> PathBuf {
        self.dst_dir.join(&self.dst_name)
    }
}

#[derive(Clone)]
pub struct Conflict {
    pub item_idx: usize,
    pub existing_size: Option<u64>,
    pub existing_is_dir: bool,
    /// Suggested unique name (for Rename).
    pub suggestion: String,
    /// Which choices make sense for this item.
    pub options: Vec<Policy>,
}

#[derive(Clone)]
pub struct Plan {
    pub dest: PathBuf,
    pub fs: FsKind,
    /// Per-file split threshold (FAT_MAX_FILE in production, tunable for tests).
    pub fat_limit: u64,
    pub items: Vec<Item>,
    /// Conflicts the user must resolve (indices into items).
    pub conflicts: Vec<Conflict>,
    /// Sources for rsync run A (new/merge/resume).
    pub rsync_a_srcs: Vec<PathBuf>,
    /// Sources for rsync run B (overwrite, --checksum).
    pub rsync_b_srcs: Vec<PathBuf>,
    /// Top-level items renamed by the user → one single-source rsync run each,
    /// targeting `<dest>/<new name>` (rsync can't rename within a multi-source run).
    pub rsync_rename: Vec<usize>,
    /// Anchored exclude patterns protecting engine-owned paths from rsync.
    pub excludes: Vec<String>,
    /// Engine item indices in execution order.
    pub engine: Vec<usize>,
    /// Approximate payload (files only) — for the free-space check.
    pub total_bytes: u64,
}

impl Plan {
    pub fn needs_run_a(&self) -> bool {
        !self.rsync_a_srcs.is_empty()
    }
    pub fn needs_run_b(&self) -> bool {
        !self.rsync_b_srcs.is_empty()
    }
    pub fn is_fat(&self) -> bool {
        self.fs == FsKind::Fat
    }
}

// ---------- name helpers ----------

/// Characters Windows/FAT forbid in file names.
pub const FAT_ILLEGAL: [char; 9] = ['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

pub fn name_is_fat_ok(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if name.bytes().any(|b| b < 0x20) {
        return false;
    }
    if FAT_ILLEGAL.iter().any(|c| name.contains(*c)) {
        return false;
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return false;
    }
    // FAT long names are limited to 255 UTF-16 code units.
    name.encode_utf16().count() <= 255
}

/// Make a name FAT-storable. Identity for already-legal names.
pub fn sanitize(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if c.is_control() || FAT_ILLEGAL.contains(&c) { '_' } else { c })
        .collect();
    while out.ends_with(' ') || out.ends_with('.') {
        out.pop();
    }
    if out.is_empty() {
        out = "datei".to_string();
    }
    while out.encode_utf16().count() > 255 {
        out.pop();
    }
    out
}

/// Split "name.ext" -> ("name", ".ext"); extensionless -> (name, "").
fn split_ext(name: &str) -> (&str, &str) {
    if let Some(i) = name.rfind('.') {
        if i > 0 && i + 1 < name.len() {
            return (&name[..i], &name[i..]);
        }
    }
    (name, "")
}

/// Dolphin-style unique name: "name (2).ext", "name (3).ext", …
pub fn unique_name(orig: &str, taken: &HashSet<String>) -> String {
    let (stem, ext) = split_ext(orig);
    for k in 2..u32::MAX {
        let cand = format!("{stem} ({k}){ext}");
        if !taken.contains(&cand) {
            return cand;
        }
    }
    unreachable!()
}

/// Escape wildcard characters for a rsync exclude pattern.
fn excl_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '*' | '?' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Which FAT chunk of `name` is the first incomplete one, if any.
/// `Some(k)` means parts 0..k exist complete and part k exists partially
/// (or is missing while later parts don't exist either).
pub fn chunk_resume_state(dst_dir: &Path, name: &str, size: u64, chunk: u64) -> Option<usize> {
    let parts = size.div_ceil(chunk);
    for k in 0..parts {
        // Part files are 1-based: name.001, name.002, …
        let p = dst_dir.join(format!("{name}.{:03}", k + 1));
        let expected = (size - k * chunk).min(chunk);
        match std::fs::metadata(&p) {
            Ok(md) if md.is_file() && md.len() == expected => continue,
            Ok(md) if md.is_file() && md.len() < expected => return Some(k as usize),
            _ => {
                return if k == 0 { None } else { Some(k as usize) };
            }
        }
    }
    None // everything complete
}

// ---------- scan ----------

pub type ProgressCb<'a> = &'a mut dyn FnMut(usize);

/// Build the transfer plan. Blocking (CIFS!) — call from a worker thread.
pub fn scan(
    dest: PathBuf,
    fs: FsKind,
    sources: &[SrcInfo],
    fat_limit: u64,
    progress: ProgressCb,
) -> Result<Plan, String> {
    let fat = fs == FsKind::Fat;

    // Destination root listing: base for top-level conflict checks & renames.
    let mut taken: HashSet<String> = HashSet::new();
    match std::fs::read_dir(&dest) {
        Ok(rd) => {
            for e in rd.flatten() {
                taken.insert(e.file_name().to_string_lossy().into_owned());
            }
        }
        Err(e) => {
            return Err(format!(
                "Zielordner nicht lesbar:\n{}\n{}",
                dest.display(),
                crate::fsutil::ioerr(&e)
            ))
        }
    }

    let mut plan = Plan {
        dest: dest.clone(),
        fs,
        fat_limit,
        items: Vec::new(),
        conflicts: Vec::new(),
        rsync_a_srcs: Vec::new(),
        rsync_b_srcs: Vec::new(),
        rsync_rename: Vec::new(),
        excludes: Vec::new(),
        engine: Vec::new(),
        total_bytes: 0,
    };

    let mut seen_names: HashSet<String> = HashSet::new();
    let mut n = 0usize;

    for (owner, src) in sources.iter().enumerate() {
        let top_name = src.name.clone();
        let top_illegal = fat && !name_is_fat_ok(&top_name);
        let top_big = fat && !src.is_dir && src.size > fat_limit;

        if top_illegal || top_big {
            // Whole argument bypasses rsync (engine copy / chunks).
            let dst_name = if top_illegal { sanitize(&top_name) } else { top_name.clone() };
            let engine = if top_big { EngineKind::Chunks } else { EngineKind::Copy };
            let item = Item {
                src: src.path.clone(),
                rel: String::new(),
                owner,
                dst_dir: dest.clone(),
                dst_name,
                size: src.size,
                is_dir: src.is_dir,
                engine,
                chunk: 0,
                policy: Policy::New,
            };
            taken.insert(top_name.clone());
            seen_names.insert(top_name.clone());
            let top_dst = item.dst_name.clone();
            push_engine_item(&mut plan, item, &dest, &top_dst, &mut taken, &mut n, progress);
            if src.is_dir {
                expand_subtree(
                    &mut plan,
                    &src.path,
                    "",
                    &top_dst,
                    owner,
                    &dest,
                    &mut taken,
                    &mut n,
                    progress,
                );
            }
            continue;
        }

        // Regular top-level item (rsync candidate).
        let dst_name = top_name.clone();
        let dst_path = dest.join(&dst_name);
        let existing = std::fs::symlink_metadata(&dst_path).ok();
        let mut item = Item {
            src: src.path.clone(),
            rel: String::new(),
            owner,
            dst_dir: dest.clone(),
            dst_name: dst_name.clone(),
            size: src.size,
            is_dir: src.is_dir,
            engine: EngineKind::Rsync,
            chunk: 0,
            policy: Policy::New,
        };
        n += 1;
        progress(n);

        if let Some(md) = existing {
            if src.is_dir && md.is_dir() {
                // Merge — not a conflict.
                item.policy = Policy::New;
                plan.rsync_a_srcs.push(src.path.clone());
            } else if src.is_dir != md.is_dir() {
                // Type clash: can't overwrite a dir with a file or vice versa.
                let suggestion = unique_name(&dst_name, &taken);
                plan.conflicts.push(Conflict {
                    item_idx: plan.items.len(),
                    existing_size: None,
                    existing_is_dir: md.is_dir(),
                    suggestion,
                    options: vec![Policy::Skip, Policy::Rename],
                });
                item.policy = Policy::Skip;
            } else {
                // file vs file conflict
                let dsize = md.len();
                let mut options = vec![Policy::Skip, Policy::Overwrite, Policy::Rename];
                if dsize < src.size {
                    options.push(Policy::Resume);
                }
                let suggestion = unique_name(&dst_name, &taken);
                plan.conflicts.push(Conflict {
                    item_idx: plan.items.len(),
                    existing_size: Some(dsize),
                    existing_is_dir: false,
                    suggestion,
                    options,
                });
                item.policy = Policy::Skip;
            }
        } else {
            item.policy = Policy::New;
            plan.rsync_a_srcs.push(src.path.clone());
        }

        // Duplicate basename among selected sources -> second one must rename.
        if seen_names.contains(&dst_name) && item.policy == Policy::New {
            let suggestion = unique_name(&dst_name, &taken);
            plan.conflicts.push(Conflict {
                item_idx: plan.items.len(),
                existing_size: None,
                existing_is_dir: false,
                suggestion,
                options: vec![Policy::Skip, Policy::Rename],
            });
            item.policy = Policy::Skip;
            plan.rsync_a_srcs.retain(|p| p != &src.path);
        }
        seen_names.insert(dst_name.clone());
        taken.insert(dst_name.clone());
        plan.items.push(item);

        // FAT: walk the subtree for big / FAT-illegal entries.
        if fat && src.is_dir {
            walk_fat(
                &mut plan,
                &src.path,
                "",
                &dst_name,
                owner,
                &dest,
                &mut taken,
                &mut n,
                progress,
            );
        }
    }

    // Size accounting (files only).
    plan.total_bytes = plan.items.iter().filter(|i| !i.is_dir).map(|i| i.size).sum();
    Ok(plan)
}

/// Apply a user decision to a conflict. Keeps the rsync run lists consistent:
/// renamed top-level rsync items get their own single-source run, and deep
/// engine items of a renamed FAT subtree are remapped to the new name.
pub fn apply_conflict(plan: &mut Plan, idx: usize, policy: Policy, name: Option<String>) {
    let is_fat = plan.is_fat();
    let old_name = plan.items[idx].dst_name.clone();
    let owner = plan.items[idx].owner;
    let engine = &mut plan.items[idx];
    engine.policy = policy;
    if let Some(n) = name {
        engine.dst_name = n;
    }
    match policy {
        Policy::New | Policy::Skip => {
            plan.rsync_a_srcs.retain(|p| *p != engine.src);
            plan.rsync_b_srcs.retain(|p| *p != engine.src);
            plan.rsync_rename.retain(|&i| i != idx);
        }
        Policy::Overwrite => {
            plan.rsync_a_srcs.retain(|p| *p != engine.src);
            plan.rsync_b_srcs.retain(|p| *p != engine.src);
            plan.rsync_rename.retain(|&i| i != idx);
            if engine.engine == EngineKind::Rsync {
                plan.rsync_b_srcs.push(engine.src.clone());
            }
        }
        Policy::Resume => {
            plan.rsync_a_srcs.retain(|p| *p != engine.src);
            plan.rsync_b_srcs.retain(|p| *p != engine.src);
            plan.rsync_rename.retain(|&i| i != idx);
            if engine.engine == EngineKind::Rsync {
                plan.rsync_a_srcs.push(engine.src.clone());
            }
        }
        Policy::Rename => {
            plan.rsync_a_srcs.retain(|p| *p != engine.src);
            plan.rsync_b_srcs.retain(|p| *p != engine.src);
            if engine.engine == EngineKind::Rsync {
                // Top-level rename: dedicated single-source rsync run to the
                // new name (rsync can't rename inside a multi-source run).
                if !plan.rsync_rename.contains(&idx) {
                    plan.rsync_rename.push(idx);
                }
                // Remap deep engine items of this FAT subtree to the new name.
                if is_fat {
                    let old_prefix = plan.dest.join(&old_name);
                    let new_prefix = plan.dest.join(engine.dst_name.clone());
                    let engine_idxs = plan.engine.clone();
                    for &di in &engine_idxs {
                        if plan.items[di].owner != owner || plan.items[di].rel.is_empty() {
                            continue;
                        }
                        if let Ok(stripped) = plan.items[di].dst_dir.strip_prefix(&old_prefix) {
                            let stripped = stripped.to_path_buf();
                            plan.items[di].dst_dir = new_prefix.join(stripped);
                        }
                    }
                }
            }
        }
    }
}

/// Sanity helper: all engine indices valid, no rsync item left in engine mode.
pub fn plan_invariants(plan: &Plan) -> bool {
    for &i in &plan.engine {
        if i >= plan.items.len() || plan.items[i].engine == EngineKind::Rsync {
            return false;
        }
    }
    true
}

fn push_engine_item(
    plan: &mut Plan,
    mut item: Item,
    dest: &Path,
    top_name: &str,
    taken: &mut HashSet<String>,
    n: &mut usize,
    progress: ProgressCb,
) {
    // Deep engine items live under <dest>/<top arg name>/<relative parents>,
    // matching rsync's tree layout; components are sanitized on FAT.
    if item.rel.is_empty() {
        item.dst_dir = dest.to_path_buf();
    } else {
        let mut comps: Vec<String> = vec![sanitize(top_name)];
        comps.extend(
            item.rel
                .split('/')
                .filter(|c| !c.is_empty())
                .map(|c| sanitize(c)),
        );
        let dir_comps = &comps[..comps.len() - 1];
        item.dst_dir = dir_comps.iter().fold(dest.to_path_buf(), |p, c| p.join(c));
    }
    if !name_is_fat_ok(&item.dst_name) {
        item.dst_name = sanitize(&item.dst_name);
    }
    item.chunk = FAT_CHUNK.min(plan.fat_limit);

    // Conflict check for the engine item's concrete destination path
    // (dir-vs-dir merges silently).
    let dst = item.dst_path();
    if let Ok(md) = std::fs::symlink_metadata(&dst) {
        if item.is_dir && md.is_dir() {
            item.policy = Policy::New; // merge
        } else {
            let mut options = vec![Policy::Skip, Policy::Overwrite, Policy::Rename];
            if !item.is_dir
                && md.is_file()
                && md.len() < item.size
                && item.engine == EngineKind::Chunks
                && chunk_resume_state(&item.dst_dir, &item.dst_name, item.size, item.chunk).is_some()
            {
                options.push(Policy::Resume);
            }
            let suggestion = unique_name(&item.dst_name, taken);
            plan.conflicts.push(Conflict {
                item_idx: plan.items.len(),
                existing_size: Some(md.len()),
                existing_is_dir: md.is_dir(),
                suggestion,
                options,
            });
            item.policy = Policy::Skip;
        }
    } else {
        item.policy = Policy::New;
    }
    taken.insert(item.dst_name.clone());

    if !item.rel.is_empty() {
        let pat = format!("/{}", excl_escape(&item.rel));
        if !plan.excludes.contains(&pat) {
            plan.excludes.push(pat);
        }
    }
    if !item.is_dir {
        plan.total_bytes += item.size;
    }
    plan.engine.push(plan.items.len());
    plan.items.push(item);
    *n += 1;
    progress(*n);
}

/// Walk a directory subtree on a FAT destination, extracting entries that
/// rsync can't handle there: files over the FAT limit (chunked) and
/// FAT-illegal names (renamed engine copies).
#[allow(clippy::too_many_arguments)]
fn walk_fat(
    plan: &mut Plan,
    root: &Path,
    rel: &str,
    top_name: &str,
    owner: usize,
    dest: &Path,
    taken: &mut HashSet<String>,
    n: &mut usize,
    progress: ProgressCb,
) {
    let dir = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    let mut entries: Vec<(PathBuf, String, bool, u64, bool)> = Vec::new();
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
        let legal = name_is_fat_ok(&name);
        entries.push((e.path(), name, ft.is_dir(), size, legal));
        *n += 1;
        progress(*n);
    }
    entries.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));

    for (path, name, is_dir, size, legal) in entries {
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };

        if !legal {
            // FAT-illegal entry: subtree leaves rsync entirely.
            plan.excludes.push(format!("/{}", excl_escape(&child_rel)));
            let item = Item {
                src: path.clone(),
                rel: child_rel.clone(),
                owner,
                dst_dir: PathBuf::new(),
                dst_name: name.clone(),
                size: if is_dir { 0 } else { size },
                is_dir,
                engine: EngineKind::Copy,
                chunk: 0,
                policy: Policy::New,
            };
            push_engine_item(plan, item, dest, top_name, taken, n, progress);
            if is_dir {
                expand_subtree(plan, root, &child_rel, top_name, owner, dest, taken, n, progress);
            }
        } else if is_dir {
            walk_fat(plan, root, &child_rel, top_name, owner, dest, taken, n, progress);
        } else if size > plan.fat_limit {
            let item = Item {
                src: path.clone(),
                rel: child_rel.clone(),
                owner,
                dst_dir: PathBuf::new(),
                dst_name: name.clone(),
                size,
                is_dir: false,
                engine: EngineKind::Chunks,
                chunk: 0,
                policy: Policy::New,
            };
            push_engine_item(plan, item, dest, top_name, taken, n, progress);
        }
    }
}

/// Engine-copy every entry of a subtree (renamed/illegal roots on FAT).
/// Directories are recreated (including empty ones); big files chunked.
#[allow(clippy::too_many_arguments)]
fn expand_subtree(
    plan: &mut Plan,
    root: &Path,
    rel: &str,
    top_name: &str,
    owner: usize,
    dest: &Path,
    taken: &mut HashSet<String>,
    n: &mut usize,
    progress: ProgressCb,
) {
    let dir = root.join(rel);
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    let mut entries: Vec<(PathBuf, String, bool, u64)> = Vec::new();
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
        entries.push((e.path(), name, ft.is_dir(), size));
        *n += 1;
        progress(*n);
    }
    entries.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));

    for (path, name, is_dir, size) in entries {
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let engine = if !is_dir && size > plan.fat_limit {
            EngineKind::Chunks
        } else {
            EngineKind::Copy
        };
        let item = Item {
            src: path.clone(),
            rel: child_rel.clone(),
            owner,
            dst_dir: PathBuf::new(),
            dst_name: name.clone(),
            size: if is_dir { 0 } else { size },
            is_dir,
            engine,
            chunk: 0,
            policy: Policy::New,
        };
        push_engine_item(plan, item, dest, top_name, taken, n, progress);
        if is_dir {
            expand_subtree(plan, root, &child_rel, top_name, owner, dest, taken, n, progress);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::{FAT_CHUNK, FAT_MAX_FILE};

    #[test]
    fn rename_generator_dolphin_style() {
        let mut t = HashSet::new();
        t.insert("video.mkv".to_string());
        assert_eq!(unique_name("video.mkv", &t), "video (2).mkv");
        t.insert("video (2).mkv".to_string());
        assert_eq!(unique_name("video.mkv", &t), "video (3).mkv");
        assert_eq!(unique_name("ohne-ext", &t), "ohne-ext (2)");
        t.insert(".hidden".to_string());
        assert_eq!(unique_name(".hidden", &t), ".hidden (2)");
    }

    #[test]
    fn sanitizer_fixes_illegal() {
        assert_eq!(sanitize("a:b*c?"), "a_b_c_");
        assert_eq!(sanitize("ende."), "ende");
        assert_eq!(sanitize("ende "), "ende");
        assert!(name_is_fat_ok("Fotos allgemein (2).jpg"));
        assert!(!name_is_fat_ok("a/b"));
        assert!(!name_is_fat_ok("Punkt."));
        assert!(!name_is_fat_ok(""));
    }

    #[test]
    fn split_ext_corner_cases() {
        assert_eq!(split_ext("a.b.c"), ("a.b", ".c"));
        assert_eq!(split_ext(".hidden"), (".hidden", ""));
        assert_eq!(split_ext("name"), ("name", ""));
    }

    #[test]
    fn scan_plain_conflicts() {
        let tmp = std::env::temp_dir().join("ft-scan-t1");
        let src = tmp.join("src");
        let dst = tmp.join("dst");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("a.txt"), b"AAA").unwrap();
        std::fs::write(dst.join("a.txt"), b"AB").unwrap(); // smaller at dest → Resume offered

        let sources = vec![SrcInfo {
            path: src.join("a.txt"),
            name: "a.txt".into(),
            size: 3,
            is_dir: false,
        }];
        let mut noop = |_n: usize| {};
        let plan = scan(dst.clone(), FsKind::Posix, &sources, FAT_MAX_FILE, &mut noop).unwrap();
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].suggestion, "a (2).txt");
        assert!(plan.conflicts[0].options.contains(&Policy::Resume));
        assert!(plan_invariants(&plan));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn scan_fat_illegal_name_engine() {
        let tmp = std::env::temp_dir().join("ft-scan-t2");
        let src = tmp.join("srcdir");
        let dst = tmp.join("dst");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("bad:name.txt"), b"x").unwrap();

        let sources = vec![SrcInfo {
            path: src.clone(),
            name: "srcdir".into(),
            size: 0,
            is_dir: true,
        }];
        let mut noop = |_n: usize| {};
        let plan = scan(dst, FsKind::Fat, &sources, FAT_MAX_FILE, &mut noop).unwrap();
        let bad = plan.items.iter().find(|i| i.src.ends_with("bad:name.txt")).unwrap();
        assert_eq!(bad.engine, EngineKind::Copy);
        assert_eq!(bad.dst_name, "bad_name.txt");
        assert!(plan.excludes.iter().any(|e| e == "/bad:name.txt"));
        assert!(plan_invariants(&plan));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn scan_fat_big_file_chunked() {
        let tmp = std::env::temp_dir().join("ft-scan-t3");
        let src = tmp.join("srcdir");
        let dst = tmp.join("dst");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("groovy.mkv"), vec![0u8; 5000]).unwrap();

        let sources = vec![SrcInfo {
            path: src.clone(),
            name: "srcdir".into(),
            size: 0,
            is_dir: true,
        }];
        let mut noop = |_n: usize| {};
        // fat_limit = 4096 bytes for the test
        let plan = scan(dst, FsKind::Fat, &sources, 4096, &mut noop).unwrap();
        let big = plan.items.iter().find(|i| i.src.ends_with("groovy.mkv")).unwrap();
        assert_eq!(big.engine, EngineKind::Chunks);
        assert!(plan.excludes.iter().any(|e| e == "/groovy.mkv"));
        assert!(plan_invariants(&plan));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn fat_max_constant() {
        assert_eq!(FAT_MAX_FILE, 4 * 1024 * 1024 * 1024 - 1);
        assert!(FAT_CHUNK < FAT_MAX_FILE);
    }

    #[test]
    fn chunk_resume_math() {
        let tmp = std::env::temp_dir().join("ft-resume-t");
        let d = tmp.join("dst");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&d).unwrap();
        // size 10, chunk 4 -> parts .001..003 with expected 4,4,2
        assert_eq!(chunk_resume_state(&d, "f.bin", 10, 4), None);
        std::fs::write(d.join("f.bin.001"), vec![0; 4]).unwrap();
        assert_eq!(chunk_resume_state(&d, "f.bin", 10, 4), Some(1));
        std::fs::write(d.join("f.bin.002"), vec![0; 2]).unwrap(); // partial
        assert_eq!(chunk_resume_state(&d, "f.bin", 10, 4), Some(1));
        std::fs::write(d.join("f.bin.002"), vec![0; 4]).unwrap(); // complete
        std::fs::write(d.join("f.bin.003"), vec![0; 1]).unwrap(); // partial
        assert_eq!(chunk_resume_state(&d, "f.bin", 10, 4), Some(2));
        std::fs::write(d.join("f.bin.003"), vec![0; 2]).unwrap(); // complete
        assert_eq!(chunk_resume_state(&d, "f.bin", 10, 4), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
