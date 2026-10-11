//! Datei übertragen (nascopy) — Dolphin service-menu applet.
//!
//! Copies selected files/folders to NAS or USB sticks via rsync, with
//! Dolphin-style conflict handling and automatic FAT32 splitting of >4 GiB files.

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
    let lock_ok = log::InstanceLock::acquire().is_ok();
    if !lock_ok {
        log::append("second instance blocked by lock");
    }

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
            .with_title("Datei übertragen")
            .with_inner_size([880.0, 600.0])
            .with_min_inner_size([680.0, 440.0])
            .with_icon(app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "Datei übertragen",
        options,
        Box::new(move |cc| Ok(Box::new(ui::App::new(cc, raw, Some(rx), lock_ok)))),
    )
}

fn print_help() {
    println!(
        "Datei übertragen (nascopy) 1.0.0\n\
         \n\
         Aufruf: nascopy [DATEIEN …]\n\
         \n\
         Wird über das Dolphin-Kontextmenü gestartet; DATEIEN sind die markierten\n\
         Objekte. Ohne Argumente erscheint ein Hinweisfenster.\n\
         \n\
         Optionen:\n\
           --selftest   Selbsttest ohne GUI (Kopierrunde inkl. FAT32-Zerlegung in /tmp)\n\
           --help       diese Hilfe\n\
         \n\
         Daten: ~/.local/state/nascopy.log · nascopy.last · nascopy.lock"
    );
}

/// Simple programmatic icon: blue disc with a white transfer arrow.
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
                    (41u8, 128u8, 185u8, 255u8)
                }
            } else {
                (0u8, 0u8, 0u8, 0u8)
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
