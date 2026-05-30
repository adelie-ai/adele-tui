//! Per-conversation model selector.
//!
//! `Ctrl+M` from the chat opens a centered picker listing flattened
//! `Connection · Model` entries from `list_available_models`. Confirming
//! a row stages a `SendPromptOverride` that rides on the next
//! `SendPrompt`; after the daemon persists it as `last_model_selection`,
//! subsequent prompts inherit the choice automatically.
//!
//! The current pick is hydrated from `ConversationDetail.model_selection`
//! when the conversation loads, so re-opening the picker pre-highlights
//! the right entry.
//!
//! Keys
//! ----
//! - `j/k` or arrows: navigate
//! - `Enter`: confirm + close
//! - `r`: refresh model list (forces the daemon to repopulate connector
//!   caches — relevant for Bedrock)
//! - `Esc` / `q`: close without changing

use std::io;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use desktop_assistant_api_model::{
    ConversationModelSelectionView, ModelListing, SendPromptOverride,
};
use desktop_assistant_client_common::TransportClient;
use futures::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};

const COLOR_BORDER: Color = Color::Rgb(82, 104, 173);
const COLOR_TITLE: Color = Color::Rgb(166, 182, 255);
const COLOR_HINT_KEY: Color = Color::Rgb(216, 223, 236);
const COLOR_HINT_DESC: Color = Color::Rgb(143, 153, 174);
const COLOR_HINT_SEP: Color = Color::Rgb(82, 90, 110);
const COLOR_LIST_HIGHLIGHT: Color = Color::Rgb(72, 102, 180);
const COLOR_LIST_HIGHLIGHT_FG: Color = Color::Rgb(245, 248, 255);
const COLOR_ERROR: Color = Color::Rgb(232, 130, 130);
const COLOR_CURRENT_PICK: Color = Color::Rgb(255, 207, 119);

/// Outcome of running the picker.
pub enum Outcome {
    /// User picked a model. Caller should stage this as a one-shot
    /// override on the next `SendPrompt`.
    Selected(SendPromptOverride),
    /// User pressed Esc / q without picking.
    Cancelled,
}

struct State {
    models: Vec<ModelListing>,
    selected: usize,
    /// The conversation's current selection, used to pre-highlight a row
    /// and mark it with a star in the list.
    current: Option<ConversationModelSelectionView>,
    error: Option<String>,
    busy: Option<String>,
    outcome: Option<Outcome>,
}

/// Run the picker until the user confirms or cancels.
pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    client: &TransportClient,
    current: Option<ConversationModelSelectionView>,
) -> anyhow::Result<Outcome> {
    let mut state = State {
        models: Vec::new(),
        selected: 0,
        current,
        error: None,
        busy: Some("Loading models...".into()),
        outcome: None,
    };

    refresh(&mut state, client, false).await;
    seed_selection(&mut state);

    let mut events = crossterm::event::EventStream::new();
    loop {
        terminal.draw(|f| draw(f, &state))?;

        if let Some(outcome) = state.outcome.take() {
            return Ok(outcome);
        }

        let evt = match events.next().await {
            Some(Ok(e)) => e,
            Some(Err(_)) | None => return Ok(Outcome::Cancelled),
        };
        let Event::Key(key) = evt else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        handle_key(&mut state, key, client).await;
    }
}

async fn handle_key(state: &mut State, key: KeyEvent, client: &TransportClient) {
    match (key.code, key.modifiers) {
        (KeyCode::Esc | KeyCode::Char('q'), m) if m.is_empty() => {
            state.outcome = Some(Outcome::Cancelled);
        }
        (KeyCode::Char('j') | KeyCode::Down, m) if m.is_empty() => advance(state, 1),
        (KeyCode::Char('k') | KeyCode::Up, m) if m.is_empty() => advance(state, -1),
        (KeyCode::Char('r'), KeyModifiers::NONE) => refresh(state, client, true).await,
        (KeyCode::Enter, m) if m.is_empty() => {
            if let Some(model) = state.models.get(state.selected) {
                state.outcome = Some(Outcome::Selected(SendPromptOverride {
                    connection_id: model.connection_id.clone(),
                    model_id: model.model.id.clone(),
                    // The picker doesn't expose effort yet — defer to the
                    // daemon's per-purpose default. Keyboard-first effort
                    // selection can ride on a follow-up.
                    effort: None,
                }));
            }
        }
        _ => {}
    }
}

fn advance(state: &mut State, delta: i32) {
    let len = state.models.len();
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

async fn refresh(state: &mut State, client: &TransportClient, refresh_cache: bool) {
    let Some(ws) = client.as_ws() else {
        state.error = Some(
            "Model selection is only available over WebSocket — switch transport with --transport ws"
                .into(),
        );
        state.busy = None;
        return;
    };
    state.busy = Some(if refresh_cache {
        "Refreshing models from daemon...".into()
    } else {
        "Loading models...".into()
    });
    match ws.list_available_models(None, refresh_cache).await {
        Ok(models) => {
            state.models = models;
            state.busy = None;
            seed_selection(state);
        }
        Err(e) => {
            state.error = Some(format!("Failed to load models: {e}"));
            state.busy = None;
        }
    }
}

/// Pre-highlight the row matching the conversation's current selection.
fn seed_selection(state: &mut State) {
    let Some(current) = &state.current else {
        return;
    };
    if let Some(idx) = state
        .models
        .iter()
        .position(|m| m.connection_id == current.connection_id && m.model.id == current.model_id)
    {
        state.selected = idx;
    }
}

// --- Rendering ---

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn draw(f: &mut Frame, state: &State) {
    let area = f.area();
    // Picker is modal but doesn't take the whole screen — overlay on top
    // of whatever the chat last drew.
    let popup_w = 80.min(area.width.saturating_sub(8));
    let popup_h = 24.min(area.height.saturating_sub(4));
    let popup = centered_rect(popup_w, popup_h, area);
    f.render_widget(Clear, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(popup);

    draw_header(f, state, chunks[0]);
    draw_list(f, state, chunks[1]);
    draw_status(f, state, chunks[2]);
    draw_hints(f, chunks[3]);
}

fn draw_header(f: &mut Frame, state: &State, area: Rect) {
    let mut lines = vec![Line::from(Span::styled(
        "Pick a model for this conversation",
        Style::default()
            .fg(COLOR_TITLE)
            .add_modifier(Modifier::BOLD),
    ))];
    if let Some(current) = &state.current {
        lines.push(Line::from(Span::styled(
            format!("Current: {} · {}", current.connection_id, current.model_id),
            Style::default()
                .fg(COLOR_CURRENT_PICK)
                .add_modifier(Modifier::ITALIC),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "Current: (none — daemon will use the interactive purpose default)",
            Style::default().fg(COLOR_HINT_DESC),
        )));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_list(f: &mut Frame, state: &State, area: Rect) {
    let items: Vec<ListItem> = if state.models.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "(no models — configure connections via F3 first)",
            Style::default().fg(COLOR_HINT_DESC),
        )))]
    } else {
        state
            .models
            .iter()
            .map(|listing| {
                let mut spans: Vec<Span<'static>> = Vec::new();
                let is_current = state.current.as_ref().is_some_and(|c| {
                    c.connection_id == listing.connection_id && c.model_id == listing.model.id
                });
                if is_current {
                    spans.push(Span::styled("★ ", Style::default().fg(COLOR_CURRENT_PICK)));
                } else {
                    spans.push(Span::raw("  "));
                }
                spans.push(Span::styled(
                    listing.connection_label.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled("  ·  ", Style::default().fg(COLOR_HINT_SEP)));
                spans.push(Span::styled(listing.model.id.clone(), Style::default()));
                if listing.model.display_name != listing.model.id {
                    spans.push(Span::styled(
                        format!("  ({})", listing.model.display_name),
                        Style::default().fg(COLOR_HINT_DESC),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect()
    };

    let title = if state.models.is_empty() {
        "Models".to_string()
    } else {
        format!("Models ({})", state.models.len())
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
    if !state.models.is_empty() {
        list_state.select(Some(state.selected));
    }
    f.render_stateful_widget(list, area, &mut list_state);
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

fn draw_hints(f: &mut Frame, area: Rect) {
    let hints: &[(&str, &str)] = &[("Enter", "confirm"), ("r", "refresh"), ("Esc", "cancel")];
    let mut spans: Vec<Span> = Vec::new();
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

#[cfg(test)]
mod tests {
    use super::*;
    use desktop_assistant_api_model::{ModelCapabilitiesView, ModelInfoView};

    fn listing(connection: &str, model: &str) -> ModelListing {
        ModelListing {
            connection_id: connection.into(),
            connection_label: format!("{connection} (test)"),
            model: ModelInfoView {
                id: model.into(),
                display_name: model.into(),
                context_limit: None,
                capabilities: ModelCapabilitiesView::default(),
            },
        }
    }

    #[test]
    fn seed_selection_finds_matching_row() {
        let mut state = State {
            models: vec![
                listing("a", "alpha"),
                listing("b", "beta"),
                listing("c", "gamma"),
            ],
            selected: 0,
            current: Some(ConversationModelSelectionView {
                connection_id: "b".into(),
                model_id: "beta".into(),
                effort: None,
            }),
            error: None,
            busy: None,
            outcome: None,
        };
        seed_selection(&mut state);
        assert_eq!(state.selected, 1);
    }

    #[test]
    fn seed_selection_keeps_zero_when_no_match() {
        let mut state = State {
            models: vec![listing("a", "alpha")],
            selected: 0,
            current: Some(ConversationModelSelectionView {
                connection_id: "z".into(),
                model_id: "zeta".into(),
                effort: None,
            }),
            error: None,
            busy: None,
            outcome: None,
        };
        seed_selection(&mut state);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn advance_wraps_at_boundaries() {
        let mut state = State {
            models: vec![listing("a", "alpha"), listing("b", "beta")],
            selected: 0,
            current: None,
            error: None,
            busy: None,
            outcome: None,
        };
        advance(&mut state, 1);
        assert_eq!(state.selected, 1);
        advance(&mut state, 1);
        assert_eq!(state.selected, 0);
        advance(&mut state, -1);
        assert_eq!(state.selected, 1);
    }

    #[test]
    fn advance_on_empty_list_is_noop() {
        let mut state = State {
            models: Vec::new(),
            selected: 0,
            current: None,
            error: None,
            busy: None,
            outcome: None,
        };
        advance(&mut state, 1);
        assert_eq!(state.selected, 0);
    }
}
