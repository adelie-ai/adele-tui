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

use std::sync::Arc;

use adele_voice_module::config::{AudioConfig, SttConfig, TtsConfig, VadConfig};
use adele_voice_module::{Dictation, Speaker, TtsBackend, build_dictation, build_speaker};
use adele_voice_stt_whisper::WhisperStt;
use adele_voice_vad_silero::SileroVad;
use serde::Deserialize;
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

/// The embedded voice pipeline: a one-shot dictation capture plus a speaker.
///
/// The `Dictation` is behind a `Mutex` because each press dictates on a spawned
/// task (capture is blocking-ish — it opens the mic and waits for speech), and
/// the lock both gives the task ownership and prevents two presses from opening
/// the mic at once. `Speaker` is cheap to clone (shared `Arc` handles), so the
/// playback task gets its own clone.
pub struct VoiceSession {
    dictation: Arc<Mutex<Dictation<SileroVad, WhisperStt>>>,
    speaker: Speaker<TtsBackend>,
}

impl VoiceSession {
    /// Wire the embedded pipeline from config. Loads the VAD/STT models and the
    /// TTS backend (local-first Kokoro→Piper fallback), so this is the expensive
    /// step; call it once, lazily, on the first dictate.
    ///
    /// Whether replies are *spoken* is no longer a property of the session: the
    /// per-conversation `Ctrl+S` speech toggle (adele-tui#73) governs that. The
    /// session just supplies the `Speaker`; the caller decides per conversation
    /// whether to use it.
    pub async fn build(cfg: &VoiceConfig) -> anyhow::Result<Self> {
        // Build the speaker first so the dictation can share its output sink as
        // an echo guard (half-duplex): the mic then won't capture and transcribe
        // Adele's own TTS playback. The stored `speaker` is the one playback runs
        // through, so the guard watches the right sink.
        let speaker = build_speaker(&cfg.tts, &cfg.audio).await;
        let dictation =
            build_dictation(&cfg.audio, &cfg.vad, &cfg.stt)?.with_echo_guard(speaker.sink());
        Ok(Self {
            dictation: Arc::new(Mutex::new(dictation)),
            speaker,
        })
    }

    /// A clonable handle to the dictation capture, for spawning a capture task.
    pub fn dictation(&self) -> Arc<Mutex<Dictation<SileroVad, WhisperStt>>> {
        Arc::clone(&self.dictation)
    }

    /// A speaker clone for spawning a playback task.
    pub fn speaker(&self) -> Speaker<TtsBackend> {
        self.speaker.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
