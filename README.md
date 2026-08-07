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
- **MCP servers admin panel** (`F5`) — list, enable/disable, add, edit, and
  remove the daemon's Model Context Protocol servers (local stdio or remote
  HTTP, with bearer-token or OAuth service-account auth), and enable/disable the
  client's compiled-in **built-in** servers (per-surface; applies on restart).
- **OAuth2 + PKCE** authentication flow and credentials stored in the system
  keyring (libsecret / kwallet via the OS).
- **Knowledge base browser/editor** for the daemon's built-in KB.
- **Process manager pane** (`Ctrl+P`) — inline overlay listing background
  tasks streamed from the daemon via `SignalEvent::Task*`; status-bar
  `(N running)` badge surfaces activity from anywhere in the UI.
- **Debug view toggle** for tool and system messages, **conversation rename**,
  **auto-reconnect** with backoff, and a **keybind hint toolbar** at the
  bottom of the window.
- **Share device info toggle** (`Ctrl+O`, on by default) - controls whether the
  client tells the assistant your name, username, home folder, hostname,
  timezone, and OS at connect so it can personalize; off means nothing about
  your device is sent. Persisted in `settings.json`; applies on the next
  (re)connect. Scriptable via `adele config set share-client-context on|off`.
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
# off (default) | embedded
#   embedded — in-process dictation + playback (this feature)
mode = "embedded"

# Whether replies are spoken is a per-conversation choice (Ctrl+S cycles the
# Adele output level), not a config setting.

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
adele                       # interactive TUI (default)
adele exec "summarize X"    # one-shot: send a prompt, print the reply, exit
adele config mcp list       # scriptable config, no daemon needed
```

`adele` with no subcommand launches the interactive TUI (the global options
below still apply). The subcommands are:

### `exec <PROMPT>` (alias: `prompt`)

Send a single prompt non-interactively, stream the reply to stdout, and exit —
no TUI. Client-hosted MCP tools (`client-mcp.toml`) still work, so this can
drive a local or remote (k8s) brain end to end. This replaces the old
`--prompt <TEXT>` flag, which still works (hidden, deprecated) for back-compat.

### `config` — scriptable config management

The non-interactive twin of the `F5` MCP-servers panel. It loads, mutates, and
saves the shared client-MCP config (`$XDG_CONFIG_HOME/adele/client-mcp.toml`)
directly, with no daemon connection:

```sh
adele config path                     # print the config file location
adele config show [--section mcp]     # print the effective config as TOML
adele config mcp list                 # list client-MCP servers + built-ins
adele config mcp add-server <NAME> --command <CMD> [--arg <A>]... \
    [--namespace <NS>] [--surface <S>]... [--enabled]
adele config mcp remove-server <NAME>
adele config mcp enable  <NAME> [--surface tui]
adele config mcp disable <NAME> [--surface tui]
adele config get share-client-context           # print the current value
adele config set share-client-context on|off    # share device info (default on)
```

**Share device info** (`share-client-context`) is the scriptable twin of the
`Ctrl+O` toggle: it persists to `settings.json`, and `off` stops the client from
sending your name, username, home folder, hostname, timezone, and OS to the
assistant at connect. Values are lenient (`on`/`off`, `true`/`false`, `yes`/`no`,
`1`/`0`); the change applies on the next (re)connect.

`config mcp list` also shows the compiled-in **built-in** servers (with tool
counts) and their status: `disabled (config)` when you turned the built-in off
for the surface, `overridden by client-MCP '<name>'` when a same-named,
surface-enabled client-MCP server shadows it, else `active`.

**Enabling/disabling a built-in** is per-surface (the TUI uses the `tui`
surface):

```sh
adele config mcp disable web            # turn the built-in `web` off for tui
adele config mcp enable  web            # turn it back on
adele config mcp disable web --surface gtk   # another surface, independently
```

`disable` adds the built-in to that surface's `disabled_builtins` list; `enable`
removes it. The change is written to `client-mcp.toml` and takes effect on the
**next client launch** (the running in-process host is not restarted). A name
that is *both* a defined client-MCP server and a built-in resolves to the
server (the server shadows the built-in), so `enable`/`disable` toggles the
server, not the built-in.

You can do the same from the interactive **`F5` panel**: the cursor moves over
the built-in rows too, and `Space`/`t` toggles the selected built-in. The panel
writes the same per-surface config and shows a green "(applies on restart)"
note; a disabled built-in renders dimmed with its reason. Edit/remove/sign-in in
the panel remain daemon-server operations.

Daemon-hosted MCP servers are out of scope for the `config` CLI — manage those
from the interactive `F5` panel.

### Global options

Given before any subcommand (they apply to the TUI and `exec` alike):

| Flag | Env var | Default | Description |
|------|---------|---------|-------------|
| `--transport` | `DESKTOP_ASSISTANT_TUI_TRANSPORT` | `local` | Transport: `local` (UDS), `ws`, or `dbus` |
| `--socket [PATH]` | | | Connect over the local Unix socket (optional path override) |
| `--ws [URL]` | | | Connect over WebSocket (optional URL override) |
| `--ws-url` | `DESKTOP_ASSISTANT_TUI_WS_URL` | `wss://127.0.0.1:11339/ws` | WebSocket URL |
| `--ws-jwt` | `DESKTOP_ASSISTANT_TUI_WS_JWT` | | Direct JWT token |
| `--ws-login-username` | `DESKTOP_ASSISTANT_TUI_WS_USERNAME` | | Login username |
| `--ws-login-password` | `DESKTOP_ASSISTANT_TUI_WS_PASSWORD` | | Login password |
| `--ws-subject` | `DESKTOP_ASSISTANT_TUI_WS_SUBJECT` | `desktop-tui` | JWT subject |
| `-v`, `--verbose` | | | Verbose logging to stderr (`-v`/`-vv`/`-vvv`) |

## Logging

`adele` is silent by default: with no `-v` and no `RUST_LOG`, it writes
nothing to either stream. Logs always go to stderr, never stdout — the
headless `exec` (and deprecated `--prompt`) path writes the model reply to
stdout, and a log line there would corrupt it.

- `-v` / `-vv` / `-vvv` raise the level for `adele` and its client crates
  (`info` / `debug` / `trace`); everything else stays at `warn`.
- `RUST_LOG`, when set, overrides the level entirely (standard
  `tracing_subscriber::EnvFilter` syntax, e.g. `RUST_LOG=debug` or
  `RUST_LOG=info,adele=trace`).

### OpenTelemetry export (`otel` feature)

Off by default: `cargo build` and `cargo install --path .` resolve no
`opentelemetry` crate. Build with `cargo build --features otel` (or
`cargo install --path . --features otel`) to also export traces, metrics,
and log records to a collector over OTLP, through the shared
[`adelie-telemetry`](https://github.com/adelie-ai/adelie-telemetry) crate.
`RUST_LOG` governs the exported log records too — there is no separate
filter for the collector.

Configuration is entirely through the standard `OTEL_*` environment
variables; `adele` adds no flags or variables of its own:

| Variable | Effect |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Collector endpoint for all three signals |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | Endpoint for traces, overrides the generic one |
| `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | Endpoint for metrics, overrides the generic one |
| `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | Endpoint for log records, overrides the generic one |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `grpc`, `http/protobuf`, or `http/json` (per-signal forms exist too) |
| `OTEL_EXPORTER_OTLP_HEADERS` | Extra headers, as `key=value,key=value` (per-signal forms exist too) |
| `OTEL_EXPORTER_OTLP_TIMEOUT` | Export timeout in milliseconds (per-signal forms exist too) |
| `OTEL_EXPORTER_OTLP_COMPRESSION` | `gzip` or `zstd` |
| `OTEL_RESOURCE_ATTRIBUTES` | Extra resource attributes, as `key=value,key=value` |

See the [`adelie-telemetry` README](https://github.com/adelie-ai/adelie-telemetry)
for the complete variable reference, the transport tradeoffs (`grpc` vs.
`http/protobuf`), and what an `otel` build costs in dependencies and binary
size.

`adele` opens a span around connecting to the daemon, around advertising its
client tools, and around one turn's reply streaming, all children of a
per-turn root span. None of them ever carry the prompt or reply text — only
ids, counts, and durations. These spans stay local to this process for now:
carrying them to the daemon as a `traceparent` so one trace covers a whole
turn end to end is tracked separately (`desktop-assistant#1152`).

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
