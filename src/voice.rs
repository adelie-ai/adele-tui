//! In-app voice: embedded dictation + reply playback, with no voice daemon.
//!
//! The TUI embeds [`adele_voice_module`] so a key press can dictate a prompt
//! (mic → Silero VAD endpoint → Whisper) and the assistant's reply can be
//! spoken back (Kokoro/Piper/Polly → speakers) — all **in-process**, reaching
//! only the orchestrator the TUI already talks to. There is **no wake word**
//! and **no D-Bus**: those stay in the voice daemon (run it if you want
//! hands-free "Hey Adele"). See adelie-ai/voice#34 (the module-vs-service
//! epic) and adele-tui#67.
//!
//! Configuration lives in its own `voice.toml` next to `settings.json`, because
//! the module's config sections are TOML-native (`Deserialize` + `Default`) and
//! the daemon uses the same shapes. The [`VoiceMode`] toggle gates the embedded
//! pipeline: it defaults to [`VoiceMode::Off`] so a TUI with no voice config
//! behaves exactly as before. Narration still routes through the voice daemon
//! (`org.desktopAssistant.Voice`) via [`crate::voice_client`] when that daemon is
//! running, regardless of this mode (the daemon path is probed independently and
//! is the preferred speaker — see adele-tui#77).
//!
//! Building the embedded pipeline loads ONNX models (hundreds of MB), so it is
//! done lazily on first use rather than at startup, and only when the mode is
//! `embedded`.
//!
//! # Dictation is not on every target
//!
//! The endpointer is Silero VAD, which needs ONNX Runtime, which has no
//! prebuilt binary for macOS on x86_64. That target therefore selects no VAD
//! backend at all (see the `adele-voice-module` entries in `Cargo.toml`) and
//! has **no dictation**: [`DICTATION_SUPPORTED`] is false, [`dictation_gate`]
//! reports [`DictationBlocked::NotCompiledIn`], and a key press says so.
//! Reply **playback** is unaffected on every target, because the Piper and
//! Polly backends need no ONNX Runtime. Dictation returns to macOS with the
//! native Apple Speech adapters in adelie-ai/voice#133.

#[cfg(not(target_os = "macos"))]
use std::sync::Arc;

use adele_voice_module::config::{AudioConfig, SttConfig, TtsConfig, VadConfig};
#[cfg(not(target_os = "macos"))]
use adele_voice_module::{Dictation, SileroVad, WhisperStt, build_dictation};
use adele_voice_module::{Speaker, TtsBackend, build_speaker};
use serde::Deserialize;
#[cfg(not(target_os = "macos"))]
use tokio::sync::Mutex;

/// The speakable-sentence chunker now lives in the shared `client-voice` crate
/// (desktop-assistant#274) so the GTK and TUI clients can't drift. Re-exported
/// at its original path so the narration path keeps calling
/// `voice::into_speakable_sentences`.
pub use adele_voice_client_common::into_speakable_sentences;

/// Drive a serialized narration loop: pull utterances off `rx` and speak each
/// one to completion before starting the next (TUI-11).
///
/// Reply narration and `say_this` asides previously each `tokio::spawn`ed their
/// own playback task, so a `say_this` aside firing mid-reply interleaved
/// sentence-by-sentence with the reply on the shared audio sink. Funnelling both
/// through this one loop makes utterances strictly sequential: the next text is
/// not even dequeued until `speak` for the current one has returned.
///
/// `speak` is the per-utterance side effect (in production: chunk + route the
/// utterance daemon-first); it is injected, and the item type `T` is generic, so
/// the serialization invariant can be unit-tested without an audio device. The
/// loop returns when the channel closes (all senders dropped), so a sender held
/// by the app for its lifetime keeps it alive and a clean shutdown ends it.
pub async fn run_narration_loop<T, F, Fut>(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<T>,
    speak: F,
) where
    F: Fn(T) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    while let Some(item) = rx.recv().await {
        speak(item).await;
    }
}

/// How the TUI sources voice. Defaults to [`VoiceMode::Off`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceMode {
    /// Voice disabled (the default — nothing loads, no mic access).
    #[default]
    Off,
    /// In-process dictation + playback via the embedded module. No daemon.
    Embedded,
}

/// User-facing voice configuration, parsed from `voice.toml`.
///
/// The four nested sections are the module's own config types, so the embedded
/// builders consume them directly. Each defaults independently, so a partial
/// file (e.g. just `mode = "embedded"`) still parses.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    /// The capability toggle (`off` | `embedded`).
    pub mode: VoiceMode,
    pub audio: AudioConfig,
    pub vad: VadConfig,
    pub stt: SttConfig,
    pub tts: TtsConfig,
}

impl VoiceConfig {
    /// Load `voice.toml` from the config dir, returning [`VoiceConfig::default`]
    /// (i.e. voice off) when the file is missing or unparseable. Voice is a
    /// convenience, never load-bearing, so a bad config degrades to "off"
    /// rather than failing TUI startup.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match toml::from_str(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!("ignoring malformed voice.toml ({e}); voice disabled");
                Self::default()
            }
        }
    }

    /// Whether the embedded pipeline should be wired up.
    pub fn embedded_enabled(&self) -> bool {
        self.mode == VoiceMode::Embedded
    }
}

/// `$XDG_CONFIG_HOME/adele-tui/voice.toml` (falling back to `~/.config`),
/// mirroring where `settings.json` lives.
fn config_path() -> Option<std::path::PathBuf> {
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(xdg) if !xdg.is_empty() => std::path::PathBuf::from(xdg),
        _ => std::path::PathBuf::from(std::env::var("HOME").ok()?).join(".config"),
    };
    Some(base.join("adele-tui").join("voice.toml"))
}

/// Whether this build contains a dictation pipeline.
///
/// Paired with the target-scoped `adele-voice-module` dependency in
/// `Cargo.toml`: macOS selects no VAD backend, because the only one available
/// needs ONNX Runtime, which has no prebuilt binary for that target. The pairing
/// is not left to care - the two disagreeing stops the build, because
/// `build_dictation` exists only when the module compiles both adapters.
///
/// This is the one place the platform is named. Everything else asks this.
pub const DICTATION_SUPPORTED: bool = cfg!(not(target_os = "macos"));

/// Why a dictation key press did not start a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictationBlocked {
    /// The build contains no dictation pipeline at all.
    NotCompiledIn,
    /// Voice is configured off.
    VoiceOff,
    /// A capture is already running.
    AlreadyListening,
    /// The session is still loading its models.
    StillLoading,
}

impl DictationBlocked {
    /// What to show the user, which must point at something they can act on.
    pub fn message(self) -> &'static str {
        match self {
            Self::NotCompiledIn => {
                "Dictation is not in this build — speech input needs a voice-activity \
                 detector, and none is available for this platform yet. Reply playback \
                 still works."
            }
            Self::VoiceOff => {
                "Voice is off — set mode = \"embedded\" in ~/.config/adele-tui/voice.toml"
            }
            Self::AlreadyListening => "Already listening…",
            Self::StillLoading => "Voice still loading models — try again in a moment",
        }
    }
}

/// Decide whether a dictation key press can start a capture.
///
/// `NotCompiledIn` is checked first on purpose. Every other message sends
/// someone to edit `voice.toml` or to wait, and neither does anything for a
/// build with no dictation in it.
pub fn dictation_gate(
    supported: bool,
    embedded_enabled: bool,
    already_dictating: bool,
    session_ready: bool,
) -> Result<(), DictationBlocked> {
    if !supported {
        return Err(DictationBlocked::NotCompiledIn);
    }
    if !embedded_enabled {
        return Err(DictationBlocked::VoiceOff);
    }
    if already_dictating {
        return Err(DictationBlocked::AlreadyListening);
    }
    if !session_ready {
        return Err(DictationBlocked::StillLoading);
    }
    Ok(())
}

/// Result of a one-shot embedded dictation capture, delivered from the capture
/// task back to the event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictationOutcome {
    /// A non-empty transcript was produced.
    Transcribed(String),
    /// The capture ended with no usable speech (timed out, near-silent, or an
    /// empty transcript) — the module returned `None`.
    NoSpeech,
    /// The capture errored (mic open failed, model error, …).
    Failed(String),
}

/// The embedded voice pipeline: a speaker, plus a one-shot dictation capture on
/// the targets that have one.
///
/// The `Dictation` is behind a `Mutex` because each press dictates on a spawned
/// task (capture is blocking-ish — it opens the mic and waits for speech), and
/// the lock both gives the task ownership and prevents two presses from opening
/// the mic at once. `Speaker` is cheap to clone (shared `Arc` handles), so the
/// playback task gets its own clone.
pub struct VoiceSession {
    #[cfg(not(target_os = "macos"))]
    dictation: Arc<Mutex<Dictation<SileroVad, WhisperStt>>>,
    speaker: Speaker<TtsBackend>,
}

impl VoiceSession {
    /// Wire the embedded pipeline from config. Loads the TTS backend and, where
    /// the build has dictation, the VAD/STT models, so this is the expensive
    /// step; call it once, lazily, on first use.
    ///
    /// Whether replies are *spoken* is no longer a property of the session: the
    /// per-conversation `Ctrl+S` speech toggle (adele-tui#73) governs that. The
    /// session just supplies the `Speaker`; the caller decides per conversation
    /// whether to use it.
    ///
    /// `build_speaker` is called on every target, with no `cfg` around it, so a
    /// build that lost reply playback would not compile. That is what holds
    /// playback independent of whether the target has dictation.
    pub async fn build(cfg: &VoiceConfig) -> anyhow::Result<Self> {
        // Build the speaker first so the dictation can share its output sink as
        // an echo guard (half-duplex): the mic then won't capture and transcribe
        // Adele's own TTS playback. The stored `speaker` is the one playback runs
        // through, so the guard watches the right sink.
        let speaker = build_speaker(&cfg.tts, &cfg.audio).await;
        #[cfg(not(target_os = "macos"))]
        {
            let dictation =
                build_dictation(&cfg.audio, &cfg.vad, &cfg.stt)?.with_echo_guard(speaker.sink());
            Ok(Self {
                dictation: Arc::new(Mutex::new(dictation)),
                speaker,
            })
        }
        #[cfg(target_os = "macos")]
        Ok(Self { speaker })
    }

    /// One capture, run to completion, or `None` where the build has no
    /// dictation pipeline.
    ///
    /// The returned future owns the lock for its whole duration: that both gives
    /// it the `&mut Dictation` it needs and stops a second press opening the mic
    /// while the first is still recording.
    #[cfg(not(target_os = "macos"))]
    pub fn capture(&self) -> Option<impl std::future::Future<Output = DictationOutcome> + use<>> {
        let handle = Arc::clone(&self.dictation);
        Some(async move {
            let mut dictation = handle.lock().await;
            match dictation.dictate().await {
                Ok(Some(text)) => DictationOutcome::Transcribed(text),
                Ok(None) => DictationOutcome::NoSpeech,
                Err(e) => DictationOutcome::Failed(e.to_string()),
            }
        })
    }

    #[cfg(target_os = "macos")]
    pub fn capture(&self) -> Option<impl std::future::Future<Output = DictationOutcome> + use<>> {
        None::<std::future::Ready<DictationOutcome>>
    }

    /// A speaker clone for spawning a playback task.
    pub fn speaker(&self) -> Speaker<TtsBackend> {
        self.speaker.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The gate is a pure function of four facts rather than a chain of `if`s
    // inside the key handler, so every reason a press does nothing can be
    // checked on any platform - including the not-compiled-in reason, which the
    // host platform would otherwise decide for the test.

    #[test]
    fn a_build_without_dictation_says_so_rather_than_looking_broken() {
        let blocked = dictation_gate(false, true, false, true).unwrap_err();
        assert_eq!(blocked, DictationBlocked::NotCompiledIn);
        let message = blocked.message();
        assert!(
            message.contains("this build"),
            "the message must say the build lacks it, not that voice is off: {message}"
        );
    }

    #[test]
    fn not_compiled_in_outranks_the_other_reasons() {
        // Every other message would send someone to edit voice.toml or wait for
        // models, neither of which can help a build with no dictation in it.
        assert_eq!(
            dictation_gate(false, false, true, false).unwrap_err(),
            DictationBlocked::NotCompiledIn
        );
    }

    #[test]
    fn voice_off_is_reported_when_the_build_has_dictation() {
        assert_eq!(
            dictation_gate(true, false, false, true).unwrap_err(),
            DictationBlocked::VoiceOff
        );
    }

    #[test]
    fn a_second_press_while_capturing_is_reported_as_already_listening() {
        assert_eq!(
            dictation_gate(true, true, true, true).unwrap_err(),
            DictationBlocked::AlreadyListening
        );
    }

    #[test]
    fn a_press_before_the_models_finish_loading_is_reported_as_loading() {
        assert_eq!(
            dictation_gate(true, true, false, false).unwrap_err(),
            DictationBlocked::StillLoading
        );
    }

    #[test]
    fn a_ready_session_on_a_supported_build_is_allowed() {
        assert!(dictation_gate(true, true, false, true).is_ok());
    }

    #[test]
    fn every_blocked_reason_has_a_non_empty_message() {
        for blocked in [
            DictationBlocked::NotCompiledIn,
            DictationBlocked::VoiceOff,
            DictationBlocked::AlreadyListening,
            DictationBlocked::StillLoading,
        ] {
            assert!(!blocked.message().is_empty());
        }
    }

    #[test]
    fn voice_mode_defaults_to_off() {
        assert_eq!(VoiceMode::default(), VoiceMode::Off);
    }

    #[test]
    fn default_config_is_off_and_not_embedded() {
        let cfg = VoiceConfig::default();
        assert_eq!(cfg.mode, VoiceMode::Off);
        assert!(!cfg.embedded_enabled());
    }

    #[test]
    fn empty_toml_parses_to_off() {
        // A present-but-empty file must not enable voice or panic.
        let cfg: VoiceConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.mode, VoiceMode::Off);
    }

    #[test]
    fn mode_embedded_parses_and_enables() {
        let cfg: VoiceConfig = toml::from_str(r#"mode = "embedded""#).unwrap();
        assert_eq!(cfg.mode, VoiceMode::Embedded);
        assert!(cfg.embedded_enabled());
    }

    #[test]
    fn mode_daemon_is_no_longer_a_valid_value() {
        // The inert `daemon` toggle value was removed (refactor #5): the TUI has
        // no daemon voice *client*, so `daemon` only ever meant `off`. It's now a
        // parse error, which `load()` turns into off + a warning.
        assert!(toml::from_str::<VoiceConfig>(r#"mode = "daemon""#).is_err());
    }

    #[test]
    fn mode_off_parses_explicitly() {
        let cfg: VoiceConfig = toml::from_str(r#"mode = "off""#).unwrap();
        assert_eq!(cfg.mode, VoiceMode::Off);
        assert!(!cfg.embedded_enabled());
    }

    #[test]
    fn unknown_mode_is_a_parse_error_not_a_silent_default() {
        // A typo'd mode should surface as a parse error (then `load()` falls
        // back to off + warns) rather than quietly meaning something.
        let err = toml::from_str::<VoiceConfig>(r#"mode = "embeded""#);
        assert!(err.is_err());
    }

    #[test]
    fn partial_config_keeps_section_defaults() {
        // Toggling voice on shouldn't force the user to spell out every model
        // path; the nested module sections fall back to their own defaults.
        // A legacy `play_replies = true` line (the key was removed in refactor
        // #5) is harmlessly ignored rather than failing the parse.
        let cfg: VoiceConfig = toml::from_str(
            r#"
                mode = "embedded"
                play_replies = true
                [tts]
                backend = "piper"
            "#,
        )
        .unwrap();
        assert!(cfg.embedded_enabled());
        assert_eq!(cfg.tts.backend, "piper");
        // Untouched sections still have sensible defaults.
        assert_eq!(cfg.stt.language, "en");
        assert_eq!(cfg.audio.input_device, "default");
    }

    #[test]
    fn unknown_top_level_keys_do_not_break_parse() {
        // Forward-compat: a newer config key shouldn't fail an older binary.
        let cfg: VoiceConfig = toml::from_str("mode = \"embedded\"\nfuture_knob = 42\n").unwrap();
        assert!(cfg.embedded_enabled());
    }

    // The `into_speakable_sentences` chunking tests moved with the function to
    // the shared `adele-voice-client-common` crate (desktop-assistant#274).

    // --- Narration queue (TUI-11) ---
    //
    // Reply narration and `say_this` asides both speak through ONE serialized
    // queue, so two utterances never interleave sentence-by-sentence on the
    // shared sink. The serialization invariant is testable without real audio
    // by driving the shared loop with a stub `speak` that records start/end
    // ordering and flags any overlap.

    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::sync::mpsc::unbounded_channel;
    use tokio::sync::oneshot;

    /// Records each utterance's start/end and whether any two overlapped.
    #[derive(Default)]
    struct SpeakRecorder {
        active: usize,
        overlapped: bool,
        log: Vec<String>,
    }

    #[tokio::test]
    async fn narration_queue_serializes_overlapping_utterances() {
        // Two utterances are queued back to back while the first is still
        // "playing"; the loop must finish the first before starting the second
        // (no overlap) and preserve submission order.
        let recorder = Arc::new(StdMutex::new(SpeakRecorder::default()));
        let (tx, rx) = unbounded_channel::<String>();

        // A barrier so the first utterance can be held "in flight" until both
        // requests are queued, proving the second waits rather than racing.
        let (release_first_tx, release_first_rx) = oneshot::channel::<()>();
        let release_first = Arc::new(StdMutex::new(Some(release_first_rx)));

        let rec = Arc::clone(&recorder);
        let loop_handle = tokio::spawn(async move {
            run_narration_loop(rx, move |text| {
                let rec = Arc::clone(&rec);
                let release_first = Arc::clone(&release_first);
                async move {
                    {
                        let mut r = rec.lock().unwrap();
                        if r.active > 0 {
                            r.overlapped = true;
                        }
                        r.active += 1;
                        r.log.push(format!("start:{text}"));
                    }
                    // The first utterance blocks on the barrier; later ones run
                    // immediately. If serialization is broken the second would
                    // start while the first is parked here. Take the receiver
                    // out (dropping the guard) BEFORE awaiting so the closure's
                    // future stays `Send`.
                    let held = release_first.lock().unwrap().take();
                    if let Some(rx) = held {
                        let _ = rx.await;
                    }
                    {
                        let mut r = rec.lock().unwrap();
                        r.active -= 1;
                        r.log.push(format!("end:{text}"));
                    }
                }
            })
            .await;
        });

        tx.send("first".to_string()).unwrap();
        tx.send("second".to_string()).unwrap();

        // Give the loop a chance to (incorrectly) start the second before the
        // first is released.
        tokio::task::yield_now().await;
        release_first_tx.send(()).unwrap();

        drop(tx); // closes the channel so the loop exits
        loop_handle.await.unwrap();

        let r = recorder.lock().unwrap();
        assert!(!r.overlapped, "utterances must never overlap on the sink");
        assert_eq!(
            r.log,
            vec!["start:first", "end:first", "start:second", "end:second"],
            "utterances must play fully, in submission order"
        );
    }

    #[tokio::test]
    async fn narration_loop_exits_when_the_sender_is_dropped() {
        // The queue task is long-lived but must terminate cleanly when the app
        // drops its sender (shutdown), not hang forever.
        let (tx, rx) = unbounded_channel::<String>();
        let handle = tokio::spawn(run_narration_loop(rx, |_text| async {}));
        drop(tx);
        // Must return promptly; the test harness would hang otherwise.
        handle.await.unwrap();
    }
}
