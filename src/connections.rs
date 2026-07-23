//! LLM-provider connection management.
//!
//! Modal screen reachable from the chat with `F3`. Lists configured
//! connections and lets the user add, edit, and delete them via the
//! daemon's `ListConnections` / `CreateConnection` / `UpdateConnection` /
//! `DeleteConnection` commands.
//!
//! Keys
//! ----
//!
//! List mode:
//! - `j/k` or arrows: navigate
//! - `Enter` / `e`: edit selected
//! - `a`: add new
//! - `d`: delete selected (with confirm + force-fallback overlay)
//! - `r`: refresh from daemon
//! - `Esc` / `q`: close
//!
//! Edit mode:
//! - `Tab` / `Shift+Tab`: cycle fields
//! - `←` / `→` / `Space` on the Type field: cycle connector type (add only)
//! - `Ctrl+S`: save
//! - `Esc`: cancel
//!
//! Delete-confirm overlay:
//! - `y` / `Enter`: confirm
//! - `f`: force-delete (purposes referencing this connection fall back to
//!   the `interactive` purpose, per the daemon's contract)
//! - `n`/`Esc`: cancel (any other key is ignored)

use std::io;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use desktop_assistant_api_model::{
    Command, CommandResult, ConnectionAvailability, ConnectionConfigView, ConnectionView,
};
use desktop_assistant_client_common::{SignalEvent, TransportClient};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use ratatui_textarea::{CursorMove, TextArea};

use crate::screen::Screen;

use crate::theme::theme;

/// Connector kinds the TUI can build forms for. Mirrors
/// `ConnectionConfigView` variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorKind {
    Anthropic,
    OpenAi,
    OpenRouter,
    Azure,
    Google,
    Bedrock,
    Ollama,
}

impl ConnectorKind {
    const ALL: &'static [ConnectorKind] = &[
        Self::Anthropic,
        Self::OpenAi,
        Self::OpenRouter,
        Self::Azure,
        Self::Google,
        Self::Bedrock,
        Self::Ollama,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
            Self::OpenRouter => "OpenRouter",
            Self::Azure => "Azure",
            Self::Google => "Google",
            Self::Bedrock => "Bedrock",
            Self::Ollama => "Ollama",
        }
    }

    /// Wire tag the daemon uses (`type =` field on `ConnectionConfigView`).
    pub fn tag(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::OpenRouter => "openrouter",
            Self::Azure => "azure",
            Self::Google => "google",
            Self::Bedrock => "bedrock",
            Self::Ollama => "ollama",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.tag() == tag)
    }

    pub fn next(self) -> Self {
        let idx = Self::ALL.iter().position(|k| *k == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        let idx = Self::ALL.iter().position(|k| *k == self).unwrap_or(0);
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    List,
    Edit,
    /// Plain delete confirm; user can promote to force in the overlay.
    DeleteConfirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Id,
    Type,
    ApiKeyEnv,
    BaseUrl,
    AwsProfile,
    Region,
    // Azure-specific knobs.
    ApiSurface,
    ApiVersion,
    // Google (Vertex / Gemini) specific knobs.
    Project,
    Location,
    CredentialsPath,
    // Shared by Azure (api_key|entra) and Google (vertex|api_key); the
    // allowed-value hint is rendered per-kind.
    AuthMode,
}

struct EditForm {
    /// `Some(id)` when editing — id is immutable in that case. `None` for
    /// add — id is freshly typed.
    editing_id: Option<String>,
    focus: Field,
    kind: ConnectorKind,
    id: TextArea<'static>,
    api_key_env: TextArea<'static>,
    base_url: TextArea<'static>,
    aws_profile: TextArea<'static>,
    region: TextArea<'static>,
    api_surface: TextArea<'static>,
    auth_mode: TextArea<'static>,
    api_version: TextArea<'static>,
    project: TextArea<'static>,
    location: TextArea<'static>,
    credentials_path: TextArea<'static>,
}

impl EditForm {
    fn empty() -> Self {
        Self {
            editing_id: None,
            focus: Field::Id,
            kind: ConnectorKind::Anthropic,
            id: single_line_textarea(),
            api_key_env: single_line_textarea(),
            base_url: single_line_textarea(),
            aws_profile: single_line_textarea(),
            region: single_line_textarea(),
            api_surface: single_line_textarea(),
            auth_mode: single_line_textarea(),
            api_version: single_line_textarea(),
            project: single_line_textarea(),
            location: single_line_textarea(),
            credentials_path: single_line_textarea(),
        }
    }

    fn from_view(view: &ConnectionView) -> Self {
        let mut form = Self::empty();
        form.editing_id = Some(view.id.clone());
        form.id.insert_str(&view.id);
        form.id.move_cursor(CursorMove::End);
        form.kind =
            ConnectorKind::from_tag(&view.connector_type).unwrap_or(ConnectorKind::Anthropic);
        // Pre-fill the type-specific fields from the daemon's echoed non-secret
        // config so editing a connection doesn't blank its stored endpoint,
        // region, project, or credential env-var *name*. The view never carries
        // a raw secret value (only the api_key_env name), so this repopulates
        // exactly what CreateConnection / UpdateConnection would accept. When
        // the daemon omits `config` (older daemon), the fields stay blank and
        // the daemon keeps its stored values on save.
        if let Some(config) = &view.config {
            form.prefill_from_config(config);
        }
        form
    }

    /// Populate the per-kind text fields from an echoed [`ConnectionConfigView`].
    /// Each variant is handled explicitly so a newly-added connector can't be
    /// silently swallowed by a wildcard and lose its fields on edit.
    fn prefill_from_config(&mut self, config: &ConnectionConfigView) {
        fn set(ta: &mut TextArea<'static>, value: &Option<String>) {
            if let Some(s) = value {
                ta.insert_str(s.as_str());
                ta.move_cursor(CursorMove::End);
            }
        }
        match config {
            ConnectionConfigView::Anthropic {
                base_url,
                api_key_env,
                ..
            }
            | ConnectionConfigView::OpenAi {
                base_url,
                api_key_env,
                ..
            }
            | ConnectionConfigView::OpenRouter {
                base_url,
                api_key_env,
                ..
            } => {
                set(&mut self.api_key_env, api_key_env);
                set(&mut self.base_url, base_url);
            }
            ConnectionConfigView::Azure {
                base_url,
                api_key_env,
                api_surface,
                auth_mode,
                api_version,
                ..
            } => {
                set(&mut self.base_url, base_url);
                set(&mut self.api_key_env, api_key_env);
                set(&mut self.api_surface, api_surface);
                set(&mut self.auth_mode, auth_mode);
                set(&mut self.api_version, api_version);
            }
            ConnectionConfigView::Google {
                base_url,
                api_key_env,
                project,
                location,
                auth_mode,
                credentials_path,
                ..
            } => {
                set(&mut self.api_key_env, api_key_env);
                set(&mut self.project, project);
                set(&mut self.location, location);
                set(&mut self.auth_mode, auth_mode);
                set(&mut self.credentials_path, credentials_path);
                set(&mut self.base_url, base_url);
            }
            ConnectionConfigView::Bedrock {
                aws_profile,
                region,
                base_url,
                ..
            } => {
                set(&mut self.aws_profile, aws_profile);
                set(&mut self.region, region);
                set(&mut self.base_url, base_url);
            }
            ConnectionConfigView::Ollama { base_url, .. } => {
                set(&mut self.base_url, base_url);
            }
        }
    }

    fn fields_for_kind(kind: ConnectorKind) -> &'static [Field] {
        match kind {
            ConnectorKind::Anthropic | ConnectorKind::OpenAi | ConnectorKind::OpenRouter => {
                &[Field::Id, Field::Type, Field::ApiKeyEnv, Field::BaseUrl]
            }
            // Azure OpenAI: the resource endpoint (base_url) is effectively
            // required, so it leads; api_version only applies to the classic
            // surface but is always offered (the daemon ignores it under v1).
            ConnectorKind::Azure => &[
                Field::Id,
                Field::Type,
                Field::BaseUrl,
                Field::ApiKeyEnv,
                Field::ApiSurface,
                Field::AuthMode,
                Field::ApiVersion,
            ],
            // Google Vertex / Gemini: project + location drive Vertex;
            // credentials_path is the SA JSON for Vertex; base_url is usually
            // blank (Gemini API variant), so it trails.
            ConnectorKind::Google => &[
                Field::Id,
                Field::Type,
                Field::ApiKeyEnv,
                Field::Project,
                Field::Location,
                Field::AuthMode,
                Field::CredentialsPath,
                Field::BaseUrl,
            ],
            ConnectorKind::Bedrock => &[
                Field::Id,
                Field::Type,
                Field::AwsProfile,
                Field::Region,
                Field::BaseUrl,
            ],
            ConnectorKind::Ollama => &[Field::Id, Field::Type, Field::BaseUrl],
        }
    }

    fn next_field(&mut self) {
        let fields = Self::fields_for_kind(self.kind);
        let pos = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        self.focus = fields[(pos + 1) % fields.len()];
    }

    fn prev_field(&mut self) {
        let fields = Self::fields_for_kind(self.kind);
        let pos = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        self.focus = fields[(pos + fields.len() - 1) % fields.len()];
    }

    fn submit(&self) -> Result<(String, ConnectionConfigView), String> {
        let id = self.id.lines().join("").trim().to_string();
        if id.is_empty() {
            return Err("Id is required".into());
        }
        let opt = |ta: &TextArea<'static>| -> Option<String> {
            let s = ta.lines().join("").trim().to_string();
            if s.is_empty() { None } else { Some(s) }
        };

        // The create form doesn't expose the advanced knobs (connect/stream
        // timeouts, the context ceiling, or Ollama's keep-warm flag), so they
        // default to `None` and the daemon applies its own defaults. The TUI's
        // edit path doesn't echo config back either (it pre-fills from id/type
        // only), so there's nothing to round-trip here — `None` is correct for
        // both create and edit.
        let config = match self.kind {
            ConnectorKind::Anthropic => ConnectionConfigView::Anthropic {
                base_url: opt(&self.base_url),
                api_key_env: opt(&self.api_key_env),
                connect_timeout_secs: None,
                stream_timeout_secs: None,
                max_context_tokens: None,
            },
            ConnectorKind::OpenAi => ConnectionConfigView::OpenAi {
                base_url: opt(&self.base_url),
                api_key_env: opt(&self.api_key_env),
                connect_timeout_secs: None,
                stream_timeout_secs: None,
                max_context_tokens: None,
            },
            ConnectorKind::OpenRouter => ConnectionConfigView::OpenRouter {
                base_url: opt(&self.base_url),
                api_key_env: opt(&self.api_key_env),
                connect_timeout_secs: None,
                stream_timeout_secs: None,
                max_context_tokens: None,
            },
            ConnectorKind::Azure => ConnectionConfigView::Azure {
                base_url: opt(&self.base_url),
                api_key_env: opt(&self.api_key_env),
                // Enum knobs: a blank field becomes `None`, letting the daemon
                // apply its primary default (api_surface = v1, auth_mode =
                // api_key). A typed value rides through verbatim for the daemon
                // to validate.
                api_surface: opt(&self.api_surface),
                auth_mode: opt(&self.auth_mode),
                api_version: opt(&self.api_version),
                connect_timeout_secs: None,
                stream_timeout_secs: None,
                max_context_tokens: None,
            },
            ConnectorKind::Google => ConnectionConfigView::Google {
                base_url: opt(&self.base_url),
                api_key_env: opt(&self.api_key_env),
                project: opt(&self.project),
                location: opt(&self.location),
                // Blank auth_mode becomes `None` -> daemon default `vertex`.
                auth_mode: opt(&self.auth_mode),
                credentials_path: opt(&self.credentials_path),
                connect_timeout_secs: None,
                stream_timeout_secs: None,
                max_context_tokens: None,
            },
            ConnectorKind::Bedrock => ConnectionConfigView::Bedrock {
                aws_profile: opt(&self.aws_profile),
                region: opt(&self.region),
                base_url: opt(&self.base_url),
                connect_timeout_secs: None,
                stream_timeout_secs: None,
                max_context_tokens: None,
            },
            ConnectorKind::Ollama => ConnectionConfigView::Ollama {
                base_url: opt(&self.base_url),
                connect_timeout_secs: None,
                stream_timeout_secs: None,
                keep_warm: None,
                max_context_tokens: None,
            },
        };

        Ok((id, config))
    }
}

fn single_line_textarea() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_cursor_line_style(Style::default());
    ta
}

use crate::in_flight::InFlight;

/// Resolved outcome of an off-loop connections RPC (modal-freeze fix). Each
/// variant carries the daemon result (stringified error); `apply_outcome` may
/// chain a follow-up `refresh_list` after a successful save/delete.
enum RpcOutcome {
    Listed(Result<CommandResult, String>),
    Saved(Result<CommandResult, String>),
    Deleted {
        force: bool,
        result: Result<CommandResult, String>,
    },
}

struct State {
    connections: Vec<ConnectionView>,
    selected: usize,
    mode: Mode,
    form: EditForm,
    error: Option<String>,
    busy: Option<String>,
    closing: bool,
}

/// Run the connections screen. Returns when the user closes it.
/// The connections manager as a [`Screen`]: its [`State`] plus the borrowed
/// client. The shared driver supplies the loop and drains daemon signals while
/// the screen is open (TUI-12).
struct ConnectionsScreen<'a> {
    state: State,
    client: &'a TransportClient,
    /// In-flight list/save/delete RPCs, polled off the draw loop by
    /// `poll_pending` so the screen never freezes during a round-trip.
    pending: InFlight<'a, RpcOutcome>,
}

impl Screen for ConnectionsScreen<'_> {
    type Outcome = ();

    fn draw(&mut self, frame: &mut Frame) {
        draw(frame, &self.state);
    }

    fn handle_key(&mut self, key: KeyEvent) -> impl std::future::Future<Output = ()> {
        // Synchronous: RPC-bearing keys enqueue into `pending` rather than
        // awaiting here, so the handler never blocks the draw/input loop.
        match self.state.mode {
            Mode::List => handle_list_key(&mut self.state, key, self.client, &mut self.pending),
            Mode::Edit => handle_edit_key(&mut self.state, key, self.client, &mut self.pending),
            Mode::DeleteConfirm => {
                handle_delete_key(&mut self.state, key, self.client, &mut self.pending)
            }
        }
        std::future::ready(())
    }

    fn take_outcome(&mut self) -> Option<()> {
        self.state.closing.then_some(())
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    async fn poll_pending(&mut self) {
        let resolved = self.pending.next().await;
        if let Some(outcome) = resolved {
            apply_outcome(&mut self.state, &mut self.pending, self.client, outcome);
        }
    }
}

pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    client: &TransportClient,
    signal_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SignalEvent>,
    sink: &mut impl crate::screen::SignalSink,
) -> anyhow::Result<()> {
    let mut screen = ConnectionsScreen {
        state: State {
            connections: Vec::new(),
            selected: 0,
            mode: Mode::List,
            form: EditForm::empty(),
            error: None,
            busy: Some("Loading connections...".into()),
            closing: false,
        },
        client,
        pending: InFlight::new(),
    };

    // Kick the initial load off-loop so "Loading connections…" shows and the
    // screen is responsive while it lands.
    refresh_list(&mut screen.state, &mut screen.pending, client);

    crate::screen::run_screen(terminal, &mut screen, signal_rx, sink).await
}

fn handle_list_key<'a>(
    state: &mut State,
    key: KeyEvent,
    client: &'a TransportClient,
    pending: &mut InFlight<'a, RpcOutcome>,
) {
    match (key.code, key.modifiers) {
        (KeyCode::Esc | KeyCode::Char('q'), m) if m.is_empty() => state.closing = true,
        (KeyCode::Char('j') | KeyCode::Down, m) if m.is_empty() => advance_selection(state, 1),
        (KeyCode::Char('k') | KeyCode::Up, m) if m.is_empty() => advance_selection(state, -1),
        (KeyCode::Enter | KeyCode::Char('e'), m) if m.is_empty() => {
            if let Some(view) = state.connections.get(state.selected).cloned() {
                state.form = EditForm::from_view(&view);
                state.error = None;
                state.mode = Mode::Edit;
            }
        }
        (KeyCode::Char('a'), m) if m.is_empty() => {
            state.form = EditForm::empty();
            state.error = None;
            state.mode = Mode::Edit;
        }
        (KeyCode::Char('d'), m)
            if m.is_empty() && state.connections.get(state.selected).is_some() =>
        {
            state.mode = Mode::DeleteConfirm;
        }
        (KeyCode::Char('r'), m) if m.is_empty() => refresh_list(state, pending, client),
        _ => {}
    }
}

fn handle_edit_key<'a>(
    state: &mut State,
    key: KeyEvent,
    client: &'a TransportClient,
    pending: &mut InFlight<'a, RpcOutcome>,
) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if ctrl && key.code == KeyCode::Char('s') {
        save_edit(state, pending, client);
        return;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            state.error = None;
            state.form = EditForm::empty();
            state.mode = Mode::List;
        }
        (KeyCode::Tab, _) => state.form.next_field(),
        (KeyCode::BackTab, _) => state.form.prev_field(),
        // Type field is special: ←/→ or Space cycle the connector kind, but
        // only on add. On edit the type is locked (the daemon's
        // `UpdateConnection` rejects type changes — easier to delete + add).
        (KeyCode::Left | KeyCode::Right | KeyCode::Char(' '), _)
            if state.form.focus == Field::Type =>
        {
            if state.form.editing_id.is_none() {
                if matches!(key.code, KeyCode::Left) {
                    state.form.kind = state.form.kind.prev();
                } else {
                    state.form.kind = state.form.kind.next();
                }
                // Reset focus to first field of new kind to avoid stranding
                // on a field the new kind doesn't have.
                let fields = EditForm::fields_for_kind(state.form.kind);
                if !fields.contains(&state.form.focus) {
                    state.form.focus = fields[0];
                }
            } else {
                state.error =
                    Some("Type can't be changed on edit — delete and add a new connection".into());
            }
        }
        _ => {
            // Forward all other keys to the focused textarea.
            // Editing the id field is rejected on edit-mode (id immutable).
            match state.form.focus {
                Field::Id => {
                    if state.form.editing_id.is_some() {
                        state.error = Some("Id is immutable on edit".into());
                    } else {
                        state.form.id.input(key);
                    }
                }
                Field::ApiKeyEnv => {
                    state.form.api_key_env.input(key);
                }
                Field::BaseUrl => {
                    state.form.base_url.input(key);
                }
                Field::AwsProfile => {
                    state.form.aws_profile.input(key);
                }
                Field::Region => {
                    state.form.region.input(key);
                }
                Field::ApiSurface => {
                    state.form.api_surface.input(key);
                }
                Field::AuthMode => {
                    state.form.auth_mode.input(key);
                }
                Field::ApiVersion => {
                    state.form.api_version.input(key);
                }
                Field::Project => {
                    state.form.project.input(key);
                }
                Field::Location => {
                    state.form.location.input(key);
                }
                Field::CredentialsPath => {
                    state.form.credentials_path.input(key);
                }
                Field::Type => {}
            }
        }
    }
}

fn handle_delete_key<'a>(
    state: &mut State,
    key: KeyEvent,
    client: &'a TransportClient,
    pending: &mut InFlight<'a, RpcOutcome>,
) {
    match (key.code, key.modifiers) {
        (KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter, _) => {
            do_delete(state, pending, client, false);
        }
        (KeyCode::Char('f') | KeyCode::Char('F'), _) => {
            do_delete(state, pending, client, true);
        }
        // A destructive confirm is dismissed only by an explicit cancel
        // (n/Esc); any other key is ignored rather than silently closing it.
        (KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc, _) => {
            state.mode = Mode::List;
        }
        _ => {}
    }
}

fn do_delete<'a>(
    state: &mut State,
    pending: &mut InFlight<'a, RpcOutcome>,
    client: &'a TransportClient,
    force: bool,
) {
    let Some(view) = state.connections.get(state.selected).cloned() else {
        state.mode = Mode::List;
        return;
    };
    state.busy = Some(if force {
        "Deleting (force)...".into()
    } else {
        "Deleting...".into()
    });
    let id = view.id.clone();
    pending.push(async move {
        RpcOutcome::Deleted {
            force,
            result: send(client, Command::DeleteConnection { id, force })
                .await
                .map_err(|e| e.to_string()),
        }
    });
}

fn advance_selection(state: &mut State, delta: i32) {
    let len = state.connections.len();
    if len == 0 {
        return;
    }
    let mut idx = state.selected as i32 + delta;
    if idx < 0 {
        idx = (len as i32) - 1;
    }
    if idx >= len as i32 {
        idx = 0;
    }
    state.selected = idx as usize;
}

fn refresh_list<'a>(
    state: &mut State,
    pending: &mut InFlight<'a, RpcOutcome>,
    client: &'a TransportClient,
) {
    state.busy = Some("Loading connections...".into());
    pending.push(async move {
        RpcOutcome::Listed(
            send(client, Command::ListConnections)
                .await
                .map_err(|e| e.to_string()),
        )
    });
}

fn save_edit<'a>(
    state: &mut State,
    pending: &mut InFlight<'a, RpcOutcome>,
    client: &'a TransportClient,
) {
    let (id, config) = match state.form.submit() {
        Ok(parts) => parts,
        Err(e) => {
            state.error = Some(e);
            return;
        }
    };

    state.busy = Some("Saving...".into());
    let cmd = if let Some(existing_id) = state.form.editing_id.clone() {
        Command::UpdateConnection {
            id: existing_id,
            config,
        }
    } else {
        Command::CreateConnection { id, config }
    };

    pending
        .push(async move { RpcOutcome::Saved(send(client, cmd).await.map_err(|e| e.to_string())) });
}

/// Apply a resolved connections RPC; chains a `refresh_list` after a successful
/// save or delete (mirroring the old inline `refresh_list().await`).
fn apply_outcome<'a>(
    state: &mut State,
    pending: &mut InFlight<'a, RpcOutcome>,
    client: &'a TransportClient,
    outcome: RpcOutcome,
) {
    state.busy = None;
    match outcome {
        RpcOutcome::Listed(result) => match result {
            Ok(CommandResult::Connections(conns)) => {
                state.connections = conns;
                if state.selected >= state.connections.len() {
                    state.selected = state.connections.len().saturating_sub(1);
                }
            }
            Ok(other) => state.error = Some(format!("Unexpected response: {other:?}")),
            Err(e) => state.error = Some(format!("Failed to load connections: {e}")),
        },
        RpcOutcome::Saved(result) => match result {
            Ok(_) => {
                state.error = None;
                state.form = EditForm::empty();
                state.mode = Mode::List;
                refresh_list(state, pending, client);
            }
            Err(e) => state.error = Some(format!("Save failed: {e}")),
        },
        RpcOutcome::Deleted { force, result } => match result {
            Ok(_) => {
                state.mode = Mode::List;
                refresh_list(state, pending, client);
            }
            Err(msg) => {
                // The daemon refuses non-force deletes when purposes still
                // reference the connection. Surface that and stay in the confirm
                // overlay so the user can press `f` to force.
                if !force && msg.to_lowercase().contains("purpose") {
                    state.error = Some(format!("{msg} — press 'f' to force"));
                } else {
                    state.error = Some(format!("Delete failed: {msg}"));
                    state.mode = Mode::List;
                }
            }
        },
    }
}

/// Send a `Command` over the transport. The shared command channel
/// (`as_commands`) exposes a generic `send_command` over both socket
/// transports (UDS + WS); D-Bus speaks a fixed set of typed methods and so
/// has no command channel. We surface a clear error in that case rather than
/// silently no-op'ing.
async fn send(client: &TransportClient, command: Command) -> anyhow::Result<CommandResult> {
    if let Some(commands) = client.as_commands() {
        commands.send_command(command).await
    } else {
        anyhow::bail!(
            "Connection management isn't available over D-Bus — switch transport with --transport ws or the local socket"
        )
    }
}

// --- Rendering ---

fn draw(f: &mut Frame, state: &State) {
    let area = f.area();
    f.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(f, chunks[0]);
    match state.mode {
        Mode::Edit => draw_edit_form(f, state, chunks[1]),
        _ => draw_list(f, state, chunks[1]),
    }
    draw_status(f, state, chunks[2]);
    draw_hints(f, state, chunks[3]);

    if matches!(state.mode, Mode::DeleteConfirm) {
        draw_delete_overlay(f, state, area);
    }
}

fn draw_header(f: &mut Frame, area: Rect) {
    let line = Line::from(vec![
        Span::styled(
            "LLM provider connections",
            Style::default()
                .fg(theme().title)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  —  Esc to return to chat",
            Style::default().fg(theme().text_dim),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_list(f: &mut Frame, state: &State, area: Rect) {
    let items: Vec<ListItem> = if state.connections.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "(no connections — press 'a' to add one)",
            Style::default().fg(theme().text_dim),
        )))]
    } else {
        state
            .connections
            .iter()
            .map(|c| {
                let availability_text = match &c.availability {
                    ConnectionAvailability::Ok => {
                        Span::styled("●", Style::default().fg(theme().ok))
                    }
                    ConnectionAvailability::Unavailable { .. } => {
                        Span::styled("●", Style::default().fg(theme().error))
                    }
                };
                let unavail_reason = match &c.availability {
                    ConnectionAvailability::Unavailable { reason } => Some(reason.clone()),
                    _ => None,
                };
                let mut spans: Vec<Span<'static>> = vec![
                    availability_text,
                    Span::raw(" "),
                    Span::styled(c.id.clone(), Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!(" [{}]", c.connector_type),
                        Style::default().fg(theme().text_dim),
                    ),
                ];
                if c.display_label != format!("{} ({})", c.id, c.connector_type) {
                    spans.push(Span::styled(
                        format!("  ·  {}", c.display_label),
                        Style::default().fg(theme().text_dim),
                    ));
                }
                if let Some(reason) = unavail_reason {
                    spans.push(Span::styled(
                        format!("  ·  {reason}"),
                        Style::default()
                            .fg(theme().error)
                            .add_modifier(Modifier::ITALIC),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect()
    };

    let title = if state.connections.is_empty() {
        "Connections".to_string()
    } else {
        format!("Connections ({})", state.connections.len())
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme().border))
                .title(Line::from(Span::styled(
                    title,
                    Style::default()
                        .fg(theme().title)
                        .add_modifier(Modifier::BOLD),
                ))),
        )
        .highlight_style(
            Style::default()
                .bg(theme().list_highlight)
                .fg(theme().list_highlight_fg)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");

    let mut list_state = ListState::default();
    if !state.connections.is_empty() {
        list_state.select(Some(state.selected));
    }
    f.render_stateful_widget(list, area, &mut list_state);
}

fn draw_edit_form(f: &mut Frame, state: &State, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme().border))
        .title(Line::from(Span::styled(
            if state.form.editing_id.is_some() {
                "Edit connection"
            } else {
                "New connection"
            },
            Style::default()
                .fg(theme().title)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let fields = EditForm::fields_for_kind(state.form.kind);
    // Each field needs a 1-line label + 3-line input. Size the layout
    // based on how many fields the current kind has.
    let mut constraints: Vec<Constraint> = Vec::with_capacity(fields.len() * 2 + 1);
    for _ in fields {
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(3));
    }
    constraints.push(Constraint::Min(0));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    for (idx, field) in fields.iter().enumerate() {
        let label_row = rows[idx * 2];
        let input_row = rows[idx * 2 + 1];
        let focused = state.form.focus == *field;
        match field {
            Field::Id => {
                let label = if state.form.editing_id.is_some() {
                    "Id (immutable)"
                } else {
                    "Id (slug; lowercase, no spaces)"
                };
                draw_field_label(f, label_row, label, focused);
                draw_text_field(f, input_row, &state.form.id, focused);
            }
            Field::Type => {
                let suffix = if state.form.editing_id.is_some() {
                    " — locked on edit"
                } else {
                    " (←/→ or Space to cycle)"
                };
                draw_field_label(f, label_row, &format!("Type{suffix}"), focused);
                draw_type_toggle(f, input_row, state, focused);
            }
            Field::ApiKeyEnv => {
                // The default env-var name is connector-specific; hint the one
                // the daemon assumes for this kind so an empty field is clear.
                let example = match state.form.kind {
                    ConnectorKind::Azure => "AZURE_OPENAI_API_KEY",
                    ConnectorKind::Google => "GOOGLE_API_KEY",
                    ConnectorKind::OpenAi => "OPENAI_API_KEY",
                    ConnectorKind::OpenRouter => "OPENROUTER_API_KEY",
                    _ => "ANTHROPIC_API_KEY",
                };
                draw_field_label(
                    f,
                    label_row,
                    &format!("API key env var name (e.g. {example})"),
                    focused,
                );
                draw_text_field_hinted(f, input_row, &state.form.api_key_env, focused, example);
            }
            Field::BaseUrl => {
                // Azure's base_url is the resource endpoint and effectively
                // required; for everyone else it's an optional override.
                let (label, placeholder) = match state.form.kind {
                    ConnectorKind::Azure => (
                        "Resource endpoint (required)",
                        "https://<resource>.openai.azure.com",
                    ),
                    _ => ("Base URL (optional)", ""),
                };
                draw_field_label(f, label_row, label, focused);
                draw_text_field_hinted(f, input_row, &state.form.base_url, focused, placeholder);
            }
            Field::AwsProfile => {
                draw_field_label(f, label_row, "AWS profile (optional)", focused);
                draw_text_field(f, input_row, &state.form.aws_profile, focused);
            }
            Field::Region => {
                draw_field_label(f, label_row, "AWS region (e.g. us-east-1)", focused);
                draw_text_field(f, input_row, &state.form.region, focused);
            }
            Field::ApiSurface => {
                draw_field_label(f, label_row, "API surface", focused);
                draw_text_field_hinted(
                    f,
                    input_row,
                    &state.form.api_surface,
                    focused,
                    "v1 | classic  (default: v1)",
                );
            }
            Field::AuthMode => {
                // Allowed values differ by kind; show the right set + default.
                let placeholder = match state.form.kind {
                    ConnectorKind::Google => "vertex | api_key  (default: vertex)",
                    _ => "api_key | entra  (default: api_key)",
                };
                draw_field_label(f, label_row, "Auth mode", focused);
                draw_text_field_hinted(f, input_row, &state.form.auth_mode, focused, placeholder);
            }
            Field::ApiVersion => {
                draw_field_label(f, label_row, "API version (classic surface only)", focused);
                draw_text_field_hinted(
                    f,
                    input_row,
                    &state.form.api_version,
                    focused,
                    "e.g. 2024-10-21",
                );
            }
            Field::Project => {
                draw_field_label(f, label_row, "GCP project (Vertex)", focused);
                draw_text_field_hinted(
                    f,
                    input_row,
                    &state.form.project,
                    focused,
                    "my-gcp-project",
                );
            }
            Field::Location => {
                draw_field_label(f, label_row, "Location / region (Vertex)", focused);
                draw_text_field_hinted(
                    f,
                    input_row,
                    &state.form.location,
                    focused,
                    "e.g. us-central1",
                );
            }
            Field::CredentialsPath => {
                draw_field_label(
                    f,
                    label_row,
                    "Service-account JSON path (Vertex, optional)",
                    focused,
                );
                draw_text_field_hinted(
                    f,
                    input_row,
                    &state.form.credentials_path,
                    focused,
                    "/path/to/service-account.json",
                );
            }
        }
    }
}

fn draw_type_toggle(f: &mut Frame, area: Rect, state: &State, focused: bool) {
    let border_color = if focused {
        theme().border_active
    } else {
        theme().border
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chip = |kind: ConnectorKind| -> Span<'static> {
        let active = state.form.kind == kind;
        let style = if active {
            Style::default()
                .fg(Color::Black)
                .bg(theme().border_active)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme().text_dim)
        };
        Span::styled(format!(" {} ", kind.label()), style)
    };

    let mut spans: Vec<Span> = Vec::new();
    for (i, k) in ConnectorKind::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(chip(*k));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), inner);
}

fn draw_field_label(f: &mut Frame, area: Rect, label: &str, focused: bool) {
    let style = if focused {
        Style::default()
            .fg(theme().border_active)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme().text_dim)
    };
    f.render_widget(Paragraph::new(Span::styled(label.to_string(), style)), area);
}

fn draw_text_field(f: &mut Frame, area: Rect, textarea: &TextArea<'static>, focused: bool) {
    draw_text_field_hinted(f, area, textarea, focused, "");
}

/// Like [`draw_text_field`] but shows `placeholder` (dimmed) while the field is
/// empty. Used for the enum knobs (api_surface / auth_mode) and other
/// free-text-but-constrained fields to list allowed values / examples inline.
fn draw_text_field_hinted(
    f: &mut Frame,
    area: Rect,
    textarea: &TextArea<'static>,
    focused: bool,
    placeholder: &str,
) {
    let mut ta = textarea.clone();
    if !placeholder.is_empty() {
        ta.set_placeholder_text(placeholder);
    }
    let border_color = if focused {
        theme().border_active
    } else {
        theme().border
    };
    ta.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color)),
    );
    f.render_widget(&ta, area);
}

fn draw_delete_overlay(f: &mut Frame, state: &State, area: Rect) {
    let label = state
        .connections
        .get(state.selected)
        .map(|c| c.id.clone())
        .unwrap_or_else(|| "this connection".to_string());
    let popup_width = 64.min(area.width.saturating_sub(4));
    let popup_height = 6.min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_width)) / 2,
        y: area.y + (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    };
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme().error))
        .title(Line::from(Span::styled(
            "Delete connection",
            Style::default()
                .fg(theme().error_text)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let body = Paragraph::new(vec![
        Line::from(Span::styled(
            format!("Delete \"{label}\"?"),
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            "y/Enter = confirm · f = force (referencing purposes fall back) · n/Esc = cancel",
            Style::default().fg(theme().text_dim),
        )),
    ])
    .wrap(Wrap { trim: true });
    f.render_widget(body, inner);
}

fn draw_status(f: &mut Frame, state: &State, area: Rect) {
    if let Some(busy) = &state.busy {
        let style = Style::default()
            .fg(theme().assistant_indicator)
            .add_modifier(Modifier::ITALIC);
        f.render_widget(
            Paragraph::new(Span::styled(format!(" ● {busy}"), style)),
            area,
        );
    } else if let Some(err) = &state.error {
        let style = Style::default().fg(theme().error);
        f.render_widget(
            Paragraph::new(Span::styled(format!(" • {err}"), style)),
            area,
        );
    }
}

fn draw_hints(f: &mut Frame, state: &State, area: Rect) {
    let hints: &[(&str, &str)] = match state.mode {
        Mode::List => &[
            ("Enter", "edit"),
            ("a", "add"),
            ("d", "delete"),
            ("r", "refresh"),
            ("Esc", "back to chat"),
        ],
        Mode::Edit => &[("Tab", "next field"), ("Ctrl+S", "save"), ("Esc", "cancel")],
        Mode::DeleteConfirm => &[("y/Enter", "confirm"), ("f", "force"), ("n/Esc", "cancel")],
    };
    let mut spans: Vec<Span> = Vec::new();
    for (idx, (key, desc)) in hints.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled("  ·  ", Style::default().fg(theme().hint_sep)));
        }
        spans.push(Span::styled(
            (*key).to_string(),
            Style::default()
                .fg(theme().hint_key)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            (*desc).to_string(),
            Style::default().fg(theme().text_dim),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(id: &str, ty: &str) -> ConnectionView {
        ConnectionView {
            id: id.into(),
            connector_type: ty.into(),
            display_label: format!("{id} ({ty})"),
            availability: ConnectionAvailability::Ok,
            has_credentials: true,
            // Echoed non-secret config (added upstream after the view shipped);
            // the TUI's edit form pre-fills from `id`/`connector_type` only, so
            // these tests don't exercise it.
            config: None,
        }
    }

    #[test]
    fn connector_kind_round_trips_via_tag() {
        for kind in ConnectorKind::ALL {
            assert_eq!(ConnectorKind::from_tag(kind.tag()), Some(*kind));
        }
    }

    #[test]
    fn connector_kind_next_prev_cycle() {
        // Picker order: Anthropic, OpenAI, OpenRouter, Azure, Google, Bedrock,
        // Ollama. Azure and Google slot between OpenRouter and Bedrock.
        assert_eq!(ConnectorKind::Anthropic.next(), ConnectorKind::OpenAi);
        assert_eq!(ConnectorKind::OpenAi.next(), ConnectorKind::OpenRouter);
        assert_eq!(ConnectorKind::OpenRouter.next(), ConnectorKind::Azure);
        assert_eq!(ConnectorKind::Azure.next(), ConnectorKind::Google);
        assert_eq!(ConnectorKind::Google.next(), ConnectorKind::Bedrock);
        assert_eq!(ConnectorKind::Ollama.next(), ConnectorKind::Anthropic);
        assert_eq!(ConnectorKind::Anthropic.prev(), ConnectorKind::Ollama);
        assert_eq!(ConnectorKind::Google.prev(), ConnectorKind::Azure);
        assert_eq!(ConnectorKind::Azure.prev(), ConnectorKind::OpenRouter);
    }

    #[test]
    fn fields_for_kind_excludes_irrelevant_fields() {
        let ollama_fields = EditForm::fields_for_kind(ConnectorKind::Ollama);
        assert!(!ollama_fields.contains(&Field::ApiKeyEnv));
        assert!(!ollama_fields.contains(&Field::Region));

        let bedrock_fields = EditForm::fields_for_kind(ConnectorKind::Bedrock);
        assert!(!bedrock_fields.contains(&Field::ApiKeyEnv));
        assert!(bedrock_fields.contains(&Field::Region));
        assert!(bedrock_fields.contains(&Field::AwsProfile));

        let anthropic_fields = EditForm::fields_for_kind(ConnectorKind::Anthropic);
        assert!(anthropic_fields.contains(&Field::ApiKeyEnv));
        assert!(!anthropic_fields.contains(&Field::Region));

        let openrouter_fields = EditForm::fields_for_kind(ConnectorKind::OpenRouter);
        assert!(openrouter_fields.contains(&Field::ApiKeyEnv));
        assert!(openrouter_fields.contains(&Field::BaseUrl));
        assert!(!openrouter_fields.contains(&Field::Region));

        let azure_fields = EditForm::fields_for_kind(ConnectorKind::Azure);
        assert!(azure_fields.contains(&Field::ApiKeyEnv));
        assert!(azure_fields.contains(&Field::BaseUrl));
        assert!(azure_fields.contains(&Field::ApiSurface));
        assert!(azure_fields.contains(&Field::AuthMode));
        assert!(azure_fields.contains(&Field::ApiVersion));
        // Azure must not surface Google/Bedrock-only fields.
        assert!(!azure_fields.contains(&Field::Project));
        assert!(!azure_fields.contains(&Field::Region));

        let google_fields = EditForm::fields_for_kind(ConnectorKind::Google);
        assert!(google_fields.contains(&Field::ApiKeyEnv));
        assert!(google_fields.contains(&Field::Project));
        assert!(google_fields.contains(&Field::Location));
        assert!(google_fields.contains(&Field::AuthMode));
        assert!(google_fields.contains(&Field::CredentialsPath));
        // Google must not surface Azure/Bedrock-only fields.
        assert!(!google_fields.contains(&Field::ApiSurface));
        assert!(!google_fields.contains(&Field::Region));
    }

    #[test]
    fn next_field_skips_to_first_when_kind_changes() {
        // After switching kind, focus must land on a field that the new
        // kind has — otherwise we could end up stuck on a non-rendered field.
        let anthropic = EditForm::fields_for_kind(ConnectorKind::Anthropic);
        let ollama = EditForm::fields_for_kind(ConnectorKind::Ollama);
        // Anthropic exposes ApiKeyEnv; Ollama does not.
        assert!(anthropic.contains(&Field::ApiKeyEnv));
        assert!(!ollama.contains(&Field::ApiKeyEnv));
    }

    #[test]
    fn submit_blank_id_is_rejected() {
        let form = EditForm::empty();
        assert!(form.submit().is_err());
    }

    #[test]
    fn submit_anthropic_emits_correct_variant() {
        let mut form = EditForm::empty();
        form.id.insert_str("work");
        form.api_key_env.insert_str("WORK_KEY");
        form.kind = ConnectorKind::Anthropic;
        let (id, config) = form.submit().unwrap();
        assert_eq!(id, "work");
        match config {
            ConnectionConfigView::Anthropic {
                api_key_env,
                base_url,
                ..
            } => {
                assert_eq!(api_key_env.as_deref(), Some("WORK_KEY"));
                assert!(base_url.is_none());
            }
            other => panic!("expected Anthropic, got {other:?}"),
        }
    }

    #[test]
    fn submit_openrouter_emits_correct_variant() {
        let mut form = EditForm::empty();
        form.id.insert_str("router");
        form.api_key_env.insert_str("OPENROUTER_API_KEY");
        form.base_url.insert_str("https://openrouter.ai/api/v1");
        form.kind = ConnectorKind::OpenRouter;
        let (id, config) = form.submit().unwrap();
        assert_eq!(id, "router");
        match config {
            ConnectionConfigView::OpenRouter {
                api_key_env,
                base_url,
                ..
            } => {
                assert_eq!(api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
                assert_eq!(base_url.as_deref(), Some("https://openrouter.ai/api/v1"));
            }
            other => panic!("expected OpenRouter, got {other:?}"),
        }
    }

    #[test]
    fn submit_azure_emits_correct_variant() {
        let mut form = EditForm::empty();
        form.id.insert_str("azure-gpt");
        form.base_url
            .insert_str("https://my-resource.openai.azure.com");
        form.api_key_env.insert_str("AZURE_OPENAI_API_KEY");
        form.api_surface.insert_str("classic");
        form.auth_mode.insert_str("entra");
        form.api_version.insert_str("2024-10-21");
        form.kind = ConnectorKind::Azure;
        let (id, config) = form.submit().unwrap();
        assert_eq!(id, "azure-gpt");
        match config {
            ConnectionConfigView::Azure {
                base_url,
                api_key_env,
                api_surface,
                auth_mode,
                api_version,
                ..
            } => {
                assert_eq!(
                    base_url.as_deref(),
                    Some("https://my-resource.openai.azure.com")
                );
                assert_eq!(api_key_env.as_deref(), Some("AZURE_OPENAI_API_KEY"));
                assert_eq!(api_surface.as_deref(), Some("classic"));
                assert_eq!(auth_mode.as_deref(), Some("entra"));
                assert_eq!(api_version.as_deref(), Some("2024-10-21"));
            }
            other => panic!("expected Azure, got {other:?}"),
        }
    }

    #[test]
    fn submit_azure_blank_enums_default_to_none() {
        // An untouched api_surface / auth_mode must submit as `None` so the
        // daemon applies its primary defaults (v1 / api_key) rather than an
        // empty string the daemon would reject.
        let mut form = EditForm::empty();
        form.id.insert_str("azure-min");
        form.base_url
            .insert_str("https://my-resource.openai.azure.com");
        form.kind = ConnectorKind::Azure;
        let (_id, config) = form.submit().unwrap();
        match config {
            ConnectionConfigView::Azure {
                api_surface,
                auth_mode,
                api_version,
                ..
            } => {
                assert!(api_surface.is_none());
                assert!(auth_mode.is_none());
                assert!(api_version.is_none());
            }
            other => panic!("expected Azure, got {other:?}"),
        }
    }

    #[test]
    fn submit_google_emits_correct_variant() {
        let mut form = EditForm::empty();
        form.id.insert_str("vertex");
        form.api_key_env.insert_str("GOOGLE_API_KEY");
        form.project.insert_str("my-gcp-project");
        form.location.insert_str("us-central1");
        form.auth_mode.insert_str("vertex");
        form.credentials_path.insert_str("/etc/adele/sa.json");
        form.kind = ConnectorKind::Google;
        let (id, config) = form.submit().unwrap();
        assert_eq!(id, "vertex");
        match config {
            ConnectionConfigView::Google {
                project,
                location,
                auth_mode,
                credentials_path,
                api_key_env,
                ..
            } => {
                assert_eq!(project.as_deref(), Some("my-gcp-project"));
                assert_eq!(location.as_deref(), Some("us-central1"));
                assert_eq!(auth_mode.as_deref(), Some("vertex"));
                assert_eq!(credentials_path.as_deref(), Some("/etc/adele/sa.json"));
                assert_eq!(api_key_env.as_deref(), Some("GOOGLE_API_KEY"));
            }
            other => panic!("expected Google, got {other:?}"),
        }
    }

    /// create -> save (daemon echoes it back as `view.config`) -> from_view ->
    /// re-submit must preserve every non-secret Azure field. Guards the
    /// explicit `prefill_from_config` Azure arm against a wildcard regression.
    #[test]
    fn azure_config_survives_edit_round_trip() {
        let mut form = EditForm::empty();
        form.id.insert_str("azure-gpt");
        form.base_url
            .insert_str("https://my-resource.openai.azure.com");
        form.api_key_env.insert_str("AZURE_OPENAI_API_KEY");
        form.api_surface.insert_str("v1");
        form.auth_mode.insert_str("api_key");
        form.kind = ConnectorKind::Azure;
        let (id, config) = form.submit().unwrap();

        let view = ConnectionView {
            id: id.clone(),
            connector_type: "azure".into(),
            display_label: format!("{id} (azure)"),
            availability: ConnectionAvailability::Ok,
            has_credentials: true,
            config: Some(config.clone()),
        };
        let reloaded = EditForm::from_view(&view);
        assert_eq!(reloaded.kind, ConnectorKind::Azure);
        let (id2, config2) = reloaded.submit().unwrap();
        assert_eq!(id, id2);
        assert_eq!(config, config2);
    }

    /// Same round-trip guarantee for Google's project/location/auth_mode.
    #[test]
    fn google_config_survives_edit_round_trip() {
        let mut form = EditForm::empty();
        form.id.insert_str("vertex");
        form.project.insert_str("my-gcp-project");
        form.location.insert_str("us-central1");
        form.auth_mode.insert_str("vertex");
        form.credentials_path.insert_str("/etc/adele/sa.json");
        form.kind = ConnectorKind::Google;
        let (id, config) = form.submit().unwrap();

        let view = ConnectionView {
            id: id.clone(),
            connector_type: "google".into(),
            display_label: format!("{id} (google)"),
            availability: ConnectionAvailability::Ok,
            has_credentials: true,
            config: Some(config.clone()),
        };
        let reloaded = EditForm::from_view(&view);
        assert_eq!(reloaded.kind, ConnectorKind::Google);
        let (id2, config2) = reloaded.submit().unwrap();
        assert_eq!(id, id2);
        assert_eq!(config, config2);
    }

    #[test]
    fn submit_bedrock_emits_correct_variant() {
        let mut form = EditForm::empty();
        form.id.insert_str("aws");
        form.region.insert_str("us-east-1");
        form.aws_profile.insert_str("dev");
        form.kind = ConnectorKind::Bedrock;
        let (id, config) = form.submit().unwrap();
        assert_eq!(id, "aws");
        match config {
            ConnectionConfigView::Bedrock {
                aws_profile,
                region,
                base_url,
                ..
            } => {
                assert_eq!(aws_profile.as_deref(), Some("dev"));
                assert_eq!(region.as_deref(), Some("us-east-1"));
                assert!(base_url.is_none());
            }
            other => panic!("expected Bedrock, got {other:?}"),
        }
    }

    #[test]
    fn submit_ollama_emits_correct_variant() {
        let mut form = EditForm::empty();
        form.id.insert_str("local");
        form.base_url.insert_str("http://127.0.0.1:11434");
        form.kind = ConnectorKind::Ollama;
        let (id, config) = form.submit().unwrap();
        assert_eq!(id, "local");
        match config {
            ConnectionConfigView::Ollama { base_url, .. } => {
                assert_eq!(base_url.as_deref(), Some("http://127.0.0.1:11434"));
            }
            other => panic!("expected Ollama, got {other:?}"),
        }
    }

    #[test]
    fn from_view_locks_id_and_picks_kind() {
        let v = view("work", "openai");
        let form = EditForm::from_view(&v);
        assert_eq!(form.editing_id.as_deref(), Some("work"));
        assert_eq!(form.kind, ConnectorKind::OpenAi);
        assert_eq!(form.id.lines(), vec!["work".to_string()]);
    }
}
