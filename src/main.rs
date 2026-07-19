//! `adele` terminal UI client binary.
//!
//! Parses CLI arguments, establishes the transport connection to the Adelie
//! daemon, and runs the interactive TUI event loop (chat plus the knowledge
//! base, connections, and purposes management screens).

use std::io;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser, parser::ValueSource};
use crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use desktop_assistant_client_common::{
    AssistantClient, ConnectionConfig, Connector, ConversationDetail, ConversationSummary,
    SignalEvent, TransportClient, TransportMode,
};
use futures::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    time::{Instant, sleep_until},
};

// The binary is a thin shim over the `adele` library crate (refactor #3): every
// screen/widget/helper module lives in `lib.rs` and is reached via `adele::`,
// not re-declared with `mod`, so there is ONE module tree instead of two that
// compile twice and silently drift (the old `mod` list had already lost
// `personality_selector` + `voice_client` from `lib.rs`). Only the orchestration
// wiring them together (the `run` event loop, its RPC/signal helpers, the voice
// plumbing types) lives in this file.
use adele::app::{AdeleOutput, App, InputMode, ScreenRequest};
use adele::in_flight::InFlight;
use adele::keys::{Action, handle_key_event};
use adele::picker::PickerOutcome;
use adele::profile::ProfileStore;
use adele::settings::Settings;
use adele::voice::{VoiceConfig, VoiceSession};
use adele::voice_client::VoiceController;
use adele::{
    client_tools, connections, credentials, kb, mcp, model_selector, personality_selector, picker,
    purposes, screen, ui, voice,
};
use client_ui_common::{Effect, UiMessage};
use desktop_assistant_api_model::ClientToolRegistration;
use desktop_assistant_client_common::mcp_host::{
    ClientMcpConfig, McpHost, default_client_mcp_path, dispatch_client_tool_call,
    merge_registrations,
};

const DEFAULT_WS_URL: &str = desktop_assistant_client_common::config::DEFAULT_WS_URL;
const DEFAULT_WS_SUBJECT: &str = desktop_assistant_client_common::config::DEFAULT_WS_SUBJECT;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lower")]
enum CliTransportMode {
    /// Local Unix domain socket (the default).
    Local,
    Ws,
    Dbus,
}

/// The `adele` command line: global connection/verbosity options (flattened,
/// parsed at the top level so they apply to the interactive TUI and the `exec`
/// one-shot alike) plus an optional subcommand.
///
/// No subcommand -> the interactive TUI (unchanged). `exec <PROMPT>` -> a
/// one-shot headless turn (the preferred replacement for the deprecated
/// `--prompt` flag). `config …` -> scriptable, daemon-free management of the
/// shared client-MCP config, the non-interactive twin of the `F5` panel.
#[derive(Debug, Parser)]
#[command(name = "adele")]
struct CliArgs {
    #[command(flatten)]
    global: Global,
    #[command(subcommand)]
    command: Option<Command>,
}

/// Connection, credential, and verbosity options shared by the interactive and
/// `exec` paths. Flattened into [`CliArgs`], so they are given before any
/// subcommand (`adele --ws exec "hi"`) and read the same way whether or not a
/// subcommand is present.
#[derive(Debug, clap::Args)]
struct Global {
    /// Transport used when neither --socket nor --ws is given. Defaults to
    /// the daemon's local Unix socket.
    #[arg(
        long,
        env = "DESKTOP_ASSISTANT_TUI_TRANSPORT",
        value_enum,
        default_value_t = CliTransportMode::Local
    )]
    transport: CliTransportMode,
    /// Connect over the daemon's local Unix socket. An optional PATH overrides
    /// the default ($XDG_RUNTIME_DIR/adelie/sock). Mutually exclusive with --ws.
    #[arg(long, value_name = "PATH", num_args = 0..=1, conflicts_with = "ws")]
    socket: Option<Option<PathBuf>>,
    /// Connect over WebSocket. An optional URL overrides --ws-url. Mutually
    /// exclusive with --socket.
    #[arg(long, value_name = "URL", num_args = 0..=1, conflicts_with = "socket")]
    ws: Option<Option<String>>,
    #[arg(
        long = "ws-url",
        env = "DESKTOP_ASSISTANT_TUI_WS_URL",
        default_value = DEFAULT_WS_URL
    )]
    ws_url: String,
    #[arg(
        long = "ws-subject",
        env = "DESKTOP_ASSISTANT_TUI_WS_SUBJECT",
        default_value = DEFAULT_WS_SUBJECT
    )]
    ws_subject: String,
    /// Deprecated: use the `exec <TEXT>` subcommand instead. Send a single
    /// prompt non-interactively, print the reply to stdout, and exit — no TUI.
    /// Kept as a hidden back-compat alias; `exec` is the preferred form.
    #[arg(long, value_name = "TEXT", hide = true)]
    prompt: Option<String>,
    /// Bearer (JWT) for the WebSocket transport, skipping interactive login.
    #[arg(long = "ws-jwt", env = "DESKTOP_ASSISTANT_TUI_WS_JWT")]
    ws_jwt: Option<String>,
    /// Username for WebSocket password login (with --ws-login-password).
    #[arg(long = "ws-login-username", env = "DESKTOP_ASSISTANT_TUI_WS_USERNAME")]
    ws_login_username: Option<String>,
    /// Password for WebSocket password login. Prefer the env var so it doesn't
    /// land in shell history / the process list.
    #[arg(long = "ws-login-password", env = "DESKTOP_ASSISTANT_TUI_WS_PASSWORD")]
    ws_login_password: Option<String>,
    /// Enable verbose logging (`-v` info, `-vv` debug, `-vvv` trace). Logs go to
    /// stderr so a headless `exec` run pipes cleanly to a file for scripting
    /// and debugging. `RUST_LOG`, when set, overrides the level entirely.
    #[arg(short = 'v', long, action = clap::ArgAction::Count)]
    verbose: u8,
}

/// Top-level subcommands. Absent -> the interactive TUI.
#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Send a single prompt non-interactively, print the reply, and exit — no
    /// TUI. Client-hosted MCP tools (client-mcp.toml) still work, so this can
    /// drive a local or remote (k8s) brain end to end. The preferred form of
    /// the old `--prompt` flag.
    #[command(alias = "prompt")]
    Exec {
        /// The prompt text to send.
        prompt: String,
    },
    /// Scriptable, daemon-free management of the shared client-MCP config — the
    /// non-interactive twin of the interactive `F5` panel.
    Config(ConfigArgs),
}

/// `config` subcommand group.
#[derive(Debug, clap::Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

/// `config <…>` operations.
#[derive(Debug, clap::Subcommand)]
enum ConfigCommand {
    /// Print the client-MCP config file location.
    Path,
    /// Print the effective config (currently the client-MCP config).
    Show {
        /// Restrict to a section. Only `mcp` is supported today (the default).
        #[arg(long)]
        section: Option<String>,
    },
    /// Manage client-hosted MCP servers.
    Mcp(McpArgs),
}

/// `config mcp` subcommand group.
#[derive(Debug, clap::Args)]
struct McpArgs {
    #[command(subcommand)]
    command: McpCommand,
}

/// `config mcp <…>` operations. These act on the client-side `client-mcp.toml`
/// only; daemon-hosted MCP servers are out of scope for this non-interactive
/// surface (manage those from the interactive `F5` panel).
#[derive(Debug, clap::Subcommand)]
enum McpCommand {
    /// List client-MCP servers (for the tui surface) and the compiled-in
    /// built-ins, with status.
    List,
    /// Define (or replace) a stdio client-MCP server.
    AddServer {
        /// Server name (also the default tool-namespace prefix).
        name: String,
        /// Command to spawn (stdio transport).
        #[arg(long)]
        command: String,
        /// A launch argument; repeat the flag for multiple. Hyphen-leading
        /// values are accepted (e.g. `--arg --root=/data`), since MCP server
        /// arguments commonly start with `--`.
        #[arg(long = "arg", value_name = "A", allow_hyphen_values = true)]
        arg: Vec<String>,
        /// Optional tool-namespace prefix.
        #[arg(long)]
        namespace: Option<String>,
        /// Surface(s) to enable it for when --enabled is given (default: tui).
        /// Repeat the flag for multiple.
        #[arg(long)]
        surface: Vec<String>,
        /// Also enable the server for the given surface(s) now.
        #[arg(long)]
        enabled: bool,
    },
    /// Remove a client-MCP server definition.
    RemoveServer {
        /// Server name.
        name: String,
    },
    /// Enable a client-MCP server for a surface.
    Enable {
        /// Server name.
        name: String,
        /// Surface to enable it for.
        #[arg(long, default_value = "tui")]
        surface: String,
    },
    /// Disable a client-MCP server for a surface.
    Disable {
        /// Server name.
        name: String,
        /// Surface to disable it for.
        #[arg(long, default_value = "tui")]
        surface: String,
    },
}

impl From<Global> for ConnectionConfig {
    fn from(global: Global) -> Self {
        let ws_url = {
            let trimmed = global.ws_url.trim();
            if trimmed.is_empty() {
                DEFAULT_WS_URL.to_string()
            } else {
                trimmed.to_string()
            }
        };

        let ws_subject = {
            let trimmed = global.ws_subject.trim();
            if trimmed.is_empty() {
                DEFAULT_WS_SUBJECT.to_string()
            } else {
                trimmed.to_string()
            }
        };

        // `--socket` and `--ws` are explicit selectors that override the
        // (always-defaulted) `--transport`. clap makes them mutually
        // exclusive, so at most one is `Some` here.
        let (transport_mode, socket_path, ws_url) = if let Some(path) = global.socket {
            (TransportMode::Uds, path, ws_url)
        } else if let Some(url) = global.ws {
            let resolved = match url {
                Some(u) if !u.trim().is_empty() => u.trim().to_string(),
                _ => ws_url,
            };
            (TransportMode::Ws, None, resolved)
        } else {
            let mode = match global.transport {
                CliTransportMode::Local => TransportMode::Uds,
                CliTransportMode::Ws => TransportMode::Ws,
                CliTransportMode::Dbus => TransportMode::Dbus,
            };
            (mode, None, ws_url)
        };

        Self {
            transport_mode,
            ws_url,
            ws_jwt: global.ws_jwt,
            ws_login_username: global.ws_login_username,
            ws_login_password: global.ws_login_password,
            ws_subject,
            socket_path,
            // Local UDS authenticates by kernel peer-cred (desktop-assistant#407):
            // no token is minted — see `Profile::to_connection_config`.
            ..Default::default()
        }
    }
}

/// Dispatch a `config` subcommand: pure, daemon-free config management that
/// loads/mutates/saves the shared `client-mcp.toml` and prints to stdout. Runs
/// before any TUI/terminal or daemon connection setup. Built-in server info
/// (name + advertised tool count) is resolved from the compiled-in set so
/// `mcp list`/`enable`/`disable` can report built-ins without a daemon.
fn run_config(command: &ConfigCommand) -> Result<()> {
    use adele::config_cmd as cc;

    let path = default_client_mcp_path();
    let mut out = io::stdout().lock();
    match command {
        ConfigCommand::Path => cc::config_path(&path, &mut out),
        ConfigCommand::Show { section } => cc::config_show(&path, section.as_deref(), &mut out),
        ConfigCommand::Mcp(mcp) => run_config_mcp(&path, &mcp.command, &mut out),
    }
}

/// Dispatch a `config mcp` subcommand against the client-MCP config at `path`.
fn run_config_mcp(
    path: &std::path::Path,
    command: &McpCommand,
    out: &mut impl std::io::Write,
) -> Result<()> {
    use adele::config_cmd as cc;

    match command {
        McpCommand::List => cc::mcp_list(path, &builtin_infos(), cc::DEFAULT_SURFACE, out),
        McpCommand::AddServer {
            name,
            command,
            arg,
            namespace,
            surface,
            enabled,
        } => {
            let surfaces = if surface.is_empty() {
                vec![cc::DEFAULT_SURFACE.to_string()]
            } else {
                surface.clone()
            };
            cc::mcp_add_server(
                path,
                name,
                command,
                arg,
                namespace.as_deref(),
                &surfaces,
                *enabled,
                out,
            )
        }
        McpCommand::RemoveServer { name } => cc::mcp_remove_server(path, name, out),
        McpCommand::Enable { name, surface } => {
            cc::mcp_set_enabled(path, name, surface, true, &builtin_infos(), out)
        }
        McpCommand::Disable { name, surface } => {
            cc::mcp_set_enabled(path, name, surface, false, &builtin_infos(), out)
        }
    }
}

/// Resolve the compiled-in built-in MCP servers to the `(name, tool_count)`
/// pairs the `config mcp` handlers render — the same set the interactive client
/// hosts in-process. Empty when built-ins are compiled out.
fn builtin_infos() -> Vec<adele::config_cmd::BuiltinInfo> {
    adele::builtins::builtin_servers()
        .iter()
        .map(|s| adele::config_cmd::BuiltinInfo::new(s.name.clone(), s.service.tools().len()))
        .collect()
}

/// Best-effort terminal restoration (TUI-1): undo everything `main`'s setup
/// pushed — raw mode, the kitty keyboard-enhancement flags, the alternate
/// screen, and bracketed paste — and re-show the cursor. Every step is
/// `let _ =` because this runs on panic/exit paths where some state may
/// already be gone; restoring as much as possible beats bailing early.
fn restore_terminal() {
    let mut stdout = io::stdout();
    let _ = disable_raw_mode();
    // Pop the kitty flags first (pushed last); harmless if the terminal never
    // accepted the push.
    let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    let _ = execute!(
        stdout,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}

/// Install a panic hook that restores the terminal before delegating to the
/// previously installed hook (TUI-1), so a panic prints its message onto a
/// usable screen instead of a raw-mode alternate-screen mess.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

/// Build the tracing `EnvFilter` directive for a `-v` count. `RUST_LOG`, when
/// set, takes precedence and this is not consulted. Higher counts widen the
/// level for our own crates while keeping third-party noise at `warn`.
fn log_filter(verbose: u8) -> String {
    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    format!(
        "warn,adele={level},desktop_assistant_client_common={level},\
         desktop_assistant_mcp_client={level},client_ui_common={level}"
    )
}

/// Install a stderr tracing subscriber when `-v`/`--verbose` is given or
/// `RUST_LOG` is set; otherwise stay silent so normal output is clean. Logging
/// to stderr keeps a headless `--prompt` run's reply (stdout) separable from
/// diagnostics. Best-effort: an already-installed global subscriber is a no-op.
fn init_logging(verbose: u8) {
    use tracing_subscriber::EnvFilter;
    let has_rust_log = std::env::var_os("RUST_LOG").is_some();
    if verbose == 0 && !has_rust_log {
        return;
    }
    let filter = if has_rust_log {
        EnvFilter::from_default_env()
    } else {
        EnvFilter::new(log_filter(verbose))
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(true)
        .try_init();
}

#[tokio::main]
async fn main() -> Result<()> {
    // Both `ring` and `aws-lc-rs` end up enabled in rustls because reqwest 0.12
    // (via oauth2) and reqwest 0.13 (via desktop-assistant-client-common) share
    // hyper-rustls and pull different provider features. Pick ring explicitly.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls ring crypto provider");

    credentials::init_store();

    let matches = CliArgs::command().get_matches();
    let cli = CliArgs::from_arg_matches(&matches)?;
    let cli_explicit = any_explicit_connection_arg(&matches);

    // Install logging first so connection/registration/streaming diagnostics are
    // captured for the headless path too (verbose or RUST_LOG; silent otherwise).
    init_logging(cli.global.verbose);

    // `config …`: pure, daemon-free config management. Dispatch and return
    // before any terminal setup or daemon connection.
    if let Some(Command::Config(cfg)) = &cli.command {
        return run_config(&cfg.command);
    }

    // Headless one-shot: `exec <PROMPT>` (preferred) or the deprecated
    // `--prompt` flag connects, prints the reply, and exits without ever
    // entering the TUI. Runs before any terminal setup.
    let exec_prompt = match &cli.command {
        Some(Command::Exec { prompt }) => Some(prompt.clone()),
        // `config` already returned above; only `None` remains here.
        _ => cli.global.prompt.clone(),
    };
    if let Some(prompt) = exec_prompt {
        let config: ConnectionConfig = cli.global.into();
        return run_headless(&config, prompt).await;
    }

    // Restore the terminal on panic BEFORE any state is pushed, chaining the
    // default hook so the panic message lands on a usable screen (TUI-1).
    install_panic_hook();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Mouse capture is deliberately NOT enabled (TUI-9): the TUI handles no
    // mouse events, and capturing them hijacks the terminal's native text
    // selection/copy and scrollback. Keyboard scrolling (Ctrl+U/D/E,
    // PageUp/Down) covers navigation. Bracketed paste keeps a multi-line
    // paste as ONE Event::Paste instead of a stream of per-line Enters (TUI-3).
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        )
    );
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, cli.global, cli_explicit).await;

    // Restore terminal (same best-effort path the panic hook uses).
    restore_terminal();
    terminal.show_cursor()?;

    result
}

/// Headless one-shot mode (`--prompt`): connect, register client-hosted MCP
/// tools, send the prompt, stream the reply to stdout, and exit. A client tool
/// call during the turn is routed to the MCP host (or resolved via the built-in
/// dispatch) and its result submitted so the turn always resumes.
async fn run_headless(config: &ConnectionConfig, prompt: String) -> Result<()> {
    use std::io::Write as _;

    let conn = Connector::connect(config)
        .await
        .map_err(|e| anyhow::anyhow!("connection failed: {e}"))?;
    let mut signal_rx = conn.subscribe();

    // Start the client-side MCP host for the `tui` surface and advertise its
    // tools (merged with the built-ins) so a headless prompt can trigger local
    // tools exactly like the interactive client.
    let servers: Vec<_> = ClientMcpConfig::load(&default_client_mcp_path())
        .resolved_servers("tui")
        .into_iter()
        .cloned()
        .collect();
    // Compiled-in built-ins (da#538 Phase C/D): host the full core MCP set
    // in-process. `McpHost::start_with` centralizes the override, skipping (and
    // logging) any built-in whose name a client-mcp.toml server already provides.
    let mcp_builtins = adele::builtins::builtin_servers();
    let host = if servers.is_empty() && mcp_builtins.is_empty() {
        None
    } else {
        Some(McpHost::start_with(&servers, mcp_builtins).await)
    };
    let host_tools = host.as_ref().map(|h| h.registrations()).unwrap_or_default();
    let builtins = vec![
        client_tools::say_this_registration(),
        client_tools::request_voice_registration(),
        client_tools::stop_voice_registration(),
    ];
    // Best-effort: client tools need a command channel (UDS/WS), not D-Bus.
    let _ = conn
        .register_client_tools(merge_registrations(builtins, host_tools))
        .await;

    let conversation_id = conn
        .client()
        .create_conversation("adele --prompt")
        .await
        .map_err(|e| anyhow::anyhow!("could not create conversation: {e}"))?;
    let request_id = conn
        .send_prompt_with_system_refinement(&conversation_id, &prompt, "")
        .await
        .map_err(|e| anyhow::anyhow!("could not send prompt: {e}"))?;

    let mut stdout = io::stdout();
    let mut streamed = false;
    while let Some(event) = signal_rx.recv().await {
        match event {
            SignalEvent::Chunk {
                request_id: rid,
                chunk,
                ..
            } if rid == request_id => {
                streamed = true;
                let _ = stdout.write_all(chunk.as_bytes());
                let _ = stdout.flush();
            }
            SignalEvent::Complete {
                request_id: rid,
                full_response,
                ..
            } if rid == request_id => {
                // Some providers don't stream chunks; fall back to the final text.
                if !streamed {
                    let _ = stdout.write_all(full_response.as_bytes());
                }
                let _ = writeln!(stdout);
                break;
            }
            SignalEvent::Error {
                request_id: rid,
                error,
                ..
            } if rid == request_id => {
                return Err(anyhow::anyhow!("{error}"));
            }
            SignalEvent::ClientToolCall {
                task_id,
                tool_call_id,
                tool_name,
                arguments,
                ..
            } => {
                let result = match host.as_ref() {
                    Some(host) if host.handles(&tool_name) => {
                        host.call(&tool_name, arguments).await
                    }
                    // The built-in client tools are TUI-visual (speak / show);
                    // headless just resolves them so the turn completes.
                    _ => client_tools::dispatch(&tool_name, &arguments, false).result,
                };
                let _ = conn
                    .submit_client_tool_result(&task_id, &tool_call_id, result)
                    .await;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Reconnect backoff state machine. Sequence: 2s → 4s → 8s → 16s → 30s,
/// then stays at 30s indefinitely. `Connected` means we're not currently
/// trying to reconnect.
#[derive(Debug)]
enum ReconnectState {
    Connected,
    Pending { next_at: Instant, delay_secs: u64 },
}

const RECONNECT_INITIAL_SECS: u64 = 2;
const RECONNECT_MAX_SECS: u64 = 30;

fn next_backoff(prev_secs: u64) -> u64 {
    prev_secs.saturating_mul(2).min(RECONNECT_MAX_SECS)
}

fn schedule_reconnect(prev: Option<u64>) -> ReconnectState {
    let delay_secs = match prev {
        None => RECONNECT_INITIAL_SECS,
        Some(p) => next_backoff(p),
    };
    ReconnectState::Pending {
        next_at: Instant::now() + std::time::Duration::from_secs(delay_secs),
        delay_secs,
    }
}

/// Returns true if the user explicitly supplied any connection-related CLI
/// flag or env var, in which case we skip the profile picker and connect
/// using the provided values (matching pre-profile-picker behavior).
fn any_explicit_connection_arg(matches: &clap::ArgMatches) -> bool {
    ["transport", "socket", "ws", "ws_url", "ws_subject"]
        .iter()
        .any(|name| {
            matches!(
                matches.value_source(name),
                Some(ValueSource::CommandLine | ValueSource::EnvVariable)
            )
        })
}

/// Outcome of a single chat session loop. `Switch` re-enters the picker;
/// `Quit` exits the program.
enum RunOutcome {
    Quit,
    Switch,
}

/// Decide between picker-driven and CLI-driven connection, then run the
/// chat loop. Loops back into the picker when the user requests a
/// connection switch.
async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    global: Global,
    cli_explicit: bool,
) -> Result<()> {
    // First connection: respect explicit CLI/env args; otherwise picker if
    // we have profiles, else fall back to CLI defaults.
    let mut config = if cli_explicit {
        ConnectionConfig::from(global)
    } else {
        let store = ProfileStore::load();
        if store.profiles.is_empty() {
            ConnectionConfig::from(global)
        } else {
            match picker::run(terminal, store).await?.0 {
                PickerOutcome::Selected(profile) => profile.to_connection_config(),
                PickerOutcome::Cancelled => return Ok(()),
            }
        }
    };

    loop {
        match run(terminal, &config).await? {
            RunOutcome::Quit => return Ok(()),
            RunOutcome::Switch => {
                // Always show the picker on switch — even if CLI args were
                // used initially, the user is opting into profile-based
                // selection now.
                let store = ProfileStore::load();
                match picker::run(terminal, store).await?.0 {
                    PickerOutcome::Selected(profile) => {
                        config = profile.to_connection_config();
                    }
                    PickerOutcome::Cancelled => return Ok(()),
                }
            }
        }
    }
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config: &ConnectionConfig,
) -> Result<RunOutcome> {
    let mut app = App::new();
    let settings = Settings::load();
    app.show_debug = settings.show_debug;

    // Start any client-side MCP servers configured for the `tui` surface and hold
    // the host in `App` so its tools are advertised (register) and routed
    // (dispatch) to whichever daemon we connect to — local or remote (k8s).
    let mcp_cfg = ClientMcpConfig::load(&default_client_mcp_path());
    let mcp_servers: Vec<_> = mcp_cfg
        .resolved_servers("tui")
        .into_iter()
        .cloned()
        .collect();
    // Compiled-in built-ins (da#538 Phase C/D): host the full core MCP set
    // in-process. `McpHost::start_with` centralizes the override, skipping (and
    // logging) any built-in whose name a configured client-mcp server already
    // provides, and reports each built-in's status for the F5 panel.
    let mcp_builtins = adele::builtins::builtin_servers();
    if !mcp_servers.is_empty() || !mcp_builtins.is_empty() {
        app.mcp_host = Some(Rc::new(
            McpHost::start_with(&mcp_servers, mcp_builtins).await,
        ));
    }

    // The `Connector` owns the transport AND the signal stream, pumping every
    // `SignalEvent` to its subscribers from a dedicated task (client-common
    // #203). The TUI holds the connector (so reconnect/disconnect can drop and
    // rebuild it) plus one `subscribe()`d receiver that feeds the `select!`
    // loop. Before there's a live connection both are the not-connected
    // sentinels: `connector = None` and a closed `signal_rx`.
    // The connector is held behind an `Rc` so an in-flight RPC future (TUI-5 /
    // #83) can hold its own clone, keeping the connection alive independently of
    // this `connector` variable — which the reconnect/disconnect arms reassign.
    // Without that, the borrow checker couldn't let a future borrow `connector`
    // across a reassignment.
    let mut connector: Option<Rc<Connector>> = None;
    let mut signal_rx: UnboundedReceiver<SignalEvent> = unbounded_channel().1;
    let mut reconnect = ReconnectState::Connected;

    // Off-loop RPC driver (TUI-5 / #83). Daemon round-trips triggered by user
    // actions (open/create/delete/rename/archive a conversation, cancel a task)
    // are pushed here as futures and polled as one more `select!` branch, so a
    // slow or wedged RPC no longer blocks redraw or input. The futures borrow
    // the `connector`'s client, so this is dropped/rebuilt with the connection.
    let mut in_flight: InFlight<'static, RpcOutcome> = InFlight::new();

    // Initial connect — on failure, fall straight into the backoff loop
    // instead of running with no connection.
    match Connector::connect(config).await {
        Ok(conn) => {
            signal_rx = subscribe_and_load(&mut app, &conn).await;
            finish_connection_init(&mut app, &conn).await;
            connector = Some(Rc::new(conn));
        }
        Err(e) => {
            reconnect = schedule_reconnect(None);
            app.status_message =
                format!("Connection failed: {e}. Reconnecting in {RECONNECT_INITIAL_SECS}s...");
        }
    }

    let mut event_stream = crossterm::event::EventStream::new();

    // Embedded voice (adele-tui#67). When voice is in `embedded` mode we build
    // the session (load the VAD/STT ONNX models + the TTS backend) once, on a
    // background task, and receive it over `session_rx`. Building does NOT open
    // the mic — `dictate()` does that only on an explicit Ctrl+G — so the model
    // load just overlaps with the user settling into the chat. Both the capture
    // result and the ready session are merged into the select! loop like every
    // other async source (per AGENTS.md). Off/daemon mode wires nothing.
    let voice_cfg = VoiceConfig::load();
    // Connect to the standalone voice daemon (`org.desktopAssistant.Voice`) for
    // narration (adele-tui#77). This is independent of the embedded pipeline and
    // of `voice.toml`'s mode: when the daemon is running it is the preferred,
    // warm speaker for reply narration + `say_this` asides; the embedded engine
    // (if `embedded` mode built one) is the fallback. Connecting never fails hard
    // — a missing daemon yields an inert controller probed per-utterance.
    let voice_daemon = VoiceController::connect().await;
    let mut voice_session: Option<VoiceSession> = None;
    // Single serialized narration queue (TUI-11): both reply narration and
    // `say_this` asides enqueue here, and one long-lived task speaks them
    // strictly one-at-a-time, so a `say_this` aside firing mid-reply no longer
    // interleaves sentence-by-sentence on the shared sink. The task ends when
    // this sender is dropped (loop teardown). Spawned, not awaited, so synth +
    // playback never block the UI.
    let (narration_tx, narration_rx) = unbounded_channel::<NarrationRequest>();
    tokio::spawn(voice::run_narration_loop(
        narration_rx,
        |req: NarrationRequest| speak_text(req.voice, req.embedded, req.text),
    ));
    let mut dictating = false;
    let (dictation_tx, mut dictation_rx) = unbounded_channel::<DictationOutcome>();
    let mut session_rx: Option<tokio::sync::oneshot::Receiver<anyhow::Result<VoiceSession>>> = None;
    if voice_cfg.embedded_enabled() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let cfg = voice_cfg.clone();
        tokio::spawn(async move {
            let _ = tx.send(VoiceSession::build(&cfg).await);
        });
        session_rx = Some(rx);
        app.status_message = "Voice: loading models… (Ctrl+G to dictate)".into();
    }

    loop {
        // Project the run loop's connection state into the model so the view
        // can render disconnect chrome — the loop owns the socket; the draw
        // path only sees `app`. Mirror the exact predicate that gates sending
        // (`connector.is_some()`) so the `offline` cue shows precisely when a
        // send would be refused.
        app.connected = connector.is_some();
        terminal.draw(|f| ui::draw(f, &mut app))?;

        if app.should_quit {
            return Ok(RunOutcome::Quit);
        }
        if app.switch_requested {
            return Ok(RunOutcome::Switch);
        }

        // Drain a deferred conversation-list refresh (#1): a
        // `ConversationListChanged` that arrived while a modal sub-screen was
        // open set this flag (the sink couldn't own `in_flight`). Now that the
        // modal has closed and the sidebar is drawn again, refetch it.
        if std::mem::take(&mut app.pending_conversation_refresh) {
            push_conversation_refresh(&mut app, &connector, &mut in_flight);
        }

        // The reconnect timer is built fresh each loop iteration so that it
        // gets re-armed when state transitions in/out of Pending.
        let next_retry = match &reconnect {
            ReconnectState::Pending { next_at, .. } => Some(*next_at),
            ReconnectState::Connected => None,
        };

        // Sub-screens: each modal runs over the shared `screen::run_screen`
        // driver, which drains the daemon signal stream while the screen is open
        // (TUI-12) so a turn parked on the TUI's `say_this` client tool is
        // answered immediately instead of stalling until the screen closes.
        //
        // A single dispatch point (CC-3): the user's request is mutually exclusive
        // (`ScreenRequest`), so the loop opens at most one modal per turn. Every
        // screen shares the same disconnect handling — a `Disconnected` drained
        // mid-screen is recorded into `disconnect` and actioned once the screen
        // returns, since the teardown touches loop-local state the sink can't own
        // — so that skeleton is hoisted around the per-screen `match`.
        if let Some(screen) = app.take_pending_screen() {
            let mut disconnect: Option<String> = None;
            match screen {
                ScreenRequest::KnowledgeBase => {
                    if let Some(conn) = connector.clone() {
                        let mut sink = SubScreenSink {
                            app: &mut app,
                            connector: &connector,
                            voice_daemon: &voice_daemon,
                            voice_session: &voice_session,
                            narration_tx: &narration_tx,
                            disconnect: &mut disconnect,
                        };
                        if let Err(e) =
                            kb::run(terminal, conn.client(), &mut signal_rx, &mut sink).await
                        {
                            sink.app.status_message = format!("KB error: {e}");
                        }
                    }
                }
                ScreenRequest::Connections => {
                    if let Some(conn) = connector.clone() {
                        let mut sink = SubScreenSink {
                            app: &mut app,
                            connector: &connector,
                            voice_daemon: &voice_daemon,
                            voice_session: &voice_session,
                            narration_tx: &narration_tx,
                            disconnect: &mut disconnect,
                        };
                        if let Err(e) =
                            connections::run(terminal, conn.client(), &mut signal_rx, &mut sink)
                                .await
                        {
                            sink.app.status_message = format!("Connections error: {e}");
                        }
                    }
                }
                ScreenRequest::Purposes => {
                    if let Some(conn) = connector.clone() {
                        let mut sink = SubScreenSink {
                            app: &mut app,
                            connector: &connector,
                            voice_daemon: &voice_daemon,
                            voice_session: &voice_session,
                            narration_tx: &narration_tx,
                            disconnect: &mut disconnect,
                        };
                        if let Err(e) =
                            purposes::run(terminal, conn.client(), &mut signal_rx, &mut sink).await
                        {
                            sink.app.status_message = format!("Purposes error: {e}");
                        }
                    }
                }
                ScreenRequest::McpServers => {
                    if let Some(conn) = connector.clone() {
                        // Resolve the client's in-process built-ins for the panel's
                        // read-only section (da#538 Phase D). Computed before the
                        // sink borrows `app` mutably; empty when no host is running.
                        let mcp_builtins = app
                            .mcp_host
                            .as_ref()
                            .map(|h| adele::builtins::builtin_dtos(h.builtin_status()))
                            .unwrap_or_default();
                        let mut sink = SubScreenSink {
                            app: &mut app,
                            connector: &connector,
                            voice_daemon: &voice_daemon,
                            voice_session: &voice_session,
                            narration_tx: &narration_tx,
                            disconnect: &mut disconnect,
                        };
                        if let Err(e) = mcp::run(
                            terminal,
                            conn.client(),
                            &mcp_builtins,
                            &mut signal_rx,
                            &mut sink,
                        )
                        .await
                        {
                            sink.app.status_message = format!("MCP servers error: {e}");
                        }
                    }
                }
                ScreenRequest::ModelPicker => {
                    let mut picked_outcome = None;
                    if let Some(conn) = connector.clone() {
                        let current = app
                            .current_conversation()
                            .and_then(|c| c.model_selection.clone());
                        let mut sink = SubScreenSink {
                            app: &mut app,
                            connector: &connector,
                            voice_daemon: &voice_daemon,
                            voice_session: &voice_session,
                            narration_tx: &narration_tx,
                            disconnect: &mut disconnect,
                        };
                        match model_selector::run(
                            terminal,
                            conn.client(),
                            current,
                            &mut signal_rx,
                            &mut sink,
                        )
                        .await
                        {
                            Ok(outcome) => picked_outcome = Some(outcome),
                            Err(e) => sink.app.status_message = format!("Model picker error: {e}"),
                        }
                    }
                    // Apply the selection AFTER the sink's borrow of `app` ends.
                    if let Some(model_selector::Outcome::Selected(picked)) = picked_outcome {
                        let label = format!("{} · {}", picked.connection_id, picked.model_id);
                        app.apply_model_override(picked);
                        app.status_message = format!("Model: {label} (applies to next message)");
                    }
                }
                ScreenRequest::PersonalityPicker => {
                    let mut saved_outcome = None;
                    // Only reachable with a loaded conversation (handle_action gates
                    // on `current_conversation`), but re-check so the borrow stays
                    // clean.
                    let conv_info = app
                        .current_conversation()
                        .map(|conv| (conv.id.clone(), conv.conversation_personality));
                    if let (Some(conn), Some((conv_id, current))) = (connector.clone(), conv_info) {
                        let mut sink = SubScreenSink {
                            app: &mut app,
                            connector: &connector,
                            voice_daemon: &voice_daemon,
                            voice_session: &voice_session,
                            narration_tx: &narration_tx,
                            disconnect: &mut disconnect,
                        };
                        match personality_selector::run(
                            terminal,
                            conn.client(),
                            conv_id,
                            current,
                            &mut signal_rx,
                            &mut sink,
                        )
                        .await
                        {
                            Ok(outcome) => saved_outcome = Some(outcome),
                            Err(e) => {
                                sink.app.status_message = format!("Personality picker error: {e}")
                            }
                        }
                    }
                    // Apply the result AFTER the sink's borrow of `app` ends.
                    if let Some(personality_selector::Outcome::Saved(stored)) = saved_outcome {
                        let cleared = stored == Default::default();
                        app.set_open_conversation_personality(stored);
                        app.status_message = if cleared {
                            "Personality cleared (using global)".into()
                        } else {
                            "Personality saved for this conversation".into()
                        };
                    }
                }
            }
            apply_sub_screen_disconnect(
                &mut app,
                &mut connector,
                &mut signal_rx,
                &mut reconnect,
                disconnect,
            );
            // Force a redraw on the next iteration so the chat reappears
            // immediately instead of waiting for the next event.
            continue;
        }

        tokio::select! {
            Some(Ok(evt)) = event_stream.next() => {
                // Bracketed paste (TUI-3): the whole paste arrives as one
                // event and goes verbatim into the focused input — never
                // through the key map, so embedded newlines can't fire
                // SubmitPrompt per line.
                if let Event::Paste(text) = &evt {
                    app.apply_paste(text);
                    continue;
                }
                if let Event::Key(key) = evt {
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    // The help overlay is informational: while it's open, ANY key
                    // dismisses it (and does nothing else).
                    if app.show_help {
                        app.show_help = false;
                        continue;
                    }
                    // The delete-confirm overlay is modal: while it's up, only an
                    // explicit confirm (y/Y/Enter) or cancel (n/N/Esc) is honored;
                    // every other key is ignored (matching the KB / connections /
                    // profile confirms). Confirm runs the existing delete path.
                    if app.delete_confirm_pending() {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                                if app.confirm_delete() {
                                    handle_action(
                                        &mut app,
                                        &connector,
                                        &mut in_flight,
                                        Action::DeleteConversation,
                                    )
                                    .await;
                                }
                            }
                            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                app.cancel_delete_confirm();
                            }
                            _ => {}
                        }
                        continue;
                    }
                    if let Some(action) = handle_key_event(key, &app.mode, app.tasks.visible) {
                        if action == Action::Dictate {
                            // Push-to-talk (adele-tui#77). Prefer the voice
                            // daemon's dictation when it is running: it captures,
                            // transcribes, and routes the whole turn — spoken
                            // prompt and reply — into the active conversation
                            // (mirroring gtk's mic button). Fall back to the
                            // embedded one-shot dictation (transcript → input)
                            // when the daemon is absent.
                            if voice_daemon.is_available().await {
                                let conv = app
                                    .current_conversation()
                                    .map(|c| c.id.clone());
                                // Barge-in: stop any in-progress narration before
                                // we start listening, so the mic doesn't capture
                                // Adele's own voice. Best-effort.
                                let _ = voice_daemon.stop_speaking().await;
                                match voice_daemon
                                    .push_to_talk_routed(conv.as_deref())
                                    .await
                                {
                                    Ok(()) => {
                                        app.status_message = "Listening… (voice daemon)".into();
                                    }
                                    Err(e) => {
                                        app.status_message =
                                            format!("Push-to-talk failed: {e}");
                                    }
                                }
                            } else {
                                start_dictation(
                                    &mut app,
                                    &voice_cfg,
                                    &voice_session,
                                    &mut dictating,
                                    &dictation_tx,
                                );
                            }
                        } else {
                            handle_action(&mut app, &connector, &mut in_flight, action).await;
                        }
                    } else {
                        match app.mode {
                            InputMode::Editing => {
                                app.textarea.input(key);
                            }
                            InputMode::Renaming => {
                                app.rename_textarea.input(key);
                            }
                            InputMode::Normal => {}
                        }
                    }
                }
            }
            Some(signal) = signal_rx.recv() => {
                // All signal handling lives in `handle_signal` (shared with the
                // sub-screen driver, TUI-12) — it touches only `App` + the voice
                // plumbing. The things it can't own are loop-local: the
                // connection teardown a `Disconnected` triggers, and the
                // `InFlight` RPC driver a `ConversationListChanged` refetch runs
                // on. It reports those back for the main loop to action here.
                match handle_signal(
                    &mut app,
                    &connector,
                    &voice_daemon,
                    &voice_session,
                    &narration_tx,
                    signal,
                )
                .await
                {
                    SignalAction::None => {}
                    SignalAction::Disconnected { reason } => {
                        // Drop the connector (closing its transport + fanout
                        // task) and reset to the not-connected sentinel receiver
                        // so the backoff loop owns reconnection. Any in-flight
                        // stream died with the connection (TUI-8): clear it so no
                        // frozen ▌ buffer lingers and the ack sentinel can't
                        // mis-claim the first post-reconnect stream.
                        app.clear_streaming_state();
                        connector = None;
                        signal_rx = unbounded_channel().1;
                        reconnect = schedule_reconnect(None);
                        app.status_message = format!(
                            "Disconnected: {reason}. Reconnecting in {RECONNECT_INITIAL_SECS}s..."
                        );
                    }
                    // The list changed elsewhere — refetch the sidebar via the
                    // same off-loop path the (un)archive/show-archived toggles
                    // use, leaving the open conversation + transcript untouched.
                    SignalAction::RefreshConversations => {
                        push_conversation_refresh(&mut app, &connector, &mut in_flight);
                    }
                }
            }
            _ = async {
                match next_retry {
                    Some(deadline) => sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let prev_delay = match &reconnect {
                    ReconnectState::Pending { delay_secs, .. } => Some(*delay_secs),
                    ReconnectState::Connected => None,
                };
                app.status_message = "Reconnecting...".to_string();
                match Connector::connect(config).await {
                    Ok(conn) => {
                        // Subscribe + refresh the sidebar first (so the resync's
                        // by-id reselect runs against the FRESH list).
                        signal_rx = subscribe_and_load(&mut app, &conn).await;
                        // Resync the open conversation after the gap (TUI-8):
                        // re-fetch its transcript (turns may have completed
                        // while we were away — the dead stream's reply only
                        // exists daemon-side) and reselect it by ID — the
                        // sidebar selection is positional and the refreshed
                        // list may have reordered.
                        if let Some(open_id) = app.current_conversation().map(|c| c.id.clone()) {
                            app.select_conversation_by_id(&open_id);
                            match conn.client().get_conversation(&open_id).await {
                                Ok(detail) => app.load_conversation(detail),
                                Err(e) => {
                                    app.status_message =
                                        format!("Error refreshing conversation: {e}");
                                }
                            }
                        }
                        finish_connection_init(&mut app, &conn).await;
                        reconnect = ReconnectState::Connected;
                        connector = Some(Rc::new(conn));
                    }
                    Err(e) => {
                        reconnect = schedule_reconnect(prev_delay);
                        let next_in = match &reconnect {
                            ReconnectState::Pending { delay_secs, .. } => *delay_secs,
                            ReconnectState::Connected => RECONNECT_MAX_SECS,
                        };
                        app.status_message =
                            format!("Reconnect failed: {e}. Retrying in {next_in}s...");
                    }
                }
            }
            // The embedded voice session finished loading (or failed). Cache it
            // for the dictate/playback paths; on failure, fall back to voice
            // off. Polled only while `session_rx` is Some — a oneshot must not
            // be awaited after it resolves, so we clear it once consumed.
            built = async {
                match session_rx.as_mut() {
                    Some(rx) => rx.await,
                    None => std::future::pending().await,
                }
            } => {
                session_rx = None;
                match built {
                    Ok(Ok(session)) => {
                        voice_session = Some(session);
                        app.status_message = "Voice ready (Ctrl+G to dictate)".into();
                    }
                    Ok(Err(e)) => {
                        app.status_message = format!("Voice unavailable: {e}");
                    }
                    // Builder task dropped without sending — treat as voice off.
                    Err(_) => {}
                }
            }
            // A dictation capture finished: drop the transcript into the prompt
            // and send it through the normal assistant path, or report why not.
            Some(outcome) = dictation_rx.recv() => {
                dictating = false;
                app.set_assistant_status("");
                match outcome {
                    DictationOutcome::Transcribed(text) => {
                        insert_dictated_text(&mut app, &text);
                        send_prompt_from_input(&mut app, &connector).await;
                    }
                    DictationOutcome::NoSpeech => {
                        app.status_message = "No speech detected".into();
                    }
                    DictationOutcome::Failed(e) => {
                        app.status_message = format!("Dictation failed: {e}");
                    }
                }
            }
            // An off-loop RPC finished (TUI-5 / #83): apply its outcome to App.
            // While RPCs sit in flight the other arms above keep firing — input
            // and redraw never wait on a slow/wedged daemon round-trip. When no
            // RPC is in flight this arm is pending-forever (an inert branch).
            Some(outcome) = in_flight.next() => {
                // When the open conversation changed (open/create), (re)point the
                // daemon's live turn-event fan-out at the now-open conversation
                // (#1 multi-client sync) so turns started elsewhere — another
                // client, or the voice daemon — render live here.
                if apply_rpc_outcome(&mut app, outcome)
                    && let Some(conn) = connector.as_ref()
                {
                    subscribe_to_open_conversation(&mut app, conn).await;
                }
            }
        }
    }
}

/// The result of an off-loop RPC, carrying everything the event loop needs to
/// apply it to [`App`] (TUI-5 / #83). The RPC future itself touches no `App`
/// state — it captures only the borrowed transport client plus owned args, runs
/// off the event loop (so the UI keeps drawing + handling input while it's in
/// flight), and resolves to one of these. The loop applies it via
/// [`apply_rpc_outcome`].
///
/// Multi-step chains (e.g. create → refresh list → open) run their steps
/// *sequentially inside the future*, so the whole chain stays off the loop; the
/// variant just carries the already-combined result.
enum RpcOutcome {
    /// `OpenConversation` / `OpenSelectedTaskConversation`: a fetched detail (or
    /// error). `enter_editing` distinguishes the "open & start typing" path from
    /// the task-jump path (which only loads).
    ConversationOpened {
        result: Result<ConversationDetail, String>,
        enter_editing: bool,
    },
    /// `NewConversation`: the create+refresh+open chain. `created_id` is the new
    /// conversation's id; `list` refreshes the sidebar; `detail` is the opened
    /// transcript.
    ConversationCreated {
        created: Result<String, String>,
        list: Option<Result<Vec<ConversationSummary>, String>>,
        detail: Option<Result<ConversationDetail, String>>,
    },
    /// `DeleteConversation`: the daemon delete result, plus an optional sidebar
    /// resync fetched only when the delete failed (to undo the optimistic local
    /// removal).
    ConversationDeleted {
        result: Result<(), String>,
        resync: Option<Result<Vec<ConversationSummary>, String>>,
    },
    /// `SubmitRename`: the rename result for `(id, title)`.
    ConversationRenamed {
        id: String,
        title: String,
        result: Result<(), String>,
    },
    /// `ArchiveConversation` / `ToggleShowArchived`: a list refresh, with a
    /// status string to show on success.
    ConversationsRefreshed {
        list: Result<Vec<ConversationSummary>, String>,
        on_success: Option<String>,
    },
    /// The RPC completed with nothing for the loop to apply (e.g. a successful
    /// task-cancel command, whose effect arrives later via `TaskCompleted`).
    Noop,
    /// A terminal error that just needs surfacing in the status line verbatim
    /// (e.g. an (un)archive RPC that failed before any refresh).
    StatusError(String),
    /// `CancelSelectedTask`: only the FAILURE is surfaced here — success is
    /// resolved authoritatively by the `TaskCompleted` signal, so a successful
    /// cancel command produces no outcome. On failure we clear the pending
    /// spinner for `task_id`.
    TaskCancelFailed { task_id: String, error: String },
}

/// Apply a completed [`RpcOutcome`] to `App` (TUI-5 / #83). This is the only
/// place RPC results touch `App`; it runs on the event loop after the RPC has
/// resolved off it.
///
/// Returns `true` when the open conversation changed (an open/create succeeded),
/// so the caller can (re)send `SubscribeConversations` for the now-open
/// conversation (#1 live multi-client sync) — this function stays transport-free,
/// so the actual command send happens on the event loop where the connector
/// lives.
fn apply_rpc_outcome(app: &mut App, outcome: RpcOutcome) -> bool {
    match outcome {
        RpcOutcome::ConversationOpened {
            result,
            enter_editing,
        } => match result {
            Ok(detail) => {
                app.load_conversation(detail);
                if enter_editing {
                    app.enter_editing_mode();
                }
                return true;
            }
            Err(e) => app.status_message = format!("Error: {e}"),
        },
        RpcOutcome::ConversationCreated {
            created,
            list,
            detail,
        } => {
            let created_id = match created {
                Ok(id) => id,
                Err(e) => {
                    app.status_message = format!("Create error: {e}");
                    return false;
                }
            };
            match list {
                Some(Ok(convs)) => {
                    let new_idx = convs.iter().position(|c| c.id == created_id);
                    app.set_conversations(convs);
                    if let Some(idx) = new_idx {
                        app.selected_conversation = Some(idx);
                    }
                }
                Some(Err(e)) => app.status_message = format!("Error refreshing: {e}"),
                None => {}
            }
            match detail {
                Some(Ok(detail)) => {
                    app.load_conversation(detail);
                    app.enter_editing_mode();
                    return true;
                }
                Some(Err(e)) => app.status_message = format!("Error opening: {e}"),
                None => {}
            }
        }
        RpcOutcome::ConversationDeleted { result, resync } => {
            if let Err(e) = result {
                app.status_message = format!("Delete error: {e}");
                // Resync the sidebar so the optimistic local removal doesn't
                // linger after a failed delete.
                if let Some(Ok(convs)) = resync {
                    app.set_conversations(convs);
                }
            }
        }
        RpcOutcome::ConversationRenamed { id, title, result } => match result {
            Ok(()) => {
                app.apply_rename(&id, &title);
                app.status_message = format!("Renamed to \"{title}\"");
            }
            Err(e) => app.status_message = format!("Rename error: {e}"),
        },
        RpcOutcome::ConversationsRefreshed { list, on_success } => match list {
            Ok(convs) => {
                // The list-only refresh (a show-archived/(un)archive toggle, or a
                // `ConversationListChanged` refetch) flows through the reducer's
                // `ConversationListRefetched`, which repaints the sidebar
                // (`SetConversations`) and re-syncs the selection
                // (`EnsureActiveConversation` — a no-op in the TUI). The open
                // conversation + its chat are deliberately left untouched. These
                // are all view-effects, so `apply_core` fully handles them and
                // returns nothing for the loop to run.
                let effects = app.apply_core(UiMessage::ConversationListRefetched(convs));
                debug_assert!(
                    effects.is_empty(),
                    "ConversationListRefetched must emit only view-effects: {effects:?}"
                );
                if let Some(msg) = on_success {
                    app.status_message = msg;
                }
            }
            Err(e) => app.status_message = format!("Error refreshing: {e}"),
        },
        RpcOutcome::Noop => {}
        RpcOutcome::StatusError(msg) => app.status_message = msg,
        RpcOutcome::TaskCancelFailed { task_id, error } => {
            app.status_message = format!("Cancel failed: {error}");
            if app.pending_task_cancel.as_ref().map(|t| t.0.as_str()) == Some(task_id.as_str()) {
                app.pending_task_cancel = None;
            }
        }
    }
    // No open-conversation change on any path that reached here.
    false
}

/// What [`handle_signal`] asks its caller to do after handling a signal. Almost
/// every signal is fully handled inside `handle_signal` (it mutates `App` and the
/// voice plumbing it was given) and yields [`SignalAction::None`]. The exceptions
/// touch loop-local state `handle_signal` doesn't own, so they are reported back
/// for the caller to action: `Disconnected` (connection teardown — `connector`,
/// `signal_rx`, `reconnect`) and `RefreshConversations` (a list refetch on the
/// loop-local `InFlight` RPC driver).
enum SignalAction {
    None,
    Disconnected {
        reason: String,
    },
    /// A `ConversationListChanged` arrived (#1): the user's list changed on
    /// another client or the voice daemon. The caller must refetch the
    /// conversation list and repaint the sidebar. Reported back rather than
    /// handled inline because the refetch runs on the loop-local `InFlight` RPC
    /// driver that `handle_signal` doesn't own — same reason `Disconnected` is
    /// returned. The open conversation + its transcript are untouched: only the
    /// sidebar list is replaced.
    RefreshConversations,
}

/// Run the controller-level effects [`App::apply_core`] bubbled up from a
/// streaming signal — the ones the view can't perform itself. Today the streaming
/// arms bubble up only [`Effect::Speak`] (reply narration, already gated by
/// core's `StreamComplete`); the view-level effects (including the side-pane
/// no-ops) were already absorbed inside `apply_core`. Later CC-3 slices route the
/// open-conversation RPC effects and handle them here.
fn run_stream_controller_effects(
    effects: Vec<Effect>,
    voice_daemon: &VoiceController,
    voice_session: &Option<VoiceSession>,
    narration_tx: &UnboundedSender<NarrationRequest>,
) {
    // Today the streaming arms bubble up only `Speak`; any other effect is
    // ignored here (later CC-3 slices route the open-conversation RPC effects).
    // Reaching the body means core's narration gate passed; skip an empty reply
    // so we don't enqueue silence.
    for effect in effects {
        if let Effect::Speak(text) = effect
            && !text.trim().is_empty()
        {
            enqueue_narration(narration_tx, voice_daemon, voice_session, text);
        }
    }
}

/// Apply one daemon [`SignalEvent`] to `App` (+ voice). Extracted from the main
/// `select!` so it can be reused by the sub-screen driver (TUI-12): while a modal
/// screen is open, [`screen::run_screen`] drains the signal stream through this
/// same function, so a turn parked on the TUI's `say_this` client tool is
/// answered immediately instead of looking hung until the screen closes.
///
/// It deliberately handles only what depends on `App`/voice; the `Disconnected`
/// teardown (loop-local) is returned for the caller to perform. During a
/// sub-screen the sub-screen driver propagates that outcome so the disconnect is
/// actioned once the screen returns.
async fn handle_signal(
    app: &mut App,
    connector: &Option<Rc<Connector>>,
    voice_daemon: &VoiceController,
    voice_session: &Option<VoiceSession>,
    narration_tx: &UnboundedSender<NarrationRequest>,
    signal: SignalEvent,
) -> SignalAction {
    match signal {
        // The streaming events (#1) route through the shared core
        // (`App::apply_core`): the reducer owns the in-flight state machine —
        // request-id claiming, originating-conversation targeting (TUI-4), and
        // the reply-narration gate — and emits effects. `apply_core` applies the
        // view-level effects (transcript, chat status, context fill) onto `App`
        // in place and returns the controller-level ones (narration) for
        // `run_stream_controller_effects` to run. The events carry
        // `conversation_id` too, but the in-flight slot routes the stream arms by
        // `request_id`, so the reducer drops it there.
        SignalEvent::UserMessageAdded {
            conversation_id,
            request_id,
            content,
        } => {
            let effects = app.apply_core(UiMessage::UserMessageAdded {
                conversation_id,
                request_id,
                content,
            });
            run_stream_controller_effects(effects, voice_daemon, voice_session, narration_tx);
        }
        SignalEvent::Chunk {
            request_id, chunk, ..
        } => {
            let effects = app.apply_core(UiMessage::StreamChunk { request_id, chunk });
            run_stream_controller_effects(effects, voice_daemon, voice_session, narration_tx);
        }
        SignalEvent::Complete {
            request_id,
            full_response,
            ..
        } => {
            // The whole narration gate — TUI-4 originating-conversation
            // targeting, the adele-tui#77 `Adele`-level gate (`Always` OR
            // `OnDemand` AND `You == Enabled`), external-turn suppression, and the
            // `say_this` dedupe — now lives in core's `StreamComplete`, which
            // emits a `Speak` effect when (and only when) the reply should be
            // spoken. `run_stream_controller_effects` routes that through the
            // single narration queue (TUI-11) so synth never blocks the UI and a
            // `say_this` aside can't interleave.
            let effects = app.apply_core(UiMessage::StreamComplete {
                request_id,
                full_response,
            });
            run_stream_controller_effects(effects, voice_daemon, voice_session, narration_tx);
        }
        SignalEvent::Error {
            request_id, error, ..
        } => {
            let effects = app.apply_core(UiMessage::StreamError {
                request_id,
                error: error.clone(),
            });
            run_stream_controller_effects(effects, voice_daemon, voice_session, narration_tx);
            // The reducer surfaces "Error: …" in the status line only for the
            // matching in-flight stream; set it unconditionally too so an error
            // for an already-resolved request still reaches the user.
            app.status_message = format!("Error: {error}");
        }
        SignalEvent::Status {
            request_id,
            message,
            ..
        } => {
            let effects = app.apply_core(UiMessage::AssistantStatus {
                request_id,
                message,
            });
            run_stream_controller_effects(effects, voice_daemon, voice_session, narration_tx);
        }
        SignalEvent::ContextUsage {
            conversation_id,
            request_id: _,
            used_tokens,
            budget_tokens,
            compaction_active,
        } => {
            let effects = app.apply_core(UiMessage::ContextUsage {
                conversation_id,
                used_tokens,
                budget_tokens,
                compaction_active,
            });
            run_stream_controller_effects(effects, voice_daemon, voice_session, narration_tx);
        }
        SignalEvent::TitleChanged {
            conversation_id,
            title,
        } => {
            app.update_conversation_title(&conversation_id, &title);
        }
        // The user's conversation list changed elsewhere (#1) — a conversation
        // was created / renamed / deleted / (un)archived by another client or
        // the voice daemon. The event carries only the affected id; the correct
        // handling for every change kind is a full list refetch, so we ignore
        // the id and ask the caller to refetch (it owns the RPC driver). The
        // open conversation + its transcript are deliberately left alone.
        SignalEvent::ConversationListChanged { .. } => {
            return SignalAction::RefreshConversations;
        }
        SignalEvent::Disconnected { reason } => {
            return SignalAction::Disconnected { reason };
        }
        SignalEvent::ConversationWarning { warning, .. } => {
            // Currently only the dangling-model-selection warning is emitted.
            // Surface a hint in the status bar; richer handling (auto-pick
            // fallback, etc.) belongs upstream with the per-conversation model
            // selector (#1).
            app.status_message = format!("Warning: {warning:?}");
        }
        // --- Background task events (issue #45 / desktop-assistant#114) ---
        //
        // Each event is forwarded verbatim into `app.tasks`. The map's invariants
        // are enforced inside `TaskPane` so this call site stays one-liner-thin.
        SignalEvent::TaskStarted { task } => {
            app.tasks.apply_task_started(task);
        }
        SignalEvent::TaskProgress { id, progress_hint } => {
            app.tasks.apply_task_progress(&id, progress_hint);
        }
        SignalEvent::TaskLogAppended { id, entry } => {
            app.tasks.apply_task_log_appended(&id, entry);
        }
        SignalEvent::TaskCompleted { id, .. } => {
            // Clear the cancel spinner if we were waiting on this task; the
            // terminal event is the authoritative resolution.
            if app.pending_task_cancel.as_ref().map(|t| t.0.as_str()) == Some(id.as_str()) {
                app.pending_task_cancel = None;
            }
            app.tasks.apply_task_completed(&id);
        }
        // The TUI has no scratchpad pane (that lives in the GTK/KDE clients), so
        // the change notification is a no-op here.
        SignalEvent::ScratchpadChanged { .. } => {}
        // Knowledge-base change broadcast (a maintenance pass or another client
        // edited an entry). The KB browser, when open, refetches live via its
        // `Screen::on_signal`; here in the chat loop there is no list to refresh.
        SignalEvent::KnowledgeChanged => {}
        // The daemon suspended a turn on a client-local tool (#107) — the TUI
        // registers `say_this` (adele-tui#73). Dispatch it, perform the side
        // effect (speak / show inline), and ALWAYS submit a result so the parked
        // turn resumes. With the per-session registry (desktop-assistant#261) a
        // concurrent voice session's tools no longer fire here. Draining this
        // while a sub-screen is open is the whole point of TUI-12.
        SignalEvent::ClientToolCall {
            task_id,
            conversation_id,
            tool_call_id,
            tool_name,
            arguments,
        } => {
            let call = client_tools::ClientToolCall {
                task_id,
                conversation_id,
                tool_call_id,
                tool_name,
                arguments,
            };
            handle_client_tool_call(
                app,
                connector,
                voice_daemon,
                voice_session,
                narration_tx,
                call,
            )
            .await;
        }
    }
    SignalAction::None
}

/// The [`SignalSink`](screen::SignalSink) used while a modal sub-screen is open
/// (TUI-12): it forwards each drained signal through the same [`handle_signal`]
/// the chat loop uses, so a parked `say_this` turn is answered immediately. A
/// `Disconnected` can't be torn down here (the connection state is loop-local),
/// so its reason is stashed in `disconnect` for [`apply_sub_screen_disconnect`]
/// to action once the screen returns.
struct SubScreenSink<'a> {
    app: &'a mut App,
    connector: &'a Option<Rc<Connector>>,
    voice_daemon: &'a VoiceController,
    voice_session: &'a Option<VoiceSession>,
    narration_tx: &'a UnboundedSender<NarrationRequest>,
    disconnect: &'a mut Option<String>,
}

impl screen::SignalSink for SubScreenSink<'_> {
    async fn handle(&mut self, signal: SignalEvent) {
        match handle_signal(
            self.app,
            self.connector,
            self.voice_daemon,
            self.voice_session,
            self.narration_tx,
            signal,
        )
        .await
        {
            SignalAction::None => {}
            SignalAction::Disconnected { reason } => {
                // Keep the FIRST disconnect reason; further signals on a dead
                // connection are moot (the stream is about to be torn down
                // anyway).
                self.disconnect.get_or_insert(reason);
            }
            // The list changed elsewhere while a modal is open. The sink can't
            // own the loop-local `InFlight` driver, and the sidebar isn't even
            // drawn behind the modal, so defer: record the request and let the
            // chat loop refetch once the screen returns.
            SignalAction::RefreshConversations => {
                self.app.pending_conversation_refresh = true;
            }
        }
    }
}

/// Action a disconnect that arrived while a sub-screen was open (TUI-12). Same
/// teardown the chat loop's `Disconnected` arm performs: drop the connector,
/// reset to the not-connected sentinel receiver, and schedule reconnect. No-op
/// when `reason` is `None` (the common case — no disconnect occurred).
fn apply_sub_screen_disconnect(
    app: &mut App,
    connector: &mut Option<Rc<Connector>>,
    signal_rx: &mut UnboundedReceiver<SignalEvent>,
    reconnect: &mut ReconnectState,
    reason: Option<String>,
) {
    let Some(reason) = reason else {
        return;
    };
    app.clear_streaming_state();
    *connector = None;
    *signal_rx = unbounded_channel().1;
    *reconnect = schedule_reconnect(None);
    app.status_message =
        format!("Disconnected: {reason}. Reconnecting in {RECONNECT_INITIAL_SECS}s...");
}

async fn handle_action(
    app: &mut App,
    connector: &Option<Rc<Connector>>,
    in_flight: &mut InFlight<'static, RpcOutcome>,
    action: Action,
) {
    // RPC arms clone the `Rc<Connector>` into an off-loop future pushed onto
    // `in_flight` (TUI-5 / #83) rather than awaiting the RPC here, so the event
    // loop keeps drawing + handling input while the RPC runs; the clone keeps
    // the connection alive even when the loop reassigns its `connector` on
    // reconnect. The non-RPC arms only need a connectivity check, for which a
    // borrow suffices.
    let client: Option<&TransportClient> = connector.as_ref().map(|c| c.client());
    match action {
        Action::Quit => app.quit(),
        Action::NextConversation => app.next_conversation(),
        Action::PreviousConversation => app.previous_conversation(),
        Action::OpenConversation => {
            if let (Some(conn), Some(id)) = (connector.as_ref(), app.selected_conversation_id()) {
                let conn = Rc::clone(conn);
                let id = id.to_string();
                in_flight.push(async move {
                    RpcOutcome::ConversationOpened {
                        result: conn
                            .client()
                            .get_conversation(&id)
                            .await
                            .map_err(|e| e.to_string()),
                        enter_editing: true,
                    }
                });
            }
        }
        Action::BeginDeleteConversation => {
            // `d` arms the confirm overlay instead of deleting outright (matching
            // the other destructive deletes). The overlay's y/Enter dispatches
            // `DeleteConversation`; n/Esc cancels. Both are driven in the event
            // loop, which renders the overlay while it's armed.
            app.begin_delete_confirm();
        }
        Action::DeleteConversation => {
            // Check connectivity BEFORE mutating local state (TUI-2's shape):
            // previously the row vanished locally while the daemon never heard
            // about the delete, resurrecting it on the next refresh.
            let Some(conn) = connector.as_ref() else {
                app.status_message = "Not connected — conversation not deleted".into();
                return;
            };
            let conn = Rc::clone(conn);
            // Optimistically remove locally (TUI-2 shape), then delete off-loop.
            // The future only resyncs the sidebar when the delete fails, so a
            // failed delete can't leave the row missing; success is silent.
            if let Some(id) = app.delete_selected_conversation() {
                let show_archived = app.show_archived;
                in_flight.push(async move {
                    match conn.client().delete_conversation(&id).await {
                        Ok(()) => RpcOutcome::ConversationDeleted {
                            result: Ok(()),
                            resync: None,
                        },
                        Err(e) => RpcOutcome::ConversationDeleted {
                            result: Err(e.to_string()),
                            resync: Some(
                                fetch_conversations(conn.client(), show_archived)
                                    .await
                                    .map_err(|e| e.to_string()),
                            ),
                        },
                    }
                });
            }
        }
        Action::NewConversation => {
            if let Some(conn) = connector.as_ref() {
                let conn = Rc::clone(conn);
                // create → refresh sidebar → open: all three steps run
                // sequentially INSIDE the future, so the whole chain stays off
                // the loop and the UI never freezes while it runs.
                let show_archived = app.show_archived;
                in_flight.push(async move {
                    let client = conn.client();
                    let created = match client.create_conversation("New Conversation").await {
                        Ok(id) => id,
                        Err(e) => {
                            return RpcOutcome::ConversationCreated {
                                created: Err(e.to_string()),
                                list: None,
                                detail: None,
                            };
                        }
                    };
                    let list = Some(
                        fetch_conversations(client, show_archived)
                            .await
                            .map_err(|e| e.to_string()),
                    );
                    let detail = Some(
                        client
                            .get_conversation(&created)
                            .await
                            .map_err(|e| e.to_string()),
                    );
                    RpcOutcome::ConversationCreated {
                        created: Ok(created),
                        list,
                        detail,
                    }
                });
            }
        }
        Action::EnterEditMode => {
            if app.current_conversation().is_some() {
                app.enter_editing_mode();
            } else {
                app.status_message = "Open a conversation first (Enter) or create one (n)".into();
            }
        }
        Action::ExitEditMode => app.enter_normal_mode(),
        Action::SubmitPrompt => send_prompt_from_input(app, connector).await,
        // Dictation is handled in the event loop (it needs the embedded voice
        // session + a result channel + a spawned capture task — loop-local
        // resources that don't belong in `handle_action`'s signature), which
        // intercepts `Dictate` BEFORE dispatching here. This arm is therefore
        // unreachable; assert that rather than silently swallowing the action, so
        // a future routing change that lets `Dictate` slip through is caught.
        Action::Dictate => {
            unreachable!("Dictate is intercepted in the event loop, never dispatched")
        }
        Action::CycleAdeleOutput => match app.cycle_current_adele_output() {
            Some(AdeleOutput::Disabled) => {
                app.status_message =
                    "Adele: Disabled for this conversation (never speaks) — Ctrl+S to cycle".into();
            }
            Some(AdeleOutput::OnDemand) => {
                app.status_message = "Adele: On Demand for this conversation (speaks replies when \
                     You is Enabled; always speaks asides) — Ctrl+S to cycle"
                    .into();
            }
            Some(AdeleOutput::Always) => {
                app.status_message =
                    "Adele: Always for this conversation (reads every reply aloud) — Ctrl+S to cycle"
                        .into();
            }
            None => {
                app.status_message =
                    "Open a conversation first — Adele output is per-conversation".into();
            }
        },
        Action::ToggleVoiceIn => match app.toggle_current_voice_in() {
            Some(true) => {
                app.status_message =
                    "You: Enabled for this conversation (push-to-talk with Ctrl+G; narrates \
                     replies when Adele is On Demand) — Ctrl+V to disable"
                        .into();
            }
            Some(false) => {
                app.status_message = "You: Disabled for this conversation (type only)".into();
            }
            None => {
                app.status_message =
                    "Open a conversation first — the You control is per-conversation".into();
            }
        },
        Action::InsertNewline => {
            app.textarea.insert_newline();
        }
        Action::ToggleShowArchived => {
            app.show_archived = !app.show_archived;
            let on_success = if app.show_archived {
                "Showing all conversations (including archived)".to_string()
            } else {
                "Showing active conversations only".to_string()
            };
            if let Some(conn) = connector.as_ref() {
                let conn = Rc::clone(conn);
                let show_archived = app.show_archived;
                in_flight.push(async move {
                    RpcOutcome::ConversationsRefreshed {
                        list: fetch_conversations(conn.client(), show_archived)
                            .await
                            .map_err(|e| e.to_string()),
                        on_success: Some(on_success),
                    }
                });
            } else {
                app.status_message = on_success;
            }
        }
        Action::ArchiveConversation => {
            if let (Some(conn), Some(id)) = (connector.as_ref(), app.selected_conversation_id()) {
                let conn = Rc::clone(conn);
                let id = id.to_string();
                // Determine if conversation is currently archived
                let is_archived = app
                    .conversations()
                    .get(app.selected_conversation.unwrap_or(0))
                    .is_some_and(|c| c.archived);
                let show_archived = app.show_archived;
                // (un)archive → refresh, off-loop. On the archive RPC erroring
                // the chain surfaces it as a status error carrying the archive
                // error message.
                in_flight.push(async move {
                    let client = conn.client();
                    let result = if is_archived {
                        client.unarchive_conversation(&id).await
                    } else {
                        client.archive_conversation(&id).await
                    };
                    match result {
                        Ok(()) => RpcOutcome::ConversationsRefreshed {
                            list: fetch_conversations(client, show_archived)
                                .await
                                .map_err(|e| e.to_string()),
                            on_success: Some(
                                if is_archived {
                                    "Conversation unarchived"
                                } else {
                                    "Conversation archived"
                                }
                                .to_string(),
                            ),
                        },
                        Err(e) => RpcOutcome::StatusError(format!("Archive error: {e}")),
                    }
                });
            }
        }
        Action::ScrollUp => app.scroll_up(5),
        Action::ScrollDown => app.scroll_down(5),
        Action::ScrollToBottom => app.scroll_to_bottom(),
        Action::BeginRename => {
            if app.selected_conversation_id().is_some() {
                app.begin_rename();
            } else {
                app.status_message = "Select a conversation to rename".into();
            }
        }
        Action::SubmitRename => {
            if let Some((id, title)) = app.submit_rename()
                && let Some(conn) = connector.as_ref()
            {
                let conn = Rc::clone(conn);
                in_flight.push(async move {
                    let result = conn
                        .client()
                        .rename_conversation(&id, &title)
                        .await
                        .map_err(|e| e.to_string());
                    RpcOutcome::ConversationRenamed { id, title, result }
                });
            }
        }
        Action::CancelRename => app.cancel_rename(),
        Action::ToggleDebug => {
            app.show_debug = !app.show_debug;
            let settings = Settings {
                show_debug: app.show_debug,
            };
            if let Err(e) = settings.save() {
                app.status_message = format!("Settings save failed: {e}");
            } else {
                app.status_message = if app.show_debug {
                    "Debug view ON (showing tool/system messages)".into()
                } else {
                    "Debug view OFF".into()
                };
            }
        }
        Action::ToggleSidebar => {
            app.show_sidebar = !app.show_sidebar;
            app.status_message = if app.show_sidebar {
                "Conversation list shown".into()
            } else {
                "Conversation list hidden (Ctrl+B to show)".into()
            };
        }
        Action::ToggleHelp => app.toggle_help(),
        Action::SwitchConnection => {
            app.switch_requested = true;
            app.status_message = "Switching connection...".into();
        }
        Action::OpenKnowledgeBase => {
            if client.is_some() {
                app.request_screen(ScreenRequest::KnowledgeBase);
            } else {
                app.status_message = "Not connected — knowledge base unavailable".into();
            }
        }
        Action::OpenConnections => {
            if client.is_some() {
                app.request_screen(ScreenRequest::Connections);
            } else {
                app.status_message = "Not connected — connections manager unavailable".into();
            }
        }
        Action::OpenPurposes => {
            if client.is_some() {
                app.request_screen(ScreenRequest::Purposes);
            } else {
                app.status_message = "Not connected — purposes manager unavailable".into();
            }
        }
        Action::OpenMcpServers => {
            if client.is_some() {
                app.request_screen(ScreenRequest::McpServers);
            } else {
                app.status_message = "Not connected — MCP servers manager unavailable".into();
            }
        }
        Action::OpenModelPicker => {
            if client.is_some() {
                app.request_screen(ScreenRequest::ModelPicker);
            } else {
                app.status_message = "Not connected — model picker unavailable".into();
            }
        }
        Action::OpenPersonalityPicker => {
            if client.is_none() {
                app.status_message = "Not connected — personality picker unavailable".into();
            } else if app.current_conversation().is_none() {
                app.status_message =
                    "Open a conversation first (Enter) — personality is per-conversation".into();
            } else {
                app.request_screen(ScreenRequest::PersonalityPicker);
            }
        }
        Action::ToggleTasksPane => {
            app.toggle_tasks_pane();
            app.status_message = if app.tasks.visible {
                "Tasks pane open (j/k navigate · c cancel · Enter open conv · Ctrl+P close)".into()
            } else {
                "Tasks pane closed".into()
            };
        }
        Action::NextTask => app.tasks.move_selection(1),
        Action::PreviousTask => app.tasks.move_selection(-1),
        Action::CancelSelectedTask => {
            if let Some(id) = app.request_cancel_selected_task()
                && let Some(conn) = connector.as_ref()
                // Only proceed when the transport offers a command channel.
                && conn.client().as_commands().is_some()
            {
                let conn = Rc::clone(conn);
                let task_id = id.0.clone();
                in_flight.push(async move {
                    let Some(commands) = conn.client().as_commands() else {
                        return RpcOutcome::Noop;
                    };
                    let cmd = desktop_assistant_api_model::Command::CancelBackgroundTask {
                        id: task_id.clone(),
                    };
                    // Success is resolved authoritatively by the `TaskCompleted`
                    // signal (status moves to "Cancelling..." then resolves), so
                    // only a failed cancel command produces an outcome here.
                    match commands.send_command(cmd).await {
                        Ok(_) => RpcOutcome::Noop,
                        Err(e) => RpcOutcome::TaskCancelFailed {
                            task_id,
                            error: e.to_string(),
                        },
                    }
                });
            }
        }
        Action::OpenSelectedTaskConversation => {
            if let Some(conv_id) = app.jump_to_selected_task_conversation()
                && let Some(conn) = connector.as_ref()
            {
                let conn = Rc::clone(conn);
                in_flight.push(async move {
                    RpcOutcome::ConversationOpened {
                        result: conn
                            .client()
                            .get_conversation(&conv_id)
                            .await
                            .map_err(|e| e.to_string()),
                        enter_editing: false,
                    }
                });
            }
        }
    }
}

/// One utterance to narrate, with the backends resolved at enqueue time
/// (adele-tui#77 / TUI-11). Carried over the narration queue so the single
/// serializing task can speak it; the backend handles are captured here (the
/// daemon controller is cheap to clone, the embedded `Speaker` shares `Arc`s) so
/// the queue task needs no shared mutable view of voice state.
struct NarrationRequest {
    voice: Option<VoiceController>,
    embedded: Option<adele_voice_module::Speaker<adele_voice_module::TtsBackend>>,
    text: String,
}

/// Enqueue `text` onto the single narration queue (TUI-11) so it plays after any
/// in-flight utterance rather than interleaving with it. The backends are
/// resolved now (daemon clone + the current embedded speaker, if any). A closed
/// queue (app shutting down) silently drops the request — narration is a
/// convenience, never load-bearing.
fn enqueue_narration(
    tx: &UnboundedSender<NarrationRequest>,
    voice_daemon: &VoiceController,
    voice_session: &Option<VoiceSession>,
    text: String,
) {
    let _ = tx.send(NarrationRequest {
        voice: Some(voice_daemon.clone()),
        embedded: voice_session.as_ref().map(VoiceSession::speaker),
        text,
    });
}

/// Speak `text` aloud, daemon-first and chunked (adele-tui#77, mirroring
/// adele-gtk#80's `window::speak_text`).
///
/// The narration queue loop (TUI-11) invokes this one utterance at a time, so
/// reply narration and `say_this` asides never overlap on the sink. It is the
/// single entry point where routing + chunking live, in three steps:
///
/// 1. **Chunk.** `text` is split into one-short-sentence-per-call pieces via
///    [`voice::into_speakable_sentences`]. Both backends' synth is one-shot with
///    a ~20s per-synth timeout, so feeding a long reply whole would blow it.
/// 2. **Route, daemon-first.** When a connected voice daemon is available, each
///    sentence goes to its warm `SayText`; otherwise, if the embedded engine is
///    present, to its `Speaker`; otherwise nothing is spoken. The backend is
///    chosen **once** for the whole utterance (not per sentence) so playback
///    never splits across engines.
/// 3. **Order.** Sentences are awaited **sequentially**, so the daemon/embedded
///    sink receives — and plays — them in order; they are never fired unordered.
///
/// Run on the narration queue task so synthesis + playback never block the UI.
/// Errors are logged once (the first failing sentence) and the rest of the
/// utterance is abandoned.
async fn speak_text(
    voice: Option<VoiceController>,
    embedded: Option<adele_voice_module::Speaker<adele_voice_module::TtsBackend>>,
    text: String,
) {
    let sentences = voice::into_speakable_sentences(&text);
    if sentences.is_empty() {
        return;
    }

    // Choose the backend once for the whole utterance: a daemon that has
    // actually connected wins (warm models), else the in-process engine. Probing
    // availability also avoids handing sentences to a daemon that vanished.
    let daemon = match voice {
        Some(controller) if controller.is_available().await => Some(controller),
        _ => None,
    };

    for sentence in sentences {
        let result = if let Some(controller) = &daemon {
            controller.say(&sentence).await
        } else if let Some(speaker) = &embedded {
            speaker.say(&sentence).await.map_err(|e| e.to_string())
        } else {
            // Neither backend present: nothing to speak, and nothing more will
            // become available mid-loop.
            return;
        };
        if let Err(e) = result {
            tracing::warn!("voice playback failed: {e}");
            return;
        }
    }
}

/// Handle a daemon `ClientToolCall` for the TUI's `say_this` tool, then submit
/// a result so the suspended turn resumes (adele-tui#73). The decision is pure
/// (see [`client_tools::dispatch`]); this just performs the side effect — speak
/// via the embedded `Speaker`, or render the text inline when speech is off —
/// and posts the outcome back over the connector. A result is ALWAYS submitted
/// (even on a transport/submit error we log rather than wedge silently).
async fn handle_client_tool_call(
    app: &mut App,
    connector: &Option<Rc<Connector>>,
    voice_daemon: &VoiceController,
    voice_session: &Option<VoiceSession>,
    narration_tx: &UnboundedSender<NarrationRequest>,
    call: client_tools::ClientToolCall,
) {
    // Gate on the call's OWN conversation, not the open one, so the per-
    // conversation controls are honored even if the user has since switched
    // tabs. The say_this aside gate is `Adele ∈ {OnDemand, Always}` (adele-tui#77).
    // A client-hosted MCP tool takes precedence: if the local MCP host owns this
    // tool name it invokes it and submits the result, and we're done. Otherwise
    // fall through to the TUI's built-in client tools (say_this / voice mode).
    if let (Some(host), Some(conn)) = (app.mcp_host.clone(), connector.as_ref())
        && dispatch_client_tool_call(
            host.as_ref(),
            conn.as_ref(),
            &call.task_id,
            &call.tool_call_id,
            &call.tool_name,
            call.arguments.clone(),
        )
        .await
    {
        return;
    }

    let say_this_spoken = app.say_this_spoken_for(&call.conversation_id);
    let outcome = client_tools::dispatch(&call.tool_name, &call.arguments, say_this_spoken);

    match outcome.effect {
        client_tools::ToolEffect::Speak(text) => {
            // Speak the aside daemon-first + chunked through the single narration
            // queue (TUI-11) so it serializes behind any in-flight reply
            // narration instead of interleaving on the sink; enqueueing doesn't
            // block submitting the result. When neither the daemon nor the
            // embedded engine is present there is nothing to speak; degrade to
            // showing the text inline instead of dropping it silently.
            let has_backend = voice_daemon.is_available().await || voice_session.is_some();
            if has_backend {
                // Show the spoken line in the transcript too (voice#126), tagged
                // Spoken, so the user sees what Adele voiced — then speak it.
                app.push_spoken_note(&call.conversation_id, &text);
                enqueue_narration(narration_tx, voice_daemon, voice_session, text);
            } else {
                app.push_speech_disabled_note(&call.conversation_id, &text);
            }
        }
        client_tools::ToolEffect::ShowDisabled(text) => {
            app.push_speech_disabled_note(&call.conversation_id, &text);
        }
        // request_voice / stop_voice (adele-tui#77): the model set the `Adele`
        // output level for its OWN conversation. Apply it to App state here so
        // the pure dispatch stays free of App.
        client_tools::ToolEffect::SetAdeleOutput(level) => {
            app.set_adele_output(&call.conversation_id, level);
        }
        client_tools::ToolEffect::None => {}
    }

    if let Some(conn) = connector.as_ref() {
        if let Err(e) = conn
            .submit_client_tool_result(&call.task_id, &call.tool_call_id, outcome.result)
            .await
        {
            app.status_message = format!("Client tool result submit failed: {e}");
        }
    } else {
        // No live connection to submit through — the turn is already lost to a
        // disconnect; surface it rather than failing silently.
        app.status_message = "Client tool call arrived while disconnected".into();
    }
}

/// First half of bringing a freshly-`Connector::connect`ed connection online
/// (refactor #4): subscribe to its signal stream and load the conversation list.
/// Returns the new `signal_rx` for the event loop.
///
/// Subscribing happens *before* anything else so no early streaming chunk is lost
/// (the connector buffers from subscribe onward). Split from
/// [`finish_connection_init`] so the reconnect path can slot its open-conversation
/// resync between the two — the resync's by-id reselect must run against the list
/// this loads.
async fn subscribe_and_load(app: &mut App, conn: &Connector) -> UnboundedReceiver<SignalEvent> {
    let signal_rx = conn.subscribe();
    match conn.client().list_conversations().await {
        Ok(convs) => app.set_conversations(convs),
        Err(e) => app.status_message = format!("Error loading conversations: {e}"),
    }
    signal_rx
}

/// Second half of bringing a connection online (refactor #4): populate the tasks
/// pane, (re)advertise the TUI's client tools, and show the connection label.
///
/// The daemon scopes client tools per session and replaces the whole set on each
/// connect (adele-tui#73 / desktop-assistant#261 / #231), so this runs on every
/// (re)connect, not just the first. Shared verbatim by the initial-connect and
/// reconnect paths so the two can't drift.
async fn finish_connection_init(app: &mut App, conn: &Connector) {
    init_background_tasks(app, conn.client()).await;
    // (Re)establish the live turn-event subscription for the currently-open
    // conversation (#1 multi-client sync) on every (re)connect — on the initial
    // connect nothing is open yet, so this sends an empty set; on a reconnect the
    // open conversation was already re-fetched by the reconnect resync before this
    // call, so this re-points the daemon's fan-out at it.
    subscribe_to_open_conversation(app, conn).await;
    register_client_tools(
        conn,
        app.mcp_host
            .as_ref()
            .map(|h| h.registrations())
            .unwrap_or_default(),
    )
    .await;
    app.status_message = conn.label().to_string();
}

/// Advertise the TUI's client tools to the daemon: `say_this` (adele-tui#73)
/// plus `request_voice` / `stop_voice` (adele-tui#75). The daemon replaces the
/// whole set each call, so this runs on every (re)connect (#231). Best-effort:
/// voice playback is a convenience, so a failure to register is only logged and
/// never blocks the chat. Over D-Bus (no command channel for client tools) this
/// is expected to fail and is silently skipped.
async fn register_client_tools(conn: &Connector, host_tools: Vec<ClientToolRegistration>) {
    // Merge the TUI's built-in client tools with the MCP host's tools into the
    // single set the daemon expects (it replaces the whole set per call).
    let builtins = vec![
        client_tools::say_this_registration(),
        client_tools::request_voice_registration(),
        client_tools::stop_voice_registration(),
    ];
    if let Err(e) = conn
        .register_client_tools(merge_registrations(builtins, host_tools))
        .await
    {
        tracing::debug!("client tool registration skipped: {e}");
    }
}

/// Result of a one-shot embedded dictation capture, delivered from the capture
/// task back to the event loop.
enum DictationOutcome {
    /// A non-empty transcript was produced.
    Transcribed(String),
    /// The capture ended with no usable speech (timed out, near-silent, or an
    /// empty transcript) — the module returned `None`.
    NoSpeech,
    /// The capture errored (mic open failed, model error, …).
    Failed(String),
}

/// Begin a one-shot dictation capture, if embedded voice is ready and not
/// already capturing. Spawns the mic→VAD→Whisper work on a task and reports the
/// outcome over `dictation_tx`; the UI just shows a "Listening…" indicator.
///
/// Gating order matters: nothing here opens the mic unless voice is in
/// `embedded` mode, the session has loaded, and no capture is already running.
fn start_dictation(
    app: &mut App,
    cfg: &VoiceConfig,
    session: &Option<VoiceSession>,
    dictating: &mut bool,
    dictation_tx: &tokio::sync::mpsc::UnboundedSender<DictationOutcome>,
) {
    if !cfg.embedded_enabled() {
        app.status_message =
            "Voice is off — set mode = \"embedded\" in ~/.config/adele-tui/voice.toml".into();
        return;
    }
    if *dictating {
        app.status_message = "Already listening…".into();
        return;
    }
    let Some(session) = session.as_ref() else {
        app.status_message = "Voice still loading models — try again in a moment".into();
        return;
    };

    *dictating = true;
    // Reuse the transient assistant-status indicator line for "Listening…".
    app.set_assistant_status("Listening…");
    let handle = session.dictation();
    let tx = dictation_tx.clone();
    tokio::spawn(async move {
        // One capture at a time: holding the lock for the whole capture both
        // gives this task the `&mut Dictation` it needs and prevents a second
        // press from opening the mic concurrently.
        let mut dictation = handle.lock().await;
        let outcome = match dictation.dictate().await {
            Ok(Some(text)) => DictationOutcome::Transcribed(text),
            Ok(None) => DictationOutcome::NoSpeech,
            Err(e) => DictationOutcome::Failed(e.to_string()),
        };
        let _ = tx.send(outcome);
    });
}

/// Drop a dictated transcript into the prompt input, ready to send. Switches to
/// editing mode and appends to any text already in the composer (with a
/// separating space) so dictation can extend a partially typed prompt.
fn insert_dictated_text(app: &mut App, text: &str) {
    app.enter_editing_mode();
    let existing = app.textarea_content();
    if !existing.is_empty() && !existing.ends_with(char::is_whitespace) {
        app.textarea.insert_char(' ');
    }
    app.textarea.insert_str(text);
}

/// Send whatever is in the prompt input to the assistant over the current
/// transport. Shared by the keyboard submit (`Enter`) and the dictation path
/// (which appends a transcript to the input, then submits via the same route),
/// so both honor the staged model override and the same ack handling.
///
/// The send *decision* lives in the shared core (`UiMessage::SubmitPrompt`,
/// Phase-2): it runs the streaming/empty gate (TUI-7), draws the user bubble
/// optimistically, and — when accepted — hands back an [`Effect::SendPrompt`]
/// for this executor to run as the actual RPC. Only the connection gate and the
/// transport-specific RPC + override stay here. A failed send rolls the
/// optimistic bubble back via `UiMessage::SendFailed` and refills the composer.
async fn send_prompt_from_input(app: &mut App, connector: &Option<Rc<Connector>>) {
    // Connection gate: transport state the core doesn't own.
    let Some(connector) = connector.as_ref() else {
        app.status_message = "Not connected — message not sent (your text is preserved)".into();
        return;
    };
    let prompt = app.textarea_content();
    let effects = app.apply_core(UiMessage::SubmitPrompt { prompt });
    let Some(Effect::SendPrompt {
        conversation_id,
        prompt,
        system_refinement,
    }) = effects
        .into_iter()
        .find(|e| matches!(e, Effect::SendPrompt { .. }))
    else {
        // Rejected (still streaming / empty / no open conversation): the core
        // already surfaced any status message and left the composer untouched.
        return;
    };
    // Accepted: the user bubble is drawn, so clear the composer and snap to the
    // bottom, then run the RPC. `send_prompt_full` carries the staged model
    // override (socket transports only); over D-Bus the refinement folds into the
    // prompt, so the no-override voice path works everywhere. `system_refinement`
    // is `None` when the conversation's `Adele:` level is Disabled.
    app.clear_composer();
    app.scroll_to_bottom();
    let client = connector.client();
    let refinement = system_refinement.as_deref().unwrap_or("");
    let result = match (app.take_pending_override(), client.as_commands()) {
        (Some(ovr), Some(commands)) => {
            commands
                .send_prompt_full(&conversation_id, &prompt, Some(ovr), refinement.to_string())
                .await
        }
        (Some(_), None) => {
            app.status_message =
                "Model override isn't supported over D-Bus — sent without override".into();
            connector
                .send_prompt_with_system_refinement(&conversation_id, &prompt, refinement)
                .await
        }
        (None, _) => {
            connector
                .send_prompt_with_system_refinement(&conversation_id, &prompt, refinement)
                .await
        }
    };
    match result {
        Ok(task_id) => app.apply_prompt_ack(task_id, conversation_id),
        Err(e) => {
            let _ = app.apply_core(UiMessage::SendFailed {
                conversation_id,
                prompt: prompt.clone(),
            });
            app.set_composer(&prompt);
            app.status_message = format!("Send error: {e} (your text is preserved)");
        }
    }
}

/// Populate the tasks pane with a daemon snapshot and subscribe to live
/// events. Failure is non-fatal — the chat still works without the
/// process-manager pane, so we surface a status hint and move on.
///
/// These commands ride the shared command channel (`as_commands`), so they
/// work over both socket transports (UDS + WS). Over D-Bus the call quietly
/// no-ops; the pane will simply stay empty.
async fn init_background_tasks(
    app: &mut App,
    client: &desktop_assistant_client_common::TransportClient,
) {
    let Some(commands) = client.as_commands() else {
        return;
    };
    let list = desktop_assistant_api_model::Command::ListBackgroundTasks {
        include_finished: false,
        limit: None,
    };
    match commands.send_command(list).await {
        Ok(desktop_assistant_api_model::CommandResult::BackgroundTasks(tasks)) => {
            app.tasks.set_initial(tasks);
        }
        Ok(other) => {
            app.status_message = format!("Unexpected ListBackgroundTasks response: {other:?}");
        }
        Err(e) => {
            app.status_message = format!("Tasks snapshot failed: {e}");
        }
    }
    if let Err(e) = commands
        .send_command(desktop_assistant_api_model::Command::SubscribeBackgroundTasks)
        .await
    {
        app.status_message = format!("Tasks subscribe failed: {e}");
    }
}

/// The set of conversation ids the TUI is currently viewing, for
/// `SubscribeConversations` (#1 live multi-client sync). The TUI shows exactly
/// one conversation at a time, so this is `[open id]` when a conversation is
/// open and empty otherwise. The command is set-replace, so an empty list tells
/// the daemon to stop fanning any conversation's turn events to this connection.
fn open_conversation_ids(app: &App) -> Vec<String> {
    app.current_conversation()
        .map(|c| vec![c.id.clone()])
        .unwrap_or_default()
}

/// Tell the daemon which conversation this connection is viewing (#1 live
/// multi-client sync) so it fans that conversation's turn events
/// (`UserMessageAdded`/`AssistantDelta`/`AssistantCompleted`/`AssistantError`/
/// `AssistantStatus`) here — including turns started by another client or the
/// voice daemon. Sent on (re)connect and whenever the open conversation changes;
/// it is set-replace, so each send carries the WHOLE viewed set.
///
/// Rides the shared command channel (`as_commands`), like
/// [`init_background_tasks`]. Best-effort: a send failure (or a transport that
/// doesn't accept the command) only surfaces a status hint — live sync is an
/// enhancement, and turns this connection initiates still arrive via its own
/// request stream regardless.
async fn subscribe_to_open_conversation(app: &mut App, conn: &Connector) {
    let Some(commands) = conn.client().as_commands() else {
        return;
    };
    let cmd = desktop_assistant_api_model::Command::SubscribeConversations {
        conversation_ids: open_conversation_ids(app),
    };
    if let Err(e) = commands.send_command(cmd).await {
        app.status_message = format!("Live-sync subscribe failed: {e}");
    }
}

/// Push an off-loop conversation-list refetch onto `in_flight` (#1), honouring
/// the current `show_archived` filter. Reuses the exact refresh path the
/// (un)archive / show-archived toggles use — the future yields
/// [`RpcOutcome::ConversationsRefreshed`], whose handler calls
/// `App::set_conversations`, which replaces ONLY the sidebar list (the open
/// conversation + its transcript are separate state and are left untouched).
/// No status message on success, so a list change elsewhere refreshes silently.
/// A no-op when disconnected — the next (re)connect's `subscribe_and_load`
/// reloads the list anyway.
fn push_conversation_refresh(
    app: &mut App,
    connector: &Option<Rc<Connector>>,
    in_flight: &mut InFlight<'static, RpcOutcome>,
) {
    let Some(conn) = connector.as_ref() else {
        return;
    };
    let conn = Rc::clone(conn);
    let show_archived = app.show_archived;
    in_flight.push(async move {
        RpcOutcome::ConversationsRefreshed {
            list: fetch_conversations(conn.client(), show_archived)
                .await
                .map_err(|e| e.to_string()),
            on_success: None,
        }
    });
}

async fn fetch_conversations(
    client: &desktop_assistant_client_common::TransportClient,
    include_archived: bool,
) -> Result<Vec<desktop_assistant_client_common::ConversationSummary>> {
    if include_archived {
        client.list_conversations_with_archived().await
    } else {
        client.list_conversations().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        let mut out = vec!["adele".to_string()];
        out.extend(parts.iter().map(|value| value.to_string()));
        out
    }

    #[test]
    fn log_filter_scales_level_with_verbosity_and_quiets_third_party() {
        assert!(
            log_filter(0).contains("adele=warn"),
            "no -v => our crates stay at warn"
        );
        assert!(log_filter(1).contains("adele=info"), "one -v => info");
        assert!(log_filter(2).contains("adele=debug"), "two -v => debug");
        assert!(log_filter(3).contains("adele=trace"), "three -v => trace");
        assert!(log_filter(9).contains("adele=trace"), "saturates at trace");
        // Our client crates track the same level so streaming/registration is visible.
        assert!(log_filter(2).contains("desktop_assistant_client_common=debug"));
        // Third-party noise stays at warn regardless of verbosity.
        assert!(
            log_filter(3).starts_with("warn,"),
            "base directive keeps third-party at warn"
        );
    }

    #[test]
    fn clap_parses_transport_flags() {
        let parsed = CliArgs::try_parse_from(args(&[
            "--transport",
            "dbus",
            "--ws-url",
            "wss://example/ws",
            "--ws-subject",
            "custom-client",
        ]))
        .unwrap();

        assert_eq!(parsed.global.transport, CliTransportMode::Dbus);
        assert_eq!(parsed.global.ws_url, "wss://example/ws");
        assert_eq!(parsed.global.ws_subject, "custom-client");
    }

    #[test]
    fn clap_default_with_no_flags_is_local_uds() {
        let cli = CliArgs::try_parse_from(args(&[])).unwrap();
        // No subcommand => interactive TUI.
        assert!(cli.command.is_none());
        let config = ConnectionConfig::from(cli.global);
        // UDS is now the default connector.
        assert_eq!(config.transport_mode, TransportMode::Uds);
        assert_eq!(config.socket_path, None); // None => daemon default socket
        assert_eq!(config.ws_subject, DEFAULT_WS_SUBJECT);
        assert_eq!(config.ws_jwt, None);
        assert_eq!(config.ws_login_username, None);
        assert_eq!(config.ws_login_password, None);
    }

    #[test]
    fn socket_flag_without_value_selects_uds_default_path() {
        let cli = CliArgs::try_parse_from(args(&["--socket"])).unwrap();
        let config = ConnectionConfig::from(cli.global);
        assert_eq!(config.transport_mode, TransportMode::Uds);
        assert_eq!(config.socket_path, None);
    }

    #[test]
    fn socket_flag_with_path_sets_socket_path() {
        let cli = CliArgs::try_parse_from(args(&["--socket=/tmp/custom.sock"])).unwrap();
        let config = ConnectionConfig::from(cli.global);
        assert_eq!(config.transport_mode, TransportMode::Uds);
        assert_eq!(config.socket_path, Some(PathBuf::from("/tmp/custom.sock")));
    }

    #[test]
    fn ws_flag_with_url_selects_websocket() {
        let cli = CliArgs::try_parse_from(args(&["--ws=wss://host/ws"])).unwrap();
        let config = ConnectionConfig::from(cli.global);
        assert_eq!(config.transport_mode, TransportMode::Ws);
        assert_eq!(config.ws_url, "wss://host/ws");
        assert_eq!(config.socket_path, None);
    }

    #[test]
    fn ws_flag_without_value_falls_back_to_default_ws_url() {
        let cli = CliArgs::try_parse_from(args(&["--ws"])).unwrap();
        let config = ConnectionConfig::from(cli.global);
        assert_eq!(config.transport_mode, TransportMode::Ws);
        assert_eq!(config.ws_url, DEFAULT_WS_URL);
    }

    #[test]
    fn socket_and_ws_are_mutually_exclusive() {
        let error = CliArgs::try_parse_from(args(&["--socket", "--ws=wss://x/ws"]))
            .expect_err("--socket and --ws must conflict");
        assert!(error.to_string().contains("cannot be used with"));
    }

    #[test]
    fn dbus_transport_still_selectable() {
        // No regression: --transport dbus continues to map to D-Bus.
        let cli = CliArgs::try_parse_from(args(&["--transport", "dbus"])).unwrap();
        let config = ConnectionConfig::from(cli.global);
        assert_eq!(config.transport_mode, TransportMode::Dbus);
    }

    #[test]
    fn transport_ws_with_ws_url_still_works() {
        // No regression: the explicit ws transport + --ws-url path.
        let cli = CliArgs::try_parse_from(args(&["--transport", "ws", "--ws-url", "wss://h/ws"]))
            .unwrap();
        let config = ConnectionConfig::from(cli.global);
        assert_eq!(config.transport_mode, TransportMode::Ws);
        assert_eq!(config.ws_url, "wss://h/ws");
    }

    #[test]
    fn clap_rejects_invalid_transport_value() {
        let error = CliArgs::try_parse_from(args(&["--transport", "http"]))
            .expect_err("transport should be validated by clap");
        let rendered = error.to_string();
        assert!(rendered.contains("ws"));
        assert!(rendered.contains("dbus"));
    }

    // --- Subcommand structure (adele-tui#122) ---

    #[test]
    fn bare_args_have_no_subcommand() {
        // Back-compat: `adele` (with only global flags) still means the
        // interactive TUI — no subcommand.
        let cli = CliArgs::try_parse_from(args(&["--socket"])).unwrap();
        assert!(cli.command.is_none(), "bare adele has no subcommand");
    }

    #[test]
    fn exec_subcommand_parses_prompt() {
        let cli = CliArgs::try_parse_from(args(&["exec", "hello there"])).unwrap();
        match cli.command {
            Some(Command::Exec { prompt }) => assert_eq!(prompt, "hello there"),
            other => panic!("expected Exec, got {other:?}"),
        }
    }

    #[test]
    fn prompt_alias_maps_to_exec() {
        // `prompt` is a subcommand alias of `exec`.
        let cli = CliArgs::try_parse_from(args(&["prompt", "hi"])).unwrap();
        assert!(matches!(cli.command, Some(Command::Exec { prompt }) if prompt == "hi"));
    }

    #[test]
    fn legacy_prompt_flag_still_parses() {
        // Back-compat: the deprecated `--prompt <TEXT>` global flag still parses
        // (no subcommand) and carries the prompt for the headless path.
        let cli = CliArgs::try_parse_from(args(&["--prompt", "legacy"])).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.global.prompt.as_deref(), Some("legacy"));
    }

    #[test]
    fn exec_accepts_global_flags_before_it() {
        // Globals are parsed at the top level, so they precede the subcommand.
        let cli = CliArgs::try_parse_from(args(&["--ws=wss://h/ws", "exec", "hi"])).unwrap();
        assert!(matches!(&cli.command, Some(Command::Exec { prompt }) if prompt == "hi"));
        let config = ConnectionConfig::from(cli.global);
        assert_eq!(config.transport_mode, TransportMode::Ws);
    }

    #[test]
    fn config_mcp_list_parses() {
        let cli = CliArgs::try_parse_from(args(&["config", "mcp", "list"])).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Config(ConfigArgs {
                command: ConfigCommand::Mcp(McpArgs {
                    command: McpCommand::List
                })
            }))
        ));
    }

    #[test]
    fn config_path_and_show_parse() {
        let path = CliArgs::try_parse_from(args(&["config", "path"])).unwrap();
        assert!(matches!(
            path.command,
            Some(Command::Config(ConfigArgs {
                command: ConfigCommand::Path
            }))
        ));

        let show = CliArgs::try_parse_from(args(&["config", "show", "--section", "mcp"])).unwrap();
        match show.command {
            Some(Command::Config(ConfigArgs {
                command: ConfigCommand::Show { section },
            })) => assert_eq!(section.as_deref(), Some("mcp")),
            other => panic!("expected config show, got {other:?}"),
        }
    }

    #[test]
    fn config_mcp_add_server_parses_flags() {
        let cli = CliArgs::try_parse_from(args(&[
            "config",
            "mcp",
            "add-server",
            "notes",
            "--command",
            "notes-mcp",
            "--arg",
            "serve",
            "--arg",
            "--root=/x",
            "--namespace",
            "nt",
            "--surface",
            "tui",
            "--enabled",
        ]))
        .unwrap();
        match cli.command {
            Some(Command::Config(ConfigArgs {
                command:
                    ConfigCommand::Mcp(McpArgs {
                        command:
                            McpCommand::AddServer {
                                name,
                                command,
                                arg,
                                namespace,
                                surface,
                                enabled,
                            },
                    }),
            })) => {
                assert_eq!(name, "notes");
                assert_eq!(command, "notes-mcp");
                assert_eq!(arg, vec!["serve".to_string(), "--root=/x".to_string()]);
                assert_eq!(namespace.as_deref(), Some("nt"));
                assert_eq!(surface, vec!["tui".to_string()]);
                assert!(enabled);
            }
            other => panic!("expected add-server, got {other:?}"),
        }
    }

    #[test]
    fn config_mcp_enable_defaults_surface_to_tui() {
        let cli = CliArgs::try_parse_from(args(&["config", "mcp", "enable", "notes"])).unwrap();
        match cli.command {
            Some(Command::Config(ConfigArgs {
                command:
                    ConfigCommand::Mcp(McpArgs {
                        command: McpCommand::Enable { name, surface },
                    }),
            })) => {
                assert_eq!(name, "notes");
                assert_eq!(surface, "tui", "surface defaults to tui");
            }
            other => panic!("expected enable, got {other:?}"),
        }
    }

    // --- Panic hook (TUI-1) ---

    #[test]
    fn panic_hook_chains_the_previously_installed_hook() {
        // Acceptance: installing our hook must not swallow the previous one —
        // the default hook's backtrace/message printing has to still run after
        // the terminal is restored.
        use std::sync::atomic::{AtomicBool, Ordering};
        static PREV_CALLED: AtomicBool = AtomicBool::new(false);

        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| PREV_CALLED.store(true, Ordering::SeqCst)));
        install_panic_hook();

        let result = std::panic::catch_unwind(|| panic!("deliberate test panic"));
        assert!(result.is_err());

        // Put the original hook back before asserting so a failure here
        // doesn't leave the silent test hook installed for other tests.
        let _ = std::panic::take_hook();
        std::panic::set_hook(original);

        assert!(
            PREV_CALLED.load(Ordering::SeqCst),
            "previous panic hook must be chained, not replaced"
        );
    }

    // --- Reconnect backoff tests ---

    #[test]
    fn next_backoff_doubles_until_cap() {
        assert_eq!(next_backoff(2), 4);
        assert_eq!(next_backoff(4), 8);
        assert_eq!(next_backoff(8), 16);
        assert_eq!(next_backoff(16), 30);
    }

    #[test]
    fn next_backoff_caps_at_30() {
        assert_eq!(next_backoff(30), 30);
        assert_eq!(next_backoff(60), 30);
    }

    #[test]
    fn schedule_reconnect_starts_at_initial_when_no_prev() {
        let s = schedule_reconnect(None);
        match s {
            ReconnectState::Pending { delay_secs, .. } => {
                assert_eq!(delay_secs, RECONNECT_INITIAL_SECS);
            }
            _ => panic!("expected Pending"),
        }
    }

    #[test]
    fn schedule_reconnect_doubles_from_prev() {
        let s = schedule_reconnect(Some(4));
        match s {
            ReconnectState::Pending { delay_secs, .. } => assert_eq!(delay_secs, 8),
            _ => panic!("expected Pending"),
        }
    }

    // --- Live conversation subscription (#1 multi-client sync) ---
    //
    // `open_conversation_ids` computes the `SubscribeConversations` set the TUI
    // sends on (re)connect and on every conversation switch. The TUI views one
    // conversation at a time, so the set is `[open id]` or empty.

    fn detail(id: &str) -> ConversationDetail {
        ConversationDetail {
            id: id.into(),
            title: format!("Conv {id}"),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        }
    }

    #[test]
    fn open_conversation_ids_is_empty_with_nothing_open() {
        let app = App::new();
        assert!(app.current_conversation().is_none());
        // Set-replace semantics: an empty set tells the daemon to stop fanning
        // any conversation's turn events here (e.g. the initial connect, before
        // anything is opened).
        assert!(open_conversation_ids(&app).is_empty());
    }

    #[test]
    fn open_conversation_ids_is_the_single_open_conversation() {
        let mut app = App::new();
        app.load_conversation(detail("conv-42"));
        assert_eq!(open_conversation_ids(&app), vec!["conv-42".to_string()]);
    }

    #[test]
    fn open_conversation_ids_follows_a_switch() {
        let mut app = App::new();
        app.load_conversation(detail("conv-1"));
        assert_eq!(open_conversation_ids(&app), vec!["conv-1".to_string()]);
        // Switching conversations re-points the subscription at the new one (the
        // whole set, since it's set-replace, not a delta).
        app.load_conversation(detail("conv-2"));
        assert_eq!(open_conversation_ids(&app), vec!["conv-2".to_string()]);
    }
}
