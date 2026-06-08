use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::app::{AdeleOutput, App, InputMode};

const INPUT_VISIBLE_LINES: u16 = 4;
const INPUT_TOTAL_HEIGHT: u16 = INPUT_VISIBLE_LINES + 2; // +2 for borders
const COLOR_PANEL_BORDER: Color = Color::Rgb(82, 104, 173);
const COLOR_LIST_BORDER: Color = Color::Rgb(62, 125, 146);
const COLOR_INPUT_BORDER_IDLE: Color = Color::Rgb(109, 122, 143);
const COLOR_INPUT_BORDER_EDIT: Color = Color::Rgb(120, 183, 109);
const COLOR_LIST_HIGHLIGHT: Color = Color::Rgb(72, 102, 180);
const COLOR_LIST_HIGHLIGHT_FG: Color = Color::Rgb(245, 248, 255);
const COLOR_USER_PREFIX: Color = Color::Rgb(255, 189, 89);
const COLOR_ASSISTANT_PREFIX: Color = Color::Rgb(92, 206, 154);
const COLOR_ASSISTANT_STREAMING: Color = Color::Rgb(132, 218, 193);
const COLOR_STATUS_DIM: Color = Color::Rgb(143, 153, 174);
const COLOR_COUNT_DIM: Color = Color::Rgb(124, 132, 148);
const COLOR_DEBUG_TOOL: Color = Color::Rgb(178, 138, 220);
const COLOR_DEBUG_SYSTEM: Color = Color::Rgb(140, 156, 196);
const COLOR_ASSISTANT_INDICATOR: Color = Color::Rgb(178, 220, 245);

fn mode_chip_style(mode: &InputMode) -> Style {
    match mode {
        InputMode::Normal => Style::default()
            .fg(Color::Black)
            .bg(Color::Rgb(122, 163, 255))
            .add_modifier(Modifier::BOLD),
        InputMode::Editing => Style::default()
            .fg(Color::Black)
            .bg(Color::Rgb(120, 214, 118))
            .add_modifier(Modifier::BOLD),
        InputMode::Renaming => Style::default()
            .fg(Color::Black)
            .bg(Color::Rgb(255, 189, 89))
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
    if app.show_sidebar {
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
        .border_style(Style::default().fg(Color::Rgb(255, 189, 89)))
        .title(Line::from(Span::styled(
            "Rename (Enter save, Esc cancel)",
            Style::default()
                .fg(Color::Rgb(255, 220, 160))
                .add_modifier(Modifier::BOLD),
        )));

    app.rename_textarea.set_block(block);
    f.render_widget(&app.rename_textarea, popup);
}

fn draw_conversation_list(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let items: Vec<ListItem> = app
        .conversations
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
                Style::default().fg(COLOR_COUNT_DIM),
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
                .border_style(Style::default().fg(COLOR_LIST_BORDER))
                .title(Line::from(Span::styled(
                    title,
                    Style::default()
                        .fg(Color::Rgb(136, 214, 240))
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

    let mut state = ListState::default();
    state.select(app.selected_conversation);
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_chat_panel(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let show_status = app.assistant_status.is_some();
    let status_height: u16 = if show_status { 1 } else { 0 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(status_height),
            Constraint::Length(INPUT_TOTAL_HEIGHT),
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
                .fg(COLOR_ASSISTANT_INDICATOR)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            message,
            Style::default()
                .fg(COLOR_ASSISTANT_INDICATOR)
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

    if let Some(conv) = &app.current_conversation {
        for msg in &conv.messages {
            // Roles fall into a few buckets: user/assistant render normally
            // (assistant via markdown); tool/system/empty-assistant render
            // only when the debug view is enabled.
            match msg.role.as_str() {
                "user" => {
                    let style = Style::default().fg(COLOR_USER_PREFIX);
                    push_user_message(&mut lines, &msg.content, style);
                    lines.push(Line::from(""));
                }
                "assistant" if !msg.content.trim().is_empty() => {
                    let style = Style::default().fg(COLOR_ASSISTANT_PREFIX);
                    push_assistant_markdown(&mut lines, &msg.content, style);
                    lines.push(Line::from(""));
                }
                "tool" if app.show_debug => {
                    let style = Style::default()
                        .fg(COLOR_DEBUG_TOOL)
                        .add_modifier(Modifier::DIM | Modifier::ITALIC);
                    push_prefixed_message(&mut lines, "tool: ", &msg.content, style);
                    lines.push(Line::from(""));
                }
                "system" if app.show_debug => {
                    let style = Style::default()
                        .fg(COLOR_DEBUG_SYSTEM)
                        .add_modifier(Modifier::DIM | Modifier::ITALIC);
                    push_prefixed_message(&mut lines, "system: ", &msg.content, style);
                    lines.push(Line::from(""));
                }
                "assistant" if app.show_debug => {
                    let style = Style::default()
                        .fg(COLOR_ASSISTANT_PREFIX)
                        .add_modifier(Modifier::DIM | Modifier::ITALIC);
                    push_prefixed_message(&mut lines, "Adele (empty): ", &msg.content, style);
                    lines.push(Line::from(""));
                }
                _ => continue,
            }
        }

        // Show streaming buffer as in-progress assistant message. Markdown is
        // applied to the partial buffer too — unclosed fences just show
        // their literal backticks until the stream catches up.
        if !app.streaming_buffer.is_empty() {
            let style = Style::default().fg(COLOR_ASSISTANT_STREAMING);
            push_assistant_markdown(&mut lines, &app.streaming_buffer, style);
            // Cursor on last line
            if let Some(last) = lines.last_mut() {
                last.spans.push(Span::styled("▌", style));
            }
        }
    } else {
        lines.push(Line::from("Press 'n' to create a new conversation."));
    }

    let chat_title = app
        .current_conversation
        .as_ref()
        .map(|conv| conv.title.as_str())
        .unwrap_or("Chat");
    let model_suffix = app
        .current_conversation
        .as_ref()
        .and_then(|conv| conv.model_selection.as_ref())
        .map(|sel| format!("  ·  {} · {}", sel.connection_id, sel.model_id))
        .unwrap_or_default();
    // Persistent cue for the two per-conversation voice controls (adele-tui#77),
    // so the user can always see both states. `Adele:` (voice output, Ctrl+S
    // cycles) shows only when not Disabled — the common default stays
    // uncluttered; `You:` (voice input, Ctrl+V toggles) shows only when Enabled.
    // A speaker glyph for Adele, a mic glyph for You so they read distinctly.
    let adele_suffix = match app.current_adele_output() {
        AdeleOutput::Disabled => String::new(),
        level => format!("  ·  🔊 Adele: {} (Ctrl+S)", level.label()),
    };
    let you_suffix = if app.current_voice_in() {
        "  ·  🎙 You: Enabled (Ctrl+V)"
    } else {
        ""
    };
    let title = if app.scroll_offset > 0 {
        format!(
            "{chat_title}{model_suffix}{adele_suffix}{you_suffix} \
             (Ctrl+u/d scroll, Ctrl+e bottom)"
        )
    } else {
        format!("{chat_title}{model_suffix}{adele_suffix}{you_suffix}")
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_PANEL_BORDER))
        .title(Line::from(Span::styled(
            title,
            Style::default()
                .fg(Color::Rgb(166, 182, 255))
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

fn draw_input(f: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let wrap_width = usize::from(area.width.saturating_sub(2)).max(1);
    app.rewrap_textarea_to_width(wrap_width);

    let (title, border_color) = match app.mode {
        InputMode::Normal => ("Input (press 'i' to edit)", COLOR_INPUT_BORDER_IDLE),
        InputMode::Editing => (
            "Input (Esc cancel, Enter send, Shift+Enter/Ctrl+J newline)",
            COLOR_INPUT_BORDER_EDIT,
        ),
        InputMode::Renaming => ("Input", COLOR_INPUT_BORDER_IDLE),
    };

    app.textarea.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(Line::from(Span::styled(
                title,
                Style::default().fg(Color::Rgb(216, 223, 236)),
            ))),
    );

    f.render_widget(&app.textarea, area);
}

fn draw_status_bar(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let badge = if app.tasks.visible {
        String::new()
    } else {
        crate::tasks::running_badge(&app.tasks)
    };
    if app.status_message.is_empty() && badge.is_empty() {
        return;
    }
    let mut spans: Vec<Span> = Vec::with_capacity(4);
    spans.push(Span::styled(" • ", Style::default().fg(COLOR_STATUS_DIM)));
    spans.push(Span::styled(
        app.status_message.as_str(),
        Style::default().fg(Color::White),
    ));
    if !badge.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            badge,
            Style::default()
                .fg(Color::Rgb(122, 163, 255))
                .add_modifier(Modifier::BOLD),
        ));
    }
    let status = Paragraph::new(Line::from(spans));
    f.render_widget(status, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{ChatMessage, ConversationDetail, ConversationSummary};
    use ratatui::{Terminal, backend::TestBackend};

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
        app.current_conversation = Some(ConversationDetail {
            id: "1".into(),
            title: "Test".into(),
            messages: vec![
                ChatMessage {
                    role: "user".into(),
                    content: "Hello".into(),
                },
                ChatMessage {
                    role: "assistant".into(),
                    content: "Hi there!".into(),
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
        app.current_conversation = Some(ConversationDetail {
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
        app.current_conversation = Some(ConversationDetail {
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
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.current_conversation = Some(ConversationDetail {
            id: "1".into(),
            title: "Test".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });
        app.start_streaming("req1".into());
        app.receive_chunk("req1", "Partial response...");
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
        app.current_conversation = Some(ConversationDetail {
            id: "1".into(),
            title: "Test".into(),
            messages: vec![
                ChatMessage {
                    role: "user".into(),
                    content: "Hello".into(),
                },
                ChatMessage {
                    role: "tool".into(),
                    content: "ran search(foo)".into(),
                },
                ChatMessage {
                    role: "system".into(),
                    content: "context updated".into(),
                },
                ChatMessage {
                    role: "assistant".into(),
                    content: "".into(), // empty — only shown in debug
                },
                ChatMessage {
                    role: "assistant".into(),
                    content: "Hi there!".into(),
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
        app.current_conversation = Some(ConversationDetail {
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
        app.current_conversation = Some(ConversationDetail {
            id: "1".into(),
            title: "Test".into(),
            messages: vec![ChatMessage {
                role: "assistant".into(),
                content: "answer with **strong** word".into(),
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
