use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use desktop_assistant_client_common::ConnectionConfig;
use serde::{Deserialize, Serialize};

/// Persisted, client-local user preferences for the TUI.
///
/// Stored as JSON at `$XDG_CONFIG_HOME/adele-tui/settings.json` (or
/// `~/.config/adele-tui/settings.json`). Loading is tolerant: a missing or
/// corrupt file yields [`Settings::default`], and any field a file omits falls
/// back to that field's `#[serde(default)]`, so upgrading past a new field never
/// breaks an older config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    /// Render tool/system/empty-assistant messages dimly inline instead of
    /// filtering them out. Off by default.
    #[serde(default)]
    pub show_debug: bool,
    /// Share basic device context (real name, username, home folder, hostname,
    /// timezone, OS) with the assistant so it can personalize replies. **On by
    /// default** -- the daemon-side default is also on, so this is the user's
    /// off-switch (da#549). When off, the client attaches no device context to
    /// the connect handshake. A settings.json predating this field parses as on.
    #[serde(default = "default_share_client_context")]
    pub share_client_context: bool,
}

/// The default for [`Settings::share_client_context`]: **on**. A standalone
/// function (rather than `bool::default`, which is `false`) so a config missing
/// the key deserializes to on, matching the daemon-side default.
fn default_share_client_context() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            show_debug: false,
            share_client_context: default_share_client_context(),
        }
    }
}

impl Settings {
    /// Load settings from the default path, returning [`Settings::default`] if
    /// the file doesn't exist or fails to parse. We never hard-fail on settings
    /// -- a corrupted file just means the user gets defaults until they toggle
    /// something and save overwrites the bad file.
    pub fn load() -> Self {
        match default_settings_path() {
            Some(path) => Self::load_from(&path),
            None => Self::default(),
        }
    }

    /// Load settings from an explicit path, returning [`Settings::default`] when
    /// the file is absent or unparseable. Injecting the path keeps the loader
    /// unit-testable and lets the `config` CLI act on the file without a TUI.
    pub fn load_from(path: &Path) -> Self {
        let Ok(bytes) = fs::read(path) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    /// Save settings to the default path, creating parent directories as needed.
    pub fn save(&self) -> io::Result<()> {
        let Some(path) = default_settings_path() else {
            return Err(io::Error::other("could not resolve settings path"));
        };
        self.save_to(&path)
    }

    /// Save settings to an explicit path, creating parent directories as needed.
    /// The injectable twin of [`Settings::save`], used by the `config` CLI and
    /// the unit tests.
    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(path, bytes)
    }

    /// Apply the persisted client-local preferences that shape an outgoing
    /// [`ConnectionConfig`] before we dial the daemon.
    ///
    /// Currently that is only [`Settings::share_client_context`] (da#549): the
    /// daemon and [`ConnectionConfig::default`] both default sharing on, so this
    /// carries the user's opt-out through to the connect handshake. Applied when
    /// the connection is built, so a mid-session toggle takes effect on the next
    /// (re)connect.
    pub fn apply_to_connection(&self, config: &mut ConnectionConfig) {
        config.share_client_context = self.share_client_context;
    }
}

/// The default settings file path: `$XDG_CONFIG_HOME/adele-tui/settings.json`,
/// falling back to `~/.config/adele-tui/settings.json`. `None` when neither
/// `XDG_CONFIG_HOME` nor `HOME` is set (no sensible location), in which case the
/// caller degrades to in-memory defaults rather than failing.
pub fn default_settings_path() -> Option<PathBuf> {
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
        // Smoke: default_settings_path is Some when HOME is set (true in test env).
        if env::var("HOME").is_ok() {
            assert!(default_settings_path().is_some());
        }
    }

    #[test]
    fn round_trip_serializes_show_debug() {
        let s = Settings {
            show_debug: true,
            share_client_context: true,
        };
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
