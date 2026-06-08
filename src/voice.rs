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
//! the daemon uses the same shapes. The [`VoiceMode`] toggle gates everything:
//! it defaults to [`VoiceMode::Off`] so a TUI with no voice config behaves
//! exactly as before. [`VoiceMode::Daemon`] is accepted but inert here — the TUI
//! has no daemon client, so it is treated as "off"; the embedded path is the
//! capability this module adds.
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

/// How the TUI sources voice. Defaults to [`VoiceMode::Off`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceMode {
    /// Voice disabled (the default — nothing loads, no mic access).
    #[default]
    Off,
    /// In-process dictation + playback via the embedded module. No daemon.
    Embedded,
    /// Defer to the system voice daemon. The TUI has no daemon voice client,
    /// so this is currently treated the same as `Off` (inert). Reserved so
    /// the toggle's vocabulary matches the epic without implying a capability
    /// the TUI doesn't yet have.
    Daemon,
}

/// User-facing voice configuration, parsed from `voice.toml`.
///
/// The four nested sections are the module's own config types, so the embedded
/// builders consume them directly. Each defaults independently, so a partial
/// file (e.g. just `mode = "embedded"`) still parses.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    /// The capability toggle (`off` | `embedded` | `daemon`).
    pub mode: VoiceMode,
    /// Seeds the per-conversation speech toggle's default (adele-tui#73). When
    /// `true`, new conversations start with speech ON (so existing
    /// `play_replies = true` users keep audio), but speech is now an in-app
    /// per-conversation control (`Ctrl+S`), no longer a global always-on gate.
    /// Only meaningful in `embedded` mode. Defaults `false` (speech off).
    pub play_replies: bool,
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
    /// per-conversation `Ctrl+S` speech toggle (adele-tui#73) governs that, with
    /// its default seeded from `cfg.play_replies`. The session just supplies the
    /// `Speaker`; the caller decides per conversation whether to use it.
    pub async fn build(cfg: &VoiceConfig) -> anyhow::Result<Self> {
        let dictation = build_dictation(&cfg.audio, &cfg.vad, &cfg.stt)?;
        let speaker = build_speaker(&cfg.tts, &cfg.audio).await;
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
        assert!(!cfg.play_replies);
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
    fn mode_daemon_parses_but_is_inert() {
        // `daemon` is a valid toggle value but the TUI has no daemon voice
        // client, so it must NOT turn on the embedded pipeline.
        let cfg: VoiceConfig = toml::from_str(r#"mode = "daemon""#).unwrap();
        assert_eq!(cfg.mode, VoiceMode::Daemon);
        assert!(!cfg.embedded_enabled());
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
        assert!(cfg.play_replies);
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
}
