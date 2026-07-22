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

    // --- Share device info (da#549 Phase 2b) ---

    #[test]
    fn share_client_context_defaults_true() {
        assert!(Settings::default().share_client_context);
    }

    #[test]
    fn missing_share_client_context_defaults_to_true() {
        // Back-compat: a settings.json written before the field existed keeps
        // sharing ON (the daemon-side default), so upgrading never silently
        // opts a user out.
        let json = r#"{"show_debug":true}"#;
        let s: Settings = serde_json::from_str(json).unwrap();
        assert!(s.share_client_context);
        assert!(s.show_debug);
    }

    #[test]
    fn empty_object_defaults_share_client_context_true() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert!(s.share_client_context);
    }

    #[test]
    fn round_trip_serializes_share_client_context() {
        let s = Settings {
            show_debug: false,
            share_client_context: false,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"share_client_context\":false"));
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn apply_to_connection_off_disables_sharing() {
        let s = Settings {
            show_debug: false,
            share_client_context: false,
        };
        let mut cfg = ConnectionConfig::default();
        assert!(
            cfg.share_client_context,
            "daemon-side default must be on before we apply the opt-out"
        );
        s.apply_to_connection(&mut cfg);
        assert!(!cfg.share_client_context);
    }

    #[test]
    fn apply_to_connection_on_keeps_sharing() {
        let mut cfg = ConnectionConfig::default();
        Settings::default().apply_to_connection(&mut cfg);
        assert!(cfg.share_client_context);
    }

    #[test]
    fn load_from_missing_file_defaults_share_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.json");
        let s = Settings::load_from(&path);
        assert!(s.share_client_context);
        assert!(!s.show_debug);
    }

    #[test]
    fn save_to_then_load_from_round_trips_both_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        let s = Settings {
            show_debug: true,
            share_client_context: false,
        };
        s.save_to(&path).expect("save");
        let back = Settings::load_from(&path);
        assert_eq!(back, s);
    }

    #[test]
    fn load_from_corrupt_file_yields_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"not json at all").expect("write");
        let s = Settings::load_from(&path);
        assert_eq!(s, Settings::default());
        assert!(s.share_client_context);
    }
}
