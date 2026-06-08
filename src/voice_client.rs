//! Client for the standalone voice daemon (`org.desktopAssistant.Voice`).
//!
//! The voice daemon (`adelie-ai/voice`) is a **separate** D-Bus service from the
//! orchestrator the rest of the TUI talks to: it owns the bus name
//! `org.desktopAssistant.Voice` (distinct from `org.desktopAssistant`) and
//! speaks its own typed interface. The TUI is just another client of it, so this
//! module talks to it directly over zbus rather than through the orchestrator's
//! transport. This is the TUI port of adele-gtk#80's `voice_client.rs`.
//!
//! Routing narration through the daemon reuses its already-warm TTS models +
//! shared audio sink, which is far faster than the in-process embedded engine
//! (the embedded `Speaker` re-loads models and is prone to the per-synth timeout
//! on long replies). When the daemon is running it is the preferred speaker; the
//! embedded engine is the fallback when it isn't.
//!
//! ## Graceful degradation
//!
//! When the daemon isn't running, the bus name has no owner. Each RPC then fails
//! (zbus returns `ServiceUnknown`/`NameHasNoOwner`); callers treat that as
//! "voice unavailable" and fall back rather than surfacing an error.
//! [`VoiceController::is_available`] probes ownership with a cheap round-trip so
//! the narration path can pick a backend per utterance.

/// Where a push-to-talk turn should be routed.
///
/// The pure decision behind dictation: when the user has a conversation open,
/// the spoken prompt and reply must land in *that* conversation
/// (`PushToTalkInConversation(<id>)`); with nothing open we fall back to the
/// daemon's own session (`PushToTalk()`). Kept as a standalone value so the
/// routing can be unit-tested without a live bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PttRoute {
    /// Dictate into this orchestrator conversation id.
    InConversation(String),
    /// No conversation open — use the daemon's own session.
    DaemonSession,
}

impl PttRoute {
    /// Decide the route from the active conversation id. A `Some` with a
    /// **non-empty** id routes into that conversation; `None` *or* an
    /// empty/whitespace id falls back to the daemon session (an empty id means
    /// "daemon session" to the daemon anyway, so we normalise to the explicit
    /// `PushToTalk()`).
    pub fn for_conversation(active_conversation: Option<&str>) -> Self {
        match active_conversation {
            Some(id) if !id.trim().is_empty() => Self::InConversation(id.to_string()),
            _ => Self::DaemonSession,
        }
    }
}

/// Typed zbus proxy for the voice daemon.
///
/// zbus derives each D-Bus method name by PascalCasing the Rust fn name, which
/// matches the daemon's own interface (`get_state` → `GetState`,
/// `push_to_talk` → `PushToTalk`, …), so no per-method `#[zbus(name = …)]`
/// overrides are needed. Only the methods the TUI drives are declared.
#[zbus::proxy(
    interface = "org.desktopAssistant.Voice",
    default_service = "org.desktopAssistant.Voice",
    default_path = "/org/desktopAssistant/Voice"
)]
pub trait Voice {
    /// Current pipeline state ("Idle" | "Listening" | "Processing" | "Speaking").
    fn get_state(&self) -> zbus::Result<String>;

    /// Start listening immediately (push-to-talk; works even with wake off).
    /// The spoken turn lands in the daemon's own session.
    fn push_to_talk(&self) -> zbus::Result<()>;

    /// Push-to-talk routed into a specific orchestrator conversation, so the
    /// spoken prompt and reply appear in the conversation the user is viewing.
    /// An empty `conversation_id` falls back to the daemon's own session.
    fn push_to_talk_in_conversation(&self, conversation_id: &str) -> zbus::Result<()>;

    /// Speak `text` through the daemon's warm TTS Speaker + audio sink (maps to
    /// `SayText`). The daemon's `Speaker` is one-shot and applies a per-synth
    /// timeout, so callers must hand it **one short sentence at a time**.
    fn say_text(&self, text: &str) -> zbus::Result<()>;

    /// Stop any in-progress TTS playback (barge-in).
    fn stop_speaking(&self) -> zbus::Result<()>;
}

/// Handle to the voice daemon.
///
/// Cheap to clone (an `Arc`-backed zbus proxy). A `None` proxy (connect failed
/// entirely — e.g. no session bus) makes every call a graceful no-op /
/// "unavailable".
#[derive(Clone)]
pub struct VoiceController {
    /// `None` when even establishing the session-bus connection failed; the
    /// controller is then inert and reports the service as unavailable.
    proxy: Option<VoiceProxy<'static>>,
}

impl VoiceController {
    /// Connect to the session bus and build the voice proxy. Returns a
    /// controller whose proxy is `None` only when the bus connection itself
    /// fails (rare — no session bus at all); a missing *daemon* still yields a
    /// live proxy, with availability probed separately via
    /// [`VoiceController::is_available`].
    pub async fn connect() -> Self {
        match zbus::Connection::session().await {
            Ok(conn) => match VoiceProxy::new(&conn).await {
                Ok(proxy) => Self { proxy: Some(proxy) },
                Err(error) => {
                    tracing::debug!(%error, "failed to build voice proxy; daemon narration off");
                    Self::unavailable()
                }
            },
            Err(error) => {
                tracing::debug!(%error, "no session bus for voice; daemon narration off");
                Self::unavailable()
            }
        }
    }

    /// An inert controller with no proxy. Every call is a graceful no-op and
    /// [`VoiceController::is_available`] reports `false`.
    pub fn unavailable() -> Self {
        Self { proxy: None }
    }

    /// Whether the voice daemon currently owns its bus name. A `false` here is
    /// normal (daemon not running / models unprovisioned) and must not be
    /// treated as an error — the caller falls back to the embedded engine.
    pub async fn is_available(&self) -> bool {
        let Some(proxy) = &self.proxy else {
            return false;
        };
        // A cheap round-trip that only succeeds when the name has an owner.
        proxy.get_state().await.is_ok()
    }

    /// Speak `text` through the daemon's warm Speaker (maps to `SayText`). The
    /// daemon's `Speaker` is **one-shot**, so the caller must hand this **one
    /// short sentence at a time** (see
    /// [`crate::voice::into_speakable_sentences`]).
    pub async fn say(&self, text: &str) -> Result<(), String> {
        let Some(proxy) = &self.proxy else {
            return Err("voice service unavailable".to_string());
        };
        proxy.say_text(text).await.map_err(|e| e.to_string())
    }

    /// Stop any in-progress TTS playback (barge-in).
    pub async fn stop_speaking(&self) -> Result<(), String> {
        let Some(proxy) = &self.proxy else {
            return Err("voice service unavailable".to_string());
        };
        proxy.stop_speaking().await.map_err(|e| e.to_string())
    }

    /// Dispatch a push-to-talk turn according to [`PttRoute::for_conversation`]:
    /// into the active conversation when one is open, else the daemon session.
    /// The routing decision itself is the pure [`PttRoute`] so it is unit-tested
    /// without a bus.
    pub async fn push_to_talk_routed(
        &self,
        active_conversation: Option<&str>,
    ) -> Result<(), String> {
        let Some(proxy) = &self.proxy else {
            return Err("voice service unavailable".to_string());
        };
        let result = match PttRoute::for_conversation(active_conversation) {
            PttRoute::InConversation(id) => proxy.push_to_talk_in_conversation(&id).await,
            PttRoute::DaemonSession => proxy.push_to_talk().await,
        };
        result.map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ptt_routes_into_the_active_conversation() {
        assert_eq!(
            PttRoute::for_conversation(Some("conv-123")),
            PttRoute::InConversation("conv-123".to_string())
        );
    }

    #[test]
    fn ptt_falls_back_to_daemon_session_with_no_conversation() {
        assert_eq!(PttRoute::for_conversation(None), PttRoute::DaemonSession);
    }

    #[test]
    fn ptt_treats_empty_conversation_id_as_no_conversation() {
        // An empty/whitespace id must not be sent as a "real" conversation; it
        // normalises to the daemon session (which is also how the daemon reads
        // an empty id), so the explicit PushToTalk() is issued.
        assert_eq!(
            PttRoute::for_conversation(Some("")),
            PttRoute::DaemonSession
        );
        assert_eq!(
            PttRoute::for_conversation(Some("   ")),
            PttRoute::DaemonSession
        );
    }

    /// An inert controller (no proxy) reports unavailable and every RPC is a
    /// graceful `Err`, never a panic — the narration path then falls back to the
    /// embedded engine.
    #[tokio::test]
    async fn unavailable_controller_is_inert() {
        let controller = VoiceController::unavailable();
        assert!(!controller.is_available().await);
        assert!(controller.say("hello").await.is_err());
        assert!(controller.stop_speaking().await.is_err());
        assert!(controller.push_to_talk_routed(Some("c1")).await.is_err());
        assert!(controller.push_to_talk_routed(None).await.is_err());
    }
}
