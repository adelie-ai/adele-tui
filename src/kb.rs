//! Knowledge base browser/editor.
//!
//! Self-contained screen reachable from the chat with `Ctrl+K`. Renders
//! over the existing terminal session — when the user closes it (`Esc`),
//! the chat resumes where it left off.
//!
//! Keys
//! ----
//!
//! List mode:
//! - `j/k` or arrows: navigate
//! - `Enter` or `e`: edit selected
//! - `n`: new entry
//! - `d`: delete selected (confirm overlay)
//! - `/`: focus search input
//! - `Esc` or `q`: close
//!
//! Search input (focused via `/`):
//! - typing → 250ms debounced search
//! - `Enter`: commit + return focus to list
//! - `Esc`: clear search, return to list
//!
//! Edit mode:
//! - `Tab` / `Shift+Tab`: cycle fields (Content, Tags, Metadata)
//! - `Ctrl+S`: save
//! - `Esc`: cancel
//!
//! Delete-confirm overlay:
//! - `y/Enter`: confirm
//! - any other key: cancel

use std::{io, time::Duration};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use desktop_assistant_api_model::KnowledgeEntryView;
use desktop_assistant_client_common::{AssistantClient, SignalEvent, TransportClient};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use ratatui_textarea::{CursorMove, TextArea};
use tokio::time::Instant;

use crate::screen::Screen;

const LIST_LIMIT: u32 = 100;
const SEARCH_LIMIT: u32 = 50;
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);

const COLOR_BORDER: Color = Color::Rgb(82, 104, 173);
const COLOR_BORDER_ACTIVE: Color = Color::Rgb(120, 183, 109);
const COLOR_TITLE: Color = Color::Rgb(166, 182, 255);
const COLOR_HINT_KEY: Color = Color::Rgb(216, 223, 236);
const COLOR_HINT_DESC: Color = Color::Rgb(143, 153, 174);
const COLOR_HINT_SEP: Color = Color::Rgb(82, 90, 110);
const COLOR_LIST_HIGHLIGHT: Color = Color::Rgb(72, 102, 180);
const COLOR_LIST_HIGHLIGHT_FG: Color = Color::Rgb(245, 248, 255);
const COLOR_ERROR: Color = Color::Rgb(232, 130, 130);
const COLOR_DELETE_BORDER: Color = Color::Rgb(232, 130, 130);
const COLOR_TIMESTAMP: Color = Color::Rgb(140, 156, 196);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    List,
    Search,
    Edit,
    DeleteConfirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditField {
    Content,
    Tags,
    Metadata,
}

const FIELD_ORDER: [EditField; 3] = [EditField::Content, EditField::Tags, EditField::Metadata];

struct EditForm {
    /// `Some(id)` when editing an existing entry; `None` for a new one.
    editing_id: Option<String>,
    /// Read-only label for the existing entry's `updated_at`.
    updated_at: Option<String>,
    focus: EditField,
    content: TextArea<'static>,
    tags: TextArea<'static>,
    metadata: TextArea<'static>,
}

impl EditForm {
    fn empty() -> Self {
        Self {
            editing_id: None,
            updated_at: None,
            focus: EditField::Content,
            content: new_textarea(),
            tags: single_line_textarea(),
            metadata: new_textarea(),
        }
    }

    fn from_entry(entry: &KnowledgeEntryView) -> Self {
        let mut content = new_textarea();
        for line in entry.content.split('\n') {
            content.insert_str(line);
            content.insert_newline();
        }
        // Drop the trailing newline insert_newline left.
        content_trim_trailing_newline(&mut content);

        let mut tags = single_line_textarea();
        tags.insert_str(entry.tags.join(", "));
        tags.move_cursor(CursorMove::End);

        let mut metadata = new_textarea();
        let pretty = serde_json::to_string_pretty(&entry.metadata)
            .unwrap_or_else(|_| entry.metadata.to_string());
        for line in pretty.split('\n') {
            metadata.insert_str(line);
            metadata.insert_newline();
        }
        content_trim_trailing_newline(&mut metadata);

        Self {
            editing_id: Some(entry.id.clone()),
            updated_at: Some(entry.updated_at.clone()),
            focus: EditField::Content,
            content,
            tags,
            metadata,
        }
    }

    fn next_field(&mut self) {
        let pos = FIELD_ORDER
            .iter()
            .position(|f| *f == self.focus)
            .unwrap_or(0);
        self.focus = FIELD_ORDER[(pos + 1) % FIELD_ORDER.len()];
    }

    fn prev_field(&mut self) {
        let pos = FIELD_ORDER
            .iter()
            .position(|f| *f == self.focus)
            .unwrap_or(0);
        self.focus = FIELD_ORDER[(pos + FIELD_ORDER.len() - 1) % FIELD_ORDER.len()];
    }

    /// Validate and return `(content, tags, metadata)` if the inputs parse.
    fn submit(&self) -> Result<(String, Vec<String>, serde_json::Value), String> {
        let content = self.content.lines().join("\n");
        if content.trim().is_empty() {
            return Err("Content is required".into());
        }
        let tags: Vec<String> = self
            .tags
            .lines()
            .join(",")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let metadata_text = self.metadata.lines().join("\n");
        let metadata = if metadata_text.trim().is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str::<serde_json::Value>(&metadata_text)
                .map_err(|e| format!("Metadata is not valid JSON: {e}"))?
        };
        Ok((content, tags, metadata))
    }
}

fn content_trim_trailing_newline(ta: &mut TextArea<'static>) {
    let lines = ta.lines();
    if let Some(last) = lines.last()
        && last.is_empty()
        && lines.len() > 1
    {
        // tui-textarea has no direct truncate; rebuild without the trailing line.
        let kept: Vec<String> = lines[..lines.len() - 1]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut replacement = TextArea::from(kept);
        replacement.set_cursor_line_style(Style::default());
        replacement.move_cursor(CursorMove::End);
        *ta = replacement;
    }
}

fn new_textarea() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_cursor_line_style(Style::default());
    ta
}

fn single_line_textarea() -> TextArea<'static> {
    new_textarea()
}

struct State {
    entries: Vec<KnowledgeEntryView>,
    selected: usize,
    mode: Mode,
    /// Mode to return to when the user finishes a transient state (Search,
    /// DeleteConfirm). Always List in practice today.
    return_mode: Mode,
    search: TextArea<'static>,
    /// Time at which the next debounced search should run. `None` outside
    /// pending search.
    search_deadline: Option<Instant>,
    edit: EditForm,
    error: Option<String>,
    busy: Option<String>,
    /// True when the user pressed Esc on the list with intent to close.
    closing: bool,
}

use crate::in_flight::InFlight;

/// Resolved outcome of an off-loop KB RPC (modal-freeze fix). `Entries` covers
/// both the list and search RPCs (both return entry vecs).
enum RpcOutcome {
    Entries(Result<Vec<KnowledgeEntryView>, String>),
    Saved(Result<KnowledgeEntryView, String>),
    Deleted {
        id: String,
        result: Result<(), String>,
    },
}

/// The KB browser as a [`Screen`]: its mutable [`State`] plus the borrowed
/// transport client the key/timer handlers need. The shared driver supplies the
/// event loop and — the reason it's shared — drains daemon signals while this
/// screen is open (TUI-12).
struct KbScreen<'a> {
    state: State,
    client: &'a TransportClient,
    /// In-flight list/search/save/delete RPCs, polled off the draw loop by
    /// `poll_pending` so the browser never freezes during a round-trip.
    pending: InFlight<'a, RpcOutcome>,
}

impl Screen for KbScreen<'_> {
    type Outcome = ();

    fn draw(&mut self, frame: &mut Frame) {
        draw(frame, &self.state);
    }

    fn handle_key(&mut self, key: KeyEvent) -> impl std::future::Future<Output = ()> {
        handle_key(&mut self.state, key, self.client, &mut self.pending);
        std::future::ready(())
    }

    fn take_outcome(&mut self) -> Option<()> {
        self.state.closing.then_some(())
    }

    fn next_timer(&self) -> Option<Instant> {
        self.state.search_deadline
    }

    fn on_timer(&mut self) -> impl std::future::Future<Output = ()> {
        // Debounced search: enqueue it off-loop instead of awaiting here, so the
        // browser keeps drawing/handling input while the search runs.
        self.state.search_deadline = None;
        let query = self.state.search.lines().join(" ").trim().to_string();
        run_search(&mut self.state, &mut self.pending, self.client, query);
        std::future::ready(())
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    async fn poll_pending(&mut self) {
        let resolved = self.pending.next().await;
        if let Some(outcome) = resolved {
            apply_outcome(&mut self.state, outcome);
        }
    }
}

/// Run the KB browser until the user closes it. Returns when `Esc/q` is
/// pressed in list mode or any unrecoverable transport error surfaces.
///
/// `signal_rx` + `sink` are forwarded to the shared driver so daemon signals keep
/// flowing while the browser is open (TUI-12).
pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    client: &TransportClient,
    signal_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SignalEvent>,
    sink: &mut impl crate::screen::SignalSink,
) -> anyhow::Result<()> {
    let mut screen = KbScreen {
        state: State {
            entries: Vec::new(),
            selected: 0,
            mode: Mode::List,
            return_mode: Mode::List,
            search: single_line_textarea(),
            search_deadline: None,
            edit: EditForm::empty(),
            error: None,
            busy: Some("Loading entries...".into()),
            closing: false,
        },
        client,
        pending: InFlight::new(),
    };

    // Initial fetch (off-loop).
    refresh_list(&mut screen.state, &mut screen.pending, client);

    crate::screen::run_screen(terminal, &mut screen, signal_rx, sink).await
}

fn handle_key<'a>(
    state: &mut State,
    key: KeyEvent,
    client: &'a TransportClient,
    pending: &mut InFlight<'a, RpcOutcome>,
) {
    match state.mode {
        Mode::List => handle_list_key(state, key, client, pending),
        Mode::Search => handle_search_key(state, key),
        Mode::Edit => handle_edit_key(state, key, client, pending),
        Mode::DeleteConfirm => handle_delete_key(state, key, client, pending),
    }
}

fn handle_list_key<'a>(
    state: &mut State,
    key: KeyEvent,
    client: &'a TransportClient,
    pending: &mut InFlight<'a, RpcOutcome>,
) {
    match (key.code, key.modifiers) {
        (KeyCode::Esc | KeyCode::Char('q'), m) if m.is_empty() => {
            state.closing = true;
        }
        (KeyCode::Char('j') | KeyCode::Down, m) if m.is_empty() => advance_selection(state, 1),
        (KeyCode::Char('k') | KeyCode::Up, m) if m.is_empty() => advance_selection(state, -1),
        (KeyCode::Enter | KeyCode::Char('e'), m) if m.is_empty() => {
            if let Some(entry) = state.entries.get(state.selected).cloned() {
                state.edit = EditForm::from_entry(&entry);
                state.error = None;
                state.mode = Mode::Edit;
            }
        }
        (KeyCode::Char('n'), m) if m.is_empty() => {
            state.edit = EditForm::empty();
            state.error = None;
            state.mode = Mode::Edit;
        }
        (KeyCode::Char('d'), m) if m.is_empty() && state.entries.get(state.selected).is_some() => {
            state.mode = Mode::DeleteConfirm;
        }
        (KeyCode::Char('/'), m) if m.is_empty() => {
            state.mode = Mode::Search;
        }
        (KeyCode::Char('r'), m) if m.is_empty() => refresh_list(state, pending, client),
        _ => {}
    }
}

fn handle_search_key(state: &mut State, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            state.search = single_line_textarea();
            state.search_deadline = None;
            state.mode = Mode::List;
            // Cleared query — schedule an immediate "back to full list" pass.
            state.search_deadline = Some(Instant::now());
        }
        (KeyCode::Enter, m) if m.is_empty() => {
            // Commit current input by running the search now.
            state.search_deadline = Some(Instant::now());
            state.mode = Mode::List;
        }
        _ => {
            state.search.input(key);
            state.search_deadline = Some(Instant::now() + SEARCH_DEBOUNCE);
        }
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
            state.edit = EditForm::empty();
            state.mode = Mode::List;
        }
        (KeyCode::Tab, _) => state.edit.next_field(),
        (KeyCode::BackTab, _) => state.edit.prev_field(),
        _ => match state.edit.focus {
            EditField::Content => {
                state.edit.content.input(key);
            }
            EditField::Tags => {
                // Tags is single-line: swallow Enter so it doesn't insert a newline.
                if matches!(key.code, KeyCode::Enter) {
                    return;
                }
                state.edit.tags.input(key);
            }
            EditField::Metadata => {
                state.edit.metadata.input(key);
            }
        },
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
            if let Some(entry) = state.entries.get(state.selected).cloned() {
                state.busy = Some("Deleting entry...".into());
                let id = entry.id.clone();
                pending.push(async move {
                    RpcOutcome::Deleted {
                        result: client
                            .delete_knowledge_entry(&id)
                            .await
                            .map_err(|e| format!("Delete failed: {e}")),
                        id,
                    }
                });
            }
            state.mode = state.return_mode;
        }
        _ => state.mode = Mode::List,
    }
}

fn advance_selection(state: &mut State, delta: i32) {
    let len = state.entries.len();
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
    state.busy = Some("Loading entries...".into());
    pending.push(async move {
        RpcOutcome::Entries(
            client
                .list_knowledge_entries(LIST_LIMIT, 0, None)
                .await
                .map_err(|e| format!("Failed to load entries: {e}")),
        )
    });
}

fn run_search<'a>(
    state: &mut State,
    pending: &mut InFlight<'a, RpcOutcome>,
    client: &'a TransportClient,
    query: String,
) {
    if query.is_empty() {
        refresh_list(state, pending, client);
        return;
    }
    state.busy = Some("Searching...".into());
    pending.push(async move {
        RpcOutcome::Entries(
            client
                .search_knowledge_entries(&query, None, SEARCH_LIMIT)
                .await
                .map_err(|e| format!("Search failed: {e}")),
        )
    });
}

fn save_edit<'a>(
    state: &mut State,
    pending: &mut InFlight<'a, RpcOutcome>,
    client: &'a TransportClient,
) {
    let (content, tags, metadata) = match state.edit.submit() {
        Ok(parts) => parts,
        Err(e) => {
            state.error = Some(e);
            return;
        }
    };

    state.busy = Some("Saving...".into());
    let editing_id = state.edit.editing_id.clone();
    pending.push(async move {
        let result = if let Some(id) = editing_id {
            client
                .update_knowledge_entry(&id, &content, tags, metadata)
                .await
        } else {
            client
                .create_knowledge_entry(&content, tags, metadata)
                .await
        };
        RpcOutcome::Saved(result.map_err(|e| format!("Save failed: {e}")))
    });
}

/// Apply a resolved KB RPC. `Saved`/`Deleted` patch the list in place (no
/// refetch); `Entries` replaces it (used by both the list and search RPCs).
fn apply_outcome(state: &mut State, outcome: RpcOutcome) {
    state.busy = None;
    match outcome {
        RpcOutcome::Entries(Ok(entries)) => {
            state.entries = entries;
            if state.selected >= state.entries.len() {
                state.selected = state.entries.len().saturating_sub(1);
            }
        }
        RpcOutcome::Entries(Err(e)) => state.error = Some(e),
        RpcOutcome::Saved(Ok(saved)) => {
            // Replace or insert the saved entry in the list.
            if let Some(existing) = state.entries.iter_mut().find(|e| e.id == saved.id) {
                *existing = saved.clone();
            } else {
                state.entries.insert(0, saved.clone());
            }
            state.selected = state
                .entries
                .iter()
                .position(|e| e.id == saved.id)
                .unwrap_or(0);
            state.error = None;
            state.edit = EditForm::empty();
            state.mode = Mode::List;
        }
        RpcOutcome::Saved(Err(e)) => state.error = Some(e),
        RpcOutcome::Deleted { id, result } => match result {
            Ok(()) => {
                if let Some(pos) = state.entries.iter().position(|e| e.id == id) {
                    state.entries.remove(pos);
                    if state.selected >= state.entries.len() {
                        state.selected = state.entries.len().saturating_sub(1);
                    }
                }
            }
            Err(e) => state.error = Some(e),
        },
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
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(f, chunks[0]);
    draw_search_bar(f, state, chunks[1]);
    match state.mode {
        Mode::Edit => draw_edit_form(f, state, chunks[2]),
        _ => draw_list(f, state, chunks[2]),
    }
    draw_status(f, state, chunks[3]);
    draw_hints(f, state, chunks[4]);

    if matches!(state.mode, Mode::DeleteConfirm) {
        draw_delete_overlay(f, state, area);
    }
}

fn draw_header(f: &mut Frame, area: Rect) {
    let line = Line::from(vec![
        Span::styled(
            "Knowledge base",
            Style::default()
                .fg(COLOR_TITLE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  —  Esc to return to chat",
            Style::default().fg(COLOR_HINT_DESC),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_search_bar(f: &mut Frame, state: &State, area: Rect) {
    let focused = matches!(state.mode, Mode::Search);
    let mut ta = state.search.clone();
    let border_color = if focused {
        COLOR_BORDER_ACTIVE
    } else {
        COLOR_BORDER
    };
    ta.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(Line::from(Span::styled(
                if focused {
                    "Search (Enter to commit, Esc to clear)"
                } else {
                    "Search (press / to focus)"
                },
                Style::default().fg(COLOR_TITLE),
            ))),
    );
    f.render_widget(&ta, area);
}

fn draw_list(f: &mut Frame, state: &State, area: Rect) {
    let items: Vec<ListItem> = if state.entries.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "(no entries — press 'n' to create one)",
            Style::default().fg(COLOR_HINT_DESC),
        )))]
    } else {
        state
            .entries
            .iter()
            .map(|entry| {
                let summary = entry.content.lines().next().unwrap_or("").to_string();
                let trimmed = if summary.chars().count() > 80 {
                    let mut s: String = summary.chars().take(80).collect();
                    s.push('…');
                    s
                } else {
                    summary
                };
                let mut spans: Vec<Span<'static>> = Vec::new();
                spans.push(Span::styled(trimmed, Style::default()));
                if !entry.tags.is_empty() {
                    spans.push(Span::styled(
                        format!("  [{}]", entry.tags.join(", ")),
                        Style::default().fg(COLOR_HINT_DESC),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect()
    };

    let title = if state.entries.is_empty() {
        "Entries".to_string()
    } else {
        format!("Entries ({})", state.entries.len())
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(COLOR_BORDER))
                .title(Line::from(Span::styled(
                    title,
                    Style::default()
                        .fg(COLOR_TITLE)
                        .add_modifier(Modifier::BOLD),
                ))),
        )
        .highlight_style(
            Style::default()
                .bg(COLOR_LIST_HIGHLIGHT)
                .fg(COLOR_LIST_HIGHLIGHT_FG)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");

    let mut list_state = ListState::default();
    if !state.entries.is_empty() {
        list_state.select(Some(state.selected));
    }
    f.render_stateful_widget(list, area, &mut list_state);
}

fn draw_edit_form(f: &mut Frame, state: &State, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BORDER))
        .title(Line::from(Span::styled(
            if state.edit.editing_id.is_some() {
                "Edit entry"
            } else {
                "New entry"
            },
            Style::default()
                .fg(COLOR_TITLE)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(inner);

    // Metadata header strip
    let header_text = match (
        state.edit.editing_id.as_deref(),
        state.edit.updated_at.as_deref(),
    ) {
        (Some(id), Some(ts)) => format!("id: {id}  ·  updated: {ts}"),
        (Some(id), None) => format!("id: {id}"),
        _ => String::from("(new)"),
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            header_text,
            Style::default().fg(COLOR_TIMESTAMP),
        )),
        rows[0],
    );

    let focus = state.edit.focus;
    draw_text_field_with_label(
        f,
        rows[1],
        &state.edit.content,
        focus == EditField::Content,
        "Content",
    );
    draw_field_label(
        f,
        rows[2],
        "Tags (comma-separated)",
        focus == EditField::Tags,
    );
    draw_text_field(f, rows[3], &state.edit.tags, focus == EditField::Tags);
    draw_field_label(f, rows[4], "Metadata (JSON)", focus == EditField::Metadata);
    draw_text_field(
        f,
        rows[5],
        &state.edit.metadata,
        focus == EditField::Metadata,
    );
}

fn draw_text_field_with_label(
    f: &mut Frame,
    area: Rect,
    textarea: &TextArea<'static>,
    focused: bool,
    title: &str,
) {
    let mut ta = textarea.clone();
    let border_color = if focused {
        COLOR_BORDER_ACTIVE
    } else {
        COLOR_BORDER
    };
    ta.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(Line::from(Span::styled(
                title.to_string(),
                Style::default().fg(COLOR_TITLE),
            ))),
    );
    f.render_widget(&ta, area);
}

fn draw_field_label(f: &mut Frame, area: Rect, label: &str, focused: bool) {
    let style = if focused {
        Style::default()
            .fg(COLOR_BORDER_ACTIVE)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(COLOR_HINT_DESC)
    };
    f.render_widget(Paragraph::new(Span::styled(label.to_string(), style)), area);
}

fn draw_text_field(f: &mut Frame, area: Rect, textarea: &TextArea<'static>, focused: bool) {
    let mut ta = textarea.clone();
    let border_color = if focused {
        COLOR_BORDER_ACTIVE
    } else {
        COLOR_BORDER
    };
    ta.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color)),
    );
    f.render_widget(&ta, area);
}

fn draw_status(f: &mut Frame, state: &State, area: Rect) {
    if let Some(busy) = &state.busy {
        let style = Style::default()
            .fg(Color::Rgb(178, 220, 245))
            .add_modifier(Modifier::ITALIC);
        f.render_widget(
            Paragraph::new(Span::styled(format!(" ● {busy}"), style)),
            area,
        );
    } else if let Some(err) = &state.error {
        let style = Style::default().fg(COLOR_ERROR);
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
            ("n", "new"),
            ("d", "delete"),
            ("/", "search"),
            ("r", "refresh"),
            ("Esc", "back to chat"),
        ],
        Mode::Search => &[("Enter", "search"), ("Esc", "clear & back")],
        Mode::Edit => &[("Tab", "next field"), ("Ctrl+S", "save"), ("Esc", "cancel")],
        Mode::DeleteConfirm => &[("y/Enter", "confirm"), ("any", "cancel")],
    };

    let mut spans: Vec<Span> = Vec::with_capacity(hints.len() * 4);
    for (idx, (key, desc)) in hints.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled("  ·  ", Style::default().fg(COLOR_HINT_SEP)));
        }
        spans.push(Span::styled(
            (*key).to_string(),
            Style::default()
                .fg(COLOR_HINT_KEY)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            (*desc).to_string(),
            Style::default().fg(COLOR_HINT_DESC),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_delete_overlay(f: &mut Frame, state: &State, area: Rect) {
    let label = state
        .entries
        .get(state.selected)
        .map(|e| {
            let summary = e.content.lines().next().unwrap_or("").to_string();
            if summary.chars().count() > 60 {
                let mut s: String = summary.chars().take(60).collect();
                s.push('…');
                s
            } else {
                summary
            }
        })
        .unwrap_or_else(|| "this entry".into());
    let popup_width = 64.min(area.width.saturating_sub(4));
    let popup_height = 5.min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_width)) / 2,
        y: area.y + (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    };
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_DELETE_BORDER))
        .title(Line::from(Span::styled(
            "Delete entry",
            Style::default()
                .fg(Color::Rgb(255, 200, 200))
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
            "y/Enter = confirm · any other key = cancel",
            Style::default().fg(COLOR_HINT_DESC),
        )),
    ])
    .wrap(Wrap { trim: true });
    f.render_widget(body, inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: &str, content: &str) -> KnowledgeEntryView {
        KnowledgeEntryView {
            id: id.into(),
            content: content.into(),
            tags: vec!["preference".into()],
            metadata: json!({"source": "manual"}),
            created_at: "2026-05-01T00:00:00Z".into(),
            updated_at: "2026-05-02T12:34:56Z".into(),
        }
    }

    #[test]
    fn empty_form_submit_rejects_blank_content() {
        let form = EditForm::empty();
        assert!(form.submit().is_err());
    }

    #[test]
    fn form_submit_parses_tags_and_metadata() {
        let mut form = EditForm::empty();
        form.content.insert_str("hello world");
        form.tags.insert_str("alpha, beta , ,gamma");
        form.metadata.insert_str("{\"k\":\"v\"}");
        let (content, tags, metadata) = form.submit().unwrap();
        assert_eq!(content, "hello world");
        assert_eq!(tags, vec!["alpha", "beta", "gamma"]);
        assert_eq!(metadata, json!({"k": "v"}));
    }

    #[test]
    fn form_submit_treats_empty_metadata_as_null() {
        let mut form = EditForm::empty();
        form.content.insert_str("body");
        let (_, _, metadata) = form.submit().unwrap();
        assert_eq!(metadata, serde_json::Value::Null);
    }

    #[test]
    fn form_submit_rejects_invalid_metadata_json() {
        let mut form = EditForm::empty();
        form.content.insert_str("body");
        form.metadata.insert_str("{not json");
        let err = form.submit().err().unwrap();
        assert!(err.contains("Metadata"));
    }

    #[test]
    fn from_entry_populates_all_fields() {
        let e = entry("abc", "first line\nsecond line");
        let form = EditForm::from_entry(&e);
        assert_eq!(form.editing_id.as_deref(), Some("abc"));
        assert!(form.updated_at.is_some());
        assert_eq!(form.content.lines().join("\n"), "first line\nsecond line");
        assert_eq!(form.tags.lines().join(""), "preference");
    }

    #[test]
    fn next_and_prev_field_cycle() {
        let mut form = EditForm::empty();
        assert_eq!(form.focus, EditField::Content);
        form.next_field();
        assert_eq!(form.focus, EditField::Tags);
        form.next_field();
        assert_eq!(form.focus, EditField::Metadata);
        form.next_field();
        assert_eq!(form.focus, EditField::Content);
        form.prev_field();
        assert_eq!(form.focus, EditField::Metadata);
    }
}
