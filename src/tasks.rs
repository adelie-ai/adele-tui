//! Process-manager pane.
//!
//! Overlay that lists background tasks (subagents and user-initiated
//! standalone agents) emitted by the daemon's background-task registry
//! (`desktop-assistant#110` / `#114`). Modeled on `connections.rs`:
//! state lives in a dedicated struct, rendering is local, and `main`
//! routes keys and events to it.
//!
//! Lifecycle
//! ---------
//!
//! 1. On WS connect, main sends `ListBackgroundTasks` once for the
//!    initial snapshot, then `SubscribeBackgroundTasks` for live
//!    updates.
//! 2. `SignalEvent::Task*` variants are forwarded into `TaskPane` via
//!    `apply_task_*` methods; the maps are the single source of truth.
//! 3. The pane is keyboard-toggled (`Ctrl+P`). When closed, a small
//!    "(N running)" badge appears in the status bar.
//!
//! Layout (when open)
//! ------------------
//!
//! ```text
//! +-------------------------- Tasks (3 running) ---------------------------+
//! | ▸ a1b2c3  Researcher: pricing  RUN   12s  ↳p=t-xyz |  18:42:01 INFO    |
//! |   d4e5f6  Subagent: parser     OK    1m                LLM: response   |
//! |   …                                                |  18:42:03 INFO    |
//! |                                                    |  tool: search()   |
//! +----------------------------------------------------+-------------------+
//! ```
//!
//! Logs are capped at 500 entries per task (UI-side); the daemon also
//! caps the in-memory log buffer.

use std::collections::{BTreeMap, HashMap, VecDeque};

use desktop_assistant_api_model::{
    LogLevel, TaskId, TaskKind, TaskLogEntry, TaskStatus, TaskView,
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

/// Per-task UI log cap. The daemon also caps its buffer; this is a
/// belt-and-suspenders limit on the TUI side so an over-chatty task
/// can't OOM us if the daemon's cap is raised.
pub const LOG_CAP: usize = 500;

/// Lightweight projection of `TaskView` for the list row. We keep most
/// fields verbatim and add a denormalized `conversation_id` so the
/// `Enter` -> jump-to-conversation path doesn't have to destructure
/// `TaskKind` every time.
#[derive(Debug, Clone)]
pub struct TaskRow {
    pub id: TaskId,
    pub title: String,
    pub status: TaskStatus,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub last_error: Option<String>,
    pub parent: Option<TaskId>,
    pub progress_hint: Option<String>,
    /// Linked conversation id for `Enter` to jump to. Present for every
    /// `TaskKind` variant (all three carry a conversation), but kept
    /// `Option<_>` so future variants can opt out.
    pub conversation_id: Option<String>,
}

impl TaskRow {
    pub fn from_view(view: &TaskView) -> Self {
        let conversation_id = match &view.kind {
            TaskKind::Conversation { conversation_id }
            | TaskKind::Standalone {
                conversation_id, ..
            }
            | TaskKind::Subagent {
                conversation_id, ..
            } => Some(conversation_id.clone()),
        };
        Self {
            id: view.id.clone(),
            title: view.title.clone(),
            status: view.status,
            started_at: view.started_at,
            ended_at: view.ended_at,
            last_error: view.last_error.clone(),
            parent: view.parent.clone(),
            progress_hint: view.progress_hint.clone(),
            conversation_id,
        }
    }
}

#[derive(Debug, Default)]
pub struct TaskPane {
    /// Ordered map by task id so iteration order is stable and tests
    /// don't have to sort. The daemon assigns roughly monotonic ids
    /// so newest-first ordering happens to fall out naturally; if we
    /// need a different ordering we can compute it at render time.
    pub tasks: BTreeMap<TaskId, TaskRow>,
    /// Per-task ring of recent log entries, capped at `LOG_CAP`. A
    /// `VecDeque` lets us drop the oldest entry in O(1) when full.
    pub task_logs: HashMap<TaskId, VecDeque<TaskLogEntry>>,
    /// Currently-highlighted task in the list.
    pub selected: Option<TaskId>,
    /// Whether the overlay is currently rendered.
    pub visible: bool,
}

impl TaskPane {
    pub fn new() -> Self {
        Self::default()
    }

    /// Toggle pane visibility. Selection is preserved across toggles;
    /// when opening with no selection we pick the first task so j/k
    /// have somewhere to start from.
    pub fn toggle(&mut self) {
        self.visible = !self.visible;
        if self.visible && self.selected.is_none() {
            self.selected = self.tasks.keys().next().cloned();
        }
    }

    /// Replace the entire task set with a snapshot (response to
    /// `ListBackgroundTasks`). Preserves the current selection if the
    /// task still exists; otherwise picks the first task.
    pub fn set_initial(&mut self, views: Vec<TaskView>) {
        self.tasks.clear();
        for v in views {
            self.tasks.insert(v.id.clone(), TaskRow::from_view(&v));
        }
        self.normalize_selection();
    }

    pub fn apply_task_started(&mut self, view: TaskView) {
        let row = TaskRow::from_view(&view);
        self.tasks.insert(view.id.clone(), row);
        if self.selected.is_none() {
            self.selected = Some(view.id);
        }
    }

    pub fn apply_task_progress(&mut self, id: &str, progress_hint: Option<String>) {
        let key = TaskId(id.to_string());
        if let Some(row) = self.tasks.get_mut(&key) {
            row.progress_hint = progress_hint;
        }
        // Unknown id -> drop silently. Per the spec a `TaskProgress`
        // event for a task we never saw `TaskStarted` for is benign
        // (could be a race during connect, before the snapshot lands).
    }

    pub fn apply_task_log_appended(&mut self, id: &str, entry: TaskLogEntry) {
        let key = TaskId(id.to_string());
        let buf = self.task_logs.entry(key).or_default();
        buf.push_back(entry);
        while buf.len() > LOG_CAP {
            buf.pop_front();
        }
    }

    pub fn apply_task_completed(
        &mut self,
        id: &str,
        status: TaskStatus,
        last_error: Option<String>,
    ) {
        let key = TaskId(id.to_string());
        if let Some(row) = self.tasks.get_mut(&key) {
            row.status = status;
            row.last_error = last_error;
            // `ended_at` is set lazily on the next snapshot; the daemon
            // is the authoritative source. We don't fabricate a
            // timestamp here.
        }
    }

    pub fn running_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|r| matches!(r.status, TaskStatus::Pending | TaskStatus::Running))
            .count()
    }

    /// Move the selection cursor up/down through the task list. Wraps
    /// at the ends; no-op on an empty list.
    pub fn move_selection(&mut self, delta: i32) {
        if self.tasks.is_empty() {
            self.selected = None;
            return;
        }
        let ids: Vec<TaskId> = self.tasks.keys().cloned().collect();
        let idx = self
            .selected
            .as_ref()
            .and_then(|sel| ids.iter().position(|k| k == sel))
            .map(|p| p as i32)
            .unwrap_or(0);
        let len = ids.len() as i32;
        let new = ((idx + delta) % len + len) % len;
        self.selected = ids.get(new as usize).cloned();
    }

    pub fn selected_row(&self) -> Option<&TaskRow> {
        self.selected.as_ref().and_then(|id| self.tasks.get(id))
    }

    /// Return the full log buffer for the selected task as a `Vec` for
    /// rendering. Allocates per draw; if it ever shows up in profiles
    /// we can keep a contiguous backing buffer.
    pub fn selected_logs_vec(&self) -> Vec<TaskLogEntry> {
        match self.selected.as_ref() {
            Some(id) => self
                .task_logs
                .get(id)
                .map(|v| v.iter().cloned().collect())
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    fn normalize_selection(&mut self) {
        if self
            .selected
            .as_ref()
            .map(|id| !self.tasks.contains_key(id))
            .unwrap_or(true)
        {
            self.selected = self.tasks.keys().next().cloned();
        }
    }
}

// --- Rendering --------------------------------------------------------

const COLOR_BORDER: Color = Color::Rgb(82, 104, 173);
const COLOR_TITLE: Color = Color::Rgb(166, 182, 255);
const COLOR_HINT_DESC: Color = Color::Rgb(143, 153, 174);
const COLOR_LIST_HIGHLIGHT: Color = Color::Rgb(72, 102, 180);
const COLOR_LIST_HIGHLIGHT_FG: Color = Color::Rgb(245, 248, 255);
const COLOR_OK: Color = Color::Rgb(132, 218, 193);
const COLOR_ERROR: Color = Color::Rgb(232, 130, 130);
const COLOR_WARN: Color = Color::Rgb(232, 200, 130);
const COLOR_RUN: Color = Color::Rgb(122, 163, 255);
const COLOR_PEND: Color = Color::Rgb(178, 138, 220);

fn status_chip(status: TaskStatus) -> Span<'static> {
    let (label, color) = match status {
        TaskStatus::Pending => ("PEND", COLOR_PEND),
        TaskStatus::Running => ("RUN ", COLOR_RUN),
        TaskStatus::Completed => ("OK  ", COLOR_OK),
        TaskStatus::Failed => ("FAIL", COLOR_ERROR),
        TaskStatus::Cancelled => ("CXL ", COLOR_WARN),
    };
    Span::styled(
        format!(" {label} "),
        Style::default()
            .fg(Color::Black)
            .bg(color)
            .add_modifier(Modifier::BOLD),
    )
}

fn level_color(level: LogLevel) -> Color {
    match level {
        LogLevel::Info => COLOR_OK,
        LogLevel::Warn => COLOR_WARN,
        LogLevel::Error => COLOR_ERROR,
    }
}

fn short_id(id: &TaskId) -> String {
    // Show the first 8 chars (or fewer) — uuids are 36 chars and the
    // full id is too noisy for a list row. The full id is still
    // available via the daemon for any drill-down.
    id.0.chars().take(8).collect()
}

/// Render the task age as a compact `12s` / `3m` / `1h` string.
///
/// Uses `ended_at` when the task is terminal, otherwise the difference
/// to "now". We don't pass `now` in; it's derived from `SystemTime` so
/// the badge updates as the user looks at the screen. For unit tests we
/// keep this pure and pass `now_ms`.
fn age_label(row: &TaskRow, now_ms: i64) -> String {
    let end = row.ended_at.unwrap_or(now_ms);
    let delta = end.saturating_sub(row.started_at).max(0) / 1000;
    if delta < 60 {
        format!("{delta}s")
    } else if delta < 3_600 {
        format!("{}m", delta / 60)
    } else if delta < 86_400 {
        format!("{}h", delta / 3_600)
    } else {
        format!("{}d", delta / 86_400)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Render the pane as a centered overlay over `area`. The caller should
/// already have drawn the chat panel underneath; we render `Clear` over
/// our bounding box so the chat doesn't bleed through.
pub fn draw_overlay(f: &mut Frame, pane: &TaskPane, area: Rect) {
    // 90% width, 80% height — leaves chat visible at the edges so the
    // user remembers what they're working on.
    let popup_w = (area.width as u32 * 90 / 100) as u16;
    let popup_h = (area.height as u32 * 80 / 100) as u16;
    let popup_w = popup_w.max(40).min(area.width);
    let popup_h = popup_h.max(8).min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };

    f.render_widget(Clear, popup);

    let title_line = Line::from(vec![
        Span::styled(
            format!("Tasks ({} running)", pane.running_count()),
            Style::default()
                .fg(COLOR_TITLE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  —  j/k navigate · c cancel · Enter open conv · Ctrl+P close",
            Style::default().fg(COLOR_HINT_DESC),
        ),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BORDER))
        .title(title_line);
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    if pane.tasks.is_empty() {
        let empty = Paragraph::new(Line::from(Span::styled(
            "(no background tasks — they will appear here as the daemon spawns them)",
            Style::default().fg(COLOR_HINT_DESC),
        )))
        .wrap(Wrap { trim: true });
        f.render_widget(empty, inner);
        return;
    }

    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);

    draw_task_list(f, pane, split[0]);
    draw_task_logs(f, pane, split[1]);
}

fn draw_task_list(f: &mut Frame, pane: &TaskPane, area: Rect) {
    let ids: Vec<&TaskId> = pane.tasks.keys().collect();
    let selected_idx = pane
        .selected
        .as_ref()
        .and_then(|sel| ids.iter().position(|id| *id == sel));

    let now = now_ms();
    let items: Vec<ListItem<'static>> = pane
        .tasks
        .values()
        .map(|row| {
            let parent_indicator = if row.parent.is_some() {
                Span::styled(" ↳", Style::default().fg(COLOR_HINT_DESC))
            } else {
                Span::raw("  ")
            };
            let progress = row
                .progress_hint
                .as_deref()
                .map(|p| format!("  ·  {p}"))
                .unwrap_or_default();
            let age = age_label(row, now);
            let mut spans: Vec<Span<'static>> = vec![
                status_chip(row.status),
                Span::raw(" "),
                Span::styled(
                    short_id(&row.id),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {age:>4}"),
                    Style::default().fg(COLOR_HINT_DESC),
                ),
                parent_indicator,
                Span::raw(" "),
                Span::styled(row.title.clone(), Style::default().fg(Color::White)),
            ];
            if !progress.is_empty() {
                spans.push(Span::styled(progress, Style::default().fg(COLOR_HINT_DESC)));
            }
            if let Some(err) = &row.last_error {
                spans.push(Span::styled(
                    format!("  ·  {err}"),
                    Style::default()
                        .fg(COLOR_ERROR)
                        .add_modifier(Modifier::ITALIC),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(COLOR_BORDER))
                .title(Line::from(Span::styled(
                    "Background tasks",
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

    let mut state = ListState::default();
    state.select(selected_idx);
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_task_logs(f: &mut Frame, pane: &TaskPane, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(COLOR_BORDER))
        .title(Line::from(Span::styled(
            "Logs",
            Style::default()
                .fg(COLOR_TITLE)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let logs = pane.selected_logs_vec();
    if logs.is_empty() {
        let empty = Paragraph::new(Line::from(Span::styled(
            "(no log entries yet)",
            Style::default().fg(COLOR_HINT_DESC),
        )));
        f.render_widget(empty, inner);
        return;
    }

    let lines: Vec<Line<'static>> = logs
        .iter()
        .flat_map(|entry| {
            // Multi-line messages get split across paragraph lines so
            // long tool outputs are readable without word-wrap weirdness.
            let head = Span::styled(
                format!("[{:>4}] ", entry.seq),
                Style::default().fg(COLOR_HINT_DESC),
            );
            let level_label = format!("{:?}", entry.level).to_uppercase();
            let level_span = Span::styled(
                format!("{level_label:5}"),
                Style::default()
                    .fg(level_color(entry.level))
                    .add_modifier(Modifier::BOLD),
            );
            let category_span = Span::styled(
                format!(" {:?} ", entry.category).to_lowercase(),
                Style::default().fg(COLOR_HINT_DESC),
            );
            let mut out: Vec<Line<'static>> = Vec::new();
            let mut first = true;
            for chunk in entry.message.split('\n') {
                if first {
                    out.push(Line::from(vec![
                        head.clone(),
                        level_span.clone(),
                        category_span.clone(),
                        Span::raw(chunk.to_string()),
                    ]));
                    first = false;
                } else {
                    out.push(Line::from(Span::raw(format!("        {chunk}"))));
                }
            }
            out
        })
        .collect();

    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}

/// "(N running)" badge text for the status bar when the pane is closed.
/// Returns an empty string when N is zero so the badge is invisible at
/// rest.
pub fn running_badge(pane: &TaskPane) -> String {
    let n = pane.running_count();
    if n == 0 {
        String::new()
    } else {
        format!("({n} running)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desktop_assistant_api_model::{
        LogCategory, LogLevel, TaskId, TaskKind, TaskLogEntry, TaskStatus, TaskView,
    };
    use ratatui::{Terminal, backend::TestBackend};

    fn view(id: &str, title: &str, status: TaskStatus) -> TaskView {
        TaskView {
            id: TaskId(id.into()),
            kind: TaskKind::Standalone {
                name: title.into(),
                conversation_id: format!("conv-{id}"),
            },
            status,
            started_at: 1,
            ended_at: None,
            last_error: None,
            parent: None,
            children: Vec::new(),
            title: title.into(),
            progress_hint: None,
        }
    }

    fn log(seq: u64, msg: &str) -> TaskLogEntry {
        TaskLogEntry {
            seq,
            timestamp: 1,
            level: LogLevel::Info,
            category: LogCategory::Status,
            message: msg.into(),
            data: None,
        }
    }

    fn render_to_buffer(pane: &TaskPane, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            draw_overlay(f, pane, f.area());
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        buf.content.iter().map(|c| c.symbol()).collect()
    }

    // --- State tests ----------------------------------------------------

    #[test]
    fn task_started_event_adds_row_to_pane() {
        let mut pane = TaskPane::new();
        pane.apply_task_started(view("t-1", "Researcher", TaskStatus::Running));
        assert_eq!(pane.tasks.len(), 1);
        let row = pane.tasks.get(&TaskId("t-1".into())).unwrap();
        assert_eq!(row.title, "Researcher");
        assert!(matches!(row.status, TaskStatus::Running));
    }

    #[test]
    fn task_completed_event_updates_status_badge() {
        let mut pane = TaskPane::new();
        pane.apply_task_started(view("t-1", "Researcher", TaskStatus::Running));
        pane.apply_task_completed("t-1", TaskStatus::Completed, None);
        let row = pane.tasks.get(&TaskId("t-1".into())).unwrap();
        assert!(matches!(row.status, TaskStatus::Completed));
    }

    #[test]
    fn task_completed_with_failure_records_last_error() {
        let mut pane = TaskPane::new();
        pane.apply_task_started(view("t-1", "Researcher", TaskStatus::Running));
        pane.apply_task_completed("t-1", TaskStatus::Failed, Some("LLM timed out".into()));
        let row = pane.tasks.get(&TaskId("t-1".into())).unwrap();
        assert!(matches!(row.status, TaskStatus::Failed));
        assert_eq!(row.last_error.as_deref(), Some("LLM timed out"));
    }

    #[test]
    fn task_progress_updates_hint() {
        let mut pane = TaskPane::new();
        pane.apply_task_started(view("t-1", "Researcher", TaskStatus::Running));
        pane.apply_task_progress("t-1", Some("step 2/5".into()));
        assert_eq!(
            pane.tasks
                .get(&TaskId("t-1".into()))
                .unwrap()
                .progress_hint
                .as_deref(),
            Some("step 2/5"),
        );
    }

    #[test]
    fn task_progress_for_unknown_id_is_benign() {
        let mut pane = TaskPane::new();
        pane.apply_task_progress("unknown", Some("x".into()));
        assert!(pane.tasks.is_empty());
    }

    #[test]
    fn task_log_appended_for_unknown_id_buffers_anyway() {
        let mut pane = TaskPane::new();
        pane.apply_task_log_appended("t-1", log(1, "hello"));
        assert_eq!(pane.task_logs.get(&TaskId("t-1".into())).unwrap().len(), 1);
    }

    #[test]
    fn task_completed_for_unknown_id_is_benign() {
        let mut pane = TaskPane::new();
        pane.apply_task_completed("missing", TaskStatus::Completed, None);
        assert!(pane.tasks.is_empty());
    }

    #[test]
    fn log_ring_caps_at_500_entries() {
        let mut pane = TaskPane::new();
        for i in 0..600u64 {
            pane.apply_task_log_appended("t-1", log(i, &format!("e{i}")));
        }
        let buf = pane.task_logs.get(&TaskId("t-1".into())).unwrap();
        assert_eq!(buf.len(), LOG_CAP);
        assert_eq!(buf.front().unwrap().seq, 100);
        assert_eq!(buf.back().unwrap().seq, 599);
    }

    #[test]
    fn rapid_event_burst_preserves_ordering() {
        let mut pane = TaskPane::new();
        pane.apply_task_started(view("t-1", "Burst", TaskStatus::Running));
        for i in 0..200u64 {
            pane.apply_task_log_appended("t-1", log(i, &format!("e{i}")));
        }
        let buf = pane.task_logs.get(&TaskId("t-1".into())).unwrap();
        let seqs: Vec<u64> = buf.iter().map(|e| e.seq).collect();
        assert_eq!(seqs.first().copied(), Some(0));
        assert_eq!(seqs.last().copied(), Some(199));
        for w in seqs.windows(2) {
            assert!(w[0] < w[1], "out-of-order seq in burst: {seqs:?}");
        }
    }

    #[test]
    fn set_initial_replaces_full_set_and_preserves_selection_if_present() {
        let mut pane = TaskPane::new();
        pane.set_initial(vec![
            view("t-1", "First", TaskStatus::Running),
            view("t-2", "Second", TaskStatus::Running),
        ]);
        pane.selected = Some(TaskId("t-2".into()));
        pane.set_initial(vec![
            view("t-2", "Second", TaskStatus::Completed),
            view("t-3", "Third", TaskStatus::Running),
        ]);
        assert_eq!(pane.tasks.len(), 2);
        assert_eq!(pane.selected.as_ref(), Some(&TaskId("t-2".into())));
        assert!(!pane.tasks.contains_key(&TaskId("t-1".into())));
    }

    #[test]
    fn set_initial_picks_first_when_selection_gone() {
        let mut pane = TaskPane::new();
        pane.set_initial(vec![view("t-1", "First", TaskStatus::Running)]);
        pane.selected = Some(TaskId("gone".into()));
        pane.set_initial(vec![view("t-2", "Second", TaskStatus::Running)]);
        assert_eq!(pane.selected.as_ref(), Some(&TaskId("t-2".into())));
    }

    #[test]
    fn toggle_flips_visibility_and_picks_first_selection() {
        let mut pane = TaskPane::new();
        pane.apply_task_started(view("t-1", "A", TaskStatus::Running));
        pane.apply_task_started(view("t-2", "B", TaskStatus::Running));
        pane.selected = None;
        assert!(!pane.visible);
        pane.toggle();
        assert!(pane.visible);
        assert_eq!(pane.selected.as_ref(), Some(&TaskId("t-1".into())));
        pane.toggle();
        assert!(!pane.visible);
    }

    #[test]
    fn move_selection_wraps_at_boundaries() {
        let mut pane = TaskPane::new();
        pane.apply_task_started(view("t-1", "A", TaskStatus::Running));
        pane.apply_task_started(view("t-2", "B", TaskStatus::Running));
        pane.apply_task_started(view("t-3", "C", TaskStatus::Running));
        pane.selected = Some(TaskId("t-1".into()));
        pane.move_selection(1);
        assert_eq!(pane.selected.as_ref(), Some(&TaskId("t-2".into())));
        pane.move_selection(1);
        assert_eq!(pane.selected.as_ref(), Some(&TaskId("t-3".into())));
        pane.move_selection(1);
        assert_eq!(pane.selected.as_ref(), Some(&TaskId("t-1".into())));
        pane.move_selection(-1);
        assert_eq!(pane.selected.as_ref(), Some(&TaskId("t-3".into())));
    }

    #[test]
    fn move_selection_on_empty_is_noop() {
        let mut pane = TaskPane::new();
        pane.move_selection(1);
        assert!(pane.selected.is_none());
    }

    #[test]
    fn running_count_only_counts_pending_and_running() {
        let mut pane = TaskPane::new();
        pane.apply_task_started(view("t-1", "A", TaskStatus::Running));
        pane.apply_task_started(view("t-2", "B", TaskStatus::Pending));
        pane.apply_task_started(view("t-3", "C", TaskStatus::Completed));
        pane.apply_task_started(view("t-4", "D", TaskStatus::Failed));
        pane.apply_task_started(view("t-5", "E", TaskStatus::Cancelled));
        assert_eq!(pane.running_count(), 2);
    }

    #[test]
    fn task_row_from_view_extracts_conversation_id_for_standalone() {
        let row = TaskRow::from_view(&view("t-1", "X", TaskStatus::Running));
        assert_eq!(row.conversation_id.as_deref(), Some("conv-t-1"));
    }

    #[test]
    fn task_row_from_view_extracts_conversation_id_for_subagent() {
        let v = TaskView {
            id: TaskId("t-1".into()),
            kind: TaskKind::Subagent {
                parent_task_id: TaskId("parent".into()),
                conversation_id: "subagent-conv".into(),
                name: "child".into(),
            },
            status: TaskStatus::Running,
            started_at: 1,
            ended_at: None,
            last_error: None,
            parent: Some(TaskId("parent".into())),
            children: Vec::new(),
            title: "child".into(),
            progress_hint: None,
        };
        let row = TaskRow::from_view(&v);
        assert_eq!(row.conversation_id.as_deref(), Some("subagent-conv"));
        assert_eq!(row.parent.as_ref(), Some(&TaskId("parent".into())));
    }

    #[test]
    fn running_badge_empty_when_zero_running() {
        let pane = TaskPane::new();
        assert_eq!(running_badge(&pane), "");
    }

    #[test]
    fn running_badge_renders_count_when_running() {
        let mut pane = TaskPane::new();
        pane.apply_task_started(view("t-1", "A", TaskStatus::Running));
        pane.apply_task_started(view("t-2", "B", TaskStatus::Running));
        pane.apply_task_started(view("t-3", "C", TaskStatus::Completed));
        assert_eq!(running_badge(&pane), "(2 running)");
    }

    // --- Render tests ---------------------------------------------------

    #[test]
    fn tasks_pane_renders_empty_state_when_no_tasks() {
        let pane = TaskPane::new();
        let dump = render_to_buffer(&pane, 80, 24);
        assert!(dump.contains("Tasks (0 running)"));
        assert!(dump.contains("no background tasks"));
    }

    #[test]
    fn tasks_pane_renders_three_running_tasks_with_status_badges() {
        let mut pane = TaskPane::new();
        pane.apply_task_started(view("aaaaaaaa", "Alpha", TaskStatus::Running));
        pane.apply_task_started(view("bbbbbbbb", "Beta", TaskStatus::Pending));
        pane.apply_task_started(view("cccccccc", "Gamma", TaskStatus::Completed));
        let dump = render_to_buffer(&pane, 120, 24);
        assert!(dump.contains("Alpha"), "no Alpha in dump:\n{dump}");
        assert!(dump.contains("Beta"));
        assert!(dump.contains("Gamma"));
        assert!(dump.contains("RUN"));
        assert!(dump.contains("PEND"));
        assert!(dump.contains("OK"));
    }

    #[test]
    fn tasks_pane_shows_log_entries_for_selected_task() {
        let mut pane = TaskPane::new();
        pane.apply_task_started(view("t-1", "Loggy", TaskStatus::Running));
        pane.selected = Some(TaskId("t-1".into()));
        pane.apply_task_log_appended("t-1", log(1, "first log line"));
        pane.apply_task_log_appended("t-1", log(2, "second log line"));
        let dump = render_to_buffer(&pane, 120, 24);
        assert!(
            dump.contains("first log line"),
            "first log missing:\n{dump}"
        );
        assert!(dump.contains("second log line"));
    }

    #[test]
    fn tasks_pane_renders_at_small_widths_without_panic() {
        let mut pane = TaskPane::new();
        pane.apply_task_started(view("t-1", "Title that is fairly long", TaskStatus::Running));
        let _ = render_to_buffer(&pane, 40, 12);
    }

    // --- age_label tests -----------------------------------------------

    fn row_with_start(start_ms: i64, end_ms: Option<i64>) -> TaskRow {
        TaskRow {
            id: TaskId("x".into()),
            title: "".into(),
            status: TaskStatus::Running,
            started_at: start_ms,
            ended_at: end_ms,
            last_error: None,
            parent: None,
            progress_hint: None,
            conversation_id: None,
        }
    }

    #[test]
    fn age_label_under_one_minute_renders_seconds() {
        let row = row_with_start(0, None);
        assert_eq!(age_label(&row, 12_000), "12s");
    }

    #[test]
    fn age_label_minutes_at_or_above_60_seconds() {
        let row = row_with_start(0, None);
        assert_eq!(age_label(&row, 120_000), "2m");
    }

    #[test]
    fn age_label_hours_at_or_above_one_hour() {
        let row = row_with_start(0, None);
        assert_eq!(age_label(&row, 7_200_000), "2h");
    }

    #[test]
    fn age_label_days_at_or_above_one_day() {
        let row = row_with_start(0, None);
        assert_eq!(age_label(&row, 2 * 86_400 * 1_000), "2d");
    }

    #[test]
    fn age_label_uses_ended_at_when_terminal() {
        let row = row_with_start(0, Some(10_000));
        // Now is way past, but ended_at pins the age at 10s.
        assert_eq!(age_label(&row, 1_000_000), "10s");
    }

    #[test]
    fn age_label_negative_delta_clamps_to_zero() {
        // Clock skew protection: if ended_at < started_at we don't
        // render "-5s" or panic.
        let row = row_with_start(100, Some(50));
        assert_eq!(age_label(&row, 0), "0s");
    }
}
