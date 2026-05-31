//! Thin wrapper around the system keyring (Secret Service on Linux).
//!
//! Credentials are scoped per-profile by keying entries with the profile id.
//! All operations are best-effort: if the keyring is unavailable (headless
//! systems, CI, env without `dbus` running) we surface the error so callers
//! can fall back to env vars or skip the credential silently.

use std::{env, io};

use keyring_core::Entry;

const SERVICE: &str = "adele-tui";

const ENV_DISABLE: &str = "ADELE_TUI_DISABLE_KEYRING";

#[derive(Debug, Clone, Copy)]
pub enum CredentialKind {
    /// Login password used for username/password auth.
    Password,
    /// Direct JWT (or OAuth access token after sign-in).
    Jwt,
    /// OAuth refresh token, paired with an access token stored under `Jwt`.
    OauthRefresh,
}

impl CredentialKind {
    fn account_prefix(self) -> &'static str {
        match self {
            CredentialKind::Password => "password",
            CredentialKind::Jwt => "jwt",
            CredentialKind::OauthRefresh => "oauth_refresh",
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

/// Run a blocking Secret Service operation without starving the async runtime.
///
/// keyring-core's Secret Service store drives D-Bus over zbus's *blocking*
/// API, which must not run directly on an async worker thread. On a
/// multi-thread runtime we hand the work to `block_in_place`; off a runtime
/// (or on a current-thread runtime) we run inline.
fn run_keyring_blocking<T>(operation: impl FnOnce() -> T) -> T {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(operation)
        }
        _ => operation(),
    }
}

/// Register the system Secret Service as keyring-core's default credential
/// store. Best-effort: if it's unavailable (headless / no session bus) we warn
/// and continue — credential calls then surface errors and callers fall back.
/// Runs at startup before raw mode, so stderr is the visible channel here (the
/// TUI has no tracing subscriber).
pub fn init_store() {
    if keyring_disabled() {
        return;
    }
    run_keyring_blocking(|| match zbus_secret_service_keyring_store::Store::new() {
        Ok(store) => keyring_core::set_default_store(store),
        Err(error) => eprintln!("adele-tui: Secret Service unavailable; keyring disabled: {error}"),
    });
}

pub fn store(profile_id: &str, kind: CredentialKind, secret: &str) -> io::Result<()> {
    if keyring_disabled() {
        return Err(io::Error::other("keyring disabled via env"));
    }
    run_keyring_blocking(|| {
        let key = account_key(profile_id, kind);
        let entry = Entry::new(SERVICE, &key).map_err(map_err)?;
        entry.set_password(secret).map_err(map_err)
    })
}

pub fn retrieve(profile_id: &str, kind: CredentialKind) -> io::Result<String> {
    if keyring_disabled() {
        return Err(io::Error::other("keyring disabled via env"));
    }
    run_keyring_blocking(|| {
        let key = account_key(profile_id, kind);
        let entry = Entry::new(SERVICE, &key).map_err(map_err)?;
        entry.get_password().map_err(map_err)
    })
}

pub fn delete(profile_id: &str, kind: CredentialKind) -> io::Result<()> {
    if keyring_disabled() {
        return Ok(()); // Treat disabled keyring as a no-op for deletes.
    }
    run_keyring_blocking(|| {
        let key = account_key(profile_id, kind);
        let entry = Entry::new(SERVICE, &key).map_err(map_err)?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            // Missing entries are not an error for the caller's purposes.
            Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(map_err(e)),
        }
    })
}

fn map_err(err: keyring_core::Error) -> io::Error {
    io::Error::other(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, Once};

    /// Serializes env-var mutation across the tests in this module so the
    /// parallel test runner doesn't race on `ADELE_TUI_DISABLE_KEYRING`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Registers an in-memory mock as keyring-core's default store exactly once
    /// for the whole test binary, so the real store path (`Entry`, `store`,
    /// `retrieve`, `delete`) is exercised without a session bus.
    static MOCK_STORE: Once = Once::new();

    fn init_mock_store() {
        MOCK_STORE.call_once(|| {
            keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
        });
    }

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

    /// Runs `f` with the keyring guaranteed ENABLED (env var removed),
    /// holding `ENV_LOCK` so a concurrent disabled-test can't make the real
    /// store calls early-return. Restores the prior env value afterwards.
    fn with_env_enabled<F: FnOnce()>(f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = env::var(ENV_DISABLE).ok();
        // SAFETY: held under ENV_LOCK so no other test in this module
        // mutates the same var concurrently.
        unsafe { env::remove_var(ENV_DISABLE) };
        f();
        if let Some(v) = prior {
            // SAFETY: still held under ENV_LOCK.
            unsafe { env::set_var(ENV_DISABLE, v) };
        }
    }

    #[test]
    fn account_key_includes_kind_and_profile_id() {
        let k = account_key("abc-123", CredentialKind::Password);
        assert_eq!(k, "password::abc-123");
        let k = account_key("abc-123", CredentialKind::Jwt);
        assert_eq!(k, "jwt::abc-123");
        let k = account_key("abc-123", CredentialKind::OauthRefresh);
        assert_eq!(k, "oauth_refresh::abc-123");
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

    #[test]
    fn store_then_retrieve_round_trips_secret() {
        init_mock_store();
        with_env_enabled(|| {
            // Unique profile id so concurrent store-path tests don't collide.
            let profile = "roundtrip-store-retrieve";
            store(profile, CredentialKind::Jwt, "s3cret").unwrap();
            assert_eq!(retrieve(profile, CredentialKind::Jwt).unwrap(), "s3cret");
            // Clean up so reruns in the same process start fresh.
            delete(profile, CredentialKind::Jwt).unwrap();
        });
    }

    #[test]
    fn retrieve_absent_key_returns_err() {
        init_mock_store();
        with_env_enabled(|| {
            let profile = "roundtrip-absent-key";
            // Ensure no leftover from a prior run in this process.
            delete(profile, CredentialKind::Password).unwrap();
            assert!(retrieve(profile, CredentialKind::Password).is_err());
        });
    }
}
