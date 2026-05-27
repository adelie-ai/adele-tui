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

## License

GNU Affero General Public License v3.0 or later (`AGPL-3.0-or-later`).
