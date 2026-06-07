//! Per-conversation personality picker.
//!
//! `Ctrl+R` from the chat opens a centered modal listing the seven personality
//! traits (the "Expressive 7": professionalism, warmth, directness, enthusiasm,
//! humor, sarcasm, pretentiousness). Each row cycles through
//! `Global → Never → Rarely → Sometimes → Often → Always`, where **Global**
//! (`None`) inherits the daemon's global personality for that trait and any
//! other level pins it for this conversation.
//!
//! Confirming calls `set_conversation_personality`, which persists the partial
//! override on the daemon (an all-`Global` selection clears it). The current
//! override is hydrated from `ConversationDetail.conversation_personality` when
//! the conversation loads, so re-opening the picker shows the pinned values.
//!
//! Keys
//! ----
//! - `j/k` or up/down: move between traits
//! - `h/l` or left/right: cycle the highlighted trait's level
//! - `Enter`: save + close
//! - `Esc` / `q`: close without saving

use std::io;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use desktop_assistant_api_model::{ConversationPersonalityView, PersonalityLevel};
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
const COLOR_GLOBAL: Color = Color::Rgb(143, 153, 174);
const COLOR_PINNED: Color = Color::Rgb(255, 207, 119);

/// The seven traits in their canonical wire order. Each entry pairs a display
/// label with getters/setters into a [`ConversationPersonalityView`], so the
/// row list and the saved override stay in lockstep with one source of truth.
struct Trait {
    label: &'static str,
    get: fn(&ConversationPersonalityView) -> Option<PersonalityLevel>,
    set: fn(&mut ConversationPersonalityView, Option<PersonalityLevel>),
}

const TRAITS: &[Trait] = &[
    Trait {
        label: "Professionalism",
        get: |p| p.professionalism,
        set: |p, v| p.professionalism = v,
    },
    Trait {
        label: "Warmth",
        get: |p| p.warmth,
        set: |p, v| p.warmth = v,
    },
    Trait {
        label: "Directness",
        get: |p| p.directness,
        set: |p, v| p.directness = v,
    },
    Trait {
        label: "Enthusiasm",
        get: |p| p.enthusiasm,
        set: |p, v| p.enthusiasm = v,
    },
    Trait {
        label: "Humor",
        get: |p| p.humor,
        set: |p, v| p.humor = v,
    },
    Trait {
        label: "Sarcasm",
        get: |p| p.sarcasm,
        set: |p, v| p.sarcasm = v,
    },
    Trait {
        label: "Pretentiousness",
        get: |p| p.pretentiousness,
        set: |p, v| p.pretentiousness = v,
    },
];

/// Cycle a trait's setting forward: `Global → Never → … → Always → Global`.
/// `None` represents **Global** (inherit). Pure so the wrap is unit-tested.
fn cycle_next(level: Option<PersonalityLevel>) -> Option<PersonalityLevel> {
    match level {
        None => Some(PersonalityLevel::Never),
        Some(PersonalityLevel::Never) => Some(PersonalityLevel::Rarely),
        Some(PersonalityLevel::Rarely) => Some(PersonalityLevel::Sometimes),
        Some(PersonalityLevel::Sometimes) => Some(PersonalityLevel::Often),
        Some(PersonalityLevel::Often) => Some(PersonalityLevel::Always),
        Some(PersonalityLevel::Always) => None,
    }
}

/// Cycle a trait's setting backward — the inverse of [`cycle_next`].
fn cycle_prev(level: Option<PersonalityLevel>) -> Option<PersonalityLevel> {
    match level {
        None => Some(PersonalityLevel::Always),
        Some(PersonalityLevel::Always) => Some(PersonalityLevel::Often),
        Some(PersonalityLevel::Often) => Some(PersonalityLevel::Sometimes),
        Some(PersonalityLevel::Sometimes) => Some(PersonalityLevel::Rarely),
        Some(PersonalityLevel::Rarely) => Some(PersonalityLevel::Never),
        Some(PersonalityLevel::Never) => None,
    }
}

/// Human label for a level, with `None` shown as the inherit sentinel.
fn level_label(level: Option<PersonalityLevel>) -> &'static str {
    match level {
        None => "Global",
        Some(PersonalityLevel::Never) => "Never",
        Some(PersonalityLevel::Rarely) => "Rarely",
        Some(PersonalityLevel::Sometimes) => "Sometimes",
        Some(PersonalityLevel::Often) => "Often",
        Some(PersonalityLevel::Always) => "Always",
    }
}

/// Outcome of running the picker.
pub enum Outcome {
    /// User saved. Carries the override the daemon stored (all-`None` = cleared).
    Saved(ConversationPersonalityView),
    /// User pressed Esc / q without saving.
    Cancelled,
}

struct State {
    conversation_id: String,
    /// Working copy of the override, mutated as the user cycles each trait.
    draft: ConversationPersonalityView,
    selected: usize,
    error: Option<String>,
    busy: Option<String>,
    outcome: Option<Outcome>,
}

/// Run the picker until the user saves or cancels.
pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    client: &TransportClient,
    conversation_id: String,
    current: Option<ConversationPersonalityView>,
) -> anyhow::Result<Outcome> {
    let mut state = State {
        conversation_id,
        draft: current.unwrap_or_default(),
        selected: 0,
        error: None,
        busy: None,
        outcome: None,
    };

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
        (KeyCode::Char('l') | KeyCode::Right, m) if m.is_empty() => cycle_selected(state, true),
        (KeyCode::Char('h') | KeyCode::Left, m) if m.is_empty() => cycle_selected(state, false),
        (KeyCode::Enter, m) if m.is_empty() => save(state, client).await,
        _ => {}
    }
}

fn advance(state: &mut State, delta: i32) {
    let len = TRAITS.len();
    let mut idx = state.selected as i32 + delta;
    if idx < 0 {
        idx = (len as i32) - 1;
    }
    if idx >= len as i32 {
        idx = 0;
    }
    state.selected = idx as usize;
}

/// Cycle the highlighted trait's level forward (`forward = true`) or backward.
fn cycle_selected(state: &mut State, forward: bool) {
    let Some(t) = TRAITS.get(state.selected) else {
        return;
    };
    let current = (t.get)(&state.draft);
    let next = if forward {
        cycle_next(current)
    } else {
        cycle_prev(current)
    };
    (t.set)(&mut state.draft, next);
}

async fn save(state: &mut State, client: &TransportClient) {
    let Some(commands) = client.as_commands() else {
        state.error = Some(
            "Personality selection isn't available over D-Bus — switch transport with --transport ws or the local socket"
                .into(),
        );
        return;
    };
    state.busy = Some("Saving personality...".into());
    state.error = None;
    match commands
        .set_conversation_personality(&state.conversation_id, state.draft)
        .await
    {
        Ok(stored) => {
            state.busy = None;
            state.outcome = Some(Outcome::Saved(stored));
        }
        Err(e) => {
            state.busy = None;
            state.error = Some(format!("Failed to save personality: {e}"));
        }
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
    let popup_w = 64.min(area.width.saturating_sub(8));
    let popup_h = 16.min(area.height.saturating_sub(4));
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

    draw_header(f, chunks[0]);
    draw_list(f, state, chunks[1]);
    draw_status(f, state, chunks[2]);
    draw_hints(f, chunks[3]);
}

fn draw_header(f: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(Span::styled(
            "Personality for this conversation",
            Style::default()
                .fg(COLOR_TITLE)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Global inherits your default; any level pins it here.",
            Style::default().fg(COLOR_HINT_DESC),
        )),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_list(f: &mut Frame, state: &State, area: Rect) {
    let label_w = TRAITS.iter().map(|t| t.label.len()).max().unwrap_or(0);
    let items: Vec<ListItem> = TRAITS
        .iter()
        .map(|t| {
            let level = (t.get)(&state.draft);
            let value_style = if level.is_none() {
                Style::default().fg(COLOR_GLOBAL)
            } else {
                Style::default()
                    .fg(COLOR_PINNED)
                    .add_modifier(Modifier::BOLD)
            };
            let spans = vec![
                Span::styled(
                    format!("{:<width$}", t.label, width = label_w),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled("   ", Style::default()),
                Span::styled("◂ ", Style::default().fg(COLOR_HINT_SEP)),
                Span::styled(format!("{:^9}", level_label(level)), value_style),
                Span::styled(" ▸", Style::default().fg(COLOR_HINT_SEP)),
            ];
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(COLOR_BORDER))
                .title(Line::from(Span::styled(
                    "Traits",
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
    list_state.select(Some(state.selected));
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
    let hints: &[(&str, &str)] = &[
        ("↑/↓", "trait"),
        ("←/→", "level"),
        ("Enter", "save"),
        ("Esc", "cancel"),
    ];
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

    #[test]
    fn cycle_next_wraps_global_to_always_to_global() {
        // Global → Never → Rarely → Sometimes → Often → Always → Global.
        let mut level = None;
        let expected = [
            Some(PersonalityLevel::Never),
            Some(PersonalityLevel::Rarely),
            Some(PersonalityLevel::Sometimes),
            Some(PersonalityLevel::Often),
            Some(PersonalityLevel::Always),
            None,
        ];
        for want in expected {
            level = cycle_next(level);
            assert_eq!(level, want);
        }
    }

    #[test]
    fn cycle_prev_is_inverse_of_cycle_next() {
        let states = [
            None,
            Some(PersonalityLevel::Never),
            Some(PersonalityLevel::Rarely),
            Some(PersonalityLevel::Sometimes),
            Some(PersonalityLevel::Often),
            Some(PersonalityLevel::Always),
        ];
        for s in states {
            assert_eq!(cycle_prev(cycle_next(s)), s);
            assert_eq!(cycle_next(cycle_prev(s)), s);
        }
    }

    #[test]
    fn traits_are_in_canonical_wire_order() {
        let labels: Vec<&str> = TRAITS.iter().map(|t| t.label).collect();
        assert_eq!(
            labels,
            vec![
                "Professionalism",
                "Warmth",
                "Directness",
                "Enthusiasm",
                "Humor",
                "Sarcasm",
                "Pretentiousness",
            ]
        );
    }

    fn state_with(draft: ConversationPersonalityView) -> State {
        State {
            conversation_id: "c-1".into(),
            draft,
            selected: 0,
            error: None,
            busy: None,
            outcome: None,
        }
    }

    #[test]
    fn cycle_selected_only_touches_highlighted_trait() {
        // Highlight "Humor" (index 4) and pin it to Never; the rest stay Global.
        let mut state = state_with(ConversationPersonalityView::default());
        state.selected = 4;
        cycle_selected(&mut state, true);
        assert_eq!(state.draft.humor, Some(PersonalityLevel::Never));
        assert_eq!(state.draft.professionalism, None);
        assert_eq!(state.draft.warmth, None);
        assert_eq!(state.draft.directness, None);
        assert_eq!(state.draft.enthusiasm, None);
        assert_eq!(state.draft.sarcasm, None);
        assert_eq!(state.draft.pretentiousness, None);
    }

    #[test]
    fn cycle_selected_backward_from_global_pins_always() {
        let mut state = state_with(ConversationPersonalityView::default());
        state.selected = 0; // Professionalism
        cycle_selected(&mut state, false);
        assert_eq!(state.draft.professionalism, Some(PersonalityLevel::Always));
    }

    #[test]
    fn draft_seeds_from_existing_override() {
        // Pre-fill mirrors how `run` hydrates `draft` from the conversation's
        // stored override: each get/set round-trips by field.
        let current = ConversationPersonalityView {
            sarcasm: Some(PersonalityLevel::Always),
            ..Default::default()
        };
        let state = state_with(current);
        assert_eq!(
            (TRAITS[5].get)(&state.draft),
            Some(PersonalityLevel::Always)
        );
    }

    #[test]
    fn advance_wraps_at_boundaries() {
        let mut state = state_with(ConversationPersonalityView::default());
        assert_eq!(state.selected, 0);
        advance(&mut state, -1);
        assert_eq!(state.selected, TRAITS.len() - 1);
        advance(&mut state, 1);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn level_label_renders_global_for_none() {
        assert_eq!(level_label(None), "Global");
        assert_eq!(level_label(Some(PersonalityLevel::Never)), "Never");
        assert_eq!(level_label(Some(PersonalityLevel::Always)), "Always");
    }
}
