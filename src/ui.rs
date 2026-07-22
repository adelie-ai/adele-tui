use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::app::{AdeleOutput, App, InputMode, MessageKind};
use crate::theme::theme;

const INPUT_VISIBLE_LINES: u16 = 4;
const INPUT_TOTAL_HEIGHT: u16 = INPUT_VISIBLE_LINES + 2; // +2 for borders

/// Below this width the conversation sidebar auto-hides so the chat keeps a
/// usable column count. This is a render-time decision only — the user's
/// `show_sidebar` toggle is never mutated, so widening the terminal restores it.
const SIDEBAR_MIN_WIDTH: u16 = 50;
/// The message area never shrinks below this; the input box gives up rows first
/// on a short terminal (so messages can't be squeezed to nothing).
const MIN_MESSAGE_ROWS: u16 = 1;

/// Whether to actually render the sidebar: the user opted in *and* the terminal
/// is wide enough to spare the columns.
fn sidebar_visible(show_sidebar: bool, width: u16) -> bool {
    show_sidebar && width >= SIDEBAR_MIN_WIDTH
}

/// Height for the composer box in a chat pane `available_height` rows tall, with
/// `status_height` rows of assistant-status line. The box wants
/// `INPUT_TOTAL_HEIGHT` rows but collapses on a short terminal so the message
/// area keeps at least `MIN_MESSAGE_ROWS` (toolbar + status bar are 1 row each).
fn chat_input_height(available_height: u16, status_height: u16) -> u16 {
    let fixed_chrome = status_height + 2; // toolbar + status bar
    let spare = available_height.saturating_sub(fixed_chrome + MIN_MESSAGE_ROWS);
    INPUT_TOTAL_HEIGHT.min(spare)
}

fn mode_chip_style(mode: &InputMode) -> Style {
    match mode {
        InputMode::Normal => Style::default()
            .fg(Color::Black)
            .bg(theme().run)
            .add_modifier(Modifier::BOLD),
        InputMode::Editing => Style::default()
            .fg(Color::Black)
            .bg(theme().mode_edit_bg)
            .add_modifier(Modifier::BOLD),
        InputMode::Renaming => Style::default()
            .fg(Color::Black)
            .bg(theme().user_prefix)
            .add_modifier(Modifier::BOLD),
    }
}

fn split_display_lines(content: &str) -> Vec<String> {
    content
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(str::to_string)
        .collect()
}

pub fn draw(f: &mut Frame, app: &mut App) {
    if sidebar_visible(app.show_sidebar, f.area().width) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(f.area());
        draw_conversation_list(f, app, chunks[0]);
        draw_chat_panel(f, app, chunks[1]);
    } else {
        draw_chat_panel(f, app, f.area());
    }

    if matches!(app.mode, InputMode::Renaming) {
        draw_rename_popup(f, app, f.area());
    }

    // Tasks pane overlays on top of the chat (and any rename popup —
    // it's strictly modal). Rendered last so it's on top.
    if app.tasks.visible {
        crate::tasks::draw_overlay(f, &app.tasks, f.area());
    }

    // Delete-confirm overlay (armed by `d` in the sidebar). Modal: the event
    // loop only honors confirm/cancel keys while it's up. Rendered on top of the
    // chat, matching the KB / connections / profile destructive-delete confirms.
    if let Some(title) = &app.pending_delete_conversation {
        draw_delete_overlay(f, title, f.area());
    }

    // Keymap help (?/F1) sits on top of everything.
    if app.show_help {
        draw_help_overlay(f, f.area());
    }
}

/// Confirm overlay for conversation-delete (`d`). Mirrors the destructive-delete
/// confirms in `kb.rs` / `connections.rs` / `picker.rs`: red border, the named
/// target, and the shared `y/Enter = confirm · n/Esc = cancel` footer.
fn draw_delete_overlay(f: &mut Frame, title: &str, area: Rect) {
    let popup_width = 60u16.min(area.width.saturating_sub(4));
    let popup_height = 5u16.min(area.height.saturating_sub(2));
    let popup = centered_rect(popup_width, popup_height, area);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme().error))
        .title(Line::from(Span::styled(
            "Delete conversation",
            Style::default()
                .fg(theme().error_text)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let body = Paragraph::new(vec![
        Line::from(Span::styled(
            format!("Delete \"{title}\"?"),
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            "y/Enter = confirm · n/Esc = cancel",
            Style::default().fg(theme().text_dim),
        )),
    ])
    .wrap(Wrap { trim: true });
    f.render_widget(body, inner);
}

/// The `?`/F1 keymap help overlay. Content comes from `keys::help_sections` so
/// the bindings stay single-sourced.
fn draw_help_overlay(f: &mut Frame, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    for (section, binds) in crate::keys::help_sections() {
        lines.push(Line::from(Span::styled(
            *section,
            Style::default()
                .fg(theme().title)
                .add_modifier(Modifier::BOLD),
        )));
        for (key, desc) in *binds {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {key:<22}"),
                    Style::default().fg(theme().hint_key),
                ),
                Span::styled(*desc, Style::default().fg(theme().text_dim)),
            ]));
        }
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        "Inside modal screens (KB / connections / purposes / MCP servers), Ctrl+S = save.",
        Style::default()
            .fg(theme().text_dim)
            .add_modifier(Modifier::ITALIC),
    )));

    let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let width = 60u16.min(area.width.saturating_sub(4));
    let popup = centered_rect(width, height, area);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme().border))
        .title(Line::from(Span::styled(
            " Keys — press any key to close ",
            Style::default()
                .fg(theme().title)
                .add_modifier(Modifier::BOLD),
        )));
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(para, popup);
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

fn draw_rename_popup(f: &mut Frame, app: &mut App, area: Rect) {
    let popup_width = area.width.saturating_sub(8).clamp(20, 72);
    let popup = centered_rect(popup_width, 3, area);

    f.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme().user_prefix))
        .title(Line::from(Span::styled(
            "Rename (Enter save, Esc cancel)",
            Style::default()
                .fg(theme().rename_title)
                .add_modifier(Modifier::BOLD),
        )));

    app.rename_textarea.set_block(block);
    f.render_widget(&app.rename_textarea, popup);
}

fn draw_conversation_list(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let items: Vec<ListItem> = app
        .conversations()
        .iter()
        .map(|c| {
            let mut spans = vec![];
            if c.archived {
                spans.push(Span::styled("⌂ ", Style::default().fg(Color::DarkGray)));
            }
            spans.push(Span::styled(
                c.title.as_str(),
                if c.archived {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default().fg(Color::White)
                },
            ));
            spans.push(Span::styled(
                format!(" ({})", c.message_count),
                Style::default().fg(theme().count_dim),
            ));
            ListItem::new(Line::from(spans))
        })
        .collect();

    let title = if app.show_archived {
        "Conversations (all)"
    } else {
        "Conversations"
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme().list_border))
                .title(Line::from(Span::styled(
                    title,
                    Style::default()
                        .fg(theme().list_title)
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

    let mut state = ListState::default();
    state.select(app.selected_conversation);
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_chat_panel(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let show_status = app.assistant_status.is_some();
    let status_height: u16 = if show_status { 1 } else { 0 };
    let input_height = chat_input_height(area.height, status_height);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(status_height),
            Constraint::Length(input_height),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    draw_messages(f, app, chunks[0]);
    if show_status {
        draw_assistant_status(f, app, chunks[1]);
    }
    draw_input(f, app, chunks[2]);
    draw_toolbar(f, app, chunks[3]);
    draw_status_bar(f, app, chunks[4]);
}

fn draw_assistant_status(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let Some(message) = app.assistant_status.as_deref() else {
        return;
    };
    let indicator = Paragraph::new(Line::from(vec![
        Span::styled(
            "● ",
            Style::default()
                .fg(theme().assistant_indicator)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            message,
            Style::default()
                .fg(theme().assistant_indicator)
                .add_modifier(Modifier::ITALIC),
        ),
    ]));
    f.render_widget(indicator, area);
}

fn draw_toolbar(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let mode_str = match app.mode {
        InputMode::Normal => "NORMAL",
        InputMode::Editing => "EDITING",
        InputMode::Renaming => "RENAME",
    };
    let chip_text = format!(" [{mode_str}] ");
    let chip_width = chip_text.chars().count() as u16;

    let mut spans: Vec<Span> = Vec::with_capacity(8);
    spans.push(Span::styled(chip_text, mode_chip_style(&app.mode)));

    let separator_after_chip = "  ";
    let after_chip_width = separator_after_chip.chars().count() as u16;
    let hints_budget = area
        .width
        .saturating_sub(chip_width)
        .saturating_sub(after_chip_width);
    if hints_budget > 0 {
        spans.push(Span::raw(separator_after_chip));
        let (hint_spans, _) = crate::toolbar::render_hints(&app.mode, hints_budget);
        spans.extend(hint_spans);
    }

    let toolbar = Paragraph::new(Line::from(spans));
    f.render_widget(toolbar, area);
}

fn push_user_message(lines: &mut Vec<Line<'static>>, content: &str, style: Style) {
    let mut first = true;
    for text_line in split_display_lines(content) {
        if first {
            lines.push(Line::from(vec![
                Span::styled("You: ", style.add_modifier(Modifier::BOLD)),
                Span::styled(text_line, style),
            ]));
            first = false;
        } else {
            lines.push(Line::from(Span::styled(text_line, style)));
        }
    }
}

fn push_prefixed_message(
    lines: &mut Vec<Line<'static>>,
    prefix: &'static str,
    content: &str,
    style: Style,
) {
    let mut first = true;
    for text_line in split_display_lines(content) {
        if first {
            lines.push(Line::from(vec![
                Span::styled(prefix, style.add_modifier(Modifier::BOLD)),
                Span::styled(text_line, style),
            ]));
            first = false;
        } else {
            lines.push(Line::from(Span::styled(text_line, style)));
        }
    }
    if first {
        // Empty content — still emit the prefix line so it shows up.
        lines.push(Line::from(Span::styled(
            prefix,
            style.add_modifier(Modifier::BOLD),
        )));
    }
}

fn push_assistant_markdown(lines: &mut Vec<Line<'static>>, content: &str, style: Style) {
    let mut rendered = crate::markdown::render(content, style);
    if rendered.is_empty() {
        rendered.push(Line::from(""));
    }
    // Prepend "Adele: " to the first non-empty line so the prefix sits with
    // the response rather than on its own row.
    let prefix_pos = rendered
        .iter()
        .position(|l| !l.spans.is_empty())
        .unwrap_or(0);
    let prefix_span = Span::styled("Adele: ", style.add_modifier(Modifier::BOLD));
    if let Some(first) = rendered.get_mut(prefix_pos) {
        first.spans.insert(0, prefix_span);
    } else {
        rendered.push(Line::from(prefix_span));
    }
    lines.extend(rendered);
}

fn draw_messages(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let mut lines: Vec<Line> = Vec::new();

    if let Some(conv) = app.current_conversation() {
        for msg in &conv.messages {
            // Client-local `say_this` lines (voice#126) carry explicit
            // presentation metadata; render their marker from `kind` rather than
            // parsing the content. Always shown (not debug-gated) — they're real
            // user-facing content — with the marker as a bold prefix.
            match msg.kind {
                MessageKind::Spoken => {
                    let style = Style::default()
                        .fg(theme().assistant_prefix)
                        .add_modifier(Modifier::ITALIC);
                    push_prefixed_message(&mut lines, "Spoken: ", &msg.content, style);
                    lines.push(Line::from(""));
                    continue;
                }
                MessageKind::SpeechDisabled => {
                    let style = Style::default()
                        .fg(theme().assistant_prefix)
                        .add_modifier(Modifier::DIM | Modifier::ITALIC);
                    push_prefixed_message(
                        &mut lines,
                        "(speech mode disabled) ",
                        &msg.content,
                        style,
                    );
                    lines.push(Line::from(""));
                    continue;
                }
                MessageKind::Normal => {}
            }
            // Roles fall into a few buckets: user/assistant render normally
            // (assistant via markdown); tool/system/empty-assistant render
            // only when the debug view is enabled.
            match msg.role.as_str() {
                "user" => {
                    let style = Style::default().fg(theme().user_prefix);
                    push_user_message(&mut lines, &msg.content, style);
                    lines.push(Line::from(""));
                }
                "assistant" if !msg.content.trim().is_empty() => {
                    let style = Style::default().fg(theme().assistant_prefix);
                    push_assistant_markdown(&mut lines, &msg.content, style);
                    lines.push(Line::from(""));
                }
                "tool" if app.show_debug => {
                    let style = Style::default()
                        .fg(theme().debug_tool)
                        .add_modifier(Modifier::DIM | Modifier::ITALIC);
                    push_prefixed_message(&mut lines, "tool: ", &msg.content, style);
                    lines.push(Line::from(""));
                }
                "system" if app.show_debug => {
                    let style = Style::default()
                        .fg(theme().debug_system)
                        .add_modifier(Modifier::DIM | Modifier::ITALIC);
                    push_prefixed_message(&mut lines, "system: ", &msg.content, style);
                    lines.push(Line::from(""));
                }
                "assistant" if app.show_debug => {
                    let style = Style::default()
                        .fg(theme().assistant_prefix)
                        .add_modifier(Modifier::DIM | Modifier::ITALIC);
                    push_prefixed_message(&mut lines, "Adele (empty): ", &msg.content, style);
                    lines.push(Line::from(""));
                }
                _ => continue,
            }
        }

        // Show streaming buffer as in-progress assistant message. Markdown is
        // applied to the partial buffer too — unclosed fences just show
        // their literal backticks until the stream catches up. Only painted
        // when the in-flight stream belongs to THIS conversation (TUI-4): a
        // backgrounded turn keeps buffering invisibly and re-appears when the
        // user switches back to its conversation.
        if !app.streaming_buffer().is_empty() && app.streaming_is_active_for_view() {
            let style = Style::default().fg(theme().ok);
            push_assistant_markdown(&mut lines, app.streaming_buffer(), style);
            // Cursor on last line
            if let Some(last) = lines.last_mut() {
                last.spans.push(Span::styled("▌", style));
            }
        }
    } else {
        lines.push(Line::from("Press 'n' to create a new conversation."));
    }

    let chat_title = app
        .current_conversation()
        .map(|conv| conv.title.as_str())
        .unwrap_or("Chat");
    let model_suffix = app
        .current_conversation()
        .and_then(|conv| conv.model_selection.as_ref())
        .map(|sel| format!("  ·  {} · {}", sel.connection_id, sel.model_id))
        .unwrap_or_default();
    // Persistent cue for the two per-conversation voice controls (adele-tui#77),
    // so the user can always see both states. `Adele:` (voice output) shows only
    // when not Disabled — the common default stays uncluttered; `You:` (voice
    // input) shows only when Enabled. The keybindings (Ctrl+S / Ctrl+V) live in
    // the `?`/F1 help overlay and the mode toolbar, so they aren't repeated in
    // the title (declutter, CC-4); plain ASCII labels keep it width-safe (the
    // former 🔊/🎙 glyphs risked double-width cells on some terminals).
    let adele_suffix = match app.current_adele_output() {
        AdeleOutput::Disabled => String::new(),
        level => format!("  ·  Adele: {}", level.label()),
    };
    let you_suffix = if app.current_voice_in() {
        "  ·  You: on"
    } else {
        ""
    };
    // A bare up-arrow marks "scrolled up from the bottom"; the scroll keys
    // themselves are in the help overlay / toolbar, so the old verbose
    // "(Ctrl+u/d scroll, Ctrl+e bottom)" prose is gone (declutter, CC-4).
    let scroll_marker = if app.scroll_offset > 0 { "  ↑" } else { "" };
    let title = format!("{chat_title}{model_suffix}{adele_suffix}{you_suffix}{scroll_marker}");

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme().border))
        .title(Line::from(Span::styled(
            title,
            Style::default()
                .fg(theme().title)
                .add_modifier(Modifier::BOLD),
        )));
    let inner_width = block.inner(area).width;
    let visible_height = block.inner(area).height;

    let messages = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });

    // Use ratatui's own line_count for accurate wrapped height
    let total_height = messages.line_count(inner_width) as u16;
    let max_scroll = total_height.saturating_sub(visible_height);
    let scroll = max_scroll.saturating_sub(app.scroll_offset);

    let messages = messages.scroll((scroll, 0));
    f.render_widget(messages, area);
}

/// The composer-title prefix for the message queue (feat/queue-messages):
/// `"N queued · "` when messages await a flush, `"editing queued · "` while one
/// is checked out into the composer, or empty when neither. Pure so the wording
/// is unit-testable without a live frame.
fn queue_title_prefix(queued_len: usize, editing: bool) -> String {
    match (queued_len, editing) {
        (0, false) => String::new(),
        (0, true) => "editing queued · ".to_string(),
        (n, true) => format!("editing queued (+{n}) · "),
        (n, false) => format!("{n} queued · "),
    }
}

fn draw_input(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let wrap_width = usize::from(area.width.saturating_sub(2)).max(1);
    // Display-only wrap (issue #84): render a throwaway wrapped copy so the
    // logical composer text (`app.textarea`) — and thus the prompt that gets
    // sent — is never mutated by terminal-width line breaks.
    let mut display = app.wrapped_display_textarea(wrap_width);

    let (base_title, mode_color) = match app.mode {
        InputMode::Normal => ("Input (press 'i' to edit)", theme().input_border_idle),
        InputMode::Editing => (
            "Input (Esc cancel, Enter send, Shift+Enter/Ctrl+J newline)",
            theme().border_active,
        ),
        InputMode::Renaming => ("Input", theme().input_border_idle),
    };
    // Prepend the "N queued" indicator (feat/queue-messages) so a burst the user
    // fired while Adele was replying is visible right where they type. Pulled from
    // core state each frame (stateless draw); mirrors the `offline ·` tag shape.
    let queue_prefix = queue_title_prefix(
        app.queued_messages_for_view().len(),
        app.editing_queued_index().is_some(),
    );
    // When the daemon link is down the run loop projects `connected = false`;
    // surface it where the user types — a warn-colored border plus an `offline`
    // tag. The tag is text (not color-only) so it still reads under NO_COLOR,
    // where the recolor is a no-op; the status bar carries the backoff detail.
    let (title, border_color) = if app.connected {
        (format!("{queue_prefix}{base_title}"), mode_color)
    } else {
        (
            format!("{queue_prefix}offline · {base_title}"),
            theme().warn,
        )
    };

    display.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(Line::from(Span::styled(
                title,
                Style::default().fg(theme().hint_key),
            ))),
    );

    f.render_widget(&display, area);
}

fn draw_status_bar(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let badge = if app.tasks.visible {
        String::new()
    } else {
        crate::tasks::running_badge(&app.tasks)
    };

    // Right-aligned context-fill indicator (#341). Read-only; reflects the
    // open conversation's last reported fill. Rendered before the left
    // status text so a long status can't shove it off the bar.
    if let Some(usage) = app.context_usage {
        let (text, color) = context_usage_span(usage);
        let w = text.chars().count() as u16;
        if w + 2 <= area.width {
            let right = ratatui::layout::Rect {
                x: area.x + area.width - w - 1,
                y: area.y,
                width: w + 1,
                height: area.height,
            };
            let para = Paragraph::new(Line::from(Span::styled(
                text,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )));
            f.render_widget(para, right);
        }
    }

    if app.status_message.is_empty() && badge.is_empty() {
        return;
    }
    let mut spans: Vec<Span> = Vec::with_capacity(4);
    spans.push(Span::styled(" • ", Style::default().fg(theme().text_dim)));
    spans.push(Span::styled(
        app.status_message.as_str(),
        Style::default().fg(Color::White),
    ));
    if !badge.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            badge,
            Style::default()
                .fg(theme().run)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let status = Paragraph::new(Line::from(spans));
    f.render_widget(status, area);
}

/// Build the context-fill readout text + colour for the status bar (#341).
/// Pure so the colour-threshold contract is unit-testable without a frame.
fn context_usage_span(usage: crate::app::ContextUsageView) -> (String, Color) {
    use crate::app::ContextFillLevel;
    // Context-fill indicator colours (#341): green well under the 0.85
    // compaction line, amber approaching it, red at/over budget.
    let color = match usage.level() {
        ContextFillLevel::Green => theme().ctx_green,
        ContextFillLevel::Amber => theme().ctx_amber,
        ContextFillLevel::Red => theme().ctx_red,
    };
    (usage.readout(), color)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{ChatMessage, ConversationDetail, ConversationSummary};
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn queue_title_prefix_reflects_count_and_editing_state() {
        assert_eq!(queue_title_prefix(0, false), "");
        assert_eq!(queue_title_prefix(1, false), "1 queued · ");
        assert_eq!(queue_title_prefix(3, false), "3 queued · ");
        // Editing the sole queued item: nothing left in the outbox, but the edit
        // is in progress.
        assert_eq!(queue_title_prefix(0, true), "editing queued · ");
        // Editing one while others remain queued.
        assert_eq!(queue_title_prefix(2, true), "editing queued (+2) · ");
    }

    #[test]
    fn context_usage_span_colours_track_thresholds() {
        use crate::app::ContextUsageView;
        let mk = |used, budget| ContextUsageView {
            used_tokens: used,
            budget_tokens: budget,
            compaction_active: false,
        };
        // Green below 0.85.
        assert_eq!(context_usage_span(mk(12_000, 32_000)).1, theme().ctx_green);
        // Amber at exactly 0.85 (27_200) and between line and budget.
        assert_eq!(context_usage_span(mk(27_200, 32_000)).1, theme().ctx_amber);
        assert_eq!(context_usage_span(mk(30_000, 32_000)).1, theme().ctx_amber);
        // Red at/over budget.
        assert_eq!(context_usage_span(mk(32_000, 32_000)).1, theme().ctx_red);
        assert_eq!(context_usage_span(mk(40_000, 32_000)).1, theme().ctx_red);
        // Text carries the readout.
        assert_eq!(context_usage_span(mk(12_000, 32_000)).0, "12k / 32k (38%)");
    }

    #[test]
    fn draw_with_context_usage_does_not_panic() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.context_usage = Some(crate::app::ContextUsageView {
            used_tokens: 30_000,
            budget_tokens: 32_000,
            compaction_active: true,
        });
        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    #[test]
    fn draw_with_context_usage_on_tiny_terminal_does_not_panic() {
        // Narrow terminal: the readout must be skipped rather than overflow.
        let backend = TestBackend::new(8, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.context_usage = Some(crate::app::ContextUsageView {
            used_tokens: 30_000,
            budget_tokens: 32_000,
            compaction_active: false,
        });
        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    #[test]
    fn draw_empty_app_does_not_panic() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    #[test]
    fn draw_with_conversations_does_not_panic() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.set_conversations(vec![
            ConversationSummary {
                id: "1".into(),
                title: "Chat 1".into(),
                message_count: 3,
                archived: false,
            },
            ConversationSummary {
                id: "2".into(),
                title: "Chat 2".into(),
                message_count: 0,
                archived: false,
            },
        ]);
        app.selected_conversation = Some(0);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    #[test]
    fn draw_with_messages_does_not_panic() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.load_conversation(ConversationDetail {
            id: "1".into(),
            title: "Test".into(),
            messages: vec![
                ChatMessage {
                    id: String::new(),
                    role: "user".into(),
                    content: "Hello".into(),
                    kind: crate::app::MessageKind::Normal,
                },
                ChatMessage {
                    id: String::new(),
                    role: "assistant".into(),
                    content: "Hi there!".into(),
                    kind: crate::app::MessageKind::Normal,
                },
            ],
            model_selection: None,
            conversation_personality: None,
        });
        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    fn title_dump(app: &mut App) -> String {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn draw_shows_adele_cue_only_when_not_disabled() {
        // The persistent Adele (voice-output) indicator (adele-tui#77) appears in
        // the chat title only when the open conversation's level is not Disabled,
        // and reflects the current level label.
        let mut app = App::new();
        app.load_conversation(ConversationDetail {
            id: "c1".into(),
            title: "ChatProbe".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });

        // Disabled (default): no "Adele:" cue in the title.
        assert!(!title_dump(&mut app).contains("Adele:"));

        // On Demand: the cue appears with the level label.
        assert_eq!(
            app.cycle_current_adele_output(),
            Some(AdeleOutput::OnDemand)
        );
        let dump = title_dump(&mut app);
        assert!(dump.contains("Adele:"));
        assert!(dump.contains("On Demand"));

        // Always: the cue updates to the new level.
        assert_eq!(app.cycle_current_adele_output(), Some(AdeleOutput::Always));
        let dump = title_dump(&mut app);
        assert!(dump.contains("Adele:"));
        assert!(dump.contains("Always"));
    }

    #[test]
    fn draw_shows_you_cue_only_when_enabled() {
        // The persistent You (voice-input) indicator (adele-tui#77) appears in
        // the chat title only while the open conversation's You is Enabled, and
        // is distinct from the Adele cue.
        let mut app = App::new();
        app.load_conversation(ConversationDetail {
            id: "c1".into(),
            title: "ChatProbe".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });

        // Disabled (default): no "You:" cue in the title.
        assert!(!title_dump(&mut app).contains("You:"));

        // Enabled: the cue appears.
        assert_eq!(app.toggle_current_voice_in(), Some(true));
        assert!(title_dump(&mut app).contains("You:"));
    }

    #[test]
    fn draw_with_streaming_buffer_does_not_panic() {
        use client_ui_common::UiMessage;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        // `load_conversation` dual-writes the open-conversation id into core so
        // the in-flight stream is active for the view; the chunk then buffers and
        // paints through the same render guard production uses.
        app.load_conversation(ConversationDetail {
            id: "1".into(),
            title: "Test".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });
        app.apply_prompt_ack("task1".into(), "1".into());
        app.apply_core(UiMessage::StreamChunk {
            request_id: "req1".into(),
            chunk: "Partial response...".into(),
        });
        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    #[test]
    fn draw_in_editing_mode_does_not_panic() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.enter_editing_mode();
        app.textarea.insert_str("typing something");
        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    #[test]
    fn draw_with_status_message_does_not_panic() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.status_message = "Error: connection lost".into();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    #[test]
    fn draw_input_marks_offline_when_disconnected() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.connected = false;
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(
            dump.contains("offline"),
            "offline tag must show when disconnected"
        );
    }

    #[test]
    fn draw_input_has_no_offline_tag_when_connected() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        // App::new() defaults connected = true.
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(!dump.contains("offline"), "no offline tag while connected");
    }

    #[test]
    fn draw_input_border_recolors_to_warn_only_when_disconnected() {
        // The disconnect cue is two-channel: the text tag (asserted above, the
        // NO_COLOR-safe channel) *and* a warn-recolored input border. `warn` is
        // unique among the palette's border colors, so a warn-colored border
        // glyph appears iff the link is down — letting us assert the recolor
        // directly rather than only the tag.
        fn input_border_has_warn(app: &mut App) -> bool {
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| draw(f, app)).unwrap();
            let buf = terminal.backend().buffer().clone();
            buf.content.iter().any(|c| {
                matches!(c.symbol(), "┌" | "┐" | "└" | "┘" | "│" | "─") && c.fg == theme().warn
            })
        }

        let mut offline = App::new();
        offline.connected = false;
        assert!(
            input_border_has_warn(&mut offline),
            "a disconnected input border must be warn-colored"
        );

        let mut online = App::new();
        // App::new() defaults connected = true.
        assert!(
            !input_border_has_warn(&mut online),
            "no border should be warn-colored while connected"
        );
    }

    #[test]
    fn sidebar_visible_requires_opt_in_and_width() {
        assert!(sidebar_visible(true, 80));
        assert!(sidebar_visible(true, SIDEBAR_MIN_WIDTH));
        // One column under the threshold hides it even though the user opted in.
        assert!(!sidebar_visible(true, SIDEBAR_MIN_WIDTH - 1));
        // Opt-out always hides it, however wide the terminal.
        assert!(!sidebar_visible(false, 200));
    }

    #[test]
    fn chat_input_height_is_full_when_tall_and_shrinks_when_short() {
        // Tall pane: the composer gets its full height, status line or not.
        assert_eq!(chat_input_height(24, 0), INPUT_TOTAL_HEIGHT);
        assert_eq!(chat_input_height(24, 1), INPUT_TOTAL_HEIGHT);
        // Short pane (height 8, no status): chrome(2) + message(1) leaves 5.
        assert_eq!(chat_input_height(8, 0), 5);
        // Tiny pane: shrinks toward zero rather than starving the messages.
        assert_eq!(chat_input_height(4, 0), 1);
        assert_eq!(chat_input_height(3, 0), 0);
    }

    #[test]
    fn chat_input_height_always_leaves_a_message_row_when_possible() {
        // Whenever the pane is tall enough to hold the fixed chrome plus a
        // message row, the composer must not consume that row.
        for h in 0..40u16 {
            for status in 0..=1u16 {
                let consumed = chat_input_height(h, status) + status + 2;
                if h >= status + 2 + MIN_MESSAGE_ROWS {
                    assert!(
                        h.saturating_sub(consumed) >= MIN_MESSAGE_ROWS,
                        "h={h} status={status}: only {} rows left for messages",
                        h.saturating_sub(consumed)
                    );
                }
            }
        }
    }

    #[test]
    fn draw_auto_hides_sidebar_on_a_narrow_terminal() {
        // Wide terminal: the opted-in sidebar (its "Conversations" title) shows.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(); // show_sidebar defaults true
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let wide: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            wide.contains("Conversations"),
            "sidebar should show on a wide terminal"
        );

        // Narrow terminal, same opted-in state: the sidebar is suppressed so the
        // chat keeps the columns — and the user's toggle is left untouched.
        let backend = TestBackend::new(40, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let narrow: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            !narrow.contains("Conversations"),
            "sidebar should auto-hide on a narrow terminal"
        );
        assert!(app.show_sidebar, "the user's toggle must be left untouched");
    }

    #[test]
    fn draw_small_terminal_does_not_panic() {
        let backend = TestBackend::new(20, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    #[test]
    fn draw_rename_popup_does_not_panic() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.set_conversations(vec![ConversationSummary {
            id: "1".into(),
            title: "Chat 1".into(),
            message_count: 0,
            archived: false,
        }]);
        app.selected_conversation = Some(0);
        app.begin_rename();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    #[test]
    fn draw_with_sidebar_hidden_does_not_render_list_title() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.set_conversations(vec![ConversationSummary {
            id: "1".into(),
            title: "Sidebar Title Probe".into(),
            message_count: 0,
            archived: false,
        }]);
        app.show_sidebar = false;
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(!dump.contains("Sidebar Title Probe"));
        assert!(!dump.contains("Conversations"));
    }

    #[test]
    fn draw_with_sidebar_visible_renders_conversation_titles() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.set_conversations(vec![ConversationSummary {
            id: "1".into(),
            title: "Visible Probe".into(),
            message_count: 0,
            archived: false,
        }]);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("Visible Probe"));
    }

    fn app_with_debug_messages(show_debug: bool) -> App {
        let mut app = App::new();
        app.show_debug = show_debug;
        app.load_conversation(ConversationDetail {
            id: "1".into(),
            title: "Test".into(),
            messages: vec![
                ChatMessage {
                    id: String::new(),
                    role: "user".into(),
                    content: "Hello".into(),
                    kind: crate::app::MessageKind::Normal,
                },
                ChatMessage {
                    id: String::new(),
                    role: "tool".into(),
                    content: "ran search(foo)".into(),
                    kind: crate::app::MessageKind::Normal,
                },
                ChatMessage {
                    id: String::new(),
                    role: "system".into(),
                    content: "context updated".into(),
                    kind: crate::app::MessageKind::Normal,
                },
                ChatMessage {
                    id: String::new(),
                    role: "assistant".into(),
                    content: "".into(), // empty — only shown in debug
                    kind: crate::app::MessageKind::Normal,
                },
                ChatMessage {
                    id: String::new(),
                    role: "assistant".into(),
                    content: "Hi there!".into(),
                    kind: crate::app::MessageKind::Normal,
                },
            ],
            model_selection: None,
            conversation_personality: None,
        });
        app
    }

    #[test]
    fn draw_with_debug_off_hides_tool_and_system() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app_with_debug_messages(false);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("Hello"));
        assert!(dump.contains("Hi there!"));
        assert!(!dump.contains("ran search"));
        assert!(!dump.contains("context updated"));
        assert!(!dump.contains("tool:"));
        assert!(!dump.contains("system:"));
    }

    #[test]
    fn draw_with_debug_on_reveals_tool_and_system() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = app_with_debug_messages(true);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("tool:"));
        assert!(dump.contains("system:"));
        assert!(dump.contains("ran search"));
        assert!(dump.contains("context updated"));
    }

    #[test]
    fn draw_with_assistant_status_renders_indicator() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.load_conversation(ConversationDetail {
            id: "1".into(),
            title: "Test".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });
        app.set_assistant_status("Searching knowledge base...");
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("Searching knowledge base"));
    }

    #[test]
    fn draw_without_assistant_status_omits_indicator() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content.iter().map(|c| c.symbol()).collect();
        // The bullet glyph used by the indicator should not appear when no status.
        assert!(!dump.contains("● "));
    }

    #[test]
    fn toolbar_renders_mode_chip_and_hints_in_normal() {
        let backend = TestBackend::new(200, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("[NORMAL]"));
        assert!(dump.contains("quit"));
        assert!(dump.contains("new"));
    }

    #[test]
    fn toolbar_switches_hints_in_editing_mode() {
        let backend = TestBackend::new(160, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.enter_editing_mode();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("[EDITING]"));
        assert!(dump.contains("send"));
        // Normal-mode-only hint should not appear in editing mode.
        assert!(!dump.contains("archive"));
    }

    #[test]
    fn assistant_markdown_strips_emphasis_markers() {
        // `**strong**` should render as `strong` (markdown punctuation
        // consumed by the parser), not literal `**strong**`.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.load_conversation(ConversationDetail {
            id: "1".into(),
            title: "Test".into(),
            messages: vec![ChatMessage {
                id: String::new(),
                role: "assistant".into(),
                content: "answer with **strong** word".into(),
                kind: crate::app::MessageKind::Normal,
            }],
            model_selection: None,
            conversation_personality: None,
        });
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("strong"));
        assert!(!dump.contains("**strong**"));
    }
}
