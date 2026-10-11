//! Persisted settings: last destination, verify default, auto-close.
//! File: ~/.local/state/filetransfer.last  (simple key=value lines)
//! Falls back once to the predecessor's nascopy.last for the destination.

use crate::log::last_path;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Settings {
    pub dest: Option<PathBuf>,
    pub verify: bool,
    pub autoclose: bool,
}

impl Default for Settings {
    fn default() -> Self {
        // "Verify" is remembered and defaults to ON (user cares about irreplaceable videos).
        Settings {
            dest: None,
            verify: true,
            autoclose: false,
        }
    }
}

impl Settings {
    pub fn load() -> Settings {
        let mut s = Settings::default();
        let text = match std::fs::read_to_string(last_path()) {
            Ok(text) => text,
            Err(_) => match crate::log::legacy_last_path() {
                // First run after the rename: adopt the old last destination.
                Some(p) => match std::fs::read_to_string(p) {
                    Ok(text) => text,
                    Err(_) => return s,
                },
                None => return s,
            },
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k.trim() {
                "dest" => {
                    let p = PathBuf::from(v.trim());
                    if p.is_dir() {
                        s.dest = Some(p);
                    }
                }
                "verify" => s.verify = v.trim() == "1",
                "autoclose" => s.autoclose = v.trim() == "1",
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(crate::log::state_base());
        let mut text = String::new();
        if let Some(d) = &self.dest {
            text.push_str(&format!("dest={}\n", d.display()));
        }
        text.push_str(&format!("verify={}\n", self.verify as u8));
        text.push_str(&format!("autoclose={}\n", self.autoclose as u8));
        let _ = std::fs::write(last_path(), text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_default_verify_on() {
        assert!(Settings::default().verify);
    }
}
