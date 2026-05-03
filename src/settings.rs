use std::{env, fs, io, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    #[serde(default)]
    pub show_debug: bool,
}

impl Settings {
    /// Load settings from disk, returning defaults if the file doesn't exist
    /// or fails to parse. We never hard-fail on settings — a corrupted file
    /// just means the user gets defaults until they toggle something and
    /// save overwrites the bad file.
    pub fn load() -> Self {
        let Some(path) = settings_path() else {
            return Self::default();
        };
        let Ok(bytes) = fs::read(&path) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    pub fn save(&self) -> io::Result<()> {
        let Some(path) = settings_path() else {
            return Err(io::Error::other("could not resolve settings path"));
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(path, bytes)
    }
}

fn settings_path() -> Option<PathBuf> {
    let base = if let Ok(xdg) = env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        PathBuf::from(xdg)
    } else {
        let home = env::var("HOME").ok()?;
        PathBuf::from(home).join(".config")
    };
    Some(base.join("adele-tui").join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_debug_off() {
        assert!(!Settings::default().show_debug);
    }

    #[test]
    fn missing_xdg_home_var_falls_back_to_home() {
        // Smoke: settings_path is Some when HOME is set (true in test env).
        if env::var("HOME").is_ok() {
            assert!(settings_path().is_some());
        }
    }

    #[test]
    fn round_trip_serializes_show_debug() {
        let s = Settings { show_debug: true };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"show_debug\":true"));
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn unknown_fields_do_not_break_parse() {
        let json = r#"{"show_debug":true,"future_field":42}"#;
        let s: Settings = serde_json::from_str(json).unwrap();
        assert!(s.show_debug);
    }

    #[test]
    fn missing_show_debug_defaults_to_false() {
        let json = r#"{}"#;
        let s: Settings = serde_json::from_str(json).unwrap();
        assert!(!s.show_debug);
    }
}
