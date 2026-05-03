//! Thin wrapper around the system keyring (Secret Service on Linux).
//!
//! Credentials are scoped per-profile by keying entries with the profile id.
//! All operations are best-effort: if the keyring is unavailable (headless
//! systems, CI, env without `dbus` running) we surface the error so callers
//! can fall back to env vars or skip the credential silently.

use std::{env, io};

use keyring::Entry;

const SERVICE: &str = "adele-tui";

const ENV_DISABLE: &str = "ADELE_TUI_DISABLE_KEYRING";

#[derive(Debug, Clone, Copy)]
pub enum CredentialKind {
    /// Login password used for username/password auth.
    Password,
    /// Direct JWT token.
    Jwt,
}

impl CredentialKind {
    fn account_prefix(self) -> &'static str {
        match self {
            CredentialKind::Password => "password",
            CredentialKind::Jwt => "jwt",
        }
    }
}

fn account_key(profile_id: &str, kind: CredentialKind) -> String {
    format!("{}::{}", kind.account_prefix(), profile_id)
}

/// Returns true when the keyring is intentionally disabled via env var.
/// Useful for headless CI runs where `dbus` isn't available.
fn keyring_disabled() -> bool {
    env::var(ENV_DISABLE)
        .ok()
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"))
}

pub fn store(profile_id: &str, kind: CredentialKind, secret: &str) -> io::Result<()> {
    if keyring_disabled() {
        return Err(io::Error::other("keyring disabled via env"));
    }
    let key = account_key(profile_id, kind);
    let entry = Entry::new(SERVICE, &key).map_err(map_err)?;
    entry.set_password(secret).map_err(map_err)
}

pub fn retrieve(profile_id: &str, kind: CredentialKind) -> io::Result<String> {
    if keyring_disabled() {
        return Err(io::Error::other("keyring disabled via env"));
    }
    let key = account_key(profile_id, kind);
    let entry = Entry::new(SERVICE, &key).map_err(map_err)?;
    entry.get_password().map_err(map_err)
}

pub fn delete(profile_id: &str, kind: CredentialKind) -> io::Result<()> {
    if keyring_disabled() {
        return Ok(()); // Treat disabled keyring as a no-op for deletes.
    }
    let key = account_key(profile_id, kind);
    let entry = Entry::new(SERVICE, &key).map_err(map_err)?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        // Missing entries are not an error for the caller's purposes.
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(map_err(e)),
    }
}

fn map_err(err: keyring::Error) -> io::Error {
    io::Error::other(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes env-var mutation across the tests in this module so the
    /// parallel test runner doesn't race on `ADELE_TUI_DISABLE_KEYRING`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env_disabled<F: FnOnce()>(f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = env::var(ENV_DISABLE).ok();
        // SAFETY: held under ENV_LOCK so no other test in this module
        // mutates the same var concurrently.
        unsafe { env::set_var(ENV_DISABLE, "1") };
        f();
        unsafe {
            match prior {
                Some(v) => env::set_var(ENV_DISABLE, v),
                None => env::remove_var(ENV_DISABLE),
            }
        }
    }

    #[test]
    fn account_key_includes_kind_and_profile_id() {
        let k = account_key("abc-123", CredentialKind::Password);
        assert_eq!(k, "password::abc-123");
        let k = account_key("abc-123", CredentialKind::Jwt);
        assert_eq!(k, "jwt::abc-123");
    }

    #[test]
    fn keyring_disabled_returns_true_when_env_set() {
        with_env_disabled(|| assert!(keyring_disabled()));
    }

    #[test]
    fn store_returns_error_when_disabled() {
        with_env_disabled(|| {
            assert!(store("test-id", CredentialKind::Password, "secret").is_err());
        });
    }

    #[test]
    fn delete_is_noop_when_disabled() {
        with_env_disabled(|| {
            assert!(delete("test-id", CredentialKind::Password).is_ok());
        });
    }
}
