//! Pre-chat profile picker.
//!
//! Renders a small modal-style screen listing saved profiles with shortcuts
//! to add or delete entries. Returns the chosen `Profile` (or `None` if the
//! user quit). Lives outside the chat UI so it doesn't widen the chat
//! state machine.

use std::io;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use desktop_assistant_client_common::TransportMode;
use futures::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use tui_textarea::{CursorMove, TextArea};

use crate::profile::{Profile, ProfileStore};

const COLOR_BORDER: Color = Color::Rgb(82, 104, 173);
const COLOR_BORDER_ACTIVE: Color = Color::Rgb(120, 183, 109);
const COLOR_TITLE: Color = Color::Rgb(166, 182, 255);
const COLOR_HINT_KEY: Color = Color::Rgb(216, 223, 236);
const COLOR_HINT_DESC: Color = Color::Rgb(143, 153, 174);
const COLOR_HINT_SEP: Color = Color::Rgb(82, 90, 110);
const COLOR_LIST_HIGHLIGHT: Color = Color::Rgb(72, 102, 180);
const COLOR_LIST_HIGHLIGHT_FG: Color = Color::Rgb(245, 248, 255);
const COLOR_ERROR: Color = Color::Rgb(232, 130, 130);

/// Outcome of running the picker.
pub enum PickerOutcome {
    /// User picked or created a profile to connect to.
    Selected(Profile),
    /// User pressed quit before selecting anything.
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    List,
    Form,
    DeleteConfirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Transport,
    Url,
    Subject,
}

const FIELD_ORDER: [Field; 4] = [Field::Name, Field::Transport, Field::Url, Field::Subject];

struct PickerState {
    store: ProfileStore,
    selected: usize,
    mode: Mode,
    error: Option<String>,
    form: FormState,
}

struct FormState {
    /// `Some(id)` when editing an existing profile; `None` for a new one.
    editing_id: Option<String>,
    focus: Field,
    name: TextArea<'static>,
    url: TextArea<'static>,
    subject: TextArea<'static>,
    transport: TransportMode,
}

impl FormState {
    fn empty() -> Self {
        let mut name = single_line_textarea();
        let mut url = single_line_textarea();
        url.insert_str("ws://127.0.0.1:11339/ws");
        url.move_cursor(CursorMove::End);
        let mut subject = single_line_textarea();
        subject.insert_str("desktop-tui");
        subject.move_cursor(CursorMove::End);
        // Default focus on Name so users can just start typing.
        name.move_cursor(CursorMove::Head);
        Self {
            editing_id: None,
            focus: Field::Name,
            name,
            url,
            subject,
            transport: TransportMode::Ws,
        }
    }

    fn from_profile(profile: &Profile) -> Self {
        let mut form = Self::empty();
        form.editing_id = Some(profile.id.clone());
        form.name = single_line_textarea();
        form.name.insert_str(&profile.name);
        form.name.move_cursor(CursorMove::End);
        form.url = single_line_textarea();
        form.url.insert_str(&profile.ws_url);
        form.url.move_cursor(CursorMove::End);
        form.subject = single_line_textarea();
        form.subject.insert_str(&profile.ws_subject);
        form.subject.move_cursor(CursorMove::End);
        form.transport = profile.transport;
        form
    }

    fn next_field(&mut self) {
        let pos = FIELD_ORDER.iter().position(|f| *f == self.focus).unwrap_or(0);
        self.focus = FIELD_ORDER[(pos + 1) % FIELD_ORDER.len()];
    }

    fn prev_field(&mut self) {
        let pos = FIELD_ORDER.iter().position(|f| *f == self.focus).unwrap_or(0);
        self.focus = FIELD_ORDER[(pos + FIELD_ORDER.len() - 1) % FIELD_ORDER.len()];
    }

    fn submit(&self) -> Result<Profile, String> {
        let name = self.name.lines().join(" ").trim().to_string();
        if name.is_empty() {
            return Err("Name is required".into());
        }
        let ws_url = self.url.lines().join(" ").trim().to_string();
        if matches!(self.transport, TransportMode::Ws) && ws_url.is_empty() {
            return Err("URL is required for WebSocket transport".into());
        }
        let ws_subject = self.subject.lines().join(" ").trim().to_string();
        let id = self.editing_id.clone().unwrap_or_else(|| {
            // Sourced from time-since-epoch in profile::new_id; reuse its format.
            crate::profile::Profile::new(
                String::new(),
                TransportMode::Ws,
                String::new(),
                String::new(),
            )
            .id
        });
        Ok(Profile {
            id,
            name,
            transport: self.transport,
            ws_url,
            ws_subject,
        })
    }
}

fn single_line_textarea() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_cursor_line_style(Style::default());
    ta
}

/// Run the picker until the user selects, creates, or quits.
pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    store: ProfileStore,
) -> io::Result<(PickerOutcome, ProfileStore)> {
    let initial_mode = if store.profiles.is_empty() {
        Mode::Form
    } else {
        Mode::List
    };
    let initial_selected = store.last_used_index().unwrap_or(0);
    let mut state = PickerState {
        selected: initial_selected,
        store,
        mode: initial_mode,
        error: None,
        form: FormState::empty(),
    };
    if state.selected >= state.store.profiles.len() {
        state.selected = state.store.profiles.len().saturating_sub(1);
    }

    let mut events = crossterm::event::EventStream::new();
    loop {
        terminal.draw(|f| draw(f, &state))?;

        let evt = match events.next().await {
            Some(Ok(e)) => e,
            Some(Err(_)) | None => return Ok((PickerOutcome::Cancelled, state.store)),
        };
        let Event::Key(key) = evt else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }

        match state.mode {
            Mode::List => {
                if let Some(outcome) = handle_list_key(&mut state, key) {
                    return Ok((outcome, state.store));
                }
            }
            Mode::Form => handle_form_key(&mut state, key),
            Mode::DeleteConfirm => handle_delete_key(&mut state, key),
        }
    }
}

fn handle_list_key(state: &mut PickerState, key: KeyEvent) -> Option<PickerOutcome> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), m) if m.is_empty() => Some(PickerOutcome::Cancelled),
        (KeyCode::Esc, _) => Some(PickerOutcome::Cancelled),
        (KeyCode::Char('j') | KeyCode::Down, m) if m.is_empty() => {
            advance_selection(state, 1);
            None
        }
        (KeyCode::Char('k') | KeyCode::Up, m) if m.is_empty() => {
            advance_selection(state, -1);
            None
        }
        (KeyCode::Enter, m) if m.is_empty() => {
            if let Some(profile) = state.store.profiles.get(state.selected).cloned() {
                state.store.mark_used(&profile.id);
                let _ = state.store.save();
                Some(PickerOutcome::Selected(profile))
            } else {
                None
            }
        }
        (KeyCode::Char('a') | KeyCode::Char('+'), m) if m.is_empty() => {
            state.form = FormState::empty();
            state.error = None;
            state.mode = Mode::Form;
            None
        }
        (KeyCode::Char('e'), m) if m.is_empty() => {
            if let Some(profile) = state.store.profiles.get(state.selected) {
                state.form = FormState::from_profile(profile);
                state.error = None;
                state.mode = Mode::Form;
            }
            None
        }
        (KeyCode::Char('d'), m) if m.is_empty() => {
            if state.store.profiles.get(state.selected).is_some() {
                state.mode = Mode::DeleteConfirm;
            }
            None
        }
        _ => None,
    }
}

fn advance_selection(state: &mut PickerState, delta: i32) {
    let len = state.store.profiles.len();
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

fn handle_form_key(state: &mut PickerState, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            state.error = None;
            state.mode = if state.store.profiles.is_empty() {
                // No way back to a list — Esc with nothing to go to means
                // exit the picker entirely; we surface that on the next
                // iteration via the outer loop. Simpler: treat as a noop
                // so the user stays in the form. Add a dedicated quit hint.
                Mode::Form
            } else {
                Mode::List
            };
            // Clear form on cancel of edit so a re-entry starts fresh.
            state.form = FormState::empty();
        }
        (KeyCode::Tab, _) => state.form.next_field(),
        (KeyCode::BackTab, _) => state.form.prev_field(),
        (KeyCode::Up, m) if m.is_empty() => state.form.prev_field(),
        (KeyCode::Down, m) if m.is_empty() => state.form.next_field(),
        (KeyCode::Enter, m) if m.is_empty() => {
            match state.form.submit() {
                Ok(profile) => {
                    let editing = state.form.editing_id.is_some();
                    if editing {
                        // Replace existing profile in place.
                        if let Some(existing) = state
                            .store
                            .profiles
                            .iter_mut()
                            .find(|p| p.id == profile.id)
                        {
                            *existing = profile.clone();
                        }
                    } else {
                        state.store.add(profile.clone());
                    }
                    if let Err(e) = state.store.save() {
                        state.error = Some(format!("Save failed: {e}"));
                        return;
                    }
                    // Reselect to the saved profile.
                    let new_index = state
                        .store
                        .profiles
                        .iter()
                        .position(|p| p.id == profile.id)
                        .unwrap_or(state.selected);
                    state.selected = new_index;
                    state.error = None;
                    state.mode = Mode::List;
                    state.form = FormState::empty();
                }
                Err(msg) => state.error = Some(msg),
            }
        }
        // Transport field uses left/right or space to flip the toggle.
        (KeyCode::Left | KeyCode::Right | KeyCode::Char(' '), _)
            if state.form.focus == Field::Transport =>
        {
            state.form.transport = match state.form.transport {
                TransportMode::Ws => TransportMode::Dbus,
                TransportMode::Dbus => TransportMode::Ws,
            };
        }
        _ => match state.form.focus {
            Field::Name => {
                state.form.name.input(key);
            }
            Field::Url => {
                state.form.url.input(key);
            }
            Field::Subject => {
                state.form.subject.input(key);
            }
            Field::Transport => {}
        },
    }
}

fn handle_delete_key(state: &mut PickerState, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter, _) => {
            if let Some(profile) = state.store.profiles.get(state.selected).cloned() {
                state.store.remove(&profile.id);
                let _ = state.store.save();
                if state.selected >= state.store.profiles.len() {
                    state.selected = state.store.profiles.len().saturating_sub(1);
                }
            }
            state.mode = if state.store.profiles.is_empty() {
                Mode::Form
            } else {
                Mode::List
            };
            if matches!(state.mode, Mode::Form) {
                state.form = FormState::empty();
            }
        }
        _ => state.mode = Mode::List,
    }
}

fn draw(f: &mut Frame, state: &PickerState) {
    let area = f.area();
    f.render_widget(Clear, area);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(f, layout[0]);
    match state.mode {
        Mode::List => draw_list(f, state, layout[1]),
        Mode::Form => draw_form(f, state, layout[1]),
        Mode::DeleteConfirm => {
            draw_list(f, state, layout[1]);
            draw_delete_overlay(f, state, area);
        }
    }
    draw_error(f, state, layout[2]);
    draw_hints(f, state, layout[3]);

    if matches!(state.mode, Mode::Form) {
        // Keep cursor blinking in the focused text field.
        position_cursor_in_form(f, state, layout[1]);
    }
}

fn draw_header(f: &mut Frame, area: Rect) {
    let title = Paragraph::new(vec![
        Line::from(Span::styled(
            "Adele connection profiles",
            Style::default()
                .fg(COLOR_TITLE)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Pick a daemon to connect to, or create a new profile.",
            Style::default().fg(COLOR_HINT_DESC),
        )),
    ]);
    f.render_widget(title, area);
}

fn draw_list(f: &mut Frame, state: &PickerState, area: Rect) {
    let items: Vec<ListItem> = if state.store.profiles.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "(no saved profiles — press 'a' to add one)",
            Style::default().fg(COLOR_HINT_DESC),
        )))]
    } else {
        state
            .store
            .profiles
            .iter()
            .enumerate()
            .map(|(idx, p)| {
                let mut spans: Vec<Span<'static>> = Vec::new();
                if state.store.last_used_index() == Some(idx) {
                    spans.push(Span::styled(
                        "★ ",
                        Style::default().fg(Color::Rgb(255, 207, 119)),
                    ));
                }
                spans.push(Span::styled(p.display_label(), Style::default()));
                ListItem::new(Line::from(spans))
            })
            .collect()
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(COLOR_BORDER))
                .title(Line::from(Span::styled(
                    "Profiles",
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
    if !state.store.profiles.is_empty() {
        list_state.select(Some(state.selected));
    }
    f.render_stateful_widget(list, area, &mut list_state);
}

fn draw_form(f: &mut Frame, state: &PickerState, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BORDER))
        .title(Line::from(Span::styled(
            if state.form.editing_id.is_some() {
                "Edit profile"
            } else {
                "New profile"
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
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(inner);

    draw_field_label(f, rows[0], "Name", state.form.focus == Field::Name);
    draw_text_field(f, rows[1], &state.form.name, state.form.focus == Field::Name);

    draw_field_label(
        f,
        rows[2],
        "Transport (←/→ or Space to toggle)",
        state.form.focus == Field::Transport,
    );
    draw_transport_toggle(f, rows[3], state);

    draw_field_label(f, rows[4], "URL", state.form.focus == Field::Url);
    draw_text_field(f, rows[5], &state.form.url, state.form.focus == Field::Url);

    draw_field_label(
        f,
        rows[6],
        "JWT subject",
        state.form.focus == Field::Subject,
    );
    draw_text_field(
        f,
        rows[7],
        &state.form.subject,
        state.form.focus == Field::Subject,
    );
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

fn draw_transport_toggle(f: &mut Frame, area: Rect, state: &PickerState) {
    let focused = state.form.focus == Field::Transport;
    let border_color = if focused {
        COLOR_BORDER_ACTIVE
    } else {
        COLOR_BORDER
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let render_chip = |label: &str, selected: bool| {
        let style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(COLOR_BORDER_ACTIVE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(COLOR_HINT_DESC)
        };
        Span::styled(format!(" {label} "), style)
    };

    let line = Line::from(vec![
        render_chip("WebSocket", state.form.transport == TransportMode::Ws),
        Span::styled("  ", Style::default()),
        render_chip("D-Bus", state.form.transport == TransportMode::Dbus),
    ]);
    f.render_widget(Paragraph::new(line), inner);
}

fn draw_delete_overlay(f: &mut Frame, state: &PickerState, area: Rect) {
    let label = state
        .store
        .profiles
        .get(state.selected)
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "this profile".to_string());
    let popup_width = 60.min(area.width.saturating_sub(4));
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
        .border_style(Style::default().fg(Color::Rgb(232, 130, 130)))
        .title(Line::from(Span::styled(
            "Delete profile",
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

fn draw_error(f: &mut Frame, state: &PickerState, area: Rect) {
    if let Some(err) = &state.error {
        let style = Style::default().fg(COLOR_ERROR);
        f.render_widget(
            Paragraph::new(Span::styled(format!(" • {err}"), style)),
            area,
        );
    }
}

fn draw_hints(f: &mut Frame, state: &PickerState, area: Rect) {
    let hints: &[(&str, &str)] = match state.mode {
        Mode::List => &[
            ("Enter", "connect"),
            ("a", "add"),
            ("e", "edit"),
            ("d", "delete"),
            ("q/Esc", "quit"),
        ],
        Mode::Form => &[
            ("Tab", "next field"),
            ("Enter", "save"),
            ("Esc", "back"),
        ],
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

fn position_cursor_in_form(f: &mut Frame, state: &PickerState, area: Rect) {
    // Re-derive the inner box of the focused field's text area. The form
    // layout is mirrored from `draw_form`; we use the same constraints.
    let inner = Block::default().borders(Borders::ALL).inner(area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(inner);

    let (textarea, row_idx) = match state.form.focus {
        Field::Name => (&state.form.name, 1),
        Field::Url => (&state.form.url, 5),
        Field::Subject => (&state.form.subject, 7),
        Field::Transport => return,
    };

    let row = rows[row_idx];
    let inner_row = Block::default().borders(Borders::ALL).inner(row);
    let (cursor_row, cursor_col) = textarea.cursor();
    let x = inner_row.x + cursor_col.min(inner_row.width.saturating_sub(1) as usize) as u16;
    let y = inner_row.y + cursor_row.min(inner_row.height.saturating_sub(1) as usize) as u16;
    f.set_cursor_position((x, y));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn make_state(profiles: Vec<Profile>) -> PickerState {
        let mut store = ProfileStore::default();
        for p in profiles {
            store.add(p);
        }
        PickerState {
            selected: 0,
            store,
            mode: Mode::List,
            error: None,
            form: FormState::empty(),
        }
    }

    #[test]
    fn list_q_returns_cancelled() {
        let mut state = make_state(vec![]);
        let outcome = handle_list_key(&mut state, key(KeyCode::Char('q')));
        assert!(matches!(outcome, Some(PickerOutcome::Cancelled)));
    }

    #[test]
    fn list_enter_returns_selected_profile() {
        let p = Profile::new("Local".into(), TransportMode::Ws, "ws://x".into(), "s".into());
        let id = p.id.clone();
        let mut state = make_state(vec![p]);
        let outcome = handle_list_key(&mut state, key(KeyCode::Enter));
        match outcome {
            Some(PickerOutcome::Selected(picked)) => assert_eq!(picked.id, id),
            _ => panic!("expected Selected outcome"),
        }
    }

    #[test]
    fn list_a_enters_form_mode() {
        let mut state = make_state(vec![]);
        handle_list_key(&mut state, key(KeyCode::Char('a')));
        assert_eq!(state.mode, Mode::Form);
        assert!(state.form.editing_id.is_none());
    }

    #[test]
    fn form_tab_cycles_focus() {
        let mut state = make_state(vec![]);
        state.mode = Mode::Form;
        assert_eq!(state.form.focus, Field::Name);
        handle_form_key(&mut state, key(KeyCode::Tab));
        assert_eq!(state.form.focus, Field::Transport);
        handle_form_key(&mut state, key(KeyCode::Tab));
        assert_eq!(state.form.focus, Field::Url);
    }

    #[test]
    fn form_transport_toggles_with_arrow() {
        let mut state = make_state(vec![]);
        state.mode = Mode::Form;
        state.form.focus = Field::Transport;
        assert_eq!(state.form.transport, TransportMode::Ws);
        handle_form_key(&mut state, key(KeyCode::Right));
        assert_eq!(state.form.transport, TransportMode::Dbus);
        handle_form_key(&mut state, key(KeyCode::Left));
        assert_eq!(state.form.transport, TransportMode::Ws);
    }

    #[test]
    fn form_submit_with_blank_name_records_error() {
        let mut state = make_state(vec![]);
        state.mode = Mode::Form;
        // Name is empty by default.
        handle_form_key(&mut state, key(KeyCode::Enter));
        assert!(state.error.is_some());
        assert_eq!(state.mode, Mode::Form);
    }

    #[test]
    fn delete_confirm_y_removes_profile() {
        let p = Profile::new("Local".into(), TransportMode::Ws, "ws://x".into(), "s".into());
        let mut state = make_state(vec![p]);
        state.mode = Mode::DeleteConfirm;
        handle_delete_key(&mut state, key(KeyCode::Char('y')));
        assert!(state.store.profiles.is_empty());
        // Falls back to Form when nothing left.
        assert_eq!(state.mode, Mode::Form);
    }

    #[test]
    fn delete_confirm_other_key_cancels() {
        let p = Profile::new("Local".into(), TransportMode::Ws, "ws://x".into(), "s".into());
        let mut state = make_state(vec![p]);
        state.mode = Mode::DeleteConfirm;
        handle_delete_key(&mut state, key(KeyCode::Char('n')));
        assert_eq!(state.store.profiles.len(), 1);
        assert_eq!(state.mode, Mode::List);
    }
}
