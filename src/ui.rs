//! egui UI — German screens: intake (drop/picker), destination picker,
//! conflicts, transfer (copy bar + completion symbol + verify bar), summary.

use crate::args::{self, SrcInfo};
use crate::conflicts::{self, EngineKind, Plan, Policy};
use crate::dest::{self, Listing, Location, LocationKind};
use crate::fsutil::{self, FsKind, FAT_MAX_FILE};
use crate::log;
use crate::state::Settings;
use crate::transfer::{self, Msg, Outcome, Prog};
use crate::verify::{self, VerifyReport, VProg};
use egui::{Align2, Color32, Context, FontId, Sense, ViewportCommand};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------- palette ----------
//
// Petrol as the single interaction accent; status colors are reserved for
// status. Two palettes (KDE light / dark), chosen at startup.

struct Palette {
    panel: Color32,
    extreme: Color32,
    faint: Color32,
    text: Color32,
    muted: Color32,
    line: Color32,
    accent: Color32,
    on_accent: Color32,
    accent_soft: Color32,
    /// Blue of the verification progress bar (distinct from petrol).
    verify: Color32,
    ok: Color32,
    warn: Color32,
    err: Color32,
    hover: Color32,
    widget: Color32,
    /// Text color that stays readable on saturated status fills (bars, discs).
    on_fill: Color32,
}

const LIGHT: Palette = Palette {
    panel: Color32::from_rgb(0xF6, 0xF9, 0xFA),
    extreme: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    faint: Color32::from_rgb(0xE8, 0xEE, 0xF1),
    text: Color32::from_rgb(0x19, 0x1F, 0x23),
    muted: Color32::from_rgb(0x4A, 0x5A, 0x64),
    line: Color32::from_rgb(0xB4, 0xC1, 0xC9),
    accent: Color32::from_rgb(0x07, 0x7D, 0x9C),
    on_accent: Color32::WHITE,
    accent_soft: Color32::from_rgba_premultiplied(10, 125, 156, 34),
    verify: Color32::from_rgb(0x1B, 0x62, 0xC7),
    ok: Color32::from_rgb(0x18, 0x9D, 0x53),
    warn: Color32::from_rgb(0xB2, 0x75, 0x0F),
    err: Color32::from_rgb(0xC2, 0x3B, 0x2F),
    hover: Color32::from_rgb(0xDD, 0xE8, 0xEC),
    widget: Color32::from_rgb(0xE6, 0xED, 0xF0),
    on_fill: Color32::WHITE,
};

const DARK: Palette = Palette {
    panel: Color32::from_rgb(0x26, 0x2C, 0x30),
    extreme: Color32::from_rgb(0x17, 0x1C, 0x1E),
    faint: Color32::from_rgb(0x2F, 0x36, 0x3A),
    text: Color32::from_rgb(0xEE, 0xF3, 0xF4),
    muted: Color32::from_rgb(0xAB, 0xB8, 0xBE),
    line: Color32::from_rgb(0x55, 0x61, 0x67),
    accent: Color32::from_rgb(0x3F, 0xC4, 0xDE),
    on_accent: Color32::from_rgb(0x0A, 0x21, 0x29),
    accent_soft: Color32::from_rgba_premultiplied(11, 31, 36, 46),
    verify: Color32::from_rgb(0x64, 0xAD, 0xEC),
    ok: Color32::from_rgb(0x40, 0xCF, 0x84),
    warn: Color32::from_rgb(0xEC, 0xC2, 0x5E),
    err: Color32::from_rgb(0xF2, 0x7E, 0x73),
    hover: Color32::from_rgb(0x35, 0x3D, 0x42),
    widget: Color32::from_rgb(0x2C, 0x33, 0x37),
    on_fill: Color32::from_rgb(0x0E, 0x1C, 0x22),
};

#[derive(Clone, PartialEq)]
pub enum Screen {
    Startup,
    Intake,
    Fatal(String),
    AlreadyRunning,
    Pick,
    Conflicts,
    Transfer,
    Summary,
}

pub enum ScanMsg {
    Prog(usize),
    Done(Result<Plan, String>),
}

pub struct App {
    pal: &'static Palette,
    pub settings: Settings,
    /// "Einstellungen" dialog (verify/autoclose) — open over any screen.
    show_settings: bool,
    /// Held for the app lifetime: enforces single instance.
    pub _lock: Option<crate::log::InstanceLock>,
    pub startup: Option<Receiver<Result<Vec<SrcInfo>, String>>>,
    /// Result of the intake file dialog (None = cancelled).
    pub picker: Option<Receiver<Option<Vec<PathBuf>>>>,
    /// Result of the sidebar folder picker (None = cancelled).
    pub dest_picker: Option<Receiver<Option<PathBuf>>>,
    pub sources: Option<Vec<SrcInfo>>,
    pub locations: Vec<Location>,
    pub screen: Screen,
    pub pick: PickState,
    pub conf: ConfState,
    pub xfer: XferState,
    pub summ: SummState,
}

pub struct PickState {
    pub loc: usize,
    pub cur: PathBuf,
    pub list_job: Option<Receiver<std::io::Result<Listing>>>,
    pub listed: Option<(PathBuf, Listing, Instant)>,
    pub banner: Option<String>,
    pub confirm_create: bool,
    pub pending_use: bool,
    pub pending_try_use: bool,
    pub new_folder: Option<String>,
    pub mkdir_job: Option<Receiver<Result<PathBuf, String>>>,
    pub scanning: Option<Receiver<ScanMsg>>,
    pub scan_prog: usize,
    pub dest_fs: Option<FsKind>,
    pub dest_free: Option<u64>,
}

pub struct ConfState {
    pub plan: Option<Plan>,
    pub apply_all: Option<Policy>,
    pub rename_edits: std::collections::HashMap<usize, String>,
    /// "Übertragen" clicked — acted on after this frame's draw is complete
    /// (taking the plan mid-frame would unwrap None below).
    pub begin: bool,
    /// "Zurück" clicked — same deferral.
    pub back: bool,
}

pub struct XferState {
    pub rx: Option<Receiver<Msg>>,
    pub cancel: Arc<AtomicBool>,
    pub plan: Plan,
    pub phase: String,
    pub step: usize,
    pub steps: usize,
    pub prog: Option<Prog>,
    pub engine: Option<(String, usize, usize, u64, u64)>,
    pub stats: Option<String>,
    pub item_outs: Vec<(usize, Result<(), String>, String)>,
    pub started: Instant,
    pub outcome: Option<Outcome>,
    /// Copy phase finished; verification (if enabled) may still run.
    pub copy_done: bool,
    /// Verification running on this screen (second bar).
    pub verify: Option<Receiver<verify::VMsg>>,
    pub verify_prog: VProg,
    pub verify_report: Option<Result<VerifyReport, String>>,
    /// Moment everything succeeded — drives the completion symbol and the
    /// short hold before the summary appears.
    pub complete_at: Option<Instant>,
    pub elapsed: Option<Duration>,
}

pub struct SummState {
    pub outcome: Outcome,
    pub stats: Option<String>,
    pub elapsed: Duration,
    pub plan: Plan,
    pub item_outs: Vec<(usize, Result<(), String>, String)>,
    pub verify: Option<Receiver<verify::VMsg>>,
    pub verify_prog: VProg,
    pub report: Option<Result<VerifyReport, String>>,
    /// When the summary appeared — grace period before auto-close.
    pub opened: Instant,
    /// Auto-close cancelled because the user opened the settings dialog here.
    pub autoclose_off: bool,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        raw: Vec<PathBuf>,
        startup: Option<Receiver<Result<Vec<SrcInfo>, String>>>,
        lock: Option<crate::log::InstanceLock>,
    ) -> App {
        let lock_ok = lock.is_some();
        let dark = system_prefers_dark();
        let pal = if dark { &DARK } else { &LIGHT };
        apply_theme(&cc.egui_ctx, pal, dark);
        let screen = if !lock_ok {
            Screen::AlreadyRunning
        } else if raw.is_empty() {
            Screen::Intake
        } else {
            Screen::Startup
        };
        App {
            pal,
            settings: Settings::load(),
            show_settings: false,
            _lock: lock,
            startup,
            picker: None,
            dest_picker: None,
            sources: None,
            locations: dest::build_locations(),
            screen,
            pick: PickState {
                loc: 0,
                cur: PathBuf::new(),
                list_job: None,
                listed: None,
                banner: None,
                confirm_create: false,
                pending_use: false,
                pending_try_use: false,
                new_folder: None,
                mkdir_job: None,
                scanning: None,
                scan_prog: 0,
                dest_fs: None,
                dest_free: None,
            },
            conf: ConfState {
                plan: None,
                apply_all: None,
                rename_edits: Default::default(),
                begin: false,
                back: false,
            },
            xfer: XferState {
                rx: None,
                cancel: Arc::new(AtomicBool::new(false)),
                plan: empty_plan(),
                phase: String::new(),
                step: 0,
                steps: 0,
                prog: None,
                engine: None,
                stats: None,
                item_outs: Vec::new(),
                started: Instant::now(),
                outcome: None,
                copy_done: false,
                verify: None,
                verify_prog: Default::default(),
                verify_report: None,
                complete_at: None,
                elapsed: None,
            },
            summ: SummState {
                outcome: Outcome::Ok,
                stats: None,
                elapsed: Duration::ZERO,
                plan: empty_plan(),
                item_outs: Vec::new(),
                verify: None,
                verify_prog: Default::default(),
                report: None,
                opened: Instant::now(),
                autoclose_off: false,
            },
        }
    }

    fn sources_ref(&self) -> &[SrcInfo] {
        self.sources.as_deref().unwrap_or(&[])
    }

    // ---------- navigation ----------

    fn set_loc(&mut self, idx: usize) {
        if let Some(loc) = self.locations.get(idx) {
            self.pick.loc = idx;
            self.pick.cur = loc.root.clone();
            self.pick.list_job = Some(dest::spawn_list(loc.root.clone()));
            self.pick.listed = None;
        }
    }

    fn descend(&mut self, name: &str) {
        self.pick.cur = self.pick.cur.join(name);
        self.pick.list_job = Some(dest::spawn_list(self.pick.cur.clone()));
        self.pick.listed = None;
        self.pick.pending_use = false;
    }

    fn go_up(&mut self) {
        let root = self.locations[self.pick.loc].root.clone();
        if self.pick.cur != root {
            if let Some(p) = self.pick.cur.parent() {
                self.pick.cur = p.to_path_buf();
                self.pick.list_job = Some(dest::spawn_list(self.pick.cur.clone()));
                self.pick.listed = None;
                self.pick.pending_use = false;
            }
        }
    }

    fn fresh_listing(&self) -> Option<Listing> {
        match &self.pick.listed {
            Some((p, l, _)) if *p == self.pick.cur && self.pick.list_job.is_none() => {
                Some(l.clone())
            }
            _ => None,
        }
    }

    /// Sidebar entry for a folder the user picked by hand.
    fn add_other_location(&mut self, path: PathBuf) {
        if let Some(i) = self.locations.iter().position(|l| l.root == path) {
            self.set_loc(i);
            return;
        }
        self.locations.push(Location {
            label: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            root: path.clone(),
            detail: "eigenen Pfad".into(),
            kind: LocationKind::Other,
        });
        self.set_loc(self.locations.len() - 1);
    }

    fn try_use(&mut self) {
        let Some(listing) = self.fresh_listing() else {
            if self.pick.list_job.is_none() {
                self.pick.list_job = Some(dest::spawn_list(self.pick.cur.clone()));
                self.pick.pending_use = true;
            }
            return;
        };
        let probe = listing.probe;
        if !probe.exists {
            self.pick.confirm_create = true;
            return;
        }
        if probe.fs.is_none() {
            self.pick.banner = Some("Ziel ist nicht erreichbar (Timeout).".into());
            return;
        }
        let dest = self.pick.cur.clone();
        for s in self.sources_ref() {
            if args::overlaps(&s.path, &dest) {
                self.pick.banner = Some(
                    "Quelle liegt im Zielbereich (oder umgekehrt) — das würde eine Endlosschleife erzeugen."
                        .into(),
                );
                return;
            }
        }
        self.pick.dest_fs = probe.fs;
        self.pick.dest_free = probe.free;
        self.settings.dest = Some(dest.clone());
        self.settings.save();
        log::append(&format!(
            "dest='{}' fs={:?} free={}",
            dest.display(),
            probe.fs,
            probe
                .free
                .map(fsutil::fmt_size)
                .unwrap_or_else(|| "?".into())
        ));
        self.start_scan(dest, probe.fs.unwrap_or(FsKind::Posix));
    }

    fn start_scan(&mut self, dest: PathBuf, fs: FsKind) {
        let sources = self.sources_ref().to_vec();
        let (tx, rx) = channel::<ScanMsg>();
        self.pick.scanning = Some(rx);
        self.pick.scan_prog = 0;
        std::thread::spawn(move || {
            let tx2 = tx.clone();
            let mut progress = move |n: usize| {
                if n % 50 == 0 {
                    let _ = tx2.send(ScanMsg::Prog(n));
                }
            };
            let res = conflicts::scan(dest, fs, &sources, FAT_MAX_FILE, &mut progress);
            let _ = tx.send(ScanMsg::Done(res));
        });
    }

    fn begin_transfer(&mut self) {
        let Some(plan) = self.conf.plan.take() else { return };
        log::append(&format!(
            "plan items={} conflicts={} engine={} bytes={} runA={} runB={} verify={}",
            plan.items.len(),
            plan.conflicts.len(),
            plan.engine.len(),
            fsutil::fmt_size(plan.total_bytes),
            plan.rsync_a_srcs.len(),
            plan.rsync_b_srcs.len(),
            self.settings.verify as u8
        ));
        let cancel = Arc::new(AtomicBool::new(false));
        self.xfer = XferState {
            rx: Some(transfer::spawn(plan.clone(), cancel.clone())),
            cancel,
            plan,
            phase: "Vorbereitung …".into(),
            step: 0,
            steps: 0,
            prog: None,
            engine: None,
            stats: None,
            item_outs: Vec::new(),
            started: Instant::now(),
            outcome: None,
            copy_done: false,
            verify: None,
            verify_prog: Default::default(),
            verify_report: None,
            complete_at: None,
            elapsed: None,
        };
        self.screen = Screen::Transfer;
    }

    // ---------- intake ----------

    /// Files chosen in the intake screen (drop or dialog) → validate off-thread.
    fn spawn_collect(&mut self, raw: Vec<PathBuf>) {
        log::append(&format!("intake files={}", raw.len()));
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(args::collect(&raw));
        });
        self.startup = Some(rx);
        self.screen = Screen::Startup;
    }

    // ---------- polling ----------

    fn poll_xfer(&mut self, ctx: &Context) {
        let Some(rx) = self.xfer.rx.take() else { return };
        let mut finished: Option<Outcome> = None;
        loop {
            match rx.try_recv() {
                Ok(Msg::Phase(label, step, steps)) => {
                    self.xfer.phase = label;
                    self.xfer.step = step;
                    self.xfer.steps = steps;
                }
                Ok(Msg::RsyncProg(p)) => {
                    self.xfer.engine = None;
                    self.xfer.prog = Some(p);
                }
                Ok(Msg::EngineProg { name, part, parts, done, total }) => {
                    self.xfer.prog = None;
                    self.xfer.engine = Some((name, part, parts, done, total));
                }
                Ok(Msg::RunStats(s)) => self.xfer.stats = Some(s),
                Ok(Msg::ItemDone { idx, res, sha }) => self.xfer.item_outs.push((idx, res, sha)),
                Ok(Msg::Finished(o)) => {
                    finished = Some(o);
                    break;
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        match finished {
            None => {
                self.xfer.rx = Some(rx);
                ctx.request_repaint_after(Duration::from_millis(120));
            }
            Some(Outcome::Ok) => {
                log::append(&format!(
                    "transfer_done outcome=ok elapsed={:.1}s",
                    self.xfer.started.elapsed().as_secs_f32()
                ));
                self.xfer.copy_done = true;
                self.xfer.elapsed = Some(self.xfer.started.elapsed());
                if self.settings.verify {
                    // Stay on this screen; the verify bar shows progress below.
                    self.xfer.verify = Some(verify::spawn(
                        self.xfer.plan.clone(),
                        self.xfer.cancel.clone(),
                    ));
                } else {
                    self.xfer.complete_at = Some(Instant::now());
                }
                // The worker channel is gone — keep frames coming for the
                // hold/verify phases (no user input needed).
                ctx.request_repaint_after(Duration::from_millis(200));
            }
            Some(o) => {
                let txt = match &o {
                    Outcome::Cancelled => "cancelled",
                    Outcome::Failed(_) => "failed",
                    Outcome::Ok => "ok",
                };
                log::append(&format!(
                    "transfer_done outcome={txt} elapsed={:.1}s",
                    self.xfer.started.elapsed().as_secs_f32()
                ));
                self.goto_summary(o);
            }
        }
    }

    fn poll_xfer_verify(&mut self, ctx: &Context) {
        let Some(rx) = self.xfer.verify.take() else { return };
        let mut done = false;
        loop {
            match rx.try_recv() {
                Ok(verify::VMsg::Prog(p)) => self.xfer.verify_prog = p,
                Ok(verify::VMsg::Done(res)) => {
                    self.xfer.verify_report = Some(res);
                    done = true;
                    break;
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        if !done {
            self.xfer.verify = Some(rx);
            ctx.request_repaint_after(Duration::from_millis(200));
            return;
        }
        if let Some(Ok(rep)) = &self.xfer.verify_report {
            log::append(&format!(
                "verify result ok={} fails={} total={}",
                rep.ok,
                rep.fails.len(),
                rep.total
            ));
        }
        let all_ok = matches!(&self.xfer.verify_report, Some(Ok(rep)) if rep.fails.is_empty());
        if all_ok {
            // Hold the completed state for a moment so it is actually seen.
            self.xfer.complete_at = Some(Instant::now());
            // No channel is active anymore — keep frames coming so the hold
            // counts down and the summary appears without user input.
            ctx.request_repaint_after(Duration::from_millis(200));
        } else {
            self.goto_summary(Outcome::Ok);
        }
    }

    /// Retry: failed engine/verify items get re-copied with Overwrite policy.
    fn retry_failed(&mut self) {
        let mut plan = self.summ.plan.clone();
        let mut any = false;
        for (idx, res, _) in &self.summ.item_outs {
            if res.is_err() {
                if let Some(it) = plan.items.get_mut(*idx) {
                    it.policy = Policy::Overwrite;
                    any = true;
                }
            }
        }
        plan.engine.retain(|&i| plan.items[i].policy == Policy::Overwrite);
        if let Some(Ok(rep)) = &self.summ.report {
            let names: Vec<String> = rep.fails.iter().map(|(n, _)| n.clone()).collect();
            for i in 0..plan.items.len() {
                if plan.items[i].engine != EngineKind::Rsync || plan.items[i].policy == Policy::Skip {
                    continue;
                }
                let n = plan.items[i].dst_name.clone();
                if names
                    .iter()
                    .any(|p| p == &n || p.starts_with(&format!("{n}/")) || p.ends_with(&format!("/{n}")))
                {
                    plan.items[i].policy = Policy::Overwrite;
                    plan.rsync_b_srcs.push(plan.items[i].src.clone());
                    any = true;
                }
            }
        }
        plan.rsync_a_srcs.clear();
        plan.conflicts.clear();
        if !any {
            return;
        }
        plan.total_bytes = plan.items.iter().filter(|i| !i.is_dir).map(|i| i.size).sum();
        log::append("retry_failed: re-transfer of failed items");
        let cancel = Arc::new(AtomicBool::new(false));
        self.xfer = XferState {
            rx: Some(transfer::spawn(plan.clone(), cancel.clone())),
            cancel,
            plan,
            phase: "Erneuter Versuch …".into(),
            step: 0,
            steps: 0,
            prog: None,
            engine: None,
            stats: None,
            item_outs: Vec::new(),
            started: Instant::now(),
            outcome: None,
            copy_done: false,
            verify: None,
            verify_prog: Default::default(),
            verify_report: None,
            complete_at: None,
            elapsed: None,
        };
        self.screen = Screen::Transfer;
    }

    fn goto_summary(&mut self, outcome: Outcome) {
        let elapsed = self
            .xfer
            .elapsed
            .unwrap_or_else(|| self.xfer.started.elapsed());
        self.summ = SummState {
            outcome,
            stats: self.xfer.stats.take(),
            elapsed,
            plan: std::mem::replace(&mut self.xfer.plan, empty_plan()),
            item_outs: std::mem::take(&mut self.xfer.item_outs),
            verify: None,
            verify_prog: Default::default(),
            report: self.xfer.verify_report.take(),
            opened: Instant::now(),
            autoclose_off: false,
        };
        self.screen = Screen::Summary;
    }

    fn poll_verify(&mut self, ctx: &Context) {
        // Re-run from the summary screen ("Erneut prüfen").
        let rx = match self.summ.verify.take() {
            Some(r) => r,
            None => return,
        };
        let mut finished = false;
        loop {
            match rx.try_recv() {
                Ok(verify::VMsg::Prog(p)) => self.summ.verify_prog = p,
                Ok(verify::VMsg::Done(res)) => {
                    self.summ.report = Some(res);
                    finished = true;
                    break;
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        if finished {
            if let Some(Ok(rep)) = &self.summ.report {
                log::append(&format!(
                    "verify result ok={} fails={} total={}",
                    rep.ok,
                    rep.fails.len(),
                    rep.total
                ));
            }
        } else {
            self.summ.verify = Some(rx);
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    fn spawn_verify(&mut self) {
        let plan = self.summ.plan.clone();
        self.summ.report = None;
        self.summ.verify = Some(verify::spawn(plan, Arc::new(AtomicBool::new(false))));
    }

    fn close(&self, ctx: &Context) {
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    /// Modal "Einstellungen": verify + auto-close. Reachable from the intake,
    /// destination picker and summary screens; "Übernehmen" saves immediately.
    fn draw_settings_dialog(&mut self, ctx: &Context) {
        let pal = self.pal;
        let mut verify = self.settings.verify;
        let mut autoclose = self.settings.autoclose;
        let mut commit = false;
        let mut close = false;
        egui::Window::new("Einstellungen")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.add_space(2.0);
                ui.checkbox(
                    &mut verify,
                    "Prüfsummen nach dem Kopieren vergleichen",
                );
                ui.weak(
                    "Vergleicht nach dem Kopieren jede Datei — dauert länger, stellt aber sicher, dass die Kopie identisch ist.",
                );
                ui.add_space(8.0);
                ui.checkbox(&mut autoclose, "Fenster automatisch schließen");
                ui.weak(
                    "Schließt das Fenster, wenn alles erfolgreich übertragen und (falls aktiviert) geprüft ist.",
                );
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if primary_button(ui, pal, "Übernehmen").clicked() {
                        commit = true;
                        close = true;
                    }
                    if ui.button("Abbrechen").clicked() {
                        close = true;
                    }
                });
            });
        if close {
            if commit {
                let changed =
                    self.settings.verify != verify || self.settings.autoclose != autoclose;
                self.settings.verify = verify;
                self.settings.autoclose = autoclose;
                if changed {
                    self.settings.save();
                    log::append(&format!(
                        "settings saved verify={} autoclose={}",
                        verify as u8, autoclose as u8
                    ));
                }
            }
            self.show_settings = false;
        }
    }
}

fn empty_plan() -> Plan {
    Plan {
        dest: PathBuf::new(),
        fs: FsKind::Posix,
        fat_limit: FAT_MAX_FILE,
        items: Vec::new(),
        conflicts: Vec::new(),
        rsync_a_srcs: Vec::new(),
        rsync_b_srcs: Vec::new(),
        rsync_rename: Vec::new(),
        excludes: Vec::new(),
        engine: Vec::new(),
        total_bytes: 0,
    }
}

fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------- theme ----------

fn apply_theme(ctx: &Context, pal: &'static Palette, dark: bool) {
    let target = if dark { egui::Theme::Dark } else { egui::Theme::Light };
    // egui keeps a style per theme; customize each, then activate the
    // system's preference.
    ctx.style_mut_of(egui::Theme::Light, |s| customize_style(s, &LIGHT));
    ctx.style_mut_of(egui::Theme::Dark, |s| customize_style(s, &DARK));
    ctx.set_theme(target);
    let _ = pal; // palette is read directly by the draw code
}

fn customize_style(style: &mut egui::Style, pal: &'static Palette) {
    let v = &mut style.visuals;
    v.panel_fill = pal.panel;
    v.window_fill = pal.panel;
    v.extreme_bg_color = pal.extreme;
    v.faint_bg_color = pal.faint;
    v.hyperlink_color = pal.accent;
    v.error_fg_color = pal.err;
    v.warn_fg_color = pal.warn;
    v.selection.bg_fill = pal.accent_soft;
    v.selection.stroke = egui::Stroke::new(1.0, pal.accent);
    v.window_stroke = egui::Stroke::new(1.0, pal.line);
    v.window_shadow = egui::Shadow::NONE;
    v.popup_shadow = egui::Shadow::NONE;
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
    ] {
        w.corner_radius = egui::CornerRadius::same(6);
    }
    v.widgets.noninteractive.bg_fill = pal.panel;
    v.widgets.noninteractive.weak_bg_fill = pal.panel;
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, pal.text);
    v.widgets.inactive.weak_bg_fill = pal.widget;
    v.widgets.inactive.bg_fill = pal.widget;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, pal.line);
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, pal.text);
    v.widgets.hovered.weak_bg_fill = pal.hover;
    v.widgets.hovered.bg_fill = pal.hover;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, pal.accent_soft);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, pal.text);
    v.widgets.active.weak_bg_fill = pal.accent_soft;
    v.widgets.active.bg_fill = pal.accent_soft;
    v.widgets.active.fg_stroke = egui::Stroke::new(1.0, pal.text);

    let sp = &mut style.spacing;
    sp.item_spacing = egui::vec2(10.0, 9.0);
    sp.button_padding = egui::vec2(14.0, 7.0);
    sp.menu_margin = egui::Margin::same(6);
    sp.window_margin = egui::Margin::same(16);
    sp.interact_size.y = 30.0;
    sp.combo_width = 220.0;

    style.text_styles.insert(egui::TextStyle::Heading, FontId::proportional(19.0));
    style.text_styles.insert(egui::TextStyle::Body, FontId::proportional(14.0));
    style.text_styles.insert(egui::TextStyle::Button, FontId::proportional(14.5));
    style.text_styles.insert(egui::TextStyle::Small, FontId::proportional(11.5));
    style.text_styles.insert(egui::TextStyle::Monospace, FontId::monospace(12.5));
}

fn system_prefers_dark() -> bool {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return true,
    };
    match std::fs::read_to_string(std::path::Path::new(&home).join(".config/kdeglobals")) {
        Ok(text) => {
            for line in text.lines() {
                let l = line.trim();
                if let Some(s) = l.strip_prefix("ColorScheme=") {
                    let s = s.to_lowercase();
                    return s.contains("dark") || s.contains("dunkel");
                }
            }
            false
        }
        Err(_) => true,
    }
}

// ---------- small shared widgets ----------

/// Filled accent button — the one primary action of a screen.
fn primary_button(ui: &mut egui::Ui, pal: &Palette, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(label).strong().color(pal.on_accent))
            .fill(pal.accent),
    )
}

/// State of a status symbol next to a progress bar.
#[derive(Clone, Copy, PartialEq)]
enum SymState {
    /// Working: faint base ring + colored arc that fills with progress.
    Run,
    /// Done and good: filled disc with a check.
    Ok,
    /// Done with problems: red disc with a cross.
    Bad,
}

/// Status symbol: ring while working (arc fills with `frac`), filled disc
/// with a check/cross once finished. `label` is drawn centered underneath.
fn draw_symbol(
    ui: &mut egui::Ui,
    pal: &Palette,
    size: f32,
    state: SymState,
    fill: Color32,
    ring: Color32,
    frac: Option<f32>,
    label: &str,
) {
    ui.vertical(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), Sense::hover());
        let pt = ui.painter();
        let c = rect.center();
        let r = size * 0.5 - 3.0;
        match state {
            SymState::Run => {
                pt.circle_stroke(c, r, egui::Stroke::new(3.0, pal.line));
                match frac {
                    Some(f) => {
                        let f = f.clamp(0.0, 1.0);
                        if f > 0.005 {
                            stroke_arc(pt, c, r, -90.0, -90.0 + 360.0 * f, egui::Stroke::new(3.0, ring));
                        }
                    }
                    None => {
                        pt.circle_stroke(c, r, egui::Stroke::new(3.0, ring));
                    }
                }            }
            SymState::Ok => {
                pt.circle_filled(c, r, fill);
                draw_check(pt, c, r * 0.72, pal.on_fill);
            }
            SymState::Bad => {
                pt.circle_filled(c, r, pal.err);
                let s = r * 0.45;
                let stroke = egui::Stroke::new(r * 0.24, pal.on_fill);
                pt.line_segment([c - egui::vec2(s, s), c + egui::vec2(s, s)], stroke);
                pt.line_segment([c - egui::vec2(s, -s), c + egui::vec2(s, -s)], stroke);
            }
        }
        if !label.is_empty() {
            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                let color = if state == SymState::Ok { fill } else { pal.muted };
                ui.label(egui::RichText::new(label).size(12.0).strong().color(color));
            });
        }
    });
}

/// Check mark inside a circle of radius `r` around `c`.
fn draw_check(pt: &egui::Painter, c: egui::Pos2, s: f32, color: Color32) {
    let a = egui::pos2(c.x - s * 0.62, c.y + s * 0.08);
    let b = egui::pos2(c.x - s * 0.14, c.y + s * 0.52);
    let d = egui::pos2(c.x + s * 0.62, c.y - s * 0.42);
    let w = s * 0.32;
    let stroke = egui::Stroke::new(w, color);
    pt.line_segment([a, b], stroke);
    pt.line_segment([b, d], stroke);
    for p in [a, b, d] {
        pt.circle_filled(p, w * 0.5, color);
    }
}

/// Clockwise arc from `a0` to `a1` degrees (0° = right, -90° = top).
fn stroke_arc(pt: &egui::Painter, c: egui::Pos2, r: f32, a0: f32, a1: f32, stroke: egui::Stroke) {
    let steps = ((a1 - a0) / 6.0).ceil().max(1.0) as usize;
    let mut prev = egui::pos2(
        c.x + r * a0.to_radians().cos(),
        c.y + r * a0.to_radians().sin(),
    );
    for i in 1..=steps {
        let a = (a0 + (a1 - a0) * i as f32 / steps as f32).to_radians();
        let p = egui::pos2(c.x + r * a.cos(), c.y + r * a.sin());
        pt.line_segment([prev, p], stroke);
        prev = p;
    }
}

/// Progress bar with a visible frame. The label sits centered inside and is
/// drawn twice — light on the filled part, dark on the rest — so it stays
/// readable at any fill level.
fn draw_bar(ui: &mut egui::Ui, pal: &Palette, size: egui::Vec2, frac: f32, fill: Color32, label: &str) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let pt = ui.painter();
    pt.rect_filled(rect, 6, pal.faint);
    let f = frac.clamp(0.0, 1.0);
    if f > 0.004 {
        let w = rect.width() * f;
        let fill_rect = egui::Rect::from_min_max(rect.left_top(), egui::pos2(rect.left() + w, rect.bottom()));
        let corners = if f >= 0.995 {
            egui::CornerRadius::same(6)
        } else {
            egui::CornerRadius {
                nw: 6,
                sw: 6,
                ne: 0,
                se: 0,
            }
        };
        pt.rect_filled(fill_rect, corners, fill);
    }
    pt.rect_stroke(rect, 6, egui::Stroke::new(1.4, pal.line), egui::StrokeKind::Inside);
    if label.is_empty() {
        return;
    }
    let font = FontId::proportional(12.5);
    // Shrink the label until it fits with padding.
    let mut text = label.to_string();
    let mut galley = pt.layout_no_wrap(text.clone(), font.clone(), pal.text);
    while galley.size().x > rect.width() - 16.0 && text.len() > 4 {
        text.truncate(text.trim_end().chars().count() - 2);
        text.push('…');
        galley = pt.layout_no_wrap(text.clone(), font.clone(), pal.text);
    }
    let tp = egui::pos2(
        rect.center().x - galley.size().x / 2.0,
        rect.center().y - galley.size().y / 2.0,
    );
    let split_x = rect.left() + rect.width() * f;
    pt.with_clip_rect(egui::Rect::from_min_max(rect.left_top(), egui::pos2(split_x, rect.bottom())))
        .galley(tp, galley.clone(), pal.on_fill);
    pt.with_clip_rect(egui::Rect::from_min_max(egui::pos2(split_x, rect.top()), rect.right_bottom()))
        .galley(tp, galley, pal.text);
}

/// One clickable row (sidebar locations, folder list) drawn by hand so the
/// hover/selected states stay calm and the two lines stay aligned.
fn draw_row(
    ui: &mut egui::Ui,
    pal: &Palette,
    height: f32,
    selected: bool,
    clickable: bool,
    paint: impl FnOnce(&egui::Painter, egui::Rect, &Palette),
) -> egui::Response {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, height), Sense::click());
    let fill = if selected {
        pal.accent_soft
    } else if clickable && resp.hovered() {
        pal.hover
    } else {
        Color32::TRANSPARENT
    };
    let pt = ui.painter();
    if fill != Color32::TRANSPARENT {
        pt.rect_filled(rect, 6, fill);
    }
    if selected {
        pt.rect_stroke(rect, 6, egui::Stroke::new(1.5, pal.accent), egui::StrokeKind::Inside);
    }
    paint(&pt.with_clip_rect(rect), rect, pal);
    if clickable && resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

// ---------- eframe impl ----------

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Heartbeat: guarantees that screen transitions, the completion hold
        // and the auto-close grace fire without user input, even when no
        // worker channel is active. This UI redraws in well under a
        // millisecond, so a 300 ms heartbeat costs nothing.
        ctx.request_repaint_after(Duration::from_millis(300));
        // No on_close_event in eframe 0.36: watch close_requested each frame.
        if ctx.input(|i| i.viewport().close_requested()) {
            let copying = self.xfer.rx.is_some() && self.xfer.outcome.is_none();
            let verifying = self.xfer.verify.is_some() && self.xfer.verify_report.is_none();
            if copying || verifying {
                self.xfer.cancel.store(true, Ordering::Relaxed);
                log::append("window closing → cancel requested");
            }
        }
        let screen = self.screen.clone();
        match screen {
            Screen::Startup => self.draw_startup(ui, &ctx),
            Screen::Intake => self.draw_intake(ui, &ctx),
            Screen::AlreadyRunning => self.draw_message(
                ui,
                &ctx,
                "Bereits geöffnet",
                "Es läuft bereits eine Übertragung.\nBitte warten Sie, bis das andere Fenster fertig ist.",
            ),
            Screen::Fatal(msg) => {
                let m = msg.clone();
                self.draw_message(ui, &ctx, "Fehler", &m);
            }
            Screen::Pick => self.draw_pick(ui, &ctx),
            Screen::Conflicts => self.draw_conflicts(ui, &ctx),
            Screen::Transfer => self.draw_transfer(ui, &ctx),
            Screen::Summary => self.draw_summary(ui, &ctx),
        }
        // Overlay on top of every screen that has the entry button.
        if self.show_settings {
            self.draw_settings_dialog(ui.ctx());
        }
    }
}

// ---------- screens ----------

impl App {
    fn draw_intake(&mut self, ui: &mut egui::Ui, ctx: &Context) {
        // File dialog result
        if let Some(rx) = &self.picker {
            match rx.try_recv() {
                Ok(Some(paths)) if !paths.is_empty() => {
                    self.picker = None;
                    self.spawn_collect(paths);
                }
                Ok(_) => self.picker = None, // cancelled
                Err(TryRecvError::Empty) => {
                    ctx.request_repaint_after(Duration::from_millis(120));
                }
                Err(TryRecvError::Disconnected) => self.picker = None,
            }
        }
        // Drag & drop
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            let mut v: Vec<PathBuf> = i
                .raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect();
            v.sort();
            v.dedup();
            v
        });
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if !dropped.is_empty() && self.picker.is_none() {
            self.spawn_collect(dropped);
            egui::CentralPanel::default().show(ui, |_ui| {});
            return;
        }

        let pal = self.pal;
        // The row must live inside a fixed-height `ui.horizontal`: a
        // full-height centered child (with_layout alone) would overflow the
        // panel's max_rect by the trailing add_space every frame, and the
        // persisted panel size would compound — the bar climbs until it
        // covers the window. Same pattern as pick/conf/xfer/summ bars.
        egui::Panel::bottom("intake_bottom").resizable(false).show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Schließen").clicked() {
                        self.close(ctx);
                    }
                    if ui.button("Einstellungen …").clicked() {
                        self.show_settings = true;
                    }
                });
            });
            ui.add_space(8.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.centered_and_justified(|ui| {
                ui.vertical_centered(|ui| {
                    ui.heading("Dateien übertragen");
                    ui.add_space(6.0);
                    ui.weak("Aus Dolphin oder jedem anderen Dateimanager");
                    ui.add_space(28.0);
                let stroke = if hovering {
                    egui::Stroke::new(2.0, pal.accent)
                } else {
                    egui::Stroke::new(1.0, pal.line)
                };
                egui::Frame::new()
                    .fill(if hovering { pal.accent_soft } else { pal.extreme })
                    .stroke(stroke)
                    .corner_radius(egui::CornerRadius::same(10))
                    .inner_margin(egui::Margin::same(24))
                    .show(ui, |ui| {
                        ui.set_min_size(egui::vec2(520.0, 140.0));
                        ui.vertical_centered(|ui| {
                            ui.add_space(26.0);
                            ui.label(egui::RichText::new("Dateien oder Ordner hierher ziehen").size(16.0));
                            ui.add_space(4.0);
                            ui.weak("Es wird nur kopiert — am Ziel wird nichts gelöscht.");
                        });
                    });
                ui.add_space(18.0);
                if self.picker.is_some() {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(18.0));
                        ui.label("Dateiauswahl geöffnet …");
                    });
                } else if primary_button(ui, pal, "Dateien wählen …").clicked() {
                    self.picker = Some(dest::spawn_file_picker());
                }
                });
            });
        });
    }

    fn draw_startup(&mut self, ui: &mut egui::Ui, ctx: &Context) {
        let Some(rx) = &self.startup else {
            self.screen = Screen::Intake;
            return;
        };
        let res = match rx.try_recv() {
            Ok(v) => Some(v),
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(100));
                None
            }
            Err(TryRecvError::Disconnected) => Some(Err("Interner Fehler beim Start.".into())),
        };
        let Some(res) = res else {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(120.0);
                    ui.add(egui::Spinner::new().size(32.0));
                    ui.add_space(12.0);
                    ui.weak("Quellen werden geprüft …");
                });
            });
            return;
        };
        self.startup = None;
        match res {
            Ok(srcs) => {
                log::append(&format!("sources ok: {} items", srcs.len()));
                self.sources = Some(srcs);
                self.set_loc(0);
                self.screen = Screen::Pick;
            }
            Err(e) => {
                log::append(&format!("startup FAILED: {e}"));
                self.screen = Screen::Fatal(e);
            }
        }
    }

    fn draw_message(&mut self, ui: &mut egui::Ui, _ctx: &Context, title: &str, msg: &str) {
        let pal = self.pal;
        let _ = pal;
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(70.0);
                ui.heading(title);
                ui.add_space(10.0);
                ui.label(msg);
                ui.add_space(18.0);
                if ui.button("Schließen").clicked() {
                    self.close(_ctx);
                }
            });
        });
    }

    fn draw_pick(&mut self, ui: &mut egui::Ui, ctx: &Context) {
        self.poll_pick_jobs(ctx);

        // Scanning overlay
        if self.pick.scanning.is_some() {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(120.0);
                    ui.add(egui::Spinner::new().size(32.0));
                    ui.add_space(12.0);
                    ui.label("Ziel wird geprüft (Konflikte, FAT32-Analyse) …");
                    ui.weak("Bei großen Ordnern auf dem NAS kann das einen Moment dauern.");
                });
            });
            return;
        }

        let pal = self.pal;

        // Bottom action bar FIRST so it reserves its space — a full-height
        // scroll area in the central panel would otherwise push it out of view.
        egui::Panel::bottom("pick_bottom").resizable(false).show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let listing = self.fresh_listing();
                let mut meta = String::from("Wird geladen …");
                if let Some(l) = &listing {
                    meta = match l.probe.free {
                        Some(f) => format!("{} frei", fsutil::fmt_size(f)),
                        None => "Ziel nicht erreichbar".into(),
                    };
                }
                ui.weak(meta);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if primary_button(ui, pal, "Hierher übertragen").clicked() {
                        self.try_use();
                    }
                    if ui.button("Neuer Ordner …").clicked() {
                        self.pick.new_folder = Some(String::new());
                    }
                    if ui.button("Abbrechen").clicked() {
                        log::append("picker cancelled by user");
                        self.close(ctx);
                    }
                    if ui.button("Einstellungen …").clicked() {
                        self.show_settings = true;
                    }
                });
            });
            ui.add_space(8.0);
        });

        // Left: locations
        egui::Panel::left("locations").resizable(false).exact_size(224.0).show(ui, |ui| {
            ui.add_space(10.0);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.style_mut().spacing.item_spacing.y = 2.0;
                let idx = self.pick.loc;
                for (i, loc) in self.locations.clone().iter().enumerate() {
                    let selected = i == idx;
                    let label = loc.label.clone();
                    let detail = loc.detail.clone();
                    let resp = draw_row(ui, pal, 46.0, selected, true, |pt, rect, pal| {
                        let x = rect.left() + 12.0;
                        pt.text(
                            egui::pos2(x, rect.top() + 8.0),
                            Align2::LEFT_TOP,
                            &label,
                            FontId::proportional(14.0),
                            if selected { pal.accent } else { pal.text },
                        );
                        pt.text(
                            egui::pos2(x, rect.top() + 26.0),
                            Align2::LEFT_TOP,
                            &detail,
                            FontId::proportional(11.5),
                            pal.muted,
                        );
                    });
                    if resp.clicked() && i != idx {
                        self.set_loc(i);
                    }
                }
                ui.add_space(6.0);
                if ui.small_button("Anderer Ordner …").clicked() && self.dest_picker.is_none() {
                    self.dest_picker = Some(dest::spawn_folder_picker());
                }
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Ziel auswählen");
            // The full path list can be very long — cap the mouse-following
            // tooltip so it can't cover the whole window.
            let paths: Vec<String> = self
                .sources_ref()
                .iter()
                .map(|s| s.path.display().to_string())
                .collect();
            let tip = if paths.len() > 8 {
                format!(
                    "{}\n… und {} weitere",
                    paths[..8].join("\n"),
                    paths.len() - 8
                )
            } else {
                paths.join("\n")
            };
            ui.label(
                egui::RichText::new(sources_summary(self.sources_ref())).weak(),
            )
            .on_hover_text(tip);
            ui.add_space(4.0);

            // Last-used shortcut
            if let Some(last) = self.settings.dest.clone() {
                let rel = last
                    .strip_prefix(&self.locations[self.pick.loc].root)
                    .map(|r| r.display().to_string())
                    .unwrap_or_else(|_| last.display().to_string());
                if ui.small_button(format!("Zuletzt verwendet: {rel}")).clicked() {
                    self.pick.cur = last.clone();
                    self.pick.list_job = Some(dest::spawn_list(last.clone()));
                    self.pick.listed = None;
                    self.pick.pending_use = true;
                }
            }

            // Breadcrumb
            ui.horizontal_wrapped(|ui| {
                let root_label = self.locations[self.pick.loc].label.clone();
                let root = self.locations[self.pick.loc].root.clone();
                if ui.small_button(root_label).clicked() && self.pick.cur != root {
                    self.set_loc(self.pick.loc);
                }
                let rel = dest::rel_components(&root, &self.pick.cur);
                let mut path = root.clone();
                for (i, c) in rel.iter().enumerate() {
                    ui.label(egui::RichText::new("›").weak());
                    path = path.join(c);
                    let is_last = i + 1 == rel.len();
                    if is_last {
                        ui.strong(c.clone());
                    } else if ui.small_button(c.clone()).clicked() {
                        self.pick.cur = path.clone();
                        self.pick.list_job = Some(dest::spawn_list(path.clone()));
                        self.pick.listed = None;
                    }
                }
                if ui.small_button("Nach oben").clicked() {
                    self.go_up();
                }
            });

            if let Some(b) = &self.pick.banner {
                ui.colored_label(pal.err, b.clone());
            }

            // Folder list
            let busy = self.pick.list_job.is_some();
            let stalled = self
                .pick
                .listed
                .as_ref()
                .map(|(_, _, t)| t.elapsed() > Duration::from_secs(3))
                .unwrap_or(false);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.style_mut().spacing.item_spacing.y = 2.0;
                if busy {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(18.0));
                        if stalled {
                            ui.colored_label(
                                pal.warn,
                                "Zielort reagiert langsam — NAS möglicherweise nicht erreichbar …",
                            );
                        } else {
                            ui.label("Wird geladen …");
                        }
                    });
                } else if let Some((_, listing, _)) = &self.pick.listed {
                    if listing.entries.is_empty() {
                        ui.weak("Dieser Ordner ist leer.");
                    }
                    for e in &listing.entries {
                        if e.is_dir {
                            let name = e.name.clone();
                            let resp = draw_row(ui, pal, 30.0, false, true, |pt, rect, pal| {
                                pt.text(
                                    egui::pos2(rect.left() + 12.0, rect.center().y),
                                    Align2::LEFT_CENTER,
                                    &name,
                                    FontId::proportional(14.0),
                                    pal.text,
                                );
                            });
                            if resp.clicked() {
                                self.descend(&name);
                                break;
                            }
                        } else {
                            let name = e.name.clone();
                            let size = fsutil::fmt_size(e.size);
                            draw_row(ui, pal, 24.0, false, false, |pt, rect, pal| {
                                let y = rect.center().y;
                                pt.text(
                                    egui::pos2(rect.left() + 12.0, y),
                                    Align2::LEFT_CENTER,
                                    &name,
                                    FontId::proportional(12.5),
                                    pal.muted,
                                );
                                pt.text(
                                    egui::pos2(rect.right() - 12.0, y),
                                    Align2::RIGHT_CENTER,
                                    &size,
                                    FontId::proportional(11.5),
                                    pal.muted,
                                );
                            });
                        }
                    }
                } else {
                    ui.weak("—");
                }
            });
        });

        // Modal: new folder
        if self.pick.new_folder.is_some() {
            let mut name = self.pick.new_folder.clone().unwrap_or_default();
            let mut close = false;
            let mut create = false;
            egui::Window::new("Neuer Ordner")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "Name für einen neuen Ordner in:\n{}",
                        self.pick.cur.display()
                    ));
                    ui.add_space(6.0);
                    let resp = ui.text_edit_singleline(&mut name);
                    resp.request_focus();
                    create = primary_button(ui, pal, "Erstellen").clicked()
                        && !name.trim().is_empty()
                        || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !name.trim().is_empty());
                    if ui.button("Abbrechen").clicked() {
                        close = true;
                    }
                });
            if create && !name.trim().is_empty() {
                self.pick.mkdir_job = Some(dest::spawn_mkdir(&self.pick.cur, name.trim()));
                self.pick.new_folder = None;
            } else if close {
                self.pick.new_folder = None;
            } else if create {
                self.pick.new_folder = Some(name);
            }
        }

        // Modal: confirm creation of missing dest
        if self.pick.confirm_create {
            let mut create = false;
            let mut cancel = false;
            egui::Window::new("Ordner fehlt")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "'{}' existiert (noch) nicht.\nSoll er erstellt werden?",
                        self.pick.cur.display()
                    ));
                    ui.horizontal(|ui| {
                        if primary_button(ui, pal, "Erstellen").clicked() {
                            create = true;
                        }
                        if ui.button("Abbrechen").clicked() {
                            cancel = true;
                        }
                    });
                });
            if create {
                self.pick.confirm_create = false;
                let cur = self.pick.cur.clone();
                let parent = cur.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| cur.clone());
                let name = cur
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.pick.mkdir_job = Some(dest::spawn_mkdir(&parent, &name));
            } else if cancel {
                self.pick.confirm_create = false;
            }
        }
    }

    fn poll_pick_jobs(&mut self, ctx: &Context) {
        // folder picker (sidebar "Anderer Ordner …")
        if self.dest_picker.is_some() {
            let job = self.dest_picker.take().unwrap();
            match job.try_recv() {
                Ok(Some(path)) if path.is_dir() => self.add_other_location(path),
                Ok(_) => {}
                Err(TryRecvError::Empty) => {
                    self.dest_picker = Some(job);
                    ctx.request_repaint_after(Duration::from_millis(120));
                }
                Err(TryRecvError::Disconnected) => {}
            }
        }
        // listing
        if self.pick.list_job.is_some() {
            let job = self.pick.list_job.take().unwrap();
            match job.try_recv() {
                Ok(Ok(listing)) => {
                    self.pick.listed = Some((self.pick.cur.clone(), listing, Instant::now()));
                    if self.pick.pending_use {
                        self.pick.pending_use = false;
                        self.pick.pending_try_use = true;
                    }
                }
                Ok(Err(e)) => {
                    self.pick.banner = Some(format!("Ordner nicht lesbar: {}", fsutil::ioerr(&e)));
                }
                Err(TryRecvError::Empty) => {
                    self.pick.list_job = Some(job);
                    ctx.request_repaint_after(Duration::from_millis(120));
                }
                Err(TryRecvError::Disconnected) => {}
            }
        }
        if self.pick.pending_try_use && self.pick.list_job.is_none() {
            self.pick.pending_try_use = false;
            self.try_use();
        }
        // mkdir
        if self.pick.mkdir_job.is_some() {
            let job = self.pick.mkdir_job.take().unwrap();
            match job.try_recv() {
                Ok(Ok(newdir)) => {
                    log::append(&format!("created {}", newdir.display()));
                    self.pick.cur = newdir.clone();
                    self.pick.list_job = Some(dest::spawn_list(newdir));
                    self.pick.listed = None;
                }
                Ok(Err(e)) => self.pick.banner = Some(e),
                Err(TryRecvError::Empty) => {
                    self.pick.mkdir_job = Some(job);
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
                Err(TryRecvError::Disconnected) => {}
            }
        }
        // scan
        if self.pick.scanning.is_some() {
            let job = self.pick.scanning.take().unwrap();
            let mut done = false;
            loop {
                match job.try_recv() {
                    Ok(ScanMsg::Prog(n)) => self.pick.scan_prog = n,
                    Ok(ScanMsg::Done(Ok(plan))) => {
                        done = true;
                        if plan.conflicts.is_empty() {
                            log::append("no conflicts — starting transfer");
                            self.conf.plan = Some(plan);
                            self.begin_transfer();
                        } else {
                            log::append(&format!("conflicts found: {}", plan.conflicts.len()));
                            self.conf.plan = Some(plan);
                            self.conf.rename_edits.clear();
                            self.screen = Screen::Conflicts;
                        }
                        break;
                    }
                    Ok(ScanMsg::Done(Err(e))) => {
                        done = true;
                        log::append(&format!("scan FAILED: {e}"));
                        self.pick.banner = Some(e);
                        self.screen = Screen::Pick;
                        break;
                    }
                    Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                }
            }
            if done {
                self.pick.scanning = None;
            } else {
                self.pick.scanning = Some(job);
                ctx.request_repaint_after(Duration::from_millis(150));
            }
        }
    }

    fn draw_conflicts(&mut self, ui: &mut egui::Ui, _ctx: &Context) {
        let Some(plan) = self.conf.plan.clone() else {
            self.screen = Screen::Pick;
            return;
        };
        let pal = self.pal;
        let n_conf = plan.conflicts.len();

        // Bottom action bar FIRST so it reserves its space.
        egui::Panel::bottom("conf_bottom").resizable(false).show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Zurück").clicked() {
                    // Defer: taking the plan or leaving this screen mid-frame
                    // would crash the central-panel pass below.
                    self.conf.back = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if primary_button(ui, pal, "Übertragen").clicked() {
                        self.conf.begin = true;
                    }
                });
            });
            ui.add_space(8.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Konflikte am Ziel");
            ui.weak(format!("Ziel: {}", plan.dest.display()));
            ui.weak(format!(
                "{} Elemente, {} — {} Konflikte",
                plan.items.len(),
                fsutil::fmt_size(plan.total_bytes),
                n_conf
            ));
            if plan.is_fat() {
                ui.colored_label(
                    pal.accent,
                    "FAT32 erkannt: Dateien über 4 GiB werden automatisch in Teile zerlegt (name.001, name.002 …).",
                );
            }
            if let Some(note) = plan_total_free_note(plan.total_bytes, self.pick.dest_free) {
                ui.colored_label(pal.err, note);
            }
            ui.add_space(6.0);

            // Apply-to-all
            ui.horizontal(|ui| {
                ui.label("Für alle Konflikte:");
                egui::ComboBox::from_id_salt("apply-all")
                    .selected_text(
                        self.conf
                            .apply_all
                            .map(|p| p.label())
                            .unwrap_or("einzeln wählen"),
                    )
                    .width(180.0)
                    .show_ui(ui, |ui| {
                        for p in [Policy::Skip, Policy::Overwrite, Policy::Rename] {
                            ui.selectable_value(&mut self.conf.apply_all, Some(p), p.label());
                        }
                    });
                if let Some(p) = self.conf.apply_all {
                    if ui.small_button("Anwenden").clicked() {
                        let plan = self.conf.plan.as_mut().unwrap();
                        let idxs: Vec<(usize, String)> = plan
                            .conflicts
                            .iter()
                            .map(|c| {
                                (
                                    c.item_idx,
                                    if p == Policy::Rename {
                                        c.suggestion.clone()
                                    } else {
                                        String::new()
                                    },
                                )
                            })
                            .collect();
                        for (idx, name) in idxs {
                            let name = if name.is_empty() { None } else { Some(name) };
                            conflicts::apply_conflict(plan, idx, p, name);
                        }
                        self.conf.apply_all = None;
                    }
                }
            });
            ui.add_space(6.0);

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("conflicts")
                    .striped(true)
                    .num_columns(4)
                    .spacing([14.0, 7.0])
                    .show(ui, |ui| {
                        ui.strong("Datei");
                        ui.strong("Größe");
                        ui.strong("Am Ziel vorhanden");
                        ui.strong("Aktion");
                        ui.end_row();

                        for ci in 0..n_conf {
                            let c = plan.conflicts[ci].clone();
                            let (disp, size, pol_now) = {
                                let p = self.conf.plan.as_ref().unwrap();
                                let item = &p.items[c.item_idx];
                                let disp = if item.rel.is_empty() {
                                    item.dst_name.clone()
                                } else {
                                    item.rel.clone()
                                };
                                (disp, item.size, item.policy)
                            };
                            ui.label(egui::RichText::new(disp).strong());
                            ui.label(fsutil::fmt_size(size));
                            ui.label(match c.existing_size {
                                Some(s) => format!("Datei, {}", fsutil::fmt_size(s)),
                                None => {
                                    if c.existing_is_dir {
                                        "Ordner".to_string()
                                    } else {
                                        "belegt".to_string()
                                    }
                                }
                            });

                            let mut pol = pol_now;
                            egui::ComboBox::from_id_salt(format!("conf-{ci}"))
                                .selected_text(pol.label())
                                .width(160.0)
                                .show_ui(ui, |ui| {
                                    for p in &c.options {
                                        ui.selectable_value(&mut pol, *p, p.label());
                                    }
                                });
                            if pol != pol_now {
                                let suggestion = plan.conflicts[ci].suggestion.clone();
                                let name = if pol == Policy::Rename {
                                    Some(suggestion)
                                } else {
                                    None
                                };
                                conflicts::apply_conflict(
                                    self.conf.plan.as_mut().unwrap(),
                                    c.item_idx,
                                    pol,
                                    name,
                                );
                            }
                            ui.end_row();

                            if pol_now == Policy::Rename {
                                ui.label("");
                                ui.label("");
                                ui.horizontal(|ui| {
                                    ui.label("Neuer Name:");
                                    let entry = self
                                        .conf
                                        .rename_edits
                                        .entry(c.item_idx)
                                        .or_insert_with(|| plan.conflicts[ci].suggestion.clone());
                                    ui.text_edit_singleline(entry);
                                });
                                ui.end_row();
                            }
                        }
                    });
            });
        });

        // Deferred bottom-bar actions — safe now that the frame is done.
        if self.conf.begin {
            self.conf.begin = false;
            if let Some(plan) = self.conf.plan.as_mut() {
                let edits: Vec<(usize, String)> = self
                    .conf
                    .rename_edits
                    .iter()
                    .map(|(k, v)| (*k, v.trim().to_string()))
                    .filter(|(_, v)| !v.is_empty())
                    .collect();
                for (idx, name) in edits {
                    if plan.items[idx].policy == Policy::Rename {
                        plan.items[idx].dst_name = name;
                    }
                }
            }
            self.begin_transfer();
        }
        if self.conf.back {
            self.conf.back = false;
            self.conf.plan = None;
            self.conf.rename_edits.clear();
            self.screen = Screen::Pick;
        }
    }

    // ---------- transfer ----------

    fn draw_transfer(&mut self, ui: &mut egui::Ui, ctx: &Context) {
        self.poll_xfer(ctx);
        self.poll_xfer_verify(ctx);

        // Hold the completed state briefly so the symbol is actually seen,
        // then hand over to the summary.
        if let Some(t) = self.xfer.complete_at {
            if t.elapsed() >= Duration::from_millis(2000) {
                self.goto_summary(Outcome::Ok);
            } else {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
        }
        if self.screen != Screen::Transfer {
            egui::CentralPanel::default().show(ui, |_ui| {});
            return; // moved to summary
        }

        let pal = self.pal;
        let verifying = self.xfer.verify.is_some();
        let complete = self.xfer.complete_at.is_some();

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading(if complete {
                "Übertragung abgeschlossen"
            } else if verifying {
                "Prüfung läuft"
            } else {
                "Übertragung läuft"
            });
            if !self.xfer.copy_done {
                if self.xfer.steps > 0 {
                    ui.weak(format!(
                        "Schritt {} von {} — {}",
                        self.xfer.step, self.xfer.steps, self.xfer.phase
                    ));
                } else {
                    ui.weak(&self.xfer.phase);
                }
            }
            ui.add_space(10.0);

            // Copy bar (green) + labeled status symbol — the verify bar below
            // aligns with the same column.
            let sym = 46.0;
            let sym_col = 92.0;
            let col = (ui.available_width() - sym_col - 8.0).max(180.0);
            let (copy_frac, copy_text) = copy_progress(&self.xfer);
            let vfrac = if self.xfer.verify_report.is_some() {
                1.0
            } else if self.xfer.verify_prog.total > 0 {
                (self.xfer.verify_prog.done as f32 / self.xfer.verify_prog.total as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            ui.horizontal(|ui| {
                draw_bar(ui, pal, egui::vec2(col, 30.0), copy_frac, pal.ok, &copy_text);
                ui.add_space(6.0);
                let (state, ring, frac, label) = if complete {
                    (SymState::Ok, pal.ok, None, "Fertig")
                } else if verifying {
                    (SymState::Run, pal.verify, Some(vfrac), "Prüfung")
                } else {
                    (SymState::Run, pal.ok, Some(copy_frac), "Kopieren")
                };
                draw_symbol(ui, pal, sym, state, pal.ok, ring, frac, label);
            });
            ui.add_space(2.0);

            // Live numbers (no ETA — the bar and symbol carry the state).
            if let Some(p) = &self.xfer.prog {
                let speed = transfer::human_to_bytes(p.speed.trim_end_matches("/s"))
                    .map(|b| fsutil::fmt_speed(b as f64))
                    .unwrap_or_default();
                let files = if p.tochk.1 > 0 {
                    format!(
                        "{} von {} Dateien",
                        p.tochk.1.saturating_sub(p.tochk.0),
                        p.tochk.1
                    )
                } else if p.xfr > 0 {
                    format!("Datei {}", p.xfr)
                } else {
                    String::new()
                };
                ui.horizontal(|ui| {
                    if !speed.is_empty() {
                        ui.label(egui::RichText::new(speed).strong());
                    }
                    if !files.is_empty() {
                        ui.weak(files);
                    }
                });
            }
            if let Some((_, part, parts, _, _)) = &self.xfer.engine {
                if *parts > 1 {
                    ui.weak(format!("Teil {part} von {parts}"));
                }
            }

            // Verification bar (blue) with its own small status symbol,
            // column-aligned below the copy bar.
            if verifying || self.xfer.verify_report.is_some() {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let (lrect, _) = ui.allocate_exact_size(egui::vec2(64.0, 22.0), Sense::hover());
                    ui.painter().text(
                        egui::pos2(lrect.left(), lrect.center().y),
                        Align2::LEFT_CENTER,
                        "Prüfung",
                        FontId::proportional(12.5),
                        pal.muted,
                    );
                    let vlabel = match &self.xfer.verify_report {
                        Some(Ok(rep)) if rep.fails.is_empty() => "Prüfsummen stimmen überein".to_string(),
                        Some(Ok(rep)) => {
                            format!("{} von {} Dateien weichen ab", rep.fails.len(), rep.total)
                        }
                        Some(Err(_)) => "Prüfung abgebrochen".to_string(),
                        None if self.xfer.verify_prog.total > 0 => {
                            let speed = if self.xfer.verify_prog.speed.is_empty() {
                                String::new()
                            } else {
                                format!(" — {}", self.xfer.verify_prog.speed)
                            };
                            format!(
                                "{} von {} geprüft{speed}",
                                fsutil::fmt_size(self.xfer.verify_prog.done),
                                fsutil::fmt_size(self.xfer.verify_prog.total)
                            )
                        }
                        None => "Prüfsummen werden verglichen …".to_string(),
                    };
                    draw_bar(
                        ui,
                        pal,
                        egui::vec2((col - 74.0).max(80.0), 22.0),
                        vfrac,
                        pal.verify,
                        &vlabel,
                    );
                    ui.add_space(6.0);
                    let vstate = match &self.xfer.verify_report {
                        Some(Ok(rep)) if rep.fails.is_empty() => SymState::Ok,
                        Some(_) => SymState::Bad,
                        None => SymState::Run,
                    };
                    draw_symbol(
                        ui,
                        pal,
                        22.0,
                        vstate,
                        pal.verify,
                        pal.verify,
                        if vstate == SymState::Run { Some(vfrac) } else { None },
                        "",
                    );
                });
            }

            ui.add_space(10.0);

            if let Some(stats) = &self.xfer.stats {
                ui.collapsing("rsync-Statistik", |ui| {
                    ui.monospace(stats);
                });
            }
        });

        // Bottom bar: cancel while something is still running.
        if !complete && self.xfer.outcome.is_none() {
            egui::Panel::bottom("xfer_bottom").resizable(false).show(ui, |ui| {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.weak("Abgebrochene Kopiervorgänge bleiben erhalten und können fortgesetzt werden.");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Abbrechen").clicked() {
                            self.xfer.cancel.store(true, Ordering::Relaxed);
                            log::append("cancel requested by user");
                        }
                    });
                });
                ui.add_space(8.0);
            });
        }
    }

    fn draw_summary(&mut self, ui: &mut egui::Ui, ctx: &Context) {
        self.poll_verify(ctx);
        let pal = self.pal;
        let outcome_txt = match &self.summ.outcome {
            Outcome::Ok => "Übertragung abgeschlossen".to_string(),
            Outcome::Cancelled => "Abgebrochen — teilweise übertragen".to_string(),
            Outcome::Failed(e) => format!("Fehler: {e}"),
        };
        let outcome_color = match &self.summ.outcome {
            Outcome::Ok => pal.text,
            Outcome::Cancelled => pal.warn,
            Outcome::Failed(_) => pal.err,
        };
        let nothing_todo = self.summ.stats.is_none()
            && self.summ.item_outs.is_empty()
            && matches!(self.summ.outcome, Outcome::Ok);
        egui::Panel::bottom("summ_bottom").resizable(false).show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.small_button("Einstellungen …").clicked() {
                    // Manual interaction wins: no sudden auto-close anymore.
                    self.summ.autoclose_off = true;
                    self.show_settings = true;
                }
                if self.summ.report.is_some() {
                    if ui.small_button("Erneut prüfen").clicked() {
                        self.spawn_verify();
                    }
                    let has_fails = self
                        .summ
                        .report
                        .as_ref()
                        .map(|r| r.as_ref().map(|x| !x.fails.is_empty()).unwrap_or(false))
                        .unwrap_or(false);
                    if has_fails && ui.button("Fehlgeschlagene erneut übertragen").clicked() {
                        self.retry_failed();
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if primary_button(ui, pal, "Fenster schließen").clicked() {
                        self.settings.save();
                        self.close(ctx);
                    }
                });
            });
            ui.add_space(8.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.colored_label(outcome_color, egui::RichText::new(&outcome_txt).heading());
            ui.weak(format!("Ziel: {}", self.summ.plan.dest.display()));
            ui.weak(format!("Dauer: {:.0} s", self.summ.elapsed.as_secs_f32()));
            if nothing_todo {
                ui.weak("Es gab nichts zu übertragen (alle Elemente übersprungen).");
            }
            if matches!(self.summ.outcome, Outcome::Cancelled) {
                ui.colored_label(
                    pal.warn,
                    "Die Übertragung wurde abgebrochen. Bereits übertragene Daten bleiben erhalten —\n\
                     einfach erneut „Dateien übertragen …“ starten, um fortzusetzen.",
                );
            }
            ui.add_space(6.0);

            if let Some(stats) = &self.summ.stats {
                ui.collapsing("rsync-Statistik", |ui| {
                    egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                        ui.monospace(stats);
                    });
                });
            }

            // engine item failures
            let fails: Vec<String> = self
                .summ
                .item_outs
                .iter()
                .filter_map(|(idx, res, _)| {
                    res.as_ref().err().map(|e| {
                        let name = self
                            .summ
                            .plan
                            .items
                            .get(*idx)
                            .map(|i| i.dst_path().display().to_string())
                            .unwrap_or_default();
                        format!("{name}: {e}")
                    })
                })
                .collect();
            if !fails.is_empty() {
                ui.colored_label(pal.err, "Fehler beim Kopieren:");
                for f in &fails {
                    ui.colored_label(pal.err, format!("  • {f}"));
                }
            }

            // chunk hints
            let chunks: Vec<String> = self
                .summ
                .plan
                .items
                .iter()
                .filter(|i| i.engine == EngineKind::Chunks && i.policy != Policy::Skip)
                .map(|i| {
                    let b = i.dst_path().display().to_string();
                    format!("cat {}.0* > {}", shq(&b), shq(&b))
                })
                .collect();
            if !chunks.is_empty() {
                ui.add_space(6.0);
                ui.strong("FAT32-Zerlegung — Zusammenführen auf dem Ziel:");
                for c in &chunks {
                    ui.horizontal(|ui| {
                        ui.monospace(c);
                        if ui.small_button("Kopieren").clicked() {
                            ctx.copy_text(c.clone());
                        }
                    });
                }
                ui.weak("Vor dem Abspielen/Weiterverwenden wieder zusammenführen (nur auf dem FAT32-Datenträger nötig).");
            }

            // verify section (report arrives prefilled from the transfer
            // screen; a manual re-run shows its progress here)
            if self.summ.verify.is_some() {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(18.0));
                    let p = &self.summ.verify_prog;
                    let amount = if p.total > 0 {
                        format!("{} von {}", fsutil::fmt_size(p.done), fsutil::fmt_size(p.total))
                    } else {
                        "…".to_string()
                    };
                    ui.label(format!("Prüfsummen werden verglichen … {amount}"));
                });
            } else if let Some(rep) = &self.summ.report {
                match rep {
                    Ok(r) => {
                        if r.fails.is_empty() {
                            ui.colored_label(pal.ok, format!("OK: {} Dateien stimmen überein.", r.ok));
                        } else {
                            ui.colored_label(
                                pal.err,
                                format!("{} von {} Dateien weichen ab:", r.fails.len(), r.total),
                            );
                            for (name, why) in &r.fails {
                                ui.colored_label(pal.err, format!("  • {name} — {why}"));
                            }
                        }
                    }
                    Err(e) => {
                        ui.colored_label(pal.err, format!("Prüfung fehlgeschlagen: {e}"));
                    }
                }
            } else if !matches!(self.summ.outcome, Outcome::Ok) {
                // cancelled/failed — no verify options
            } else {
                ui.weak(
                    "Tipp: Prüfsummen-Prüfung und automatisches Schließen \
                     stehen in den Einstellungen …",
                );
            }
        });

        // Auto-close — but only after a short grace period so the summary
        // (and its settings dialog) stays reachable, and never while the
        // settings dialog is open or the user interacted with it.
        let report_ok = self
            .summ
            .report
            .as_ref()
            .map(|r| r.as_ref().map(|x| x.fails.is_empty()).unwrap_or(true))
            .unwrap_or(true);
        let autoclose_pending = self.settings.autoclose
            && !self.summ.autoclose_off
            && !self.show_settings
            && matches!(self.summ.outcome, Outcome::Ok)
            && self.summ.verify.is_none()
            && report_ok;
        if autoclose_pending {
            // Keep frames coming so the grace period counts down without
            // user input.
            ctx.request_repaint_after(Duration::from_millis(250));
            if self.summ.opened.elapsed() >= Duration::from_millis(2000) {
                log::append("autoclose");
                self.settings.save();
                self.close(ctx);
            }
        }
    }
}

// ---------- transfer helpers ----------

/// (fraction, bar text) of the copy phase.
fn copy_progress(x: &XferState) -> (f32, String) {
    if let Some((name, part, parts, done, total)) = &x.engine {
        let frac = if *total > 0 { *done as f32 / *total as f32 } else { 0.0 };
        let teil = if *parts > 1 {
            format!(" — Teil {part} von {parts}")
        } else {
            String::new()
        };
        (
            frac.clamp(0.0, 1.0),
            format!(
                "{name}{teil}: {} von {}",
                fsutil::fmt_size(*done),
                fsutil::fmt_size(*total)
            ),
        )
    } else if let Some(p) = &x.prog {
        if x.copy_done {
            return (1.0, "Kopieren abgeschlossen".into());
        }
        let frac = (p.pct / 100.0).clamp(0.0, 1.0);
        let approx_total = if p.pct > 0.5 {
            (p.done as f64 / (p.pct as f64 / 100.0)) as u64
        } else {
            0
        };
        let size_part = if approx_total > 0 {
            format!("{} von ~{}", fsutil::fmt_size(p.done), fsutil::fmt_size(approx_total))
        } else {
            fsutil::fmt_size(p.done)
        };
        (frac, format!("{size_part} — {} %", p.pct.round() as u64))
    } else if x.copy_done {
        (1.0, "Kopieren abgeschlossen".into())
    } else if x.steps > 0 {
        (
            (x.step as f32 - 0.5) / x.steps.max(1) as f32,
            x.phase.clone(),
        )
    } else {
        (0.0, x.phase.clone())
    }
}

fn plan_total_free_note(need: u64, free: Option<u64>) -> Option<String> {
    let f = free?;
    if f < need {
        Some(format!(
            "Zu wenig Platz am Ziel: benötigt {}, frei {}.",
            fsutil::fmt_size(need),
            fsutil::fmt_size(f)
        ))
    } else {
        None
    }
}

fn sources_summary(sources: &[SrcInfo]) -> String {
    let n_files = sources.iter().filter(|s| !s.is_dir).count();
    let n_dirs = sources.iter().filter(|s| s.is_dir).count();
    let bytes = args::total_size(sources);
    let mut parts = Vec::new();
    match n_files {
        0 => {}
        1 => parts.push("1 Datei".to_string()),
        n => parts.push(format!("{n} Dateien")),
    }
    match n_dirs {
        0 => {}
        1 => parts.push("1 Ordner".to_string()),
        n => parts.push(format!("{n} Ordner")),
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("Von: {}, {}", parts.join(", "), fsutil::fmt_size(bytes))
}
