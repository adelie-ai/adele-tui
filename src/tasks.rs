//! Process-manager pane (skeleton — failing tests first).
//!
//! See the issue-45 implementation commit for the full module. This
//! initial skeleton exists only so the tests below can reference types
//! and methods; every public method is unimplemented until the next
//! commit lands.

use std::collections::{BTreeMap, HashMap, VecDeque};

use desktop_assistant_api_model::{TaskId, TaskLogEntry, TaskStatus, TaskView};
use ratatui::{Frame, layout::Rect};

pub const LOG_CAP: usize = 500;

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
    pub conversation_id: Option<String>,
}

impl TaskRow {
    pub fn from_view(_view: &TaskView) -> Self {
        unimplemented!("filled in by the issue-45 implementation commit")
    }
}

#[derive(Debug, Default)]
pub struct TaskPane {
    pub tasks: BTreeMap<TaskId, TaskRow>,
    pub task_logs: HashMap<TaskId, VecDeque<TaskLogEntry>>,
    pub selected: Option<TaskId>,
    pub visible: bool,
}

impl TaskPane {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn toggle(&mut self) {
        unimplemented!("filled in by the issue-45 implementation commit")
    }

    pub fn set_initial(&mut self, _views: Vec<TaskView>) {
        unimplemented!("filled in by the issue-45 implementation commit")
    }

    pub fn apply_task_started(&mut self, _view: TaskView) {
        unimplemented!("filled in by the issue-45 implementation commit")
    }

    pub fn apply_task_progress(&mut self, _id: &str, _progress_hint: Option<String>) {
        unimplemented!("filled in by the issue-45 implementation commit")
    }

    pub fn apply_task_log_appended(&mut self, _id: &str, _entry: TaskLogEntry) {
        unimplemented!("filled in by the issue-45 implementation commit")
    }

    pub fn apply_task_completed(
        &mut self,
        _id: &str,
        _status: TaskStatus,
        _last_error: Option<String>,
    ) {
        unimplemented!("filled in by the issue-45 implementation commit")
    }

    pub fn running_count(&self) -> usize {
        unimplemented!("filled in by the issue-45 implementation commit")
    }

    pub fn move_selection(&mut self, _delta: i32) {
        unimplemented!("filled in by the issue-45 implementation commit")
    }

    pub fn selected_row(&self) -> Option<&TaskRow> {
        unimplemented!("filled in by the issue-45 implementation commit")
    }

    pub fn selected_logs_vec(&self) -> Vec<TaskLogEntry> {
        unimplemented!("filled in by the issue-45 implementation commit")
    }
}

pub fn draw_overlay(_f: &mut Frame, _pane: &TaskPane, _area: Rect) {
    unimplemented!("filled in by the issue-45 implementation commit")
}

pub fn running_badge(_pane: &TaskPane) -> String {
    unimplemented!("filled in by the issue-45 implementation commit")
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
}
