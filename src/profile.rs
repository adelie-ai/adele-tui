//! Saved connection profiles.
//!
//! A profile bundles the transport metadata needed to dial a specific
//! daemon. Credentials live in the system keyring (see
//! [`crate::credentials`]); the profile only carries non-secret hints
//! (username, "has a stored password" flags) so the UI can show what
//! state to expect.
//!
//! Persisted to `$XDG_CONFIG_HOME/adele-tui/profiles.json` (or
//! `~/.config/adele-tui/profiles.json`).

use std::{env, fs, io, path::PathBuf, time::SystemTime};

use desktop_assistant_client_common::{ConnectionConfig, TransportMode};
use serde::{Deserialize, Serialize};

use crate::credentials::{self, CredentialKind};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Profile {
    pub id: String,
    pub name: String,
    #[serde(with = "transport_mode_serde")]
    pub transport: TransportMode,
    /// WebSocket URL. Only meaningful when `transport == Ws`.
    #[serde(default)]
    pub ws_url: String,
    /// JWT subject claim. Only meaningful when `transport == Ws`.
    #[serde(default)]
    pub ws_subject: String,
    /// Unix-domain-socket path. Only meaningful when `transport == Uds`;
    /// `None` uses the daemon's default socket
    /// (`$XDG_RUNTIME_DIR/adelie/sock`).
    #[serde(default)]
    pub socket_path: Option<PathBuf>,
    /// Username for password-based login. `None` means no username/password
    /// auth — fall back to JWT or anonymous.
    #[serde(default)]
    pub username: Option<String>,
    /// True when a password has been stored in the keyring under this
    /// profile's id. The actual secret never leaves the keyring.
    #[serde(default)]
    pub has_password: bool,
    /// True when a JWT token has been stored in the keyring under this
    /// profile's id.
    #[serde(default)]
    pub has_jwt: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileStore {
    #[serde(default)]
    pub profiles: Vec<Profile>,
    #[serde(default)]
    pub last_used: Option<String>,
}

impl Profile {
    pub fn new(name: String, transport: TransportMode, ws_url: String, ws_subject: String) -> Self {
        Self {
            id: new_id(),
            name,
            transport,
            ws_url,
            ws_subject,
            socket_path: None,
            username: None,
            has_password: false,
            has_jwt: false,
        }
    }

    /// Build a connection config, pulling stored credentials from the keyring.
    /// Keyring failures are silently downgraded to "no credential" — callers
    /// should still allow CLI/env fallback.
    pub fn to_connection_config(&self) -> ConnectionConfig {
        let password = if self.has_password {
            credentials::retrieve(&self.id, CredentialKind::Password).ok()
        } else {
            None
        };
        let jwt = if self.has_jwt {
            credentials::retrieve(&self.id, CredentialKind::Jwt).ok()
        } else {
            None
        };
        ConnectionConfig {
            transport_mode: self.transport,
            ws_url: self.ws_url.clone(),
            ws_subject: self.ws_subject.clone(),
            ws_jwt: jwt,
            ws_login_username: self.username.clone(),
            ws_login_password: password,
            socket_path: self.socket_path.clone(),
            // Mint the socket-handshake JWT from the local `adelie-mint` minter
            // (#101/#316) — the preferred, non-retiring source — rather than
            // falling through to the deprecated D-Bus `generate_ws_jwt` path
            // (which the cutover removed). Inert for the D-Bus transport, which
            // authenticates by peer credentials and needs no token.
            minter_socket: desktop_assistant_client_common::minter::default_minter_socket_path(),
            ..Default::default()
        }
    }

    /// Drop any keyring entries associated with this profile. Errors are
    /// swallowed — there's nothing useful for a caller to do with them.
    pub fn purge_credentials(&self) {
        let _ = credentials::delete(&self.id, CredentialKind::Password);
        let _ = credentials::delete(&self.id, CredentialKind::Jwt);
        let _ = credentials::delete(&self.id, CredentialKind::OauthRefresh);
    }

    /// Short display label combining name and URL/transport.
    pub fn display_label(&self) -> String {
        let detail = match self.transport {
            TransportMode::Ws => self.ws_url.clone(),
            TransportMode::Dbus => "D-Bus".to_string(),
            TransportMode::Uds => match &self.socket_path {
                Some(path) => format!("local · {}", path.display()),
                None => "Local socket".to_string(),
            },
        };
        if detail.is_empty() {
            self.name.clone()
        } else {
            format!("{}  ·  {}", self.name, detail)
        }
    }
}

impl ProfileStore {
    pub fn load() -> Self {
        let Some(path) = profiles_path() else {
            return Self::default();
        };
        let Ok(bytes) = fs::read(&path) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    pub fn save(&self) -> io::Result<()> {
        let Some(path) = profiles_path() else {
            return Err(io::Error::other("could not resolve profiles path"));
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(path, bytes)
    }

    pub fn add(&mut self, profile: Profile) {
        self.profiles.push(profile);
    }

    pub fn remove(&mut self, id: &str) -> Option<Profile> {
        let idx = self.profiles.iter().position(|p| p.id == id)?;
        let removed = self.profiles.remove(idx);
        if self.last_used.as_deref() == Some(id) {
            self.last_used = None;
        }
        // Drop the keyring entries — leaving orphaned secrets after a
        // delete is a footgun, especially when re-adding under a new id.
        removed.purge_credentials();
        Some(removed)
    }

    pub fn mark_used(&mut self, id: &str) {
        if self.profiles.iter().any(|p| p.id == id) {
            self.last_used = Some(id.to_string());
        }
    }

    pub fn last_used_index(&self) -> Option<usize> {
        let id = self.last_used.as_deref()?;
        self.profiles.iter().position(|p| p.id == id)
    }
}

fn profiles_path() -> Option<PathBuf> {
    let base = if let Ok(xdg) = env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        PathBuf::from(xdg)
    } else {
        let home = env::var("HOME").ok()?;
        PathBuf::from(home).join(".config")
    };
    Some(base.join("adele-tui").join("profiles.json"))
}

fn new_id() -> String {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

mod transport_mode_serde {
    use super::TransportMode;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &TransportMode, s: S) -> Result<S::Ok, S::Error> {
        let token = match value {
            TransportMode::Ws => "ws",
            TransportMode::Dbus => "dbus",
            TransportMode::Uds => "local",
        };
        s.serialize_str(token)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<TransportMode, D::Error> {
        let token = String::deserialize(d)?;
        match token.as_str() {
            "ws" => Ok(TransportMode::Ws),
            "dbus" => Ok(TransportMode::Dbus),
            "local" | "uds" => Ok(TransportMode::Uds),
            other => Err(serde::de::Error::custom(format!(
                "unknown transport mode {other:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_serializes_transport_as_token() {
        let profile = Profile::new(
            "Local".into(),
            TransportMode::Ws,
            "ws://127.0.0.1:11339/ws".into(),
            "desktop-tui".into(),
        );
        let json = serde_json::to_string(&profile).unwrap();
        assert!(json.contains("\"transport\":\"ws\""));
        let back: Profile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, profile);
    }

    #[test]
    fn unknown_transport_token_fails_loud() {
        let json = r#"{"id":"1","name":"X","transport":"smtp","ws_url":"","ws_subject":""}"#;
        assert!(serde_json::from_str::<Profile>(json).is_err());
    }

    #[test]
    fn store_remove_clears_last_used_when_matches() {
        let mut store = ProfileStore::default();
        let p = Profile::new("a".into(), TransportMode::Ws, "ws://x".into(), "s".into());
        let id = p.id.clone();
        store.add(p);
        store.mark_used(&id);
        assert_eq!(store.last_used.as_deref(), Some(id.as_str()));
        store.remove(&id);
        assert!(store.last_used.is_none());
    }

    #[test]
    fn store_mark_used_ignores_unknown_id() {
        let mut store = ProfileStore::default();
        store.mark_used("does-not-exist");
        assert!(store.last_used.is_none());
    }

    #[test]
    fn last_used_index_finds_correct_position() {
        let mut store = ProfileStore::default();
        let a = Profile::new("a".into(), TransportMode::Ws, "ws://a".into(), "s".into());
        let b = Profile::new("b".into(), TransportMode::Ws, "ws://b".into(), "s".into());
        let b_id = b.id.clone();
        store.add(a);
        store.add(b);
        store.mark_used(&b_id);
        assert_eq!(store.last_used_index(), Some(1));
    }

    #[test]
    fn unknown_fields_in_json_are_ignored() {
        let json = r#"{
            "profiles": [{"id":"1","name":"X","transport":"ws","ws_url":"ws://x","ws_subject":"s","extra_field":42}],
            "last_used": null,
            "another_unknown": "ok"
        }"#;
        let store: ProfileStore = serde_json::from_str(json).unwrap();
        assert_eq!(store.profiles.len(), 1);
        assert_eq!(store.profiles[0].name, "X");
    }

    #[test]
    fn to_connection_config_strips_credentials() {
        let p = Profile::new(
            "Local".into(),
            TransportMode::Ws,
            "ws://127.0.0.1:11339/ws".into(),
            "desktop-tui".into(),
        );
        let cfg = p.to_connection_config();
        assert_eq!(cfg.transport_mode, TransportMode::Ws);
        assert_eq!(cfg.ws_url, "ws://127.0.0.1:11339/ws");
        assert!(cfg.ws_jwt.is_none());
        assert!(cfg.ws_login_username.is_none());
        assert!(cfg.ws_login_password.is_none());
    }

    #[test]
    fn uds_transport_serializes_as_local_token_and_round_trips() {
        let mut p = Profile::new(
            "Home".into(),
            TransportMode::Uds,
            String::new(),
            String::new(),
        );
        p.socket_path = Some(PathBuf::from("/run/user/1000/adelie/sock"));
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"transport\":\"local\""));
        let back: Profile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn uds_token_accepts_both_local_and_uds_spellings() {
        for token in ["local", "uds"] {
            let json = format!(
                r#"{{"id":"1","name":"X","transport":"{token}","ws_url":"","ws_subject":""}}"#
            );
            let p: Profile = serde_json::from_str(&json).unwrap();
            assert_eq!(p.transport, TransportMode::Uds);
            // Absent socket_path defaults to None (use the daemon's default).
            assert!(p.socket_path.is_none());
        }
    }

    #[test]
    fn to_connection_config_carries_socket_path_for_uds() {
        let mut p = Profile::new(
            "Home".into(),
            TransportMode::Uds,
            String::new(),
            String::new(),
        );
        p.socket_path = Some(PathBuf::from("/tmp/custom.sock"));
        let cfg = p.to_connection_config();
        assert_eq!(cfg.transport_mode, TransportMode::Uds);
        assert_eq!(cfg.socket_path, Some(PathBuf::from("/tmp/custom.sock")));
    }
}
