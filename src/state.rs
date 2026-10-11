//! Persisted settings: last destination, verify default, auto-close.
//! File: ~/.local/state/nascopy.last  (simple key=value lines)

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
        let Ok(text) = std::fs::read_to_string(last_path()) else {
            return s;
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
        let dir = crate::log::state_dir();
        let _ = std::fs::create_dir_all(&dir);
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
