//! Mode-aware keybind hints rendered in the bottom toolbar.
//!
//! Each hint is `(key_label, description)` and listed in priority order.
//! When the toolbar can't fit every hint, lower-priority entries get dropped
//! from the right.

use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};

use crate::app::InputMode;

const COLOR_HINT_KEY: Color = Color::Rgb(216, 223, 236);
const COLOR_HINT_DESC: Color = Color::Rgb(143, 153, 174);
const COLOR_HINT_SEP: Color = Color::Rgb(82, 90, 110);

const HINTS_NORMAL: &[(&str, &str)] = &[
    ("n", "new"),
    ("Enter", "open"),
    ("i", "edit"),
    ("r", "rename"),
    ("d", "delete"),
    ("A", "archive"),
    ("a", "archived"),
    ("q", "quit"),
    ("Ctrl+K", "kb"),
    ("F3", "connections"),
    ("F4", "purposes"),
    ("Ctrl+B", "sidebar"),
    ("Ctrl+T", "debug"),
];

const HINTS_EDITING: &[(&str, &str)] = &[
    ("Enter", "send"),
    ("S+Enter", "newline"),
    ("Esc", "back"),
    ("Ctrl+e", "bottom"),
    ("Ctrl+B", "sidebar"),
];

const HINTS_RENAMING: &[(&str, &str)] = &[("Enter", "save"), ("Esc", "cancel")];

pub fn hints_for(mode: &InputMode) -> &'static [(&'static str, &'static str)] {
    match mode {
        InputMode::Normal => HINTS_NORMAL,
        InputMode::Editing => HINTS_EDITING,
        InputMode::Renaming => HINTS_RENAMING,
    }
}

/// Build hint spans that fit within `max_width` cells, dropping the lowest-
/// priority entries first. Returns the spans and the cell width consumed.
pub fn render_hints(mode: &InputMode, max_width: u16) -> (Vec<Span<'static>>, u16) {
    let hints = hints_for(mode);
    let mut chosen: Vec<&(&str, &str)> = Vec::new();
    let mut width: u16 = 0;

    for hint in hints {
        let cost = hint_cell_width(hint, !chosen.is_empty());
        if width.saturating_add(cost) > max_width {
            break;
        }
        width = width.saturating_add(cost);
        chosen.push(hint);
    }

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(chosen.len() * 4);
    for (idx, (key, desc)) in chosen.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled(
                "  ·  ",
                Style::default().fg(COLOR_HINT_SEP),
            ));
        }
        spans.push(Span::styled(
            (*key).to_string(),
            Style::default()
                .fg(COLOR_HINT_KEY)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" ", Style::default()));
        spans.push(Span::styled(
            (*desc).to_string(),
            Style::default().fg(COLOR_HINT_DESC),
        ));
    }
    (spans, width)
}

/// Width of `"key desc"` plus the separator if it's not the first hint.
fn hint_cell_width(hint: &(&str, &str), with_separator: bool) -> u16 {
    let key_len = hint.0.chars().count();
    let desc_len = hint.1.chars().count();
    let pair = key_len + 1 + desc_len; // key + space + desc
    let sep = if with_separator { 5 } else { 0 }; // "  ·  "
    (pair + sep).min(u16::MAX as usize) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_mode_has_hints() {
        let hints = hints_for(&InputMode::Normal);
        assert!(!hints.is_empty());
        assert!(hints.iter().any(|(k, _)| *k == "q"));
    }

    #[test]
    fn editing_mode_has_hints() {
        let hints = hints_for(&InputMode::Editing);
        assert!(!hints.is_empty());
        assert!(hints.iter().any(|(k, _)| *k == "Enter"));
    }

    #[test]
    fn render_hints_fits_within_width() {
        let (spans, width) = render_hints(&InputMode::Normal, 200);
        assert!(!spans.is_empty());
        assert!(width <= 200);
    }

    #[test]
    fn render_hints_drops_lowest_priority_under_pressure() {
        let (full_spans, _) = render_hints(&InputMode::Normal, 200);
        let (narrow_spans, _) = render_hints(&InputMode::Normal, 20);
        assert!(narrow_spans.len() < full_spans.len());
    }

    #[test]
    fn render_hints_truncates_to_zero_when_no_room() {
        let (spans, width) = render_hints(&InputMode::Normal, 0);
        assert!(spans.is_empty());
        assert_eq!(width, 0);
    }

    #[test]
    fn render_hints_keeps_highest_priority_first() {
        // Width just enough for the first hint of normal mode ("n new").
        let (spans, _) = render_hints(&InputMode::Normal, 6);
        // First span is the key span; verify it's "n".
        assert!(spans.first().is_some_and(|s| s.content == "n"));
    }
}
