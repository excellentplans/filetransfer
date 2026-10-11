//! egui UI: alle Bildschirme (Deutsch) — Zielwahl, Konflikte, Fortschritt, Ergebnis.

use crate::args::{self, SrcInfo};
use crate::conflicts::{self, EngineKind, Plan, Policy};
use crate::dest::{self, Listing, Location};
use crate::fsutil::{self, FsKind, FAT_MAX_FILE};
use crate::log;
use crate::state::Settings;
use crate::transfer::{self, Msg, Outcome, Prog};
use crate::verify::{self, VerifyReport};
use egui::{Align2, Color32, Context, ViewportCommand};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ACCENT: Color32 = Color32::from_rgb(0x3D, 0xAE, 0xD9);
const ACCENT_DIM: Color32 = Color32::from_rgba_premultiplied(0x3D, 0xAE, 0xD9, 60);
const GREEN: Color32 = Color32::from_rgb(0x2E, 0xCC, 0x71);
const RED: Color32 = Color32::from_rgb(0xE7, 0x4C, 0x3C);
const WARN: Color32 = Color32::from_rgb(0xF3, 0xB0, 0x2C);

#[derive(Clone, PartialEq)]
pub enum Screen {    Startup,
    NoArgs,
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
    pub settings: Settings,
    pub startup: Option<Receiver<Result<Vec<SrcInfo>, String>>>,
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
}

pub struct SummState {
    pub outcome: Outcome,
    pub stats: Option<String>,
    pub elapsed: Duration,
    pub plan: Plan,
    pub item_outs: Vec<(usize, Result<(), String>, String)>,
    pub verify: Option<Receiver<verify::VMsg>>,
    pub verify_prog: (usize, usize),
    pub report: Option<Result<VerifyReport, String>>,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        raw: Vec<PathBuf>,
        startup: Option<Receiver<Result<Vec<SrcInfo>, String>>>,
        lock_ok: bool,
    ) -> App {
        apply_breeze_look(&cc.egui_ctx);
        let screen = if !lock_ok {
            Screen::AlreadyRunning
        } else if raw.is_empty() {
            Screen::NoArgs
        } else {
            Screen::Startup
        };
        App {
            settings: Settings::load(),
            startup,
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
            },
            summ: SummState {
                outcome: Outcome::Ok,
                stats: None,
                elapsed: Duration::ZERO,
                plan: empty_plan(),
                item_outs: Vec::new(),
                verify: None,
                verify_prog: (0, 0),
                report: None,
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
        };
        self.screen = Screen::Transfer;
    }

    fn poll_xfer(&mut self, ctx: &Context) {
        let rx = match self.xfer.rx.take() {
            Some(r) => r,
            None => return,
        };
        let mut done = false;
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
                    self.xfer.outcome = Some(o);
                    done = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if done {
            let outcome = self.xfer.outcome.clone().unwrap_or(Outcome::Ok);
            let outcome_txt = match &outcome {
                Outcome::Ok => "ok",
                Outcome::Cancelled => "cancelled",
                Outcome::Failed(_) => "failed",
            };
            let elapsed = self.xfer.started.elapsed();
            log::append(&format!(
                "transfer_done outcome={outcome_txt} elapsed={:.1}s",
                elapsed.as_secs_f32()
            ));
            let verify_rx = if matches!(outcome, Outcome::Ok) && self.settings.verify {
                Some(verify::spawn(self.xfer.plan.clone(), self.xfer.cancel.clone()))
            } else {
                None
            };
            self.summ = SummState {
                outcome,
                stats: self.xfer.stats.take(),
                elapsed,
                plan: std::mem::replace(&mut self.xfer.plan, empty_plan()),
                item_outs: std::mem::take(&mut self.xfer.item_outs),
                verify: verify_rx,
                verify_prog: (0, 0),
                report: None,
            };
            self.screen = Screen::Summary;
        } else {
            self.xfer.rx = Some(rx);
            ctx.request_repaint_after(Duration::from_millis(120));
        }
    }

    fn poll_verify(&mut self, ctx: &Context) {
        let rx = match self.summ.verify.take() {
            Some(r) => r,
            None => return,
        };
        let mut finished = false;
        loop {
            match rx.try_recv() {
                Ok(verify::VMsg::Prog(d, t)) => self.summ.verify_prog = (d, t),
                Ok(verify::VMsg::Done(res)) => {
                    self.summ.report = Some(res);
                    finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
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
        };
        self.screen = Screen::Transfer;
    }

    fn close(&self, ctx: &Context) {
        ctx.send_viewport_cmd(ViewportCommand::Close);
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

// ---------- look ----------

fn apply_breeze_look(ctx: &Context) {
    let dark = system_prefers_dark();
    ctx.set_theme(if dark { egui::Theme::Dark } else { egui::Theme::Light });
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        v.hyperlink_color = ACCENT;
        v.selection.bg_fill = ACCENT_DIM;
        v.widgets.active.bg_fill = ACCENT_DIM;
        v.widgets.hovered.bg_fill = ACCENT_DIM;
    });
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

// ---------- eframe impl ----------

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // No on_close_event in eframe 0.36: watch close_requested each frame.
        if ctx.input(|i| i.viewport().close_requested())
            && self.xfer.rx.is_some()
            && self.xfer.outcome.is_none()
        {
            self.xfer.cancel.store(true, Ordering::Relaxed);
            log::append("window closing during transfer → cancel requested");
        }
        let screen = self.screen.clone();
        match screen {
            Screen::Startup => self.draw_startup(ui, &ctx),
            Screen::NoArgs => self.draw_message(
                ui,
                &ctx,
                "Datei übertragen",
                "Bitte markieren Sie zuerst Dateien oder Ordner in Dolphin und starten Sie\n\
                 „Datei übertragen …“ über das Kontextmenü (rechte Maustaste).",
            ),
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
    }
}

// ---------- screens ----------

impl App {
    fn draw_startup(&mut self, ui: &mut egui::Ui, ctx: &Context) {
        egui::CentralPanel::default().show(ui, |_ui| {});
        let mut res = None;
        if let Some(rx) = &self.startup {
            match rx.try_recv() {
                Ok(v) => res = Some(v),
                Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(100)),
                Err(TryRecvError::Disconnected) => {
                    res = Some(Err("Interner Fehler beim Start.".into()))
                }
            }
        }
        if let Some(res) = res {
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
        } else {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);
                        ui.add(egui::Spinner::new().size(32.0));
                        ui.add_space(12.0);
                        ui.label("Quellen werden geprüft …");
                    });
                });
            });
        }
    }

    fn draw_message(&mut self, ui: &mut egui::Ui, ctx: &Context, title: &str, msg: &str) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(24.0);
            ui.heading(title);
            ui.add_space(8.0);
            ui.label(msg);
            ui.add_space(16.0);
            if ui.button("Schließen").clicked() {
                self.close(ctx);
            }
        });
    }

    fn draw_pick(&mut self, ui: &mut egui::Ui, ctx: &Context) {
        self.poll_pick_jobs(ctx);

        // Scanning overlay
        if self.pick.scanning.is_some() {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(40.0);
                        ui.add(egui::Spinner::new().size(32.0));
                        ui.add_space(12.0);
                        ui.label("Ziel wird geprüft (Konflikte, FAT32-Analyse) …");
                        ui.weak("Bei großen Ordnern auf dem NAS kann das einen Moment dauern.");
                    });
                });
            });
            return;
        }

        // Bottom action bar FIRST so it reserves its space — a full-height
        // scroll area in the central panel would otherwise push it out of view.
        egui::Panel::bottom("pick_bottom").resizable(false).show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Neuer Ordner …").clicked() {
                    self.pick.new_folder = Some(String::new());
                }
                if ui.button("Abbrechen").clicked() {
                    log::append("picker cancelled by user");
                    self.close(ctx);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new("Diesen Ordner verwenden").strong(),
                        ))
                        .clicked()
                    {
                        self.try_use();
                    }
                    let here = self.pick.cur.display().to_string();
                    let free = self
                        .fresh_listing()
                        .and_then(|l| l.probe.free)
                        .map(fsutil::fmt_size);
                    ui.weak(format!(
                        "{here}{}",
                        free.map(|f| format!(" · {f} frei")).unwrap_or_default()
                    ));
                });
            });
        });

        // Left: locations
        egui::Panel::left("locations")
            .resizable(false)
            .exact_size(210.0)
            .show(ui, |ui| {
                ui.add_space(8.0);
                ui.strong("Zielorte");
                ui.separator();
                let idx = self.pick.loc;
                for (i, loc) in self.locations.clone().iter().enumerate() {
                    let sel = i == idx;
                    if ui
                        .add_sized(
                            [ui.available_width(), 30.0],
                            egui::Button::new(egui::RichText::new(loc.label.clone()).strong())
                                .selected(sel)
                                .fill(if sel {
                                    ACCENT_DIM
                                } else {
                                    ui.style().visuals.extreme_bg_color
                                }),
                        )
                        .clicked()
                    {
                        if i != idx {
                            self.set_loc(i);
                        }
                    }
                    ui.label(egui::RichText::new(format!("      {}", loc.detail)).small().weak());
                }
            });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong("Quellen:");
                ui.label(sources_summary(self.sources_ref()));
            });
            ui.separator();

            // Last-used shortcut
            if let Some(last) = self.settings.dest.clone() {
                let rel = last
                    .strip_prefix(&self.locations[self.pick.loc].root)
                    .map(|r| r.display().to_string())
                    .unwrap_or_else(|_| last.display().to_string());
                if ui.button(format!("Zuletzt verwendet: {rel}")).clicked() {
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
                    ui.label("›");
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
                if ui.small_button("Aufwärts").clicked() {
                    self.go_up();
                }
            });
            ui.separator();

            if let Some(b) = &self.pick.banner {
                ui.colored_label(RED, b.clone());
                ui.add_space(4.0);
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
                if busy {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(20.0));
                        if stalled {
                            ui.colored_label(
                                WARN,
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
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new(e.name.clone()).strong(),
                                    )
                                    .fill(egui::Color32::TRANSPARENT),
                                )
                                .clicked()
                            {
                                let name = e.name.clone();
                                self.descend(&name);
                                break;
                            }
                        } else {
                            ui.add_enabled(
                                false,
                                egui::Button::new(
                                    egui::RichText::new(format!(
                                        "   {}  ({})",
                                        e.name,
                                        fsutil::fmt_size(e.size)
                                    ))
                                    .weak(),
                                )
                                .fill(egui::Color32::TRANSPARENT),
                            );
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
                    create = ui
                        .add_enabled(!name.trim().is_empty(), egui::Button::new("Erstellen"))
                        .clicked()
                        || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
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
                        if ui.button("Erstellen").clicked() {
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
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
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
        let n_conf = plan.conflicts.len();

        // Bottom action bar FIRST so it reserves its space.
        egui::Panel::bottom("conf_bottom").resizable(false).show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Zurück").clicked() {
                    self.conf.plan = None;
                    self.screen = Screen::Pick;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new("Übertragen starten").strong(),
                        ))
                        .clicked()
                    {
                        let edits: Vec<(usize, String)> = self
                            .conf
                            .rename_edits
                            .iter()
                            .map(|(k, v)| (*k, v.trim().to_string()))
                            .filter(|(_, v)| !v.is_empty())
                            .collect();
                        let plan = self.conf.plan.as_mut().unwrap();
                        for (idx, name) in edits {
                            if plan.items[idx].policy == Policy::Rename {
                                plan.items[idx].dst_name = name;
                            }
                        }
                        self.begin_transfer();
                    }
                });
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Konflikte am Ziel");
            ui.add_space(4.0);
            ui.label(format!(
                "Ziel: {} · {} Elemente, {} · {} Konflikte",
                plan.dest.display(),
                plan.items.len(),
                fsutil::fmt_size(plan.total_bytes),
                n_conf
            ));
            if plan.is_fat() {
                ui.colored_label(
                    ACCENT,
                    "FAT32 erkannt: Dateien über 4 GiB werden automatisch in Teile zerlegt (name.001, name.002 …).",
                );
            }
            if let Some(note) = plan_total_free_note(plan.total_bytes, self.pick.dest_free) {
                ui.colored_label(RED, note);
            }
            ui.separator();

            // Apply-to-all
            ui.horizontal(|ui| {
                ui.label("Für alle:");
                egui::ComboBox::from_id_salt("apply-all")
                    .selected_text(
                        self.conf
                            .apply_all
                            .map(|p| p.label())
                            .unwrap_or("— einzeln wählen —"),
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
                ui.weak("(setzt alle Zeilen auf dieselbe Aktion)");
            });
            ui.separator();

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("conflicts")
                    .striped(true)
                    .num_columns(4)
                    .spacing([14.0, 7.0])
                    .show(ui, |ui| {
                        ui.strong("Datei / Ordner");
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
    }

    fn draw_transfer(&mut self, ui: &mut egui::Ui, ctx: &Context) {
        self.poll_xfer(ctx);
        if self.screen != Screen::Transfer {
            return; // moved to summary
        }
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Übertragung läuft …");
            ui.add_space(8.0);
            if self.xfer.steps > 0 {
                ui.weak(format!(
                    "Schritt {} von {} — {}",
                    self.xfer.step, self.xfer.steps, self.xfer.phase
                ));
            } else {
                ui.weak(&self.xfer.phase);
            }
            ui.add_space(8.0);

            let frac: f32;
            let mut detail = String::new();
            if let Some((name, part, parts, done, total)) = &self.xfer.engine {
                frac = if *total > 0 { *done as f32 / *total as f32 } else { 0.0 };
                let teil = if *parts > 1 {
                    format!(" — Teil {part}/{parts}")
                } else {
                    String::new()
                };
                detail = format!(
                    "{name}{teil} · {} von {}",
                    fsutil::fmt_size(*done),
                    fsutil::fmt_size(*total)
                );
            } else if let Some(p) = &self.xfer.prog {
                frac = (p.pct / 100.0).clamp(0.0, 1.0);
                let approx_total = if p.pct > 0.5 {
                    (p.done as f64 / (p.pct as f64 / 100.0)) as u64
                } else {
                    0
                };
                detail = format!(
                    "{} übertragen{} · {} · Rest {} · Datei #{}",
                    fsutil::fmt_size(p.done),
                    if approx_total > 0 {
                        format!(" von ~{}", fsutil::fmt_size(approx_total))
                    } else {
                        String::new()
                    },
                    fsutil::fmt_speed(transfer::human_to_bytes(
                        p.speed.trim_end_matches("/s")
                    )
                    .unwrap_or(0) as f64),
                    p.eta,
                    p.xfr
                );
            } else {
                frac = if self.xfer.steps > 0 {
                    (self.xfer.step as f32 - 0.5) / self.xfer.steps.max(1) as f32
                } else {
                    0.0
                };
            }
            ui.add(
                egui::ProgressBar::new(frac)
                    .desired_width(ui.available_width())
                    .text(detail),
            );
            ui.add_space(12.0);

            if let Some(stats) = &self.xfer.stats {
                ui.collapsing("rsync-Statistik", |ui| {
                    ui.monospace(stats);
                });
            }

            ui.add_space(8.0);
            if self.xfer.outcome.is_none() {
                if ui.button("Abbrechen").clicked() {
                    self.xfer.cancel.store(true, Ordering::Relaxed);
                    log::append("cancel requested by user");
                }
                ui.weak("Abgebrochene Kopiervorgänge bleiben erhalten und können fortgesetzt werden.");
            }
        });
    }

    fn draw_summary(&mut self, ui: &mut egui::Ui, ctx: &Context) {
        self.poll_verify(ctx);
        let outcome_txt = match &self.summ.outcome {
            Outcome::Ok => "Übertragung abgeschlossen".to_string(),
            Outcome::Cancelled => "Abgebrochen — teilweise übertragen".to_string(),
            Outcome::Failed(e) => format!("Fehler: {e}"),
        };
        let nothing_todo = self.summ.stats.is_none()
            && self.summ.item_outs.is_empty()
            && matches!(self.summ.outcome, Outcome::Ok);
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading(&outcome_txt);
            ui.weak(format!(
                "Dauer: {:.0} s · Ziel: {}",
                self.summ.elapsed.as_secs_f32(),
                self.summ.plan.dest.display()
            ));
            if nothing_todo {
                ui.weak("Es gab nichts zu übertragen (alle Elemente übersprungen).");
            }
            ui.add_space(8.0);

            if matches!(self.summ.outcome, Outcome::Cancelled) {
                ui.colored_label(
                    WARN,
                    "Die Übertragung wurde abgebrochen. Bereits übertragene Daten bleiben erhalten —\n\
                     einfach erneut „Datei übertragen …“ starten, um fortzusetzen.",
                );
            }

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
                ui.colored_label(RED, "Fehler beim Kopieren:");
                for f in &fails {
                    ui.colored_label(RED, format!("  • {f}"));
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
                ui.add_space(8.0);
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

            // verify section
            ui.add_space(8.0);
            ui.separator();
            if self.summ.verify.is_some() {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(18.0));
                    ui.label(format!(
                        "Prüfsummen werden verglichen … {} / {}",
                        self.summ.verify_prog.0, self.summ.verify_prog.1
                    ));
                });
            } else if let Some(rep) = &self.summ.report {
                match rep {
                    Ok(r) => {
                        if r.fails.is_empty() {
                            ui.colored_label(GREEN, format!("OK: {} Dateien stimmen überein.", r.ok));
                        } else {
                            ui.colored_label(
                                RED,
                                format!("{} von {} Dateien weichen ab:", r.fails.len(), r.total),
                            );
                            for (name, why) in &r.fails {
                                ui.colored_label(RED, format!("  • {name} — {why}"));
                            }
                        }
                    }
                    Err(e) => {
                        ui.colored_label(RED, format!("Prüfung fehlgeschlagen: {e}"));
                    }
                }
            } else {
                ui.checkbox(
                    &mut self.settings.verify,
                    "Prüfsummen nach dem Kopieren vergleichen",
                );
            }
            ui.checkbox(&mut self.settings.autoclose, "Fenster automatisch schließen");
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                if self.summ.report.is_some() {
                    if ui.button("Erneut prüfen").clicked() {
                        self.spawn_verify();
                    }
                    let has_fails = self
                        .summ
                        .report
                        .as_ref()
                        .map(|r| r.as_ref().map(|x| !x.fails.is_empty()).unwrap_or(false))
                        .unwrap_or(false);
                    if has_fails && ui.button("Fehlerhafte erneut kopieren").clicked() {
                        self.retry_failed();
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Schließen").clicked() {
                        self.settings.save();
                        self.close(ctx);
                    }
                });
            });
        });

        // auto-close
        if self.settings.autoclose
            && matches!(self.summ.outcome, Outcome::Ok)
            && self.summ.verify.is_none()
            && self
                .summ
                .report
                .as_ref()
                .map(|r| r.as_ref().map(|x| x.fails.is_empty()).unwrap_or(true))
                .unwrap_or(true)
        {
            log::append("autoclose");
            self.settings.save();
            self.close(ctx);
        }
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
    if n_files > 0 {
        parts.push(format!("{n_files} Datei(en)"));
    }
    if n_dirs > 0 {
        parts.push(format!("{n_dirs} Ordner"));
    }
    format!("{} · {}", parts.join(", "), fsutil::fmt_size(bytes))
}
