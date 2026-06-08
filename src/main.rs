//! `adele` terminal UI client binary.
//!
//! Parses CLI arguments, establishes the transport connection to the Adelie
//! daemon, and runs the interactive TUI event loop (chat plus the knowledge
//! base, connections, and purposes management screens).

mod app;
mod client_tools;
mod connections;
mod credentials;
mod kb;
mod keys;
mod markdown;
mod model_selector;
mod oauth;
mod personality_selector;
mod picker;
mod profile;
mod purposes;
mod settings;
mod tasks;
mod toolbar;
mod ui;
mod voice;

use std::io;
use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser, parser::ValueSource};
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use desktop_assistant_client_common::{
    AssistantClient, ConnectionConfig, Connector, SignalEvent, TransportClient, TransportMode,
};
use futures::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::{
    sync::mpsc::{UnboundedReceiver, unbounded_channel},
    time::{Instant, sleep_until},
};

use app::{App, InputMode};
use keys::{Action, handle_key_event};
use picker::PickerOutcome;
use profile::ProfileStore;
use settings::Settings;
use voice::{VoiceConfig, VoiceSession};

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

#[derive(Debug, Parser)]
#[command(name = "adele")]
struct CliArgs {
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
}

impl From<CliArgs> for ConnectionConfig {
    fn from(cli: CliArgs) -> Self {
        let ws_url = {
            let trimmed = cli.ws_url.trim();
            if trimmed.is_empty() {
                DEFAULT_WS_URL.to_string()
            } else {
                trimmed.to_string()
            }
        };

        let ws_subject = {
            let trimmed = cli.ws_subject.trim();
            if trimmed.is_empty() {
                DEFAULT_WS_SUBJECT.to_string()
            } else {
                trimmed.to_string()
            }
        };

        // `--socket` and `--ws` are explicit selectors that override the
        // (always-defaulted) `--transport`. clap makes them mutually
        // exclusive, so at most one is `Some` here.
        let (transport_mode, socket_path, ws_url) = if let Some(path) = cli.socket {
            (TransportMode::Uds, path, ws_url)
        } else if let Some(url) = cli.ws {
            let resolved = match url {
                Some(u) if !u.trim().is_empty() => u.trim().to_string(),
                _ => ws_url,
            };
            (TransportMode::Ws, None, resolved)
        } else {
            let mode = match cli.transport {
                CliTransportMode::Local => TransportMode::Uds,
                CliTransportMode::Ws => TransportMode::Ws,
                CliTransportMode::Dbus => TransportMode::Dbus,
            };
            (mode, None, ws_url)
        };

        Self {
            transport_mode,
            ws_url,
            ws_jwt: None,
            ws_login_username: None,
            ws_login_password: None,
            ws_subject,
            socket_path,
            ..Default::default()
        }
    }
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

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
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

    let result = run_app(&mut terminal, cli, cli_explicit).await;

    // Restore terminal
    disable_raw_mode()?;
    let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
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

/// Per-turn system-prompt refinement sent while a conversation is in voice mode
/// (adele-tui#75). It shapes the reply for the ear without touching the visible
/// transcript or later turns. A deliberately concise echo of the voice daemon's
/// `spoken_response_hint` essence — NOT a copy of its full paragraph.
const VOICE_SYSTEM_REFINEMENT: &str = "This reply will be read aloud, so write it to be heard, \
    not read. Keep it brief and conversational — a few short sentences — and lead with the answer. \
    Use no markdown or formatting of any kind (no asterisks, backticks, bullets, or emoji); write \
    plain spoken prose. Spell out symbols, abbreviations, and acronyms as words (say \"and\" not \
    \"&\", \"percent\" not \"%\", \"for example\" not \"e.g.\"), and don't read out URLs, file \
    paths, or email addresses — describe them instead.";

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
    cli: CliArgs,
    cli_explicit: bool,
) -> Result<()> {
    // First connection: respect explicit CLI/env args; otherwise picker if
    // we have profiles, else fall back to CLI defaults.
    let mut config = if cli_explicit {
        ConnectionConfig::from(cli)
    } else {
        let store = ProfileStore::load();
        if store.profiles.is_empty() {
            ConnectionConfig::from(cli)
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

    // The `Connector` owns the transport AND the signal stream, pumping every
    // `SignalEvent` to its subscribers from a dedicated task (client-common
    // #203). The TUI holds the connector (so reconnect/disconnect can drop and
    // rebuild it) plus one `subscribe()`d receiver that feeds the `select!`
    // loop. Before there's a live connection both are the not-connected
    // sentinels: `connector = None` and a closed `signal_rx`.
    let mut connector: Option<Connector> = None;
    let mut signal_rx: UnboundedReceiver<SignalEvent> = unbounded_channel().1;
    let mut reconnect = ReconnectState::Connected;

    // Initial connect — on failure, fall straight into the backoff loop
    // instead of running with no connection.
    match Connector::connect(config).await {
        Ok(conn) => {
            // Subscribe before any prompt so no early streaming chunk is lost.
            signal_rx = conn.subscribe();
            match conn.client().list_conversations().await {
                Ok(convs) => app.set_conversations(convs),
                Err(e) => app.status_message = format!("Error loading conversations: {e}"),
            }
            init_background_tasks(&mut app, conn.client()).await;
            // Advertise the TUI's `say_this` client tool (adele-tui#73). Scoped
            // to this session via desktop-assistant#261, so a concurrent voice
            // session's tools never fire on a TUI turn.
            register_client_tools(&conn).await;
            app.status_message = conn.label().to_string();
            connector = Some(conn);
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
    // Seed the per-conversation speech toggle's default (adele-tui#73). Existing
    // `play_replies = true` users keep audio (now per-conversation + toggleable
    // via Ctrl+S); everyone else defaults to speech OFF. Only meaningful in
    // embedded mode — there is no `Speaker` to narrate with otherwise.
    if voice_cfg.embedded_enabled() {
        app.set_speech_default(voice_cfg.play_replies);
    }
    let mut voice_session: Option<VoiceSession> = None;
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
        terminal.draw(|f| ui::draw(f, &mut app))?;

        if app.should_quit {
            return Ok(RunOutcome::Quit);
        }
        if app.switch_requested {
            return Ok(RunOutcome::Switch);
        }

        // The reconnect timer is built fresh each loop iteration so that it
        // gets re-armed when state transitions in/out of Pending.
        let next_retry = match &reconnect {
            ReconnectState::Pending { next_at, .. } => Some(*next_at),
            ReconnectState::Connected => None,
        };

        if app.kb_requested {
            app.kb_requested = false;
            if let Some(conn) = connector.as_ref()
                && let Err(e) = kb::run(terminal, conn.client()).await
            {
                app.status_message = format!("KB error: {e}");
            }
            // Force a redraw on the next iteration so the chat reappears
            // immediately instead of waiting for the next event.
            continue;
        }

        if app.connections_requested {
            app.connections_requested = false;
            if let Some(conn) = connector.as_ref()
                && let Err(e) = connections::run(terminal, conn.client()).await
            {
                app.status_message = format!("Connections error: {e}");
            }
            continue;
        }

        if app.purposes_requested {
            app.purposes_requested = false;
            if let Some(conn) = connector.as_ref()
                && let Err(e) = purposes::run(terminal, conn.client()).await
            {
                app.status_message = format!("Purposes error: {e}");
            }
            continue;
        }

        if app.model_picker_requested {
            app.model_picker_requested = false;
            if let Some(conn) = connector.as_ref() {
                let client = conn.client();
                let current = app
                    .current_conversation
                    .as_ref()
                    .and_then(|c| c.model_selection.clone());
                match model_selector::run(terminal, client, current).await {
                    Ok(model_selector::Outcome::Selected(picked)) => {
                        let label = format!("{} · {}", picked.connection_id, picked.model_id);
                        app.apply_model_override(picked);
                        app.status_message = format!("Model: {label} (applies to next message)");
                    }
                    Ok(model_selector::Outcome::Cancelled) => {}
                    Err(e) => app.status_message = format!("Model picker error: {e}"),
                }
            }
            continue;
        }

        if app.personality_picker_requested {
            app.personality_picker_requested = false;
            // Only reachable with a loaded conversation (handle_action gates on
            // `current_conversation`), but re-check so the borrow stays clean.
            if let (Some(conn), Some(conv)) =
                (connector.as_ref(), app.current_conversation.as_ref())
            {
                let conv_id = conv.id.clone();
                let current = conv.conversation_personality;
                match personality_selector::run(terminal, conn.client(), conv_id, current).await {
                    Ok(personality_selector::Outcome::Saved(stored)) => {
                        if let Some(c) = app.current_conversation.as_mut() {
                            c.conversation_personality = Some(stored);
                        }
                        app.status_message = if stored == Default::default() {
                            "Personality cleared (using global)".into()
                        } else {
                            "Personality saved for this conversation".into()
                        };
                    }
                    Ok(personality_selector::Outcome::Cancelled) => {}
                    Err(e) => app.status_message = format!("Personality picker error: {e}"),
                }
            }
            continue;
        }

        tokio::select! {
            Some(Ok(evt)) = event_stream.next() => {
                if let Event::Key(key) = evt {
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    if let Some(action) = handle_key_event(key, &app.mode, app.tasks.visible) {
                        if action == Action::Dictate {
                            start_dictation(
                                &mut app,
                                &voice_cfg,
                                &voice_session,
                                &mut dictating,
                                &dictation_tx,
                            );
                        } else {
                            handle_action(&mut app, &connector, action).await;
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
                match signal {
                    SignalEvent::Chunk { request_id, chunk } => {
                        app.receive_chunk(&request_id, &chunk);
                    }
                    SignalEvent::Complete { request_id, full_response } => {
                        app.complete_streaming(&request_id, &full_response);
                        // Speak the reply aloud (embedded TTS, no daemon) only
                        // when the OPEN conversation has audio on — read-aloud OR
                        // voice mode (adele-tui#73/#75). The completing reply
                        // streams into the open conversation, so that gate is
                        // right. Fire-and-forget on a task so synth+playback
                        // never blocks the UI; `Speaker` is cheap to clone.
                        if let Some(session) = voice_session.as_ref()
                            && app.current_audio_enabled()
                            && !full_response.trim().is_empty()
                        {
                            let speaker = session.speaker();
                            tokio::spawn(async move {
                                if let Err(e) = speaker.say(&full_response).await {
                                    tracing::warn!("voice playback failed: {e}");
                                }
                            });
                        }
                    }
                    SignalEvent::Error { request_id, error } => {
                        app.streaming_error(&request_id, &error);
                        app.status_message = format!("Error: {error}");
                    }
                    SignalEvent::Status { request_id: _, message } => {
                        app.set_assistant_status(message);
                    }
                    SignalEvent::TitleChanged { conversation_id, title } => {
                        app.update_conversation_title(&conversation_id, &title);
                    }
                    SignalEvent::Disconnected { reason } => {
                        // Drop the connector (closing its transport + fanout
                        // task) and reset to the not-connected sentinel receiver
                        // so the backoff loop owns reconnection.
                        connector = None;
                        signal_rx = unbounded_channel().1;
                        reconnect = schedule_reconnect(None);
                        app.status_message = format!(
                            "Disconnected: {reason}. Reconnecting in {RECONNECT_INITIAL_SECS}s..."
                        );
                    }
                    SignalEvent::ConversationWarning { warning, .. } => {
                        // Currently only the dangling-model-selection warning is
                        // emitted. Surface a hint in the status bar; richer
                        // handling (auto-pick fallback, etc.) belongs upstream
                        // with the per-conversation model selector (#1).
                        app.status_message = format!("Warning: {warning:?}");
                    }
                    // --- Background task events (issue #45 / desktop-assistant#114) ---
                    //
                    // Each event is forwarded verbatim into `app.tasks`. The
                    // map's invariants are enforced inside `TaskPane` so this
                    // call site stays one-liner-thin.
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
                        // Clear the cancel spinner if we were waiting on this
                        // task; the terminal event is the authoritative
                        // resolution.
                        if app.pending_task_cancel.as_ref().map(|t| t.0.as_str()) == Some(id.as_str()) {
                            app.pending_task_cancel = None;
                        }
                        app.tasks.apply_task_completed(&id);
                    }
                    // The TUI has no scratchpad pane (that lives in the GTK/KDE
                    // clients), so the change notification is a no-op here.
                    SignalEvent::ScratchpadChanged { .. } => {}
                    // The daemon suspended a turn on a client-local tool (#107)
                    // — the TUI registers `say_this` (adele-tui#73). Dispatch
                    // it, perform the side effect (speak / show inline), and
                    // ALWAYS submit a result so the parked turn resumes. With
                    // the per-session registry (desktop-assistant#261) a
                    // concurrent voice session's tools no longer fire here.
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
                        handle_client_tool_call(&mut app, &connector, &voice_session, call).await;
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
                        // Subscribe before any prompt so no early chunk is lost.
                        signal_rx = conn.subscribe();
                        match conn.client().list_conversations().await {
                            Ok(convs) => app.set_conversations(convs),
                            Err(e) => app.status_message = format!("Error loading conversations: {e}"),
                        }
                        init_background_tasks(&mut app, conn.client()).await;
                        // Re-advertise client tools — the daemon replaces the
                        // per-session set on each connect (adele-tui#73 / #231).
                        register_client_tools(&conn).await;
                        reconnect = ReconnectState::Connected;
                        app.status_message = conn.label().to_string();
                        connector = Some(conn);
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
        }
    }
}

async fn handle_action(app: &mut App, connector: &Option<Connector>, action: Action) {
    // The action handlers all want the transport client; pull it out once as an
    // `Option<&TransportClient>` so each arm reads the same as before.
    let client: Option<&TransportClient> = connector.as_ref().map(Connector::client);
    match action {
        Action::Quit => app.quit(),
        Action::NextConversation => app.next_conversation(),
        Action::PreviousConversation => app.previous_conversation(),
        Action::OpenConversation => {
            if let (Some(client), Some(id)) = (client, app.selected_conversation_id()) {
                let id = id.to_string();
                match client.get_conversation(&id).await {
                    Ok(detail) => {
                        app.load_conversation(detail);
                        app.enter_editing_mode();
                    }
                    Err(e) => app.status_message = format!("Error: {e}"),
                }
            }
        }
        Action::DeleteConversation => {
            if let Some(id) = app.delete_selected_conversation()
                && let Some(client) = client
                && let Err(e) = client.delete_conversation(&id).await
            {
                app.status_message = format!("Delete error: {e}");
            }
        }
        Action::NewConversation => {
            if let Some(client) = client {
                match client.create_conversation("New Conversation").await {
                    Ok(id) => {
                        match fetch_conversations(client, app.show_archived).await {
                            Ok(convs) => {
                                let new_idx = convs.iter().position(|c| c.id == id);
                                app.set_conversations(convs);
                                if let Some(idx) = new_idx {
                                    app.selected_conversation = Some(idx);
                                }
                            }
                            Err(e) => app.status_message = format!("Error refreshing: {e}"),
                        }
                        match client.get_conversation(&id).await {
                            Ok(detail) => {
                                app.load_conversation(detail);
                                app.enter_editing_mode();
                            }
                            Err(e) => app.status_message = format!("Error opening: {e}"),
                        }
                    }
                    Err(e) => app.status_message = format!("Create error: {e}"),
                }
            }
        }
        Action::EnterEditMode => {
            if app.current_conversation.is_some() {
                app.enter_editing_mode();
            } else {
                app.status_message = "Open a conversation first (Enter) or create one (n)".into();
            }
        }
        Action::ExitEditMode => app.enter_normal_mode(),
        Action::SubmitPrompt => send_prompt_from_input(app, connector).await,
        // Dictation is handled in the event loop (it needs the embedded voice
        // session + a result channel + a spawned capture task — loop-local
        // resources that don't belong in `handle_action`'s signature).
        Action::Dictate => {}
        Action::ToggleSpeech => match app.toggle_current_speech() {
            Some(true) => {
                app.status_message =
                    "Read aloud ON for this conversation (replies read aloud) — Ctrl+S to stop"
                        .into();
            }
            Some(false) => {
                app.status_message = "Read aloud OFF for this conversation".into();
            }
            None => {
                app.status_message =
                    "Open a conversation first — read aloud is per-conversation".into();
            }
        },
        Action::ToggleVoiceMode => match app.toggle_current_voice_mode() {
            Some(true) => {
                app.status_message =
                    "Voice mode ON for this conversation (replies read aloud + kept conversational) \
                     — Ctrl+V to stop"
                        .into();
            }
            Some(false) => {
                app.status_message = "Voice mode OFF for this conversation".into();
            }
            None => {
                app.status_message =
                    "Open a conversation first — voice mode is per-conversation".into();
            }
        },
        Action::InsertNewline => {
            app.textarea.insert_newline();
        }
        Action::ToggleShowArchived => {
            app.show_archived = !app.show_archived;
            if let Some(client) = client {
                match fetch_conversations(client, app.show_archived).await {
                    Ok(convs) => app.set_conversations(convs),
                    Err(e) => app.status_message = format!("Error refreshing: {e}"),
                }
            }
            app.status_message = if app.show_archived {
                "Showing all conversations (including archived)".into()
            } else {
                "Showing active conversations only".into()
            };
        }
        Action::ArchiveConversation => {
            if let (Some(client), Some(id)) = (client, app.selected_conversation_id()) {
                let id = id.to_string();
                // Determine if conversation is currently archived
                let is_archived = app
                    .conversations
                    .get(app.selected_conversation.unwrap_or(0))
                    .is_some_and(|c| c.archived);
                let result = if is_archived {
                    client.unarchive_conversation(&id).await
                } else {
                    client.archive_conversation(&id).await
                };
                match result {
                    Ok(()) => {
                        match fetch_conversations(client, app.show_archived).await {
                            Ok(convs) => app.set_conversations(convs),
                            Err(e) => app.status_message = format!("Error refreshing: {e}"),
                        }
                        app.status_message = if is_archived {
                            "Conversation unarchived".into()
                        } else {
                            "Conversation archived".into()
                        };
                    }
                    Err(e) => app.status_message = format!("Archive error: {e}"),
                }
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
                && let Some(client) = client
            {
                match client.rename_conversation(&id, &title).await {
                    Ok(()) => {
                        app.apply_rename(&id, &title);
                        app.status_message = format!("Renamed to \"{title}\"");
                    }
                    Err(e) => app.status_message = format!("Rename error: {e}"),
                }
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
        Action::SwitchConnection => {
            app.switch_requested = true;
            app.status_message = "Switching connection...".into();
        }
        Action::OpenKnowledgeBase => {
            if client.is_some() {
                app.kb_requested = true;
            } else {
                app.status_message = "Not connected — knowledge base unavailable".into();
            }
        }
        Action::OpenConnections => {
            if client.is_some() {
                app.connections_requested = true;
            } else {
                app.status_message = "Not connected — connections manager unavailable".into();
            }
        }
        Action::OpenPurposes => {
            if client.is_some() {
                app.purposes_requested = true;
            } else {
                app.status_message = "Not connected — purposes manager unavailable".into();
            }
        }
        Action::OpenModelPicker => {
            if client.is_some() {
                app.model_picker_requested = true;
            } else {
                app.status_message = "Not connected — model picker unavailable".into();
            }
        }
        Action::OpenPersonalityPicker => {
            if client.is_none() {
                app.status_message = "Not connected — personality picker unavailable".into();
            } else if app.current_conversation.is_none() {
                app.status_message =
                    "Open a conversation first (Enter) — personality is per-conversation".into();
            } else {
                app.personality_picker_requested = true;
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
                && let Some(client) = client
                && let Some(commands) = client.as_commands()
            {
                let cmd =
                    desktop_assistant_api_model::Command::CancelBackgroundTask { id: id.0.clone() };
                match commands.send_command(cmd).await {
                    Ok(_) => {
                        // Status will move to "Cancelling..." then resolve
                        // when `TaskCompleted { status: Cancelled }` arrives.
                    }
                    Err(e) => {
                        app.status_message = format!("Cancel failed: {e}");
                        app.pending_task_cancel = None;
                    }
                }
            }
        }
        Action::OpenSelectedTaskConversation => {
            if let Some(conv_id) = app.jump_to_selected_task_conversation()
                && let Some(client) = client
            {
                match client.get_conversation(&conv_id).await {
                    Ok(detail) => {
                        app.load_conversation(detail);
                    }
                    Err(e) => app.status_message = format!("Open conversation error: {e}"),
                }
            }
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
    connector: &Option<Connector>,
    voice_session: &Option<VoiceSession>,
    call: client_tools::ClientToolCall,
) {
    // Gate on the call's OWN conversation, not the open one, so the per-
    // conversation controls are honored even if the user has since switched
    // tabs. The say_this audio gate is read-aloud OR voice-mode (adele-tui#75).
    let audio_enabled = app.audio_enabled_for(&call.conversation_id);
    let outcome = client_tools::dispatch(&call.tool_name, &call.arguments, audio_enabled);

    match outcome.effect {
        client_tools::ToolEffect::Speak(text) => {
            // Fire-and-forget playback so we don't block submitting the result.
            if let Some(session) = voice_session.as_ref() {
                let speaker = session.speaker();
                tokio::spawn(async move {
                    if let Err(e) = speaker.say(&text).await {
                        tracing::warn!("say_this playback failed: {e}");
                    }
                });
            } else {
                // Audio enabled but no pipeline (voice off / failed to load):
                // degrade to showing the text rather than dropping it.
                app.push_speech_disabled_note(&call.conversation_id, &text);
            }
        }
        client_tools::ToolEffect::ShowDisabled(text) => {
            app.push_speech_disabled_note(&call.conversation_id, &text);
        }
        // request_voice / stop_voice (adele-tui#75): the model entered/left
        // voice mode for its OWN conversation. Apply it to App state here so the
        // pure dispatch stays free of App.
        client_tools::ToolEffect::SetVoiceMode(on) => {
            app.set_voice_mode(&call.conversation_id, on);
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

/// Advertise the TUI's client tools to the daemon: `say_this` (adele-tui#73)
/// plus `request_voice` / `stop_voice` (adele-tui#75). The daemon replaces the
/// whole set each call, so this runs on every (re)connect (#231). Best-effort:
/// voice playback is a convenience, so a failure to register is only logged and
/// never blocks the chat. Over D-Bus (no command channel for client tools) this
/// is expected to fail and is silently skipped.
async fn register_client_tools(conn: &Connector) {
    if let Err(e) = conn
        .register_client_tools(vec![
            client_tools::say_this_registration(),
            client_tools::request_voice_registration(),
            client_tools::stop_voice_registration(),
        ])
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
async fn send_prompt_from_input(app: &mut App, connector: &Option<Connector>) {
    if let Some((conv_id, prompt)) = app.submit_prompt()
        && let Some(connector) = connector.as_ref()
    {
        let client = connector.client();
        let override_selection = app.take_pending_override();
        // In voice mode (adele-tui#75) carry a concise read-aloud system
        // refinement so the reply is shaped for speech. It only refines the
        // system prompt for THIS turn — never stored, never in the transcript.
        let refinement = if app.voice_mode_for(&conv_id) {
            VOICE_SYSTEM_REFINEMENT
        } else {
            ""
        };
        // Use the command-channel `send_prompt_full` when a model override is
        // staged (model picker) and/or a refinement applies; the high-level
        // `send_prompt` carries neither. `as_commands()` is `Some` on both
        // socket transports (UDS + WS) but `None` on D-Bus. The Connector's
        // refinement helper already folds the refinement into the prompt over
        // D-Bus, so the no-override voice-mode path works everywhere.
        let result = match (override_selection, client.as_commands()) {
            (Some(ovr), Some(commands)) => {
                commands
                    .send_prompt_full(&conv_id, &prompt, Some(ovr), refinement.to_string())
                    .await
            }
            (Some(_), None) => {
                app.status_message =
                    "Model override isn't supported over D-Bus — sent without override".into();
                connector
                    .send_prompt_with_system_refinement(&conv_id, &prompt, refinement)
                    .await
            }
            (None, _) => {
                connector
                    .send_prompt_with_system_refinement(&conv_id, &prompt, refinement)
                    .await
            }
        };
        match result {
            Ok(task_id) => app.apply_prompt_ack(task_id),
            Err(e) => app.status_message = format!("Send error: {e}"),
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

        assert_eq!(parsed.transport, CliTransportMode::Dbus);
        assert_eq!(parsed.ws_url, "wss://example/ws");
        assert_eq!(parsed.ws_subject, "custom-client");
    }

    #[test]
    fn clap_default_with_no_flags_is_local_uds() {
        let cli = CliArgs::try_parse_from(args(&[])).unwrap();
        let config = ConnectionConfig::from(cli);
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
        let config = ConnectionConfig::from(cli);
        assert_eq!(config.transport_mode, TransportMode::Uds);
        assert_eq!(config.socket_path, None);
    }

    #[test]
    fn socket_flag_with_path_sets_socket_path() {
        let cli = CliArgs::try_parse_from(args(&["--socket=/tmp/custom.sock"])).unwrap();
        let config = ConnectionConfig::from(cli);
        assert_eq!(config.transport_mode, TransportMode::Uds);
        assert_eq!(config.socket_path, Some(PathBuf::from("/tmp/custom.sock")));
    }

    #[test]
    fn ws_flag_with_url_selects_websocket() {
        let cli = CliArgs::try_parse_from(args(&["--ws=wss://host/ws"])).unwrap();
        let config = ConnectionConfig::from(cli);
        assert_eq!(config.transport_mode, TransportMode::Ws);
        assert_eq!(config.ws_url, "wss://host/ws");
        assert_eq!(config.socket_path, None);
    }

    #[test]
    fn ws_flag_without_value_falls_back_to_default_ws_url() {
        let cli = CliArgs::try_parse_from(args(&["--ws"])).unwrap();
        let config = ConnectionConfig::from(cli);
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
        let config = ConnectionConfig::from(cli);
        assert_eq!(config.transport_mode, TransportMode::Dbus);
    }

    #[test]
    fn transport_ws_with_ws_url_still_works() {
        // No regression: the explicit ws transport + --ws-url path.
        let cli = CliArgs::try_parse_from(args(&["--transport", "ws", "--ws-url", "wss://h/ws"]))
            .unwrap();
        let config = ConnectionConfig::from(cli);
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
}
