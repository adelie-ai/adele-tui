//! Purposes view — assign `(connection, model, effort)` per purpose.
//!
//! Modal screen reachable from the chat with `F4`. Lists the four
//! purposes (Interactive / Dreaming / Embedding / Titling) and lets the
//! user reassign each one. Non-interactive purposes can inherit from
//! the interactive purpose by selecting `primary` for both connection
//! and model.
//!
//! Reads via `GetPurposes` and writes via `SetPurpose`. Connection +
//! model lists come from `ListConnections` + `list_available_models`
//! (the latter filtered by the selected connection).
//!
//! Keys
//! ----
//!
//! List mode:
//! - `j/k` or arrows: navigate purposes
//! - `Enter` / `e`: edit selected
//! - `r`: refresh
//! - `Esc` / `q`: close
//!
//! Edit mode:
//! - `Tab` / `Shift+Tab`: cycle fields (Connection / Model / Effort)
//! - `←` / `→` / `Space`: cycle the focused field's value
//! - `Ctrl+S`: save
//! - `Esc`: cancel

use std::io;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use desktop_assistant_api_model::{
    Command, CommandResult, ConnectionView, EffortLevel, ModelListing, PurposeConfigView,
    PurposeKindApi, PurposesView,
};
use desktop_assistant_client_common::{SignalEvent, TransportClient};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};

use crate::screen::Screen;

use crate::theme::theme;

/// The four purposes in display order.
const PURPOSES: &[PurposeKindApi] = &[
    PurposeKindApi::Interactive,
    PurposeKindApi::Dreaming,
    PurposeKindApi::Consolidation,
    PurposeKindApi::Embedding,
    PurposeKindApi::Titling,
];

const PRIMARY: &str = "primary";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    List,
    Edit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditField {
    Connection,
    Model,
    Effort,
}

const FIELD_ORDER: [EditField; 3] = [EditField::Connection, EditField::Model, EditField::Effort];

struct EditState {
    purpose: PurposeKindApi,
    /// Current selection; "primary" or a real connection id.
    connection: String,
    /// Current model id, or "primary" (only valid when connection is also "primary").
    model: String,
    effort: Option<EffortLevel>,
    field: EditField,
}

impl EditState {
    fn next_field(&mut self) {
        let pos = FIELD_ORDER
            .iter()
            .position(|f| *f == self.field)
            .unwrap_or(0);
        self.field = FIELD_ORDER[(pos + 1) % FIELD_ORDER.len()];
    }

    fn prev_field(&mut self) {
        let pos = FIELD_ORDER
            .iter()
            .position(|f| *f == self.field)
            .unwrap_or(0);
        self.field = FIELD_ORDER[(pos + FIELD_ORDER.len() - 1) % FIELD_ORDER.len()];
    }

    /// True when the interactive purpose is being edited. The interactive
    /// purpose can't inherit — there's nothing to inherit *from* — so
    /// `primary` is dropped from the cyclers in that case.
    fn is_interactive(&self) -> bool {
        matches!(self.purpose, PurposeKindApi::Interactive)
    }

    fn from_view(purpose: PurposeKindApi, view: Option<&PurposeConfigView>) -> Self {
        match view {
            Some(v) => Self {
                purpose,
                connection: v.connection.clone(),
                model: v.model.clone(),
                effort: v.effort,
                field: EditField::Connection,
            },
            None => Self {
                // Inherit by default for non-interactive when nothing is
                // saved; interactive falls back to placeholder strings the
                // user must replace before save.
                purpose,
                connection: if matches!(purpose, PurposeKindApi::Interactive) {
                    String::new()
                } else {
                    PRIMARY.to_string()
                },
                model: if matches!(purpose, PurposeKindApi::Interactive) {
                    String::new()
                } else {
                    PRIMARY.to_string()
                },
                effort: None,
                field: EditField::Connection,
            },
        }
    }

    fn submit(&self) -> Result<PurposeConfigView, String> {
        let connection = self.connection.trim();
        let model = self.model.trim();
        if connection.is_empty() {
            return Err("Pick a connection".into());
        }
        if model.is_empty() {
            return Err("Pick a model".into());
        }
        if self.is_interactive() && (connection == PRIMARY || model == PRIMARY) {
            return Err("Interactive purpose can't inherit (no primary to inherit from)".into());
        }
        // The daemon's contract is that connection/model are *both*
        // "primary" together, or neither. Reject mixed pairs early.
        let conn_is_primary = connection == PRIMARY;
        let model_is_primary = model == PRIMARY;
        if conn_is_primary != model_is_primary {
            return Err(
                "Connection and model must both be 'primary' for inherit, or both real ids".into(),
            );
        }
        Ok(PurposeConfigView {
            connection: connection.to_string(),
            model: model.to_string(),
            effort: self.effort,
            max_context_tokens: None,
        })
    }
}

use crate::in_flight::InFlight;

/// Refreshed purposes-screen data — result of the sequential off-loop
/// `ListConnections` → `GetPurposes` → `list_available_models` chain.
struct RefreshData {
    connections: Vec<ConnectionView>,
    purposes: PurposesView,
    models: Vec<ModelListing>,
}

/// Resolved outcome of an off-loop purposes RPC (modal-freeze fix).
enum RpcOutcome {
    // Boxed: `RefreshData` (three vecs) dwarfs the `Saved` variant, so box it to
    // keep the enum small (clippy::large_enum_variant).
    Refreshed(Result<Box<RefreshData>, String>),
    Saved(Result<(), String>),
}

struct State {
    connections: Vec<ConnectionView>,
    models: Vec<ModelListing>,
    purposes: PurposesView,
    selected_row: usize,
    mode: Mode,
    edit: EditState,
    error: Option<String>,
    busy: Option<String>,
    closing: bool,
}

/// Run the purposes screen until the user closes it.
/// The purposes view as a [`Screen`]: its [`State`] plus the borrowed client.
/// The shared driver supplies the loop and drains daemon signals while the screen
/// is open (TUI-12).
struct PurposesScreen<'a> {
    state: State,
    client: &'a TransportClient,
    /// In-flight list/save RPCs, polled off the draw loop by `poll_pending` so
    /// the screen never freezes during the (sequential) refresh or a save.
    pending: InFlight<'a, RpcOutcome>,
}

impl Screen for PurposesScreen<'_> {
    type Outcome = ();

    fn draw(&mut self, frame: &mut Frame) {
        draw(frame, &self.state);
    }

    fn handle_key(&mut self, key: KeyEvent) -> impl std::future::Future<Output = ()> {
        match self.state.mode {
            Mode::List => handle_list_key(&mut self.state, key, self.client, &mut self.pending),
            Mode::Edit => handle_edit_key(&mut self.state, key, self.client, &mut self.pending),
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
    let mut screen = PurposesScreen {
        state: State {
            connections: Vec::new(),
            models: Vec::new(),
            purposes: PurposesView::default(),
            selected_row: 0,
            mode: Mode::List,
            edit: EditState::from_view(PurposeKindApi::Interactive, None),
            error: None,
            busy: Some("Loading...".into()),
            closing: false,
        },
        client,
        pending: InFlight::new(),
    };

    refresh_all(&mut screen.state, &mut screen.pending, client);

    crate::screen::run_screen(terminal, &mut screen, signal_rx, sink).await
}

/// Enqueue the full refresh off-loop (modal-freeze fix). Sets `busy` and returns;
/// `apply_outcome` installs the data once `poll_pending` resolves it.
fn refresh_all<'a>(
    state: &mut State,
    pending: &mut InFlight<'a, RpcOutcome>,
    client: &'a TransportClient,
) {
    state.busy = Some("Loading purposes...".into());
    pending.push(async move { RpcOutcome::Refreshed(load_all(client).await.map(Box::new)) });
}

/// Run the three refresh RPCs sequentially in one off-loop future, short-circuiting
/// on the first failure. Pre-fetches the full model list once (filtered
/// client-side per connection in the editor cyclers).
async fn load_all(client: &TransportClient) -> Result<RefreshData, String> {
    let connections = match send(client, Command::ListConnections).await {
        Ok(CommandResult::Connections(c)) => c,
        Ok(other) => {
            return Err(format!(
                "Unexpected response listing connections: {other:?}"
            ));
        }
        Err(e) => return Err(format!("Failed to list connections: {e}")),
    };
    let purposes = match send(client, Command::GetPurposes).await {
        Ok(CommandResult::Purposes(p)) => p,
        Ok(other) => return Err(format!("Unexpected response loading purposes: {other:?}")),
        Err(e) => return Err(format!("Failed to load purposes: {e}")),
    };
    let Some(commands) = client.as_commands() else {
        return Err(
            "Purposes management isn't available over D-Bus — switch transport with --transport ws or the local socket"
                .into(),
        );
    };
    let models = commands
        .list_available_models(None, false)
        .await
        .map_err(|e| format!("Failed to list models: {e}"))?;
    Ok(RefreshData {
        connections,
        purposes,
        models,
    })
}

/// Apply a resolved purposes RPC; chains a `refresh_all` after a successful save.
fn apply_outcome<'a>(
    state: &mut State,
    pending: &mut InFlight<'a, RpcOutcome>,
    client: &'a TransportClient,
    outcome: RpcOutcome,
) {
    state.busy = None;
    match outcome {
        RpcOutcome::Refreshed(Ok(data)) => {
            let RefreshData {
                connections,
                purposes,
                models,
            } = *data;
            state.connections = connections;
            state.purposes = purposes;
            state.models = models;
        }
        RpcOutcome::Refreshed(Err(e)) => state.error = Some(e),
        RpcOutcome::Saved(Ok(())) => {
            state.error = None;
            state.mode = Mode::List;
            refresh_all(state, pending, client);
        }
        RpcOutcome::Saved(Err(e)) => state.error = Some(format!("Save failed: {e}")),
    }
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
            let purpose = PURPOSES[state.selected_row];
            let view = purpose_slot(&state.purposes, purpose);
            state.edit = EditState::from_view(purpose, view);
            state.error = None;
            state.mode = Mode::Edit;
        }
        (KeyCode::Char('r'), m) if m.is_empty() => refresh_all(state, pending, client),
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
            state.mode = Mode::List;
        }
        (KeyCode::Tab, _) => state.edit.next_field(),
        (KeyCode::BackTab, _) => state.edit.prev_field(),
        (KeyCode::Left, m) if m.is_empty() => cycle_field(state, -1),
        (KeyCode::Right | KeyCode::Char(' '), m) if m.is_empty() => cycle_field(state, 1),
        _ => {}
    }
}

fn cycle_field(state: &mut State, delta: i32) {
    match state.edit.field {
        EditField::Connection => cycle_connection(state, delta),
        EditField::Model => cycle_model(state, delta),
        EditField::Effort => cycle_effort(state, delta),
    }
}

fn cycle_connection(state: &mut State, delta: i32) {
    let mut options: Vec<String> = state.connections.iter().map(|c| c.id.clone()).collect();
    if !state.edit.is_interactive() {
        options.insert(0, PRIMARY.to_string());
    }
    if options.is_empty() {
        return;
    }
    let pos = options
        .iter()
        .position(|o| o == &state.edit.connection)
        .unwrap_or(0);
    let next = wrap_index(pos as i32 + delta, options.len());
    state.edit.connection = options[next].clone();
    // When connection changes, reset model to a sensible default so the
    // user doesn't end up with a stale "model from previous connection".
    state.edit.model = if state.edit.connection == PRIMARY {
        PRIMARY.to_string()
    } else {
        // First model from the new connection, if any; else blank.
        state
            .models
            .iter()
            .find(|m| m.connection_id == state.edit.connection)
            .map(|m| m.model.id.clone())
            .unwrap_or_default()
    };
}

fn cycle_model(state: &mut State, delta: i32) {
    let connection = state.edit.connection.clone();
    let mut options: Vec<String> = if connection == PRIMARY {
        // Inherit case — only "primary" is a valid model.
        vec![PRIMARY.to_string()]
    } else {
        state
            .models
            .iter()
            .filter(|m| m.connection_id == connection)
            .map(|m| m.model.id.clone())
            .collect()
    };
    if !state.edit.is_interactive() && connection != PRIMARY {
        // For non-interactive purposes, "primary" model with a real
        // connection isn't allowed (mixed pair); but you can still pick a
        // real model. So we don't add "primary" here.
        let _ = (); // no-op
    }
    if options.is_empty() {
        // No models available for this connection. Cycling does nothing;
        // the user can fix by changing connection or refreshing.
        options.push(state.edit.model.clone());
    }
    let pos = options
        .iter()
        .position(|o| o == &state.edit.model)
        .unwrap_or(0);
    let next = wrap_index(pos as i32 + delta, options.len());
    state.edit.model = options[next].clone();
}

fn cycle_effort(state: &mut State, delta: i32) {
    let options: &[Option<EffortLevel>] = &[
        None,
        Some(EffortLevel::Low),
        Some(EffortLevel::Medium),
        Some(EffortLevel::High),
    ];
    let pos = options
        .iter()
        .position(|o| *o == state.edit.effort)
        .unwrap_or(0);
    let next = wrap_index(pos as i32 + delta, options.len());
    state.edit.effort = options[next];
}

fn wrap_index(i: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let len = len as i32;
    (((i % len) + len) % len) as usize
}

fn advance_selection(state: &mut State, delta: i32) {
    let len = PURPOSES.len();
    let mut idx = state.selected_row as i32 + delta;
    if idx < 0 {
        idx = (len as i32) - 1;
    }
    if idx >= len as i32 {
        idx = 0;
    }
    state.selected_row = idx as usize;
}

fn save_edit<'a>(
    state: &mut State,
    pending: &mut InFlight<'a, RpcOutcome>,
    client: &'a TransportClient,
) {
    let config = match state.edit.submit() {
        Ok(c) => c,
        Err(e) => {
            state.error = Some(e);
            return;
        }
    };

    state.busy = Some("Saving...".into());
    let purpose = state.edit.purpose;
    pending.push(async move {
        RpcOutcome::Saved(
            send(client, Command::SetPurpose { purpose, config })
                .await
                .map(|_| ())
                .map_err(|e| e.to_string()),
        )
    });
}

fn purpose_slot(view: &PurposesView, kind: PurposeKindApi) -> Option<&PurposeConfigView> {
    match kind {
        PurposeKindApi::Interactive => view.interactive.as_ref(),
        PurposeKindApi::Dreaming => view.dreaming.as_ref(),
        PurposeKindApi::Consolidation => view.consolidation.as_ref(),
        PurposeKindApi::Embedding => view.embedding.as_ref(),
        PurposeKindApi::Titling => view.titling.as_ref(),
    }
}

fn purpose_label(kind: PurposeKindApi) -> &'static str {
    match kind {
        PurposeKindApi::Interactive => "Interactive",
        PurposeKindApi::Dreaming => "Dreaming",
        PurposeKindApi::Consolidation => "Consolidation",
        PurposeKindApi::Embedding => "Embedding",
        PurposeKindApi::Titling => "Titling",
    }
}

fn effort_label(effort: Option<EffortLevel>) -> &'static str {
    match effort {
        None => "(unset)",
        Some(EffortLevel::Low) => "low",
        Some(EffortLevel::Medium) => "medium",
        Some(EffortLevel::High) => "high",
    }
}

async fn send(client: &TransportClient, command: Command) -> anyhow::Result<CommandResult> {
    if let Some(commands) = client.as_commands() {
        commands.send_command(command).await
    } else {
        anyhow::bail!(
            "Purposes management isn't available over D-Bus — switch transport with --transport ws or the local socket"
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
            Constraint::Min(7),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(f, chunks[0]);
    match state.mode {
        Mode::List => draw_list(f, state, chunks[1]),
        Mode::Edit => draw_edit_form(f, state, chunks[1]),
    }
    draw_status(f, state, chunks[2]);
    draw_hints(f, state, chunks[3]);
}

fn draw_header(f: &mut Frame, area: Rect) {
    let line = Line::from(vec![
        Span::styled(
            "Purposes",
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
    let items: Vec<ListItem> = PURPOSES
        .iter()
        .map(|kind| {
            let view = purpose_slot(&state.purposes, *kind);
            let label = purpose_label(*kind);
            let mut spans = vec![
                Span::styled(
                    format!("{label:<12}"),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
            ];
            match view {
                None => {
                    spans.push(Span::styled(
                        if matches!(kind, PurposeKindApi::Interactive) {
                            "(unconfigured)"
                        } else {
                            "(inherit primary)"
                        },
                        Style::default()
                            .fg(theme().debug_system)
                            .add_modifier(Modifier::ITALIC),
                    ));
                }
                Some(cfg) => {
                    let connection_text = if cfg.connection == PRIMARY {
                        "primary".to_string()
                    } else {
                        cfg.connection.clone()
                    };
                    let model_text = if cfg.model == PRIMARY {
                        "primary".to_string()
                    } else {
                        cfg.model.clone()
                    };
                    spans.push(Span::styled(connection_text, Style::default()));
                    spans.push(Span::styled(" · ", Style::default().fg(theme().hint_sep)));
                    spans.push(Span::styled(model_text, Style::default()));
                    if let Some(eff) = cfg.effort {
                        spans.push(Span::styled(
                            format!("  ({})", effort_label(Some(eff))),
                            Style::default().fg(theme().text_dim),
                        ));
                    }
                }
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme().border))
                .title(Line::from(Span::styled(
                    "Purpose · Connection · Model · Effort",
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
    list_state.select(Some(state.selected_row));
    f.render_stateful_widget(list, area, &mut list_state);
}

fn draw_edit_form(f: &mut Frame, state: &State, area: Rect) {
    let title = format!("Edit purpose: {}", purpose_label(state.edit.purpose));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme().border))
        .title(Line::from(Span::styled(
            title,
            Style::default()
                .fg(theme().title)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .split(inner);

    let label_for = |s: &str, focused: bool| {
        let style = if focused {
            Style::default()
                .fg(theme().border_active)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme().text_dim)
        };
        Paragraph::new(Span::styled(s.to_string(), style))
    };
    let value_for = |s: String, focused: bool| {
        let style = if focused {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let prefix = if focused { "  ▸ " } else { "    " };
        Paragraph::new(Span::styled(format!("{prefix}{s}"), style))
    };

    let conn_focused = state.edit.field == EditField::Connection;
    f.render_widget(
        label_for("Connection (←/→ or Space)", conn_focused),
        rows[0],
    );
    f.render_widget(
        value_for(state.edit.connection.clone(), conn_focused),
        rows[1],
    );

    let model_focused = state.edit.field == EditField::Model;
    f.render_widget(label_for("Model (←/→ or Space)", model_focused), rows[2]);
    f.render_widget(value_for(state.edit.model.clone(), model_focused), rows[3]);

    let effort_focused = state.edit.field == EditField::Effort;
    f.render_widget(
        label_for("Effort (Anthropic-only; n/a elsewhere)", effort_focused),
        rows[4],
    );
    f.render_widget(
        value_for(effort_label(state.edit.effort).to_string(), effort_focused),
        rows[5],
    );
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
        Mode::List => &[("Enter", "edit"), ("r", "refresh"), ("Esc", "back to chat")],
        Mode::Edit => &[
            ("Tab", "next field"),
            ("←/→", "cycle value"),
            ("Ctrl+S", "save"),
            ("Esc", "cancel"),
        ],
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

    fn empty_purposes() -> PurposesView {
        PurposesView::default()
    }

    #[test]
    fn from_view_uses_existing_config_when_present() {
        let view = PurposeConfigView {
            connection: "work".into(),
            model: "claude-3-5-sonnet".into(),
            effort: Some(EffortLevel::High),
            max_context_tokens: None,
        };
        let edit = EditState::from_view(PurposeKindApi::Interactive, Some(&view));
        assert_eq!(edit.connection, "work");
        assert_eq!(edit.model, "claude-3-5-sonnet");
        assert_eq!(edit.effort, Some(EffortLevel::High));
    }

    #[test]
    fn from_view_defaults_non_interactive_to_inherit() {
        let edit = EditState::from_view(PurposeKindApi::Dreaming, None);
        assert_eq!(edit.connection, PRIMARY);
        assert_eq!(edit.model, PRIMARY);
    }

    #[test]
    fn from_view_defaults_interactive_to_blank() {
        let edit = EditState::from_view(PurposeKindApi::Interactive, None);
        assert!(edit.connection.is_empty());
        assert!(edit.model.is_empty());
    }

    #[test]
    fn submit_rejects_blank_interactive() {
        let edit = EditState::from_view(PurposeKindApi::Interactive, None);
        assert!(edit.submit().is_err());
    }

    #[test]
    fn submit_rejects_primary_for_interactive() {
        let mut edit = EditState::from_view(PurposeKindApi::Interactive, None);
        edit.connection = PRIMARY.into();
        edit.model = PRIMARY.into();
        let err = edit.submit().unwrap_err();
        assert!(err.contains("can't inherit"));
    }

    #[test]
    fn submit_rejects_mixed_primary_pair() {
        let mut edit = EditState::from_view(PurposeKindApi::Dreaming, None);
        edit.connection = "work".into();
        edit.model = PRIMARY.into();
        let err = edit.submit().unwrap_err();
        assert!(err.contains("'primary'"));
    }

    #[test]
    fn submit_accepts_primary_pair_for_non_interactive() {
        let edit = EditState::from_view(PurposeKindApi::Embedding, None);
        let cfg = edit.submit().unwrap();
        assert_eq!(cfg.connection, PRIMARY);
        assert_eq!(cfg.model, PRIMARY);
    }

    #[test]
    fn submit_accepts_real_pair() {
        let mut edit = EditState::from_view(PurposeKindApi::Interactive, None);
        edit.connection = "work".into();
        edit.model = "claude-3-5-sonnet".into();
        edit.effort = Some(EffortLevel::Medium);
        let cfg = edit.submit().unwrap();
        assert_eq!(cfg.connection, "work");
        assert_eq!(cfg.model, "claude-3-5-sonnet");
        assert_eq!(cfg.effort, Some(EffortLevel::Medium));
    }

    #[test]
    fn cycle_effort_walks_through_all_options() {
        let mut state = State {
            connections: Vec::new(),
            models: Vec::new(),
            purposes: empty_purposes(),
            selected_row: 0,
            mode: Mode::Edit,
            edit: EditState::from_view(PurposeKindApi::Interactive, None),
            error: None,
            busy: None,
            closing: false,
        };
        // Default starts at None
        assert_eq!(state.edit.effort, None);
        cycle_effort(&mut state, 1);
        assert_eq!(state.edit.effort, Some(EffortLevel::Low));
        cycle_effort(&mut state, 1);
        assert_eq!(state.edit.effort, Some(EffortLevel::Medium));
        cycle_effort(&mut state, 1);
        assert_eq!(state.edit.effort, Some(EffortLevel::High));
        cycle_effort(&mut state, 1);
        assert_eq!(state.edit.effort, None);
        // Reverse direction
        cycle_effort(&mut state, -1);
        assert_eq!(state.edit.effort, Some(EffortLevel::High));
    }

    #[test]
    fn next_prev_field_cycle() {
        let mut edit = EditState::from_view(PurposeKindApi::Interactive, None);
        assert_eq!(edit.field, EditField::Connection);
        edit.next_field();
        assert_eq!(edit.field, EditField::Model);
        edit.next_field();
        assert_eq!(edit.field, EditField::Effort);
        edit.next_field();
        assert_eq!(edit.field, EditField::Connection);
        edit.prev_field();
        assert_eq!(edit.field, EditField::Effort);
    }

    #[test]
    fn wrap_index_handles_negative_and_overflow() {
        assert_eq!(wrap_index(-1, 3), 2);
        assert_eq!(wrap_index(0, 3), 0);
        assert_eq!(wrap_index(3, 3), 0);
        assert_eq!(wrap_index(4, 3), 1);
        assert_eq!(wrap_index(0, 0), 0);
    }
}
