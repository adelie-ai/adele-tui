use desktop_assistant_client_common::api;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::app::{App, InputMode, Screen};
use crate::views::connections::{ConnectorKind, DeleteStage, FormField};
use crate::views::purposes::{PURPOSES_ORDER, PurposeField, effort_label, purpose_label};
use crate::views::selector;

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
    match app.screen {
        Screen::Chat => draw_chat_screen(f, app),
        Screen::Connections => draw_connections_screen(f, app),
        Screen::Purposes => draw_purposes_screen(f, app),
    }

    // Overlays render on top of whatever screen is active.
    if app.model_selector.open {
        draw_model_selector_popup(f, app);
    }
}

fn draw_chat_screen(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(f.area());

    draw_conversation_list(f, app, chunks[0]);
    draw_chat_panel(f, app, chunks[1]);
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
    let notice_lines: u16 = if app.inline_notice.is_some() { 1 } else { 0 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(INPUT_TOTAL_HEIGHT),
            Constraint::Length(notice_lines),
            Constraint::Length(1),
        ])
        .split(area);

    draw_messages(f, app, chunks[0]);
    draw_input(f, app, chunks[1]);
    if notice_lines > 0 {
        draw_inline_notice(f, app, chunks[2]);
    }
    draw_status_bar(f, app, chunks[3]);
}

fn draw_inline_notice(f: &mut Frame, app: &App, area: Rect) {
    if let Some(notice) = app.inline_notice.as_deref() {
        let p = Paragraph::new(Line::from(vec![
            Span::styled("! ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(notice, Style::default().fg(Color::Yellow)),
        ]));
        f.render_widget(p, area);
    }
}

fn draw_messages(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let mut lines: Vec<Line> = Vec::new();

    if let Some(conv) = &app.current_conversation {
        for msg in &conv.messages {
            let (prefix, style) = match msg.role.as_str() {
                "user" => ("You: ", Style::default().fg(COLOR_USER_PREFIX)),
                "assistant" if !msg.content.trim().is_empty() => {
                    ("Adele: ", Style::default().fg(COLOR_ASSISTANT_PREFIX))
                }
                // Skip tool, system, and empty assistant messages
                _ => continue,
            };
            // Split content on newlines so ratatui renders them as separate lines
            let mut first = true;
            for text_line in split_display_lines(&msg.content) {
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
            lines.push(Line::from("")); // spacing
        }

        // Show streaming buffer as in-progress assistant message
        if !app.streaming_buffer.is_empty() {
            let style = Style::default().fg(COLOR_ASSISTANT_STREAMING);
            let mut first = true;
            for text_line in split_display_lines(&app.streaming_buffer) {
                if first {
                    lines.push(Line::from(vec![
                        Span::styled("Adele: ", style.add_modifier(Modifier::BOLD)),
                        Span::styled(text_line, style),
                    ]));
                    first = false;
                } else {
                    lines.push(Line::from(Span::styled(text_line, style)));
                }
            }
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
    let title = if app.scroll_offset > 0 {
        format!("{chat_title} (Ctrl+u/d scroll, Ctrl+e bottom)")
    } else {
        chat_title.to_string()
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
    let mode_str = match app.mode {
        InputMode::Normal => "NORMAL",
        InputMode::Editing => "EDITING",
    };

    let selection_label = app.current_selection_label();

    let status = Paragraph::new(Line::from(vec![
        Span::styled(format!(" [{mode_str}] "), mode_chip_style(&app.mode)),
        Span::styled(" • ", Style::default().fg(COLOR_STATUS_DIM)),
        Span::styled(
            "model: ",
            Style::default().fg(COLOR_STATUS_DIM),
        ),
        Span::styled(
            selection_label,
            Style::default()
                .fg(Color::Rgb(200, 220, 255))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" • ", Style::default().fg(COLOR_STATUS_DIM)),
        Span::styled(
            app.status_message.as_str(),
            Style::default().fg(Color::White),
        ),
    ]));

    f.render_widget(status, area);
}

// --- Connections screen ----------------------------------------------------

fn draw_connections_screen(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    draw_connections_list(f, app, chunks[0]);
    draw_connections_hint_bar(f, app, chunks[1]);

    if app.connections_view.form.is_some() {
        draw_connection_form_popup(f, app);
    }
    if app.connections_view.delete.is_some() {
        draw_connection_delete_popup(f, app);
    }
}

fn draw_connections_list(f: &mut Frame, app: &App, area: Rect) {
    let view = &app.connections_view;
    let items: Vec<ListItem> = view
        .connections
        .iter()
        .map(|c| {
            let avail_chip = match &c.availability {
                api::ConnectionAvailability::Ok => {
                    Span::styled("ok", Style::default().fg(Color::Green))
                }
                api::ConnectionAvailability::Unavailable { reason } => Span::styled(
                    format!("unavail ({reason})"),
                    Style::default().fg(Color::Red),
                ),
            };
            let creds = if c.has_credentials {
                Span::styled("creds", Style::default().fg(Color::Green))
            } else {
                Span::styled("no-creds", Style::default().fg(Color::Yellow))
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", c.id),
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("[{}] ", c.connector_type),
                    Style::default().fg(Color::Rgb(136, 214, 240)),
                ),
                avail_chip,
                Span::raw("  "),
                creds,
            ]))
        })
        .collect();

    let title = if view.loading {
        "Connections (loading…)"
    } else {
        "Connections"
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_LIST_BORDER))
        .title(Line::from(Span::styled(
            title,
            Style::default()
                .fg(Color::Rgb(136, 214, 240))
                .add_modifier(Modifier::BOLD),
        )));

    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(COLOR_LIST_HIGHLIGHT)
                .fg(COLOR_LIST_HIGHLIGHT_FG)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");
    let mut state = ListState::default();
    state.select(view.selected);
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_connections_hint_bar(f: &mut Frame, app: &App, area: Rect) {
    let hint = match &app.connections_view.status {
        Some(s) => s.clone(),
        None => "(a)dd  (c/⏎) configure  (d) remove  (r) refresh models  (q/esc) back".into(),
    };
    let p = Paragraph::new(Line::from(Span::styled(
        hint,
        Style::default().fg(COLOR_STATUS_DIM),
    )));
    f.render_widget(p, area);
}

fn draw_connection_form_popup(f: &mut Frame, app: &App) {
    let Some(form) = app.connections_view.form.as_ref() else {
        return;
    };
    let area = centered_rect(70, 70, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_PANEL_BORDER))
        .title(Line::from(Span::styled(
            if form.existing {
                "Configure connection"
            } else {
                "Add connection"
            },
            Style::default()
                .fg(Color::Rgb(166, 182, 255))
                .add_modifier(Modifier::BOLD),
        )));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    // Row 0: id
    lines.push(form_row(
        "Id",
        &form.id,
        form.field_cursor == 0,
        form.existing,
    ));
    // Row 1: kind
    lines.push(form_row(
        "Type",
        form.kind.as_label(),
        form.field_cursor == 1,
        false,
    ));
    // Row 2..: connector-specific fields
    for (i, field) in form.kind.fields().iter().enumerate() {
        let value: String = match field {
            FormField::ApiKeyEnv => form.api_key_env.clone(),
            FormField::BaseUrl => form.base_url.clone(),
            FormField::AwsProfile => form.aws_profile.clone(),
            FormField::Region => form.region.clone(),
            FormField::OllamaAutoPull => {
                if form.ollama_auto_pull {
                    "on".into()
                } else {
                    "off".into()
                }
            }
        };
        lines.push(form_row(
            field.label(),
            &value,
            form.field_cursor == 2 + i,
            false,
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        match form.kind {
            ConnectorKind::Anthropic | ConnectorKind::OpenAi => {
                "API Key: enter the name of the env var that holds your key (we never store the key itself)."
            }
            ConnectorKind::Bedrock => {
                "Bedrock uses ambient AWS credentials (profile + region)."
            }
            ConnectorKind::Ollama => "Ollama runs locally; no credentials needed.",
        },
        Style::default().fg(COLOR_STATUS_DIM),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Tab/Shift-Tab: next/prev field  •  ←/→ on Type: cycle  •  Enter: save  •  Esc: cancel",
        Style::default().fg(COLOR_STATUS_DIM),
    )));

    if let Some(err) = form.error.as_deref() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("Error: {err}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }

    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}

fn form_row(label: &str, value: &str, active: bool, read_only: bool) -> Line<'static> {
    let label_style = if active {
        Style::default()
            .fg(Color::Rgb(255, 189, 89))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Rgb(190, 200, 225))
    };
    let value_style = if read_only {
        Style::default().fg(COLOR_STATUS_DIM)
    } else if active {
        Style::default()
            .fg(Color::Rgb(245, 248, 255))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let marker = if active { "▸ " } else { "  " };
    let shown = if value.is_empty() {
        "<empty>".to_string()
    } else {
        value.to_string()
    };
    Line::from(vec![
        Span::styled(marker, label_style),
        Span::styled(format!("{label:<22}"), label_style),
        Span::styled(shown, value_style),
    ])
}

fn draw_connection_delete_popup(f: &mut Frame, app: &App) {
    let Some(prompt) = app.connections_view.delete.as_ref() else {
        return;
    };
    let area = centered_rect(60, 30, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Rgb(205, 100, 100)))
        .title(Line::from(Span::styled(
            "Remove connection",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::raw("Remove connection "),
        Span::styled(
            prompt.id.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("?"),
    ]));
    lines.push(Line::from(""));
    match &prompt.stage {
        DeleteStage::Initial => {
            lines.push(Line::from(Span::styled(
                "y/⏎ confirm  •  n/esc cancel",
                Style::default().fg(COLOR_STATUS_DIM),
            )));
        }
        DeleteStage::OfferForce { server_error } => {
            lines.push(Line::from(Span::styled(
                format!("Daemon refused: {server_error}"),
                Style::default().fg(Color::Yellow),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Referencing purposes will fall back to \"primary\" if you force the removal.",
                Style::default().fg(COLOR_STATUS_DIM),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "f force remove  •  n/esc cancel",
                Style::default().fg(COLOR_STATUS_DIM),
            )));
        }
    }
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}

// --- Purposes screen -------------------------------------------------------

fn draw_purposes_screen(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    draw_purposes_list(f, app, chunks[0]);
    draw_purposes_hint_bar(f, app, chunks[1]);

    if app.purposes_view.editor.is_some() {
        draw_purpose_editor_popup(f, app);
    }
}

fn draw_purposes_list(f: &mut Frame, app: &App, area: Rect) {
    let view = &app.purposes_view;
    let header = Line::from(vec![
        Span::styled(format!("{:<14}", "Purpose"), Style::default().fg(Color::Rgb(136, 214, 240)).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{:<22}", "Connection"), Style::default().fg(Color::Rgb(136, 214, 240)).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{:<30}", "Model"), Style::default().fg(Color::Rgb(136, 214, 240)).add_modifier(Modifier::BOLD)),
        Span::styled("Effort", Style::default().fg(Color::Rgb(136, 214, 240)).add_modifier(Modifier::BOLD)),
    ]);

    let items: Vec<ListItem> = PURPOSES_ORDER
        .iter()
        .map(|p| {
            let cfg = match p {
                api::PurposeKindApi::Interactive => view.purposes.interactive.as_ref(),
                api::PurposeKindApi::Dreaming => view.purposes.dreaming.as_ref(),
                api::PurposeKindApi::Embedding => view.purposes.embedding.as_ref(),
                api::PurposeKindApi::Titling => view.purposes.titling.as_ref(),
            };
            let (conn, model, effort) = match cfg {
                Some(c) => (c.connection.clone(), c.model.clone(), c.effort),
                None => ("—".into(), "—".into(), None),
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<14}", purpose_label(*p)),
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{conn:<22}"), Style::default().fg(Color::Rgb(216, 223, 236))),
                Span::styled(format!("{model:<30}"), Style::default().fg(Color::Rgb(216, 223, 236))),
                Span::styled(effort_label(effort), Style::default().fg(Color::Rgb(216, 223, 236))),
            ]))
        })
        .collect();

    let title = if view.loading {
        "Purposes (loading…)"
    } else {
        "Purposes"
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_LIST_BORDER))
        .title(Line::from(Span::styled(
            title,
            Style::default()
                .fg(Color::Rgb(136, 214, 240))
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Split header from list body.
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    let header_p = Paragraph::new(header);
    f.render_widget(header_p, body[0]);

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(COLOR_LIST_HIGHLIGHT)
                .fg(COLOR_LIST_HIGHLIGHT_FG)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");
    let mut state = ListState::default();
    state.select(Some(view.selected));
    f.render_stateful_widget(list, body[1], &mut state);
}

fn draw_purposes_hint_bar(f: &mut Frame, app: &App, area: Rect) {
    let hint = match &app.purposes_view.status {
        Some(s) => s.clone(),
        None => "j/k navigate  •  c/⏎ edit  •  q/esc back".into(),
    };
    let p = Paragraph::new(Line::from(Span::styled(
        hint,
        Style::default().fg(COLOR_STATUS_DIM),
    )));
    f.render_widget(p, area);
}

fn draw_purpose_editor_popup(f: &mut Frame, app: &App) {
    let Some(editor) = app.purposes_view.editor.as_ref() else {
        return;
    };
    let area = centered_rect(70, 50, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_PANEL_BORDER))
        .title(Line::from(Span::styled(
            format!("Edit purpose — {}", purpose_label(editor.purpose)),
            Style::default()
                .fg(Color::Rgb(166, 182, 255))
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let lines = vec![
        form_row("Connection", &editor.connection, editor.field == PurposeField::Connection, false),
        form_row("Model", &editor.model, editor.field == PurposeField::Model, false),
        form_row(
            "Effort",
            effort_label(editor.effort),
            editor.field == PurposeField::Effort,
            false,
        ),
        Line::from(""),
        Line::from(Span::styled(
            "Connection/Model accept a connection id, a model id, or \"primary\" (not allowed on interactive).",
            Style::default().fg(COLOR_STATUS_DIM),
        )),
        Line::from(Span::styled(
            "Effort: (l)ow, (m)edium, (h)igh, (x) clear",
            Style::default().fg(COLOR_STATUS_DIM),
        )),
        Line::from(Span::styled(
            "Tab/Shift-Tab: next/prev field  •  Enter: save  •  Esc: cancel",
            Style::default().fg(COLOR_STATUS_DIM),
        )),
    ];
    let mut lines = lines;
    if let Some(err) = editor.error.as_deref() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("Error: {err}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}

// --- Model selector popup --------------------------------------------------

fn draw_model_selector_popup(f: &mut Frame, app: &App) {
    let area = centered_rect(60, 60, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_PANEL_BORDER))
        .title(Line::from(Span::styled(
            "Select model for this conversation",
            Style::default()
                .fg(Color::Rgb(166, 182, 255))
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let view = &app.model_selector;

    let mut items: Vec<ListItem> = Vec::with_capacity(view.entries.len() + 1);
    items.push(ListItem::new(Line::from(vec![Span::styled(
        "Auto (coming soon)",
        Style::default()
            .fg(COLOR_STATUS_DIM)
            .add_modifier(Modifier::ITALIC),
    )])));
    for entry in &view.entries {
        let label = format!("{} · {}", entry.connection_label, entry.model.display_name);
        let meta = {
            let mut caps = Vec::new();
            if entry.model.capabilities.reasoning {
                caps.push("reasoning");
            }
            if entry.model.capabilities.vision {
                caps.push("vision");
            }
            if entry.model.capabilities.tools {
                caps.push("tools");
            }
            if entry.model.capabilities.embedding {
                caps.push("embedding");
            }
            if caps.is_empty() {
                String::new()
            } else {
                format!(" [{}]", caps.join(","))
            }
        };
        items.push(ListItem::new(Line::from(vec![
            Span::styled(label, Style::default().fg(Color::White)),
            Span::styled(meta, Style::default().fg(COLOR_STATUS_DIM)),
        ])));
    }

    let body_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(inner);

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(COLOR_LIST_HIGHLIGHT)
                .fg(COLOR_LIST_HIGHLIGHT_FG)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");
    let mut state = ListState::default();
    state.select(Some(view.highlight));
    f.render_stateful_widget(list, body_rows[0], &mut state);

    let hint = match &view.status {
        Some(s) => s.clone(),
        None => {
            let sel = app
                .current_conversation
                .as_ref()
                .and_then(|c| app.conversation_selections.get(&c.id));
            format!(
                "current: {}   •   j/k navigate  •  Enter confirm  •  r refresh  •  Esc cancel",
                selector::status_bar_label(sel),
            )
        }
    };
    let p = Paragraph::new(Line::from(Span::styled(
        hint,
        Style::default().fg(COLOR_STATUS_DIM),
    )))
    .alignment(Alignment::Left)
    .wrap(Wrap { trim: true });
    f.render_widget(p, body_rows[1]);
}

// --- layout helpers --------------------------------------------------------

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
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
            warnings: vec![],
        });
        terminal.draw(|f| draw(f, &mut app)).unwrap();
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
    warnings: vec![],
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
    fn draw_connections_screen_does_not_panic() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.screen = Screen::Connections;
        app.connections_view.connections = vec![
            crate::views::connections::tests_fixture("primary", "openai"),
            crate::views::connections::tests_fixture("work", "anthropic"),
        ];
        app.connections_view.selected = Some(0);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    #[test]
    fn draw_connection_form_popup_does_not_panic() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.screen = Screen::Connections;
        app.connections_view.start_add();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    #[test]
    fn draw_connection_delete_popup_does_not_panic() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.screen = Screen::Connections;
        app.connections_view.connections =
            vec![crate::views::connections::tests_fixture("a", "openai")];
        app.connections_view.selected = Some(0);
        app.connections_view.start_delete();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    #[test]
    fn draw_purposes_screen_does_not_panic() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.screen = Screen::Purposes;
        app.purposes_view.purposes = api::PurposesView {
            interactive: Some(api::PurposeConfigView {
                connection: "primary".into(),
                model: "gpt-5".into(),
                effort: Some(api::EffortLevel::High),
            }),
            ..Default::default()
        };
        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    #[test]
    fn draw_purpose_editor_popup_does_not_panic() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.screen = Screen::Purposes;
        app.purposes_view.start_edit();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    #[test]
    fn draw_model_selector_popup_does_not_panic() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new();
        app.model_selector.open = true;
        app.model_selector.entries = vec![api::ModelListing {
            connection_id: "work".into(),
            connection_label: "work".into(),
            model: api::ModelInfoView {
                id: "gpt-5".into(),
                display_name: "GPT-5".into(),
                context_limit: Some(200_000),
                capabilities: api::ModelCapabilitiesView {
                    reasoning: true,
                    vision: true,
                    tools: true,
                    embedding: false,
                },
            },
        }];
        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }

    #[test]
    fn status_bar_shows_auto_label_by_default() {
        let app = App::new();
        assert_eq!(app.current_selection_label(), "Auto (purpose)");
    }

    #[test]
    fn status_bar_shows_pinned_model_when_set() {
        let mut app = App::new();
        app.current_conversation = Some(ConversationDetail {
            id: "c1".into(),
            title: "Test".into(),
            messages: vec![],
            warnings: vec![],
        });
        app.conversation_selections.set(
            "c1".into(),
            api::SendPromptOverride {
                connection_id: "work".into(),
                model_id: "gpt-5".into(),
                effort: None,
            },
        );
        assert_eq!(app.current_selection_label(), "work · gpt-5");
    }

    #[test]
    fn dangling_warning_on_load_sets_inline_notice_and_hydrates_selection() {
        let mut app = App::new();
        app.load_conversation(ConversationDetail {
            id: "c1".into(),
            title: "Test".into(),
            messages: vec![],
            warnings: vec![api::ConversationWarning::DanglingModelSelection {
                previous_selection: api::ConversationModelSelectionView {
                    connection_id: "old".into(),
                    model_id: "gone".into(),
                    effort: None,
                },
                fallback_to: api::ConversationModelSelectionView {
                    connection_id: "new".into(),
                    model_id: "ok".into(),
                    effort: None,
                },
            }],
        });
        assert!(app.inline_notice.as_deref().unwrap().contains("old·gone"));
        // Selection was hydrated from the fallback.
        assert_eq!(
            app.conversation_selections
                .get("c1")
                .unwrap()
                .connection_id,
            "new"
        );
    }
}
