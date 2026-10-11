//! Dateien übertragen (filetransfer) — Dolphin service-menu applet and
//! standalone tool.
//!
//! Copies selected files/folders to network shares, USB drives or local
//! folders via rsync, with Dolphin-style conflict handling and automatic
//! FAT32 splitting of >4 GiB files. Without arguments it accepts drag & drop
//! or opens a file picker.

mod args;
mod conflicts;
mod dest;
mod fsutil;
mod log;
mod selftest;
mod state;
mod transfer;
mod ui;
mod verify;

use egui::ViewportBuilder;

fn main() -> eframe::Result {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    if argv.iter().any(|a| a == "--selftest") {
        selftest::run();
        return Ok(());
    }
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return Ok(());
    }

    log::append(&format!("=== invoked files={}", argv.len()));

    let raw = args::raw_args();
    let (tx, rx) = std::sync::mpsc::channel();
    {
        let raw = raw.clone();
        std::thread::spawn(move || {
            let r = args::collect(&raw);
            let _ = tx.send(r);
        });
    }

    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("Dateien übertragen")
            .with_inner_size([900.0, 620.0])
            .with_min_inner_size([700.0, 460.0])
            .with_icon(app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "Dateien übertragen",
        options,
        Box::new(move |cc| Ok(Box::new(ui::App::new(cc, raw, Some(rx))))),
    )
}

fn print_help() {
    println!(
        "Dateien übertragen (filetransfer) 1.3.0\n\
         \n\
         Aufruf: filetransfer [DATEIEN …]\n\
         \n\
         Wird über das Dolphin-Kontextmenü gestartet; DATEIEN sind die markierten\n\
         Objekte. Ohne Argumente erscheint eine Ablage: Dateien hineinziehen oder\n\
         über den Dateiauswahl-Dialog wählen.\n\
         \n\
         Optionen:\n\
           --selftest   Selbsttest ohne GUI (Kopierrunde inkl. FAT32-Zerlegung in /tmp)\n\
           --help       diese Hilfe\n\
         \n\
         Einstellungen (Prüfsummen-Prüfung, automatisches Schließen) sind in der\n\
         App über „Einstellungen …“ erreichbar.\n\
         \n\
         Daten: ~/.local/state/filetransfer.log · filetransfer.last ·
         filetransfer.job.<pid> (Ziel-Anmeldung laufender Übertragungen)"
    );
}

/// Simple programmatic icon: petrol disc with a white transfer arrow.
fn app_icon() -> egui::IconData {
    const S: usize = 64;
    let mut rgba = vec![0u8; S * S * 4];
    let c = 31.5f32;
    for y in 0..S {
        for x in 0..S {
            let dx = x as f32 - c;
            let dy = y as f32 - c;
            let r = (dx * dx + dy * dy).sqrt();
            let (cr, cg, cb, ca) = if r <= 30.0 {
                let shaft = (13..=38).contains(&x) && (27..=36).contains(&y);
                let head = (36..=52).contains(&x)
                    && (y as f32 - c).abs() <= (52.0 - x as f32) * 0.9 + 0.5;
                if shaft || head {
                    (255u8, 255u8, 255u8, 255u8)
                } else {
                    (0x0Eu8, 0x70u8, 0x86u8, 255u8) // petrol #0E7086
                }
            } else {
                (0, 0, 0, 0)
            };
            let i = (y * S + x) * 4;
            rgba[i] = cr;
            rgba[i + 1] = cg;
            rgba[i + 2] = cb;
            rgba[i + 3] = ca;
        }
    }
    egui::IconData {
        width: S as u32,
        height: S as u32,
        rgba,
    }
}
