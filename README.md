# Adele TUI

Terminal UI client for the [Adelie AI Platform](https://github.com/adelie-ai/desktop-assistant),
built with [ratatui](https://ratatui.rs/).

Connects to the `desktop-assistant-daemon` over WebSocket or D-Bus and streams
chat, tool calls, and background tasks.

## What it does today

- **Streaming chat** with markdown rendering and syntax-highlighted fenced
  code blocks. Sidebar lists conversations; `Ctrl+B` toggles it.
- **Per-conversation model selector** (`Ctrl+M`) and **connection switcher**
  (`F2`) — no daemon restart required.
- **Per-conversation personality picker** (`Ctrl+R`) — pin any of the seven
  traits (professionalism, warmth, directness, enthusiasm, humor, sarcasm,
  pretentiousness) for the active conversation; unpinned traits stay `Global`
  (inherit your default disposition).
- **Connection profiles**, **Connections view**, and **Purposes view** for
  CRUD over LLM provider configs and assigning a connection/model/effort to
  each purpose (chat, background, vector).
- **OAuth2 + PKCE** authentication flow and credentials stored in the system
  keyring (libsecret / kwallet via the OS).
- **Knowledge base browser/editor** for the daemon's built-in KB.
- **Process manager pane** (`Ctrl+P`) — inline overlay listing background
  tasks streamed from the daemon via `SignalEvent::Task*`; status-bar
  `(N running)` badge surfaces activity from anywhere in the UI.
- **Debug view toggle** for tool and system messages, **conversation rename**,
  **auto-reconnect** with backoff, and a **keybind hint toolbar** at the
  bottom of the window.
- **Embedded voice** (optional, off by default) — `Ctrl+G` dictates a prompt
  (mic → speech-to-text) straight into the composer and sends it; replies can
  be spoken back. Runs in-process with **no voice daemon and no wake word**.

## Voice (embedded dictation + playback)

The TUI can do in-app dictation and reply playback by embedding the
[`adele-voice-module`](https://github.com/adelie-ai/voice) library — entirely
in-process, reaching only the daemon/orchestrator the TUI already talks to. No
voice daemon and no wake word: those stay in the voice *service* (run the voice
daemon if you want hands-free "Hey Adele").

It is **off by default**. Enable it in `~/.config/adele-tui/voice.toml`:

```toml
# off (default) | embedded | daemon
#   embedded — in-process dictation + playback (this feature)
#   daemon   — reserved; the TUI has no daemon voice client, so it acts as off
mode = "embedded"

# Speak assistant replies aloud after they finish streaming.
play_replies = false

# All sections below are optional; each falls back to the same defaults the
# voice daemon uses (models under $XDG_DATA_HOME/adele-voice/models).
[tts]
backend = "kokoro"   # kokoro (local, default) | piper (local) | polly (AWS, billable)
```

Then press **`Ctrl+G`** to dictate: speak after the "Listening…" indicator
appears; the transcript is placed in the prompt and sent. The Silero VAD/Whisper
models (and the TTS backend) load once in the background at startup; until they
are ready, `Ctrl+G` reports that voice is still loading. If the models are not
provisioned the TUI just reports voice is unavailable and otherwise runs
normally — voice is a convenience, never load-bearing.

## Requirements

- Rust toolchain (edition 2024, Rust 1.85+)
- A running `desktop-assistant-daemon` instance
- For D-Bus transport, a Linux session bus (`DBUS_SESSION_BUS_ADDRESS`)

## Build and install

```sh
cargo build
cargo install --path .   # installs `adele` to ~/.cargo/bin
```

## Run

```sh
adele
```

### CLI options

| Flag | Env var | Default | Description |
|------|---------|---------|-------------|
| `--transport` | `DESKTOP_ASSISTANT_TUI_TRANSPORT` | `ws` | Transport: `ws` or `dbus` |
| `--ws-url` | `DESKTOP_ASSISTANT_TUI_WS_URL` | `ws://127.0.0.1:11339/ws` | WebSocket URL |
| `--ws-jwt` | `DESKTOP_ASSISTANT_TUI_WS_JWT` | | Direct JWT token |
| `--ws-login-username` | `DESKTOP_ASSISTANT_TUI_WS_LOGIN_USERNAME` | | Login username |
| `--ws-login-password` | `DESKTOP_ASSISTANT_TUI_WS_LOGIN_PASSWORD` | | Login password |
| `--ws-subject` | `DESKTOP_ASSISTANT_TUI_WS_SUBJECT` | `desktop-tui` | JWT subject |
| `--dbus-service` | `DESKTOP_ASSISTANT_DBUS_SERVICE` | `org.desktopAssistant` | D-Bus service name |

## Test

```sh
cargo test
```

## Architecture

The shared protocol types and transport clients live in
[`adelie-ai/desktop-assistant`](https://github.com/adelie-ai/desktop-assistant)
under `crates/api-model` and `crates/client-common`. This repo pulls them in
as git dependencies so all Adele clients (TUI, GTK, KDE) share one source of
truth. `Cargo.lock` pins the exact revision; `cargo update` advances it.

The embedded voice module is **path-depended** from a sibling `voice` checkout
(`../voice/crates/module`), mirroring how the GTK client path-deps
`desktop-assistant`. Clone [`adelie-ai/voice`](https://github.com/adelie-ai/voice)
next to this repo to build with voice; revisit a git-dep/publish once the
module's API stabilizes (see voice#34).

## License

GNU Affero General Public License v3.0 or later (`AGPL-3.0-or-later`).
