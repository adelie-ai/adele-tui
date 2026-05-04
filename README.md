# Adele TUI

Terminal UI client for the Adelie Desktop Assistant, built with [ratatui](https://ratatui.rs/).

Connects to the `desktop-assistant-daemon` over WebSocket or D-Bus to provide a full-featured chat interface in the terminal.

## Requirements

- Rust toolchain (edition 2024, Rust 1.85+)
- A running `desktop-assistant-daemon` instance

For D-Bus transport, a Linux session bus is required (`DBUS_SESSION_BUS_ADDRESS`).

## Build

```sh
cargo build
```

## Install

```sh
cargo install --path .
```

This installs the `adele` binary to `~/.cargo/bin/`.

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
under `crates/api-model` and `crates/client-common`. This repo pulls
them in as git dependencies so all Adelie clients (TUI, GTK, KDE)
share one source of truth. `Cargo.lock` pins the exact revision;
`cargo update` advances it.

## License

Licensed under **GNU Affero General Public License v3.0 or later** (`AGPL-3.0-or-later`).
