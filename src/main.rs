mod app;
mod connections;
mod credentials;
mod kb;
mod keys;
mod markdown;
mod oauth;
mod picker;
mod model_selector;
mod profile;
mod purposes;
mod settings;
mod tasks;
mod toolbar;
mod ui;

use std::io;

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
    AssistantClient, ConnectionConfig, SignalEvent, TransportClient, TransportMode,
    connect_transport, transport::transport_label,
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

const DEFAULT_WS_URL: &str = desktop_assistant_client_common::config::DEFAULT_WS_URL;
const DEFAULT_WS_SUBJECT: &str = desktop_assistant_client_common::config::DEFAULT_WS_SUBJECT;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lower")]
enum CliTransportMode {
    Ws,
    Dbus,
}

#[derive(Debug, Parser)]
#[command(name = "adele")]
struct CliArgs {
    #[arg(
        long,
        env = "DESKTOP_ASSISTANT_TUI_TRANSPORT",
        value_enum,
        default_value_t = CliTransportMode::Ws
    )]
    transport: CliTransportMode,
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

        let transport_mode = match cli.transport {
            CliTransportMode::Ws => TransportMode::Ws,
            CliTransportMode::Dbus => TransportMode::Dbus,
        };

        Self {
            transport_mode,
            ws_url,
            ws_jwt: None,
            ws_login_username: None,
            ws_login_password: None,
            ws_subject,
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
    ["transport", "ws_url", "ws_subject"]
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

    let mut client: Option<TransportClient> = None;
    let mut signal_rx: UnboundedReceiver<SignalEvent> = unbounded_channel().1;
    let mut reconnect = ReconnectState::Connected;

    // Initial connect — on failure, fall straight into the backoff loop
    // instead of running with no client.
    match connect_transport(config).await {
        Ok((transport_client, rx)) => {
            match transport_client.list_conversations().await {
                Ok(convs) => app.set_conversations(convs),
                Err(e) => app.status_message = format!("Error loading conversations: {e}"),
            }
            init_background_tasks(&mut app, &transport_client).await;
            app.status_message = transport_label(config);
            client = Some(transport_client);
            signal_rx = rx;
        }
        Err(e) => {
            reconnect = schedule_reconnect(None);
            app.status_message =
                format!("Connection failed: {e}. Reconnecting in {RECONNECT_INITIAL_SECS}s...");
        }
    }

    let mut event_stream = crossterm::event::EventStream::new();

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
            if let Some(client) = client.as_ref() {
                if let Err(e) = kb::run(terminal, client).await {
                    app.status_message = format!("KB error: {e}");
                }
            }
            // Force a redraw on the next iteration so the chat reappears
            // immediately instead of waiting for the next event.
            continue;
        }

        if app.connections_requested {
            app.connections_requested = false;
            if let Some(client) = client.as_ref() {
                if let Err(e) = connections::run(terminal, client).await {
                    app.status_message = format!("Connections error: {e}");
                }
            }
            continue;
        }

        if app.purposes_requested {
            app.purposes_requested = false;
            if let Some(client) = client.as_ref() {
                if let Err(e) = purposes::run(terminal, client).await {
                    app.status_message = format!("Purposes error: {e}");
                }
            }
            continue;
        }

        if app.model_picker_requested {
            app.model_picker_requested = false;
            if let Some(client) = client.as_ref() {
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

        tokio::select! {
            Some(Ok(evt)) = event_stream.next() => {
                if let Event::Key(key) = evt {
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    if let Some(action) = handle_key_event(key, &app.mode, app.tasks.visible) {
                        handle_action(&mut app, &client, action).await;
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
                        client = None;
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
                    SignalEvent::TaskCompleted { id, status, last_error } => {
                        // Clear the cancel spinner if we were waiting on this
                        // task. The terminal status (`Cancelled`/`Completed`/
                        // `Failed`) is the authoritative resolution.
                        if app.pending_task_cancel.as_ref().map(|t| t.0.as_str()) == Some(id.as_str()) {
                            app.pending_task_cancel = None;
                        }
                        app.tasks.apply_task_completed(&id, status, last_error);
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
                match connect_transport(config).await {
                    Ok((transport_client, rx)) => {
                        match transport_client.list_conversations().await {
                            Ok(convs) => app.set_conversations(convs),
                            Err(e) => app.status_message = format!("Error loading conversations: {e}"),
                        }
                        init_background_tasks(&mut app, &transport_client).await;
                        client = Some(transport_client);
                        signal_rx = rx;
                        reconnect = ReconnectState::Connected;
                        app.status_message = transport_label(config);
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
        }
    }
}

async fn handle_action(
    app: &mut App,
    client: &Option<desktop_assistant_client_common::TransportClient>,
    action: Action,
) {
    match action {
        Action::Quit => app.quit(),
        Action::NextConversation => app.next_conversation(),
        Action::PreviousConversation => app.previous_conversation(),
        Action::OpenConversation => {
            if let (Some(client), Some(id)) = (client.as_ref(), app.selected_conversation_id()) {
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
                && let Some(client) = client.as_ref()
                && let Err(e) = client.delete_conversation(&id).await
            {
                app.status_message = format!("Delete error: {e}");
            }
        }
        Action::NewConversation => {
            if let Some(client) = client.as_ref() {
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
        Action::SubmitPrompt => {
            if let Some((conv_id, prompt)) = app.submit_prompt()
                && let Some(client) = client.as_ref()
            {
                let override_selection = app.take_pending_override();
                // Use the WS override path when one was staged via the
                // model picker; the trait-level `send_prompt` only takes
                // the bare prompt. D-Bus + override isn't supported yet —
                // we fall back to plain send and warn.
                let result = match (override_selection, client.as_ws()) {
                    (Some(ovr), Some(ws)) => {
                        ws.send_prompt_with_override(&conv_id, &prompt, Some(ovr)).await
                    }
                    (Some(_), None) => {
                        app.status_message =
                            "Model override only works over WebSocket — sent without override"
                                .into();
                        client.send_prompt(&conv_id, &prompt).await
                    }
                    (None, _) => client.send_prompt(&conv_id, &prompt).await,
                };
                match result {
                    Ok(request_id) if request_id.is_empty() => {
                        app.start_streaming_without_request_id()
                    }
                    Ok(request_id) => app.start_streaming(request_id),
                    Err(e) => app.status_message = format!("Send error: {e}"),
                }
            }
        }
        Action::InsertNewline => {
            app.textarea.insert_newline();
        }
        Action::ToggleShowArchived => {
            app.show_archived = !app.show_archived;
            if let Some(client) = client.as_ref() {
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
            if let (Some(client), Some(id)) = (client.as_ref(), app.selected_conversation_id()) {
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
                && let Some(client) = client.as_ref()
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
        Action::ToggleTasksPane => {
            app.toggle_tasks_pane();
            app.status_message = if app.tasks.visible {
                "Tasks pane open (j/k navigate · c cancel · Enter open conv · Ctrl+P close)"
                    .into()
            } else {
                "Tasks pane closed".into()
            };
        }
        Action::NextTask => app.tasks.move_selection(1),
        Action::PreviousTask => app.tasks.move_selection(-1),
        Action::CancelSelectedTask => {
            if let Some(id) = app.request_cancel_selected_task()
                && let Some(client) = client.as_ref()
                && let Some(ws) = client.as_ws()
            {
                let cmd = desktop_assistant_api_model::Command::CancelBackgroundTask {
                    id: id.0.clone(),
                };
                match ws.send_command(cmd).await {
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
                && let Some(client) = client.as_ref()
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

/// Populate the tasks pane with a daemon snapshot and subscribe to live
/// events. Failure is non-fatal — the chat still works without the
/// process-manager pane, so we surface a status hint and move on.
///
/// Both commands only exist over WS today. Over D-Bus the call quietly
/// no-ops; the pane will simply stay empty.
async fn init_background_tasks(
    app: &mut App,
    client: &desktop_assistant_client_common::TransportClient,
) {
    let Some(ws) = client.as_ws() else {
        return;
    };
    let list = desktop_assistant_api_model::Command::ListBackgroundTasks {
        include_finished: false,
        limit: None,
    };
    match ws.send_command(list).await {
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
    if let Err(e) = ws
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
    fn clap_defaults_map_to_runtime_defaults() {
        let cli = CliArgs::try_parse_from(args(&[])).unwrap();
        let config = ConnectionConfig::from(cli);
        assert_eq!(config.transport_mode, TransportMode::Ws);
        assert_eq!(config.ws_url, DEFAULT_WS_URL);
        assert_eq!(config.ws_subject, DEFAULT_WS_SUBJECT);
        assert_eq!(config.ws_jwt, None);
        assert_eq!(config.ws_login_username, None);
        assert_eq!(config.ws_login_password, None);
    }

    #[test]
    fn clap_rejects_invalid_transport_value() {
        let error = CliArgs::try_parse_from(args(&["--transport", "http"]))
            .err()
            .expect("transport should be validated by clap");
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
