mod app;
mod keys;
mod ui;
mod views;

use std::io;

use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use desktop_assistant_client_common::{
    AssistantClient, ConnectionConfig, SignalEvent, TransportClient, TransportMode, api,
    connect_transport, transport::transport_label,
};
use futures::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};

use app::{App, Screen};
use keys::{Action, route_key};

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
    let config = ConnectionConfig::from(CliArgs::parse());

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

    let result = run(&mut terminal, &config).await;

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

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config: &ConnectionConfig,
) -> Result<()> {
    let mut app = App::new();

    let (client, mut signal_rx) = match connect_transport(config).await {
        Ok((transport_client, rx)) => {
            match transport_client.list_conversations().await {
                Ok(convs) => app.set_conversations(convs),
                Err(e) => app.status_message = format!("Error loading conversations: {e}"),
            }
            app.status_message = transport_label(config);
            (Some(transport_client), rx)
        }
        Err(e) => {
            app.status_message = format!("Connection failed: {e}");
            (None, tokio::sync::mpsc::unbounded_channel().1)
        }
    };

    let mut event_stream = crossterm::event::EventStream::new();

    loop {
        terminal.draw(|f| ui::draw(f, &mut app))?;

        if app.should_quit {
            break;
        }

        tokio::select! {
            Some(Ok(evt)) = event_stream.next() => {
                if let Event::Key(key) = evt {
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    match route_key(key, &app) {
                        Some(action) => handle_action(&mut app, &client, action).await,
                        None => {
                            // Forward unhandled keys to the textarea only when
                            // the chat input is focused.
                            if matches!(app.screen, Screen::Chat)
                                && matches!(app.mode, app::InputMode::Editing)
                                && !app.model_selector.open
                            {
                                app.textarea.input(key);
                            }
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
                        app.status_message.clear();
                    }
                    SignalEvent::Error { request_id, error } => {
                        app.streaming_error(&request_id, &error);
                        app.status_message = format!("Error: {error}");
                    }
                    SignalEvent::Status { request_id: _, message } => {
                        app.status_message = message;
                    }
                    SignalEvent::TitleChanged { conversation_id, title } => {
                        app.update_conversation_title(&conversation_id, &title);
                    }
                    SignalEvent::ConversationWarning { conversation_id, warning } => {
                        app.apply_conversation_warning(&conversation_id, &warning);
                    }
                    SignalEvent::Disconnected { reason } => {
                        app.status_message = format!("Disconnected: {reason}");
                    }
                }
            }
        }
    }

    Ok(())
}

async fn handle_action(
    app: &mut App,
    client: &Option<TransportClient>,
    action: Action,
) {
    match action {
        Action::Quit => app.quit(),
        Action::NextConversation => app.next_conversation(),
        Action::PreviousConversation => app.previous_conversation(),
        Action::OpenConversation => handle_open_conversation(app, client).await,
        Action::DeleteConversation => {
            if let Some(id) = app.delete_selected_conversation()
                && let Some(client) = client.as_ref()
                && let Err(e) = client.delete_conversation(&id).await
            {
                app.status_message = format!("Delete error: {e}");
            }
        }
        Action::NewConversation => handle_new_conversation(app, client).await,
        Action::EnterEditMode => {
            if app.current_conversation.is_some() {
                app.enter_editing_mode();
            } else {
                app.status_message = "Open a conversation first (Enter) or create one (n)".into();
            }
        }
        Action::ExitEditMode => app.enter_normal_mode(),
        Action::SubmitPrompt => handle_submit_prompt(app, client).await,
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
        Action::ArchiveConversation => handle_archive(app, client).await,
        Action::ScrollUp => app.scroll_up(5),
        Action::ScrollDown => app.scroll_down(5),
        Action::ScrollToBottom => app.scroll_to_bottom(),

        // --- Screen switches ---
        Action::OpenConnectionsView => {
            app.switch_to_connections();
            refresh_connections(app, client).await;
        }
        Action::OpenPurposesView => {
            app.switch_to_purposes();
            refresh_purposes(app, client).await;
        }
        Action::BackToChat => app.switch_to_chat(),

        // --- Connections list ---
        Action::ConnectionsNext => app.connections_view.select_next(),
        Action::ConnectionsPrevious => app.connections_view.select_previous(),
        Action::ConnectionsAdd => app.connections_view.start_add(),
        Action::ConnectionsConfigure => app.connections_view.start_configure(),
        Action::ConnectionsRemove => app.connections_view.start_delete(),
        Action::ConnectionsRefreshModels => {
            refresh_models(app, client, true).await;
            let count = app.model_selector.entries.len();
            app.connections_view.status = Some(format!("Refreshed — {count} models available"));
        }

        // --- Connection form ---
        Action::ConnectionsFormCancel => app.connections_view.form = None,
        Action::ConnectionsFormNextField => {
            if let Some(f) = app.connections_view.form.as_mut() {
                f.next_field();
            }
        }
        Action::ConnectionsFormPreviousField => {
            if let Some(f) = app.connections_view.form.as_mut() {
                f.previous_field();
            }
        }
        Action::ConnectionsFormCycleKindNext => {
            if let Some(f) = app.connections_view.form.as_mut() {
                f.cycle_kind_next();
            }
        }
        Action::ConnectionsFormCycleKindPrev => {
            if let Some(f) = app.connections_view.form.as_mut() {
                f.cycle_kind_prev();
            }
        }
        Action::ConnectionsFormInsertChar(ch) => {
            if let Some(f) = app.connections_view.form.as_mut() {
                f.insert_char(ch);
            }
        }
        Action::ConnectionsFormBackspace => {
            if let Some(f) = app.connections_view.form.as_mut() {
                f.backspace();
            }
        }
        Action::ConnectionsFormToggleAutoPull => {
            if let Some(f) = app.connections_view.form.as_mut() {
                f.toggle_auto_pull();
            }
        }
        Action::ConnectionsFormSubmit => handle_connection_form_submit(app, client).await,

        // --- Delete confirm ---
        Action::ConnectionsDeleteConfirm => handle_delete_connection(app, client, false).await,
        Action::ConnectionsDeleteForce => handle_delete_connection(app, client, true).await,
        Action::ConnectionsDeleteCancel => app.connections_view.delete = None,

        // --- Purposes ---
        Action::PurposesNext => app.purposes_view.select_next(),
        Action::PurposesPrevious => app.purposes_view.select_previous(),
        Action::PurposesEdit => app.purposes_view.start_edit(),
        Action::PurposesEditorCancel => app.purposes_view.close_editor(),
        Action::PurposesEditorNextField => {
            if let Some(ed) = app.purposes_view.editor.as_mut() {
                ed.next_field();
            }
        }
        Action::PurposesEditorPreviousField => {
            if let Some(ed) = app.purposes_view.editor.as_mut() {
                ed.previous_field();
            }
        }
        Action::PurposesEditorInsertChar(ch) => {
            if let Some(ed) = app.purposes_view.editor.as_mut() {
                ed.insert_char(ch);
            }
        }
        Action::PurposesEditorBackspace => {
            if let Some(ed) = app.purposes_view.editor.as_mut() {
                ed.backspace();
            }
        }
        Action::PurposesEditorSubmit => handle_purpose_submit(app, client).await,

        // --- Model selector ---
        Action::OpenModelSelector => handle_open_selector(app, client).await,
        Action::ModelSelectorNext => app.model_selector.highlight_next(),
        Action::ModelSelectorPrevious => app.model_selector.highlight_previous(),
        Action::ModelSelectorConfirm => {
            app.apply_model_selection();
        }
        Action::ModelSelectorCancel => app.model_selector.close(),
        Action::ModelSelectorRefresh => refresh_models(app, client, true).await,
    }
}

async fn handle_open_conversation(app: &mut App, client: &Option<TransportClient>) {
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

async fn handle_new_conversation(app: &mut App, client: &Option<TransportClient>) {
    let Some(client) = client.as_ref() else {
        return;
    };
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

async fn handle_submit_prompt(app: &mut App, client: &Option<TransportClient>) {
    let Some((conv_id, prompt, override_sel)) = app.submit_prompt() else {
        return;
    };
    let Some(client) = client.as_ref() else {
        return;
    };
    match client
        .send_prompt_with_override(&conv_id, &prompt, override_sel)
        .await
    {
        Ok(request_id) if request_id.is_empty() => app.start_streaming_without_request_id(),
        Ok(request_id) => app.start_streaming(request_id),
        Err(e) => app.status_message = format!("Send error: {e}"),
    }
}

async fn handle_archive(app: &mut App, client: &Option<TransportClient>) {
    let (Some(client), Some(id)) = (client.as_ref(), app.selected_conversation_id()) else {
        return;
    };
    let id = id.to_string();
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

async fn refresh_connections(app: &mut App, client: &Option<TransportClient>) {
    app.connections_view.loading = true;
    app.connections_view.status = None;
    if let Some(client) = client.as_ref() {
        match client.list_connections().await {
            Ok(list) => app.connections_view.set_connections(list),
            Err(e) => {
                app.connections_view.loading = false;
                app.connections_view.status = Some(format!("Load error: {e}"));
            }
        }
    } else {
        app.connections_view.loading = false;
        app.connections_view.status = Some("Not connected".into());
    }
}

async fn refresh_purposes(app: &mut App, client: &Option<TransportClient>) {
    app.purposes_view.loading = true;
    app.purposes_view.status = None;
    if let Some(client) = client.as_ref() {
        match client.get_purposes().await {
            Ok(v) => app.purposes_view.set_purposes(v),
            Err(e) => {
                app.purposes_view.loading = false;
                app.purposes_view.status = Some(format!("Load error: {e}"));
            }
        }
    } else {
        app.purposes_view.loading = false;
        app.purposes_view.status = Some("Not connected".into());
    }
}

async fn refresh_models(app: &mut App, client: &Option<TransportClient>, force: bool) {
    app.model_selector.loading = true;
    app.model_selector.status = None;
    let Some(client) = client.as_ref() else {
        app.model_selector.loading = false;
        app.model_selector.status = Some("Not connected".into());
        return;
    };
    match client.list_available_models(None, force).await {
        Ok(list) => {
            app.model_selector.set_entries(list);
            // Re-sync highlight to the conversation's current selection.
            let sel = app
                .current_conversation
                .as_ref()
                .and_then(|c| app.conversation_selections.get(&c.id))
                .cloned();
            app.model_selector.highlight_for(sel.as_ref());
        }
        Err(e) => {
            app.model_selector.loading = false;
            app.model_selector.status = Some(format!("Load error: {e}"));
        }
    }
}

async fn handle_connection_form_submit(app: &mut App, client: &Option<TransportClient>) {
    let Some(form) = app.connections_view.form.as_ref().cloned() else {
        return;
    };
    let Some(config) = form.to_api_config() else {
        if let Some(f) = app.connections_view.form.as_mut() {
            f.error = Some("Id is required".into());
        }
        return;
    };
    let Some(transport) = client.as_ref() else {
        if let Some(f) = app.connections_view.form.as_mut() {
            f.error = Some("Not connected".into());
        }
        return;
    };

    let result = if form.existing {
        transport.update_connection(&form.id, config).await
    } else {
        transport.create_connection(&form.id, config).await
    };

    match result {
        Ok(()) => {
            app.connections_view.form = None;
            app.connections_view.status = Some(if form.existing {
                format!("Updated {}", form.id)
            } else {
                format!("Created {}", form.id)
            });
            // Re-list through the same `&Option<TransportClient>`.
            refresh_connections(app, client).await;
        }
        Err(e) => {
            if let Some(f) = app.connections_view.form.as_mut() {
                f.error = Some(format!("{e}"));
            }
        }
    }
}

async fn handle_delete_connection(
    app: &mut App,
    client: &Option<TransportClient>,
    force: bool,
) {
    let Some(prompt) = app.connections_view.delete.as_ref().cloned() else {
        return;
    };
    let Some(transport) = client.as_ref() else {
        app.connections_view.status = Some("Not connected".into());
        return;
    };

    match transport.delete_connection(&prompt.id, force).await {
        Ok(()) => {
            app.connections_view.delete = None;
            app.connections_view.status = Some(format!("Deleted {}", prompt.id));
            refresh_connections(app, client).await;
        }
        Err(e) => {
            let err = format!("{e}");
            if !force {
                if let Some(p) = app.connections_view.delete.as_mut() {
                    p.advance_to_force(err);
                }
            } else {
                app.connections_view.delete = None;
                app.connections_view.status = Some(format!("Delete error: {err}"));
            }
        }
    }
}

async fn handle_purpose_submit(app: &mut App, client: &Option<TransportClient>) {
    let Some(editor) = app.purposes_view.editor.as_ref().cloned() else {
        return;
    };
    let cfg = match editor.to_api_config() {
        Ok(cfg) => cfg,
        Err(e) => {
            if let Some(ed) = app.purposes_view.editor.as_mut() {
                ed.error = Some(e);
            }
            return;
        }
    };
    let Some(transport) = client.as_ref() else {
        if let Some(ed) = app.purposes_view.editor.as_mut() {
            ed.error = Some("Not connected".into());
        }
        return;
    };
    match transport.set_purpose(editor.purpose, cfg).await {
        Ok(()) => {
            app.purposes_view.editor = None;
            app.purposes_view.status = Some(format!(
                "Saved {}",
                views::purposes::purpose_label(editor.purpose)
            ));
            refresh_purposes(app, client).await;
        }
        Err(e) => {
            if let Some(ed) = app.purposes_view.editor.as_mut() {
                ed.error = Some(format!("{e}"));
            }
        }
    }
}

async fn handle_open_selector(app: &mut App, client: &Option<TransportClient>) {
    app.model_selector.open();
    if app.model_selector.entries.is_empty() {
        refresh_models(app, client, false).await;
    } else {
        // Re-sync highlight to the conversation's current selection.
        let sel = app
            .current_conversation
            .as_ref()
            .and_then(|c| app.conversation_selections.get(&c.id))
            .cloned();
        app.model_selector.highlight_for(sel.as_ref());
    }
}

async fn fetch_conversations(
    client: &TransportClient,
    include_archived: bool,
) -> Result<Vec<desktop_assistant_client_common::ConversationSummary>> {
    if include_archived {
        client.list_conversations_with_archived().await
    } else {
        client.list_conversations().await
    }
}

// Silence unused imports when compiling without dbus feature flags in play.
#[allow(dead_code)]
fn _api_sanity_check(_v: api::ConnectionConfigView) {}

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
}
