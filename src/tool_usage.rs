//! Per-conversation tool-cost view (desktop-assistant#599).
//!
//! Answers "what ate my context in this conversation", per tool. The daemon
//! aggregates the figures; this module orders them, groups them, and draws
//! them.
//!
//! # Why there are two orderings and not one
//!
//! Frequency and payload are independently material, and each ordering buries
//! the other's signal. Forty calls to one tool is a retry storm or a search
//! loop, and it says so even when every result is tiny - but a token ordering
//! puts it near the bottom. One call that returns half a megabyte is the usual
//! cause of a blown context budget, and a count ordering puts it last. So the
//! view sorts by either axis, and the bars follow whichever is active, so a
//! re-sort visibly re-ranks instead of leaving the old shape behind.
//!
//! Token cost is the default, because "what ate my context" is the question
//! someone opens this view to answer.
//!
//! Everything above the draw path is pure: [`State`] holds no transport, so the
//! ordering, the grouping, the bar widths and the key handling are all testable
//! without a terminal or a daemon.
//!
//! Keys
//! ----
//!
//! - `j/k` or arrows: move the selection
//! - `s`: switch the sort axis (token cost / call count)
//! - `Enter` or `Space`: fold a server's group open or closed
//! - `r`: re-read the figures
//! - `Esc` or `q`: close

use std::collections::BTreeSet;
use std::io;

use crossterm::event::{KeyCode, KeyEvent};
use desktop_assistant_api_model::{Command, CommandResult, ToolTier, ToolUsageView};
use desktop_assistant_client_common::{SignalEvent, TransportClient};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};

use crate::in_flight::InFlight;
use crate::mcp::sanitize;
use crate::screen::Screen;
use crate::theme::theme;

/// Heading for the tools nothing attributes to a server.
///
/// Neither the daemon's own `namespace` field nor the tool's exposed name says
/// where such a tool came from, so the heading says exactly that. Inventing a
/// server called "unknown" would name something that does not exist, and a
/// reader would go looking for it.
///
/// The parentheses are load-bearing: a server name is a config table key, drawn
/// from letters, digits, `-` and `_`, so no server can declare a name that
/// renders as this heading and pass itself off as the absence of one.
pub const UNATTRIBUTED_HEADING: &str = "(unattributed)";

/// The dim note beside [`UNATTRIBUTED_HEADING`], so the group reads as an
/// absence of information rather than as a server.
pub const UNATTRIBUTED_NOTE: &str = "no server reported";

/// What a conversation with no tool calls says. Not an error, and not an empty
/// chart that reads as broken.
pub const EMPTY_STATE: &str = "No tool calls in this conversation";

/// What a read still in flight says.
pub const READING: &str = "Reading tool cost...";

/// How the daemon's MCP executor joins a server's namespace to a tool name.
const NAMESPACE_SEPARATOR: &str = "__";

/// The column a name is padded to, and the width it is cut at, in terminal
/// cells. A tool name and a server name are both server-declared, so one absurd
/// value must not push the figures off the row.
const NAME_COLUMN: usize = 32;

/// What ends a name the column had to cut.
const TRUNCATION_MARKER: &str = "...";

/// Cells the figures beside a bar need. The bar gives up its width to them
/// first, because a figure can be reasoned about without a bar and a bar cannot
/// be reasoned about without a figure.
const FIGURES_COLUMN: usize = 34;

/// Longest server-declared name kept from the wire.
///
/// A tool name and a namespace are declared by a third-party MCP server and
/// nothing on the wire bounds them. This view sorts on those strings and
/// rebuilds every row on every draw and every key press, so one absurd name
/// would cost that work on every frame. Cutting once at ingest bounds all of
/// it. The cap is far above any real tool name, so nothing legitimate is
/// touched.
const MAX_STORED_NAME_CHARS: usize = 128;

/// Longest bar the list will draw. Past this a bar stops being easier to
/// compare and starts crowding out the figure beside it.
const MAX_BAR_CELLS: usize = 24;

/// Partial-cell bar glyphs, one eighth to seven eighths.
const BAR_EIGHTHS: [char; 7] = [
    '\u{258f}', '\u{258e}', '\u{258d}', '\u{258c}', '\u{258b}', '\u{258a}', '\u{2589}',
];

/// A whole bar cell.
const BAR_FULL: char = '\u{2588}';

/// Which figure ranks the rows, the groups, and the bars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Axis {
    /// Estimated tokens still resident, the default. "What ate my context".
    #[default]
    Tokens,
    /// Invocations the model requested. A retry storm reads here and nowhere
    /// else.
    Calls,
}

impl Axis {
    /// How the header names this axis.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Tokens => "Token cost",
            Self::Calls => "Call count",
        }
    }

    /// The other axis, for the `s` key.
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Tokens => Self::Calls,
            Self::Calls => Self::Tokens,
        }
    }
}

/// One namespace's rows and its subtotals - "which server is this session
/// leaning on".
pub struct Group<'a> {
    /// The server, or `None` when nothing attributes these tools to one.
    pub namespace: Option<&'a str>,
    /// The group's rows, heaviest first on the active axis.
    pub rows: Vec<&'a ToolUsageView>,
    pub total_calls: u64,
    pub total_tokens: u64,
}

impl Group<'_> {
    /// The group's heading text.
    #[must_use]
    pub fn heading(&self) -> &str {
        self.namespace.unwrap_or(UNATTRIBUTED_HEADING)
    }

    /// The heading's subtotals, the ranked figure first.
    ///
    /// Both figures, for the same reason a tool row carries both: the axis that
    /// is not ranking is the one that explains the other. It matters more on a
    /// heading, because a folded group's rows are gone from the screen, so a
    /// heading carrying one figure would hide what that server cost on the
    /// other.
    #[must_use]
    pub fn subtotal_line(&self, axis: Axis) -> String {
        let tokens = format!("{} tokens", format_count(self.total_tokens));
        let calls = plural_calls(self.total_calls);
        match axis {
            Axis::Tokens => format!("{tokens} \u{b7} {calls}"),
            Axis::Calls => format!("{calls} \u{b7} {tokens}"),
        }
    }

    /// The figure this group ranks on.
    fn value(&self, axis: Axis) -> u64 {
        match axis {
            Axis::Tokens => self.total_tokens,
            Axis::Calls => self.total_calls,
        }
    }
}

/// One conversation's tool cost, ordered and grouped for reading.
pub struct Report<'a> {
    rows: &'a [ToolUsageView],
    axis: Axis,
}

impl<'a> Report<'a> {
    #[must_use]
    pub fn new(rows: &'a [ToolUsageView], axis: Axis) -> Self {
        Self { rows, axis }
    }

    #[must_use]
    pub fn axis(&self) -> Axis {
        self.axis
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// How many distinct tools the conversation called.
    #[must_use]
    pub fn distinct_tools(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn total_calls(&self) -> u64 {
        self.rows.iter().map(|r| u64::from(r.call_count)).sum()
    }

    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.rows.iter().map(|r| r.result_tokens).sum()
    }

    /// Every row, heaviest first on the active axis.
    ///
    /// Ties break on the tool name, so the same figures list in the same order
    /// every time the view is opened.
    #[must_use]
    pub fn ranked(&self) -> Vec<&'a ToolUsageView> {
        let mut ranked: Vec<&'a ToolUsageView> = self.rows.iter().collect();
        ranked.sort_by(|a, b| {
            row_value(b, self.axis)
                .cmp(&row_value(a, self.axis))
                .then_with(|| a.tool_name.cmp(&b.tool_name))
        });
        ranked
    }

    /// The rows grouped by server, groups and rows both heaviest first.
    #[must_use]
    pub fn groups(&self) -> Vec<Group<'a>> {
        let mut groups: Vec<Group<'a>> = Vec::new();
        for row in self.ranked() {
            let namespace = resolved_namespace(row);
            match groups.iter_mut().find(|g| g.namespace == namespace) {
                Some(group) => {
                    group.total_calls += u64::from(row.call_count);
                    group.total_tokens += row.result_tokens;
                    group.rows.push(row);
                }
                None => groups.push(Group {
                    namespace,
                    rows: vec![row],
                    total_calls: u64::from(row.call_count),
                    total_tokens: row.result_tokens,
                }),
            }
        }
        groups.sort_by(|a, b| {
            b.value(self.axis)
                .cmp(&a.value(self.axis))
                .then_with(|| a.heading().cmp(b.heading()))
        });
        groups
    }

    /// How long a row's bar is, as a fraction of the heaviest row on the active
    /// axis.
    ///
    /// Zero when every row measures zero, rather than a full bar from dividing
    /// by nothing: a conversation whose tools returned nothing must not read as
    /// one where they returned everything.
    #[must_use]
    pub fn fraction(&self, row: &ToolUsageView) -> f64 {
        let peak = self
            .rows
            .iter()
            .map(|r| row_value(r, self.axis))
            .max()
            .unwrap_or(0);
        if peak == 0 {
            return 0.0;
        }
        row_value(row, self.axis) as f64 / peak as f64
    }

    /// The header totals: distinct tools, total calls, total tokens.
    #[must_use]
    pub fn totals_line(&self) -> String {
        let tools = if self.distinct_tools() == 1 {
            "1 tool".to_string()
        } else {
            format!("{} tools", format_count(self.distinct_tools() as u64))
        };
        format!(
            "{tools} \u{b7} {} \u{b7} {} tokens",
            plural_calls(self.total_calls()),
            format_count(self.total_tokens())
        )
    }
}

/// The figure a row ranks on.
fn row_value(row: &ToolUsageView, axis: Axis) -> u64 {
    match axis {
        Axis::Tokens => row.result_tokens,
        Axis::Calls => u64::from(row.call_count),
    }
}

/// "1 result" / "N results".
fn plural_results(results: u64) -> String {
    if results == 1 {
        "1 result".to_string()
    } else {
        format!("{} results", format_count(results))
    }
}

/// "1 call" / "N calls".
fn plural_calls(calls: u64) -> String {
    if calls == 1 {
        "1 call".to_string()
    } else {
        format!("{} calls", format_count(calls))
    }
}

/// The server a tool belongs to, or `None` when nothing says.
///
/// Prefers what the daemon reported. Falls back to the namespace encoded in the
/// tool's own exposed name, because the daemon does not fill the field in yet
/// (desktop-assistant#1312) and its MCP executor builds that name as
/// `"<namespace>__<tool>"`. So this reads the daemon's own encoding rather than
/// guessing, and it retires itself the day the field arrives.
///
/// The tool is split off the END of the name, matching how the daemon reads the
/// same encoding (`rsplit_once` in its own tool-provenance classifier). A server
/// name may itself contain the separator - `home__assistant` is a legal config
/// key - and splitting at the first separator would file its tools under a
/// server called `home` that does not exist, and would merge its subtotals with
/// every other `home__*` server.
///
/// A name with no separator, or one that begins with the separator, resolves to
/// `None`: an empty group heading is worse than an honest unattributed one.
#[must_use]
pub fn resolved_namespace(row: &ToolUsageView) -> Option<&str> {
    if let Some(namespace) = row.namespace.as_deref()
        && !namespace.is_empty()
    {
        return Some(namespace);
    }
    let (prefix, _) = row.tool_name.rsplit_once(NAMESPACE_SEPARATOR)?;
    (!prefix.is_empty()).then_some(prefix)
}

/// The note for a row whose results the model now reads as a pointer, or
/// `None` when it has none.
///
/// `evicted_results` counts two shapes that this view cannot tell apart, and
/// they move the figures in OPPOSITE directions:
///
/// - A completed agentic step distilled its results into a scratchpad note.
///   The conversation still holds every byte, so `result_bytes` counts them in
///   full while the model reads a short pointer instead. Here the figures
///   OVER-state what the turn costs today. This is the normal, healthy shape
///   for long agentic work, and it is not data loss.
/// - A conversation compacted by an older build had the row overwritten. Those
///   bytes are unrecoverable and count zero, so the figures UNDER-state what
///   the tool once cost.
///
/// So the note names the fact and neither direction, because asserting either
/// would be a claim this view cannot support. It states no size either: peak
/// cost is desktop-assistant#675, and nothing here estimates it. The wording
/// matches the sibling clients so one reading does not contradict another.
#[must_use]
pub fn eviction_note(row: &ToolUsageView) -> Option<String> {
    (row.evicted_results > 0).then(|| {
        format!(
            "{} now read as a pointer",
            plural_results(u64::from(row.evicted_results))
        )
    })
}

/// A `width`-cell bar for `fraction` of the widest row.
///
/// Eighth-block glyphs give sub-cell resolution, so two rows that differ by
/// less than a whole cell still differ on screen. A non-zero fraction always
/// draws at least one eighth, so a small but real cost is never invisible;
/// exactly zero draws nothing.
#[must_use]
pub fn bar_cells(fraction: f64, width: usize) -> String {
    if width == 0 || fraction.is_nan() || fraction <= 0.0 {
        return String::new();
    }
    let capacity = width * 8;
    let eighths = (fraction.min(1.0) * capacity as f64).round() as usize;
    let eighths = eighths.clamp(1, capacity);
    let mut bar = String::with_capacity(width);
    for _ in 0..eighths / 8 {
        bar.push(BAR_FULL);
    }
    if !eighths.is_multiple_of(8) {
        bar.push(BAR_EIGHTHS[eighths % 8 - 1]);
    }
    bar
}

/// A count with thousands separators, so a six-figure token figure can be read
/// at a glance.
#[must_use]
pub fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A byte count in binary units.
///
/// The unit is chosen AFTER the value is rounded to its printed precision.
/// Choosing it first prints figures in the wrong unit at the boundary: 1048575
/// bytes is 1023.99 KiB, which rounds to "1024.0 KiB" - a number that should
/// have been carried into the next unit.
#[must_use]
pub fn format_bytes(value: u64) -> String {
    const STEP: f64 = 1024.0;
    const UNITS: [&str; 3] = ["KiB", "MiB", "GiB"];
    if value < STEP as u64 {
        return format!("{value} B");
    }
    let mut scaled = value as f64 / STEP;
    let mut unit = 0;
    while unit + 1 < UNITS.len() && (scaled * 10.0).round() / 10.0 >= STEP {
        scaled /= STEP;
        unit += 1;
    }
    format!("{scaled:.1} {}", UNITS[unit])
}

/// The short chip label for a provenance tier.
///
/// The wire strings ("network_egress", "code_execution") are too long for a row
/// chip, so the chip carries a word and [`ToolTier::is_gated`] carries the
/// meaning: whether a turn that has read outside content will refuse this tool.
#[must_use]
pub fn tier_label(tier: ToolTier) -> &'static str {
    match tier {
        ToolTier::Read => "read",
        ToolTier::Present => "present",
        ToolTier::Mutate => "mutate",
        ToolTier::Egress => "network",
        ToolTier::Execution => "execute",
        ToolTier::Unclassified => "unclassified",
    }
}

/// One drawn line of the list: a server heading, or a tool under it.
pub enum Entry<'a> {
    /// A server heading, with its subtotals and whether it is folded away.
    Group { group: Group<'a>, collapsed: bool },
    /// One tool's row.
    Tool(&'a ToolUsageView),
}

/// What the key handler asks the caller to do off-loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Re-read the figures from the daemon.
    Refresh,
}

/// The view's state. Holds no transport, so every ordering, grouping and
/// key-handling rule below is testable without a terminal or a daemon.
pub struct State {
    /// The daemon's rows, unordered. [`Report`] does the ordering.
    pub rows: Vec<ToolUsageView>,
    pub axis: Axis,
    /// Index into [`State::entries`], not into `rows`: the selection walks the
    /// drawn list, headings included, because a heading is what folds.
    pub selected: usize,
    pub busy: Option<String>,
    pub error: Option<String>,
    pub closing: bool,
    /// Servers the user folded away, keyed by namespace. Collapsed rather than
    /// expanded state is stored, so a group that appears after a re-read starts
    /// open. The unattributed group keys on the empty string, which
    /// [`resolved_namespace`] never returns, so it cannot collide with a real
    /// server.
    collapsed: BTreeSet<String>,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    #[must_use]
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            axis: Axis::default(),
            selected: 0,
            busy: None,
            error: None,
            closing: false,
            collapsed: BTreeSet::new(),
        }
    }

    /// The ordered, grouped reading of the current rows.
    #[must_use]
    pub fn report(&self) -> Report<'_> {
        Report::new(&self.rows, self.axis)
    }

    /// Whether `namespace`'s group is folded away.
    #[must_use]
    pub fn is_collapsed(&self, namespace: Option<&str>) -> bool {
        self.collapsed.contains(fold_key(namespace))
    }

    /// The drawn list: a heading per server, then that server's tools unless it
    /// is folded away.
    #[must_use]
    pub fn entries(&self) -> Vec<Entry<'_>> {
        let mut entries = Vec::new();
        for group in self.report().groups() {
            let collapsed = self.is_collapsed(group.namespace);
            let rows = if collapsed {
                Vec::new()
            } else {
                group.rows.clone()
            };
            entries.push(Entry::Group { group, collapsed });
            entries.extend(rows.into_iter().map(Entry::Tool));
        }
        entries
    }

    /// Mark a read as in flight. Clears the previous failure, so a re-read
    /// never shows a stale error beside a live request.
    pub fn begin_read(&mut self) {
        self.busy = Some(READING.into());
        self.error = None;
    }

    /// Install a fresh answer from the daemon.
    ///
    /// Server-declared names are cut here rather than on the draw path, so the
    /// bound holds for the sort and the row rebuild as well as for the render.
    pub fn install_rows(&mut self, rows: Vec<ToolUsageView>) {
        self.rows = rows.into_iter().map(bound_names).collect();
        self.error = None;
        self.selected = 0;
    }

    /// Handle one key press. Pure: it mutates this state and reports what the
    /// caller must do off-loop.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Effect> {
        if !key.modifiers.is_empty() {
            return None;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.closing = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('s') => {
                self.axis = self.axis.toggled();
                // A new order is a new list, and the old index pointed into the
                // old one; start the reading at the top of the new ranking.
                self.selected = 0;
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.toggle_fold(),
            KeyCode::Char('r') => return Some(Effect::Refresh),
            _ => {}
        }
        None
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.entries().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let last = len as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    /// Fold or unfold the group the selection sits in.
    ///
    /// A heading folds itself. A tool row folds the group it belongs to, and
    /// the selection moves up to the heading that is left standing - otherwise
    /// it would point at a row that is no longer drawn.
    fn toggle_fold(&mut self) {
        let Some((namespace, heading_index)) = self.selected_group() else {
            return;
        };
        let key = fold_key(namespace.as_deref()).to_string();
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        self.selected = heading_index;
        let len = self.entries().len();
        self.selected = self.selected.min(len.saturating_sub(1));
    }

    /// The namespace of the group the selection sits in, and the index of that
    /// group's heading. `None` when the list is empty.
    fn selected_group(&self) -> Option<(Option<String>, usize)> {
        let entries = self.entries();
        let index = match entries.get(self.selected)? {
            Entry::Group { .. } => self.selected,
            // A tool is always drawn under its own heading, so the nearest
            // heading above the selection is the group it belongs to.
            Entry::Tool(_) => entries[..self.selected]
                .iter()
                .rposition(|entry| matches!(entry, Entry::Group { .. }))?,
        };
        match &entries[index] {
            Entry::Group { group, .. } => Some((group.namespace.map(str::to_string), index)),
            Entry::Tool(_) => None,
        }
    }
}

/// Cut a row's server-declared names to [`MAX_STORED_NAME_CHARS`].
fn bound_names(mut row: ToolUsageView) -> ToolUsageView {
    row.tool_name = row.tool_name.chars().take(MAX_STORED_NAME_CHARS).collect();
    row.namespace = row
        .namespace
        .map(|n| n.chars().take(MAX_STORED_NAME_CHARS).collect());
    row
}

/// The set key for a group. The unattributed group keys on the empty string,
/// which [`resolved_namespace`] never returns, so no real server collides
/// with it.
fn fold_key(namespace: Option<&str>) -> &str {
    namespace.unwrap_or("")
}

/// The tool-cost view as a [`Screen`]: its [`State`] plus the borrowed client.
struct ToolUsageScreen<'a> {
    state: State,
    client: &'a TransportClient,
    conversation_id: String,
    /// The in-flight read, polled off the draw loop so a slow daemon never
    /// freezes the screen.
    pending: InFlight<'a, Result<Vec<ToolUsageView>, String>>,
}

impl Screen for ToolUsageScreen<'_> {
    type Outcome = ();

    fn draw(&mut self, frame: &mut Frame) {
        draw(frame, &self.state);
    }

    fn handle_key(&mut self, key: KeyEvent) -> impl std::future::Future<Output = ()> {
        if let Some(Effect::Refresh) = self.state.handle_key(key) {
            refresh(
                &mut self.state,
                &mut self.pending,
                self.client,
                &self.conversation_id,
            );
        }
        std::future::ready(())
    }

    fn take_outcome(&mut self) -> Option<()> {
        self.state.closing.then_some(())
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    async fn poll_pending(&mut self) {
        if let Some(outcome) = self.pending.next().await {
            self.state.busy = None;
            match outcome {
                Ok(rows) => self.state.install_rows(rows),
                // An empty answer means "no tool calls" and draws the empty
                // state; this arm is a real failure to ask, so it must not read
                // as a quiet zero.
                Err(e) => self.state.error = Some(e),
            }
        }
    }
}

/// Run the tool-cost view for one conversation until the user closes it.
pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    client: &TransportClient,
    conversation_id: String,
    signal_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SignalEvent>,
    sink: &mut impl crate::screen::SignalSink,
) -> anyhow::Result<()> {
    let mut screen = ToolUsageScreen {
        state: State::new(),
        client,
        conversation_id,
        pending: InFlight::new(),
    };
    let id = screen.conversation_id.clone();
    refresh(&mut screen.state, &mut screen.pending, client, &id);

    crate::screen::run_screen(terminal, &mut screen, signal_rx, sink).await
}

/// Enqueue the read off-loop. `poll_pending` installs the rows when it resolves.
fn refresh<'a>(
    state: &mut State,
    pending: &mut InFlight<'a, Result<Vec<ToolUsageView>, String>>,
    client: &'a TransportClient,
    conversation_id: &str,
) {
    state.begin_read();
    let id = conversation_id.to_string();
    pending.push(async move { load(client, id).await });
}

async fn load(
    client: &TransportClient,
    conversation_id: String,
) -> Result<Vec<ToolUsageView>, String> {
    let Some(commands) = client.as_commands() else {
        return Err(
            "Tool cost isn't available over D-Bus - switch transport with --transport ws or the local socket"
                .into(),
        );
    };
    match commands
        .send_command(Command::GetToolUsage { conversation_id })
        .await
    {
        Ok(CommandResult::ToolUsage(rows)) => Ok(rows),
        Ok(other) => Err(format!("Unexpected response reading tool cost: {other:?}")),
        Err(e) => Err(format!("Failed to read tool cost: {e}")),
    }
}

// --- draw --------------------------------------------------------------------

fn draw(f: &mut Frame, state: &State) {
    let area = f.area();
    f.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(f, state, chunks[0]);
    draw_body(f, state, chunks[1]);
    draw_status(f, state, chunks[2]);
    draw_hints(f, chunks[3]);
}

fn draw_header(f: &mut Frame, state: &State, area: Rect) {
    let report = state.report();
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "Tool cost",
            Style::default()
                .fg(theme().title)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("   ranked by ", Style::default().fg(theme().text_dim)),
        Span::styled(
            state.axis.label(),
            Style::default()
                .fg(theme().pinned)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  (s switches)", Style::default().fg(theme().text_dim)),
    ])];
    // The totals belong to a report that has rows. On an empty conversation
    // three zeroes read as a measurement; the empty state says it plainly.
    if !report.is_empty() {
        lines.push(Line::from(Span::styled(
            report.totals_line(),
            Style::default().fg(theme().text_dim),
        )));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_body(f: &mut Frame, state: &State, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme().border));

    if let Some(error) = &state.error {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "Could not read tool cost",
                    Style::default()
                        .fg(theme().error_text)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    sanitize(error),
                    Style::default().fg(theme().error_text),
                )),
            ])
            .block(block),
            area,
        );
        return;
    }

    let report = state.report();
    if report.is_empty() {
        // A read still in flight has nothing to say yet, and saying "no tool
        // calls" before the answer arrives would be a claim, not a reading.
        let message = if state.busy.is_some() {
            vec![Line::from(Span::styled(
                READING,
                Style::default().fg(theme().text_dim),
            ))]
        } else {
            vec![
                Line::from(Span::styled(
                    EMPTY_STATE,
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    "Tools this conversation ran, and what they cost, appear here.",
                    Style::default().fg(theme().text_dim),
                )),
            ]
        };
        f.render_widget(Paragraph::new(message).block(block), area);
        return;
    }

    let bar_width = bar_width_for(area.width);
    let items: Vec<ListItem> = state
        .entries()
        .iter()
        .map(|entry| match entry {
            Entry::Group { group, collapsed } => {
                group_item(group, *collapsed, state.axis, bar_width)
            }
            Entry::Tool(row) => tool_item(row, &report, bar_width),
        })
        .collect();

    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(theme().list_highlight)
            .fg(theme().list_highlight_fg),
    );
    let mut list_state = ListState::default();
    list_state.select(Some(state.selected));
    f.render_stateful_widget(list, area, &mut list_state);
}

/// How wide the bars are for a list of this width.
///
/// The name column and the figures are paid first, and the bar takes what is
/// left up to [`MAX_BAR_CELLS`]. On a terminal too narrow to hold all three the
/// bar shrinks to nothing and the figures still fit: a figure can be reasoned
/// about with no bar, and a bar cannot be reasoned about with no figure.
fn bar_width_for(area_width: u16) -> usize {
    usize::from(area_width)
        // borders, the two-cell indent, the gap after the name column, and the
        // two gaps around the bar
        .saturating_sub(4 + 2 + NAME_COLUMN + 1 + 2 + FIGURES_COLUMN)
        .min(MAX_BAR_CELLS)
}

/// A server heading: the fold marker, the server, and its subtotals.
///
/// The subtotals stay whether the group is open or folded, so folding a server
/// away never hides what it cost. They start in the same column as the tool
/// rows' figures, so a heading and its rows read down one column.
fn group_item<'a>(
    group: &Group<'_>,
    collapsed: bool,
    axis: Axis,
    bar_width: usize,
) -> ListItem<'a> {
    let marker = if collapsed { "\u{25b8} " } else { "\u{25be} " };
    let heading = drawn_heading(group);
    let mut spans = vec![
        Span::styled(marker, Style::default().fg(theme().hint_sep)),
        Span::styled(
            pad_to_width(&heading, NAME_COLUMN + 1 + bar_width + 2),
            Style::default()
                .fg(theme().title)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            group.subtotal_line(axis),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    if group.namespace.is_none() {
        spans.push(Span::styled(
            format!("  ({UNATTRIBUTED_NOTE})"),
            Style::default().fg(theme().text_dim),
        ));
    }
    ListItem::new(Line::from(spans))
}

/// One tool: its name, its bar, and its figures on the first line; what the
/// figures do not say on the second.
fn tool_item<'a>(row: &ToolUsageView, report: &Report<'_>, bar_width: usize) -> ListItem<'a> {
    let name = drawn_name(row);
    let active = report.axis();
    let tokens = format!("{} tokens", format_count(row.result_tokens));
    let calls = plural_calls(u64::from(row.call_count));
    // The bar encodes the ranked figure, so the ranked figure is what sits
    // beside it. A bar drawn next to some OTHER number reads as a claim about
    // that number, and the two then look anti-correlated at a glance.
    let (ranked, other) = match active {
        Axis::Tokens => (tokens, calls),
        Axis::Calls => (calls, tokens),
    };
    let bar = bar_cells(report.fraction(row), bar_width);
    let mut first = vec![
        Span::raw("  "),
        Span::styled(pad_to_width(&name, NAME_COLUMN), Style::default()),
        Span::raw(" "),
        Span::styled(
            pad_to_width(&bar, bar_width),
            Style::default().fg(theme().assistant_indicator),
        ),
        Span::raw("  "),
        Span::styled(ranked, Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(" \u{b7} ", Style::default().fg(theme().hint_sep)),
        // Both figures on every row: the axis that is not ranking is the one
        // that explains the other.
        Span::styled(other, Style::default().fg(theme().text_dim)),
    ];
    if let Some(tier) = row.tool_tier {
        first.push(Span::styled(
            format!("  [{}]", tier_label(tier)),
            Style::default().fg(if tier.is_gated() {
                theme().warn
            } else {
                theme().text_dim
            }),
        ));
    }

    // The eviction mark leads the second line. A narrow terminal cuts from the
    // right, and a mark placed last is the first thing to go - leaving the
    // resident figure to be read as what the model sees, which for a completed
    // step it is not.
    //
    // It is drawn in the neutral shade, not the warning one. A rising eviction
    // count with steady bytes is the normal shape of long agentic work, so
    // colouring it as a warning would make routine progress read as a problem.
    let mut second = vec![Span::raw("  ")];
    if let Some(note) = eviction_note(row) {
        second.push(Span::styled(
            note,
            Style::default().fg(theme().debug_system),
        ));
        second.push(Span::styled(
            " \u{b7} ",
            Style::default().fg(theme().hint_sep),
        ));
    }
    second.push(Span::styled(
        format!(
            "largest {} \u{b7} resident {}",
            format_bytes(row.max_result_bytes),
            format_bytes(row.result_bytes)
        ),
        Style::default().fg(theme().text_dim),
    ));

    ListItem::new(vec![Line::from(first), Line::from(second)])
}

/// The tool's name as its group shows it. The server prefix is already the
/// heading above, so repeating it on every row buys nothing and costs the
/// column the name needs.
fn display_name(row: &ToolUsageView) -> &str {
    match resolved_namespace(row) {
        Some(namespace) => row
            .tool_name
            .strip_prefix(&format!("{namespace}{NAMESPACE_SEPARATOR}"))
            .unwrap_or(&row.tool_name),
        None => &row.tool_name,
    }
}

/// The name a tool row is drawn under: the server prefix dropped (the heading
/// above already carries it), control characters replaced, and the result cut
/// to the name column.
///
/// A tool name and a server name both come from the server that declared them,
/// so `ESC` and its friends must not reach the terminal. Every drawn name goes
/// through here, so there is one place to check rather than one per call site.
fn drawn_name(row: &ToolUsageView) -> String {
    clamp_name(&sanitize(display_name(row)))
}

/// The heading a group is drawn under, given the same treatment as a row name.
fn drawn_heading(group: &Group<'_>) -> String {
    clamp_name(&sanitize(group.heading()))
}

/// Cut a server-declared name to the column it is drawn in.
fn clamp_name(name: &str) -> String {
    clamp_to_width(name, NAME_COLUMN)
}

/// Terminal cells one character occupies. A width the table does not know
/// counts as one cell, so a name can never measure zero and slip past the cut.
fn char_cells(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c)
        .unwrap_or(0)
        .max(1)
}

/// Terminal cells `text` occupies.
///
/// Characters are the wrong unit for a column. One CJK glyph is one character
/// and two cells, so a name cut and padded by character count takes up to twice
/// the column it was given and pushes the figures beside it off the row; a name
/// of zero-width characters takes far less and leaves the columns ragged.
fn display_width(text: &str) -> usize {
    text.chars().map(char_cells).sum()
}

/// Cut `text` to at most `width` terminal cells, marking the cut.
fn clamp_to_width(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }
    // Below the marker's own width there is no room to say that a cut happened.
    // NAME_COLUMN is far above this; the branch is here so the helper is total.
    let marker = if width >= TRUNCATION_MARKER.len() {
        TRUNCATION_MARKER
    } else {
        ""
    };
    let budget = width - marker.len();
    let mut kept = String::new();
    let mut used = 0;
    for c in text.chars() {
        let cells = char_cells(c);
        if used + cells > budget {
            break;
        }
        kept.push(c);
        used += cells;
    }
    kept.push_str(marker);
    kept
}

/// Pad `text` with spaces to `width` terminal cells, so what follows starts in
/// the same column on every row whatever glyphs the name uses.
fn pad_to_width(text: &str, width: usize) -> String {
    let mut padded = text.to_string();
    for _ in 0..width.saturating_sub(display_width(text)) {
        padded.push(' ');
    }
    padded
}

fn draw_status(f: &mut Frame, state: &State, area: Rect) {
    let Some(busy) = &state.busy else {
        return;
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            busy.clone(),
            Style::default().fg(theme().debug_system),
        )),
        area,
    );
}

fn draw_hints(f: &mut Frame, area: Rect) {
    let hints = [
        ("j/k", "move"),
        ("s", "sort"),
        ("Enter", "fold"),
        ("r", "reread"),
        ("Esc", "close"),
    ];
    let mut spans = Vec::with_capacity(hints.len() * 4);
    for (index, (key, description)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(
                "  \u{b7}  ",
                Style::default().fg(theme().hint_sep),
            ));
        }
        spans.push(Span::styled(
            *key,
            Style::default()
                .fg(theme().hint_key)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            *description,
            Style::default().fg(theme().text_dim),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::tool_usage_view;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

    /// A tool row: name, calls, resident tokens.
    fn tool(name: &str, calls: u32, tokens: u64) -> ToolUsageView {
        ToolUsageView {
            tool_name: name.into(),
            call_count: calls,
            result_tokens: tokens,
            result_bytes: tokens * 4,
            max_result_bytes: tokens * 4,
            ..tool_usage_view()
        }
    }

    /// The call mix the acceptance criteria describe: one chatty tool with tiny
    /// results, one infrequent tool with an enormous one, and a middling third.
    fn mixed_rows() -> Vec<ToolUsageView> {
        vec![
            tool("fileio__read_file", 40, 800),
            tool("web__fetch", 2, 50_000),
            tool("say_this", 5, 300),
        ]
    }

    fn state_with(rows: Vec<ToolUsageView>) -> State {
        State {
            rows,
            ..State::new()
        }
    }

    /// Render `state` off-screen and return the buffer's text.
    fn rendered(state: &State, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        term.draw(|f| draw(f, state)).expect("draw");
        term.backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    /// Render `state` off-screen and return one style per cell, for the
    /// properties that live in colour rather than in text.
    fn rendered_styles(state: &State, w: u16, h: u16) -> Vec<Style> {
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        term.draw(|f| draw(f, state)).expect("draw");
        term.backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.style())
            .collect()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn names(rows: &[&ToolUsageView]) -> Vec<String> {
        rows.iter().map(|r| r.tool_name.clone()).collect()
    }

    // --- Acceptance criteria (adele-tui#132) ---------------------------------

    #[test]
    fn a_known_call_mix_renders_one_row_per_tool_with_its_counts_and_tokens() {
        let state = state_with(mixed_rows());
        let out = rendered(&state, 140, 40);

        for (name, calls, tokens) in [
            ("read_file", "40 calls", "800 tokens"),
            ("fetch", "2 calls", "50,000 tokens"),
            ("say_this", "5 calls", "300 tokens"),
        ] {
            assert!(out.contains(name), "{name} must have a row, got: {out}");
            assert!(out.contains(calls), "{name} must show {calls}, got: {out}");
            assert!(
                out.contains(tokens),
                "{name} must show {tokens}, got: {out}"
            );
        }
        assert_eq!(
            state.report().distinct_tools(),
            3,
            "one row per tool, not one per call"
        );
    }

    #[test]
    fn sorting_by_token_cost_puts_the_infrequent_but_huge_tool_first() {
        let rows = mixed_rows();
        let report = Report::new(&rows, Axis::Tokens);
        assert_eq!(
            names(&report.ranked()),
            vec!["web__fetch", "fileio__read_file", "say_this"],
            "two calls returning 50k tokens must outrank forty returning 800"
        );
    }

    #[test]
    fn sorting_by_call_count_puts_the_chatty_tool_first() {
        let rows = mixed_rows();
        let report = Report::new(&rows, Axis::Calls);
        assert_eq!(
            names(&report.ranked()),
            vec!["fileio__read_file", "say_this", "web__fetch"],
            "forty calls must outrank two, whatever they returned"
        );
    }

    #[test]
    fn namespace_grouping_shows_correct_subtotals() {
        let rows = vec![
            tool("fileio__read_file", 40, 800),
            tool("fileio__write_file", 3, 200),
            tool("web__fetch", 2, 50_000),
        ];
        let report = Report::new(&rows, Axis::Tokens);
        let groups = report.groups();

        assert_eq!(groups.len(), 2, "two servers, two groups");
        assert_eq!(groups[0].namespace, Some("web"), "heaviest group first");
        assert_eq!(groups[0].total_tokens, 50_000);
        assert_eq!(groups[0].total_calls, 2);
        assert_eq!(groups[1].namespace, Some("fileio"));
        assert_eq!(groups[1].total_tokens, 1_000, "800 + 200");
        assert_eq!(groups[1].total_calls, 43, "40 + 3");

        let out = rendered(&state_with(rows), 140, 40);
        assert!(out.contains("fileio"), "the group heading must render");
        assert!(
            out.contains("1,000 tokens"),
            "the fileio subtotal must render, got: {out}"
        );
    }

    #[test]
    fn a_tool_with_evicted_results_gets_a_note_that_claims_neither_direction() {
        let rows = vec![ToolUsageView {
            evicted_results: 3,
            ..tool("web__fetch", 4, 1_000)
        }];
        let note = eviction_note(&rows[0]).expect("a row with evictions carries a note");
        assert!(note.contains('3'), "the note names how many: {note}");
        assert!(
            note.contains("pointer"),
            "the note names what the model now reads: {note}"
        );

        // The count covers two shapes that move the figures in OPPOSITE
        // directions - a completed step keeps every byte, an old compaction
        // lost them - and this view cannot tell them apart. Claiming either
        // direction, or a size, would be a claim the data does not support.
        for forbidden in [
            "under-report",
            "over-report",
            "higher",
            "lower",
            "lost",
            "actual",
        ] {
            assert!(
                !note.contains(forbidden),
                "the note must not claim what it cannot know ({forbidden}): {note}"
            );
        }

        assert!(
            eviction_note(&tool("web__fetch", 4, 1_000)).is_none(),
            "a row with no evictions carries no note"
        );

        let out = rendered(&state_with(rows), 140, 40);
        assert!(
            out.contains("3 results now read as a pointer"),
            "the mark must be visible on the row, got: {out}"
        );
    }

    /// A rising eviction count with steady bytes is the normal shape of long
    /// agentic work, so the mark must not be dressed as a problem.
    #[test]
    fn the_eviction_mark_is_not_drawn_as_a_warning() {
        let evicted = ToolUsageView {
            evicted_results: 3,
            ..tool("web__fetch", 4, 1_000)
        };
        let warn = theme().warn;
        let styles = rendered_styles(&state_with(vec![evicted]), 140, 40);
        let plain = rendered_styles(&state_with(vec![tool("web__fetch", 4, 1_000)]), 140, 40);
        assert_eq!(
            styles.iter().filter(|s| s.fg == Some(warn)).count(),
            plain.iter().filter(|s| s.fg == Some(warn)).count(),
            "an evicted row must not add warning-coloured cells"
        );
    }

    /// A narrow terminal cuts from the right, so the mark is placed where the
    /// cut cannot reach it first. Checking only the widest case would pin the
    /// favourable end of the range.
    #[test]
    fn the_eviction_mark_survives_a_narrow_terminal() {
        // The byte figures are at their widest, so the line is at its longest
        // and the narrow widths below actually discriminate. Testing only a
        // roomy terminal would pin the favourable end of the range.
        let rows = vec![ToolUsageView {
            evicted_results: 3,
            max_result_bytes: 1_048_524,
            result_bytes: 1_073_213_337,
            ..tool("web__fetch", 4, 1_000)
        }];
        let state = state_with(rows);
        for width in [44, 60, 80, 140] {
            let out = rendered(&state, width, 40);
            assert!(
                out.contains("3 results"),
                "at {width} columns the reader must still be told the model \
                 reads these as a pointer, got: {out}"
            );
        }
    }

    #[test]
    fn an_empty_conversation_shows_the_empty_state_not_an_error() {
        let state = state_with(Vec::new());
        let out = rendered(&state, 140, 40);
        assert!(
            out.contains(EMPTY_STATE),
            "an empty report reads as no tool calls, got: {out}"
        );
        assert!(
            state.error.is_none(),
            "an empty report is not an error condition"
        );
        assert!(
            !out.to_lowercase().contains("error"),
            "an empty report must not read as broken, got: {out}"
        );
    }

    // --- What the ticket asks the view to render -----------------------------

    /// Requirement 2 is a RENDERED property. Checking `fraction` and
    /// `bar_cells` as pure functions leaves the draw path free to omit the bar
    /// entirely, so this one renders and reads the glyphs back.
    #[test]
    fn a_bar_is_drawn_on_every_row_and_the_heaviest_row_draws_the_longest() {
        let out = rendered(&state_with(mixed_rows()), 140, 40);
        assert!(
            out.contains(BAR_FULL),
            "the rows must actually carry bars, got: {out}"
        );

        // Cells, not bytes: a bar glyph is three bytes, and a light row draws a
        // partial cell rather than a whole one.
        let is_bar = |c: char| c == BAR_FULL || BAR_EIGHTHS.contains(&c);
        let runs: Vec<usize> = out
            .split(|c| !is_bar(c))
            .map(|run| run.chars().count())
            .filter(|cells| *cells > 0)
            .collect();
        assert_eq!(runs.len(), 3, "one bar per tool row, got {runs:?}");
        let heaviest = *runs.iter().max().expect("three bars");
        let lightest = *runs.iter().min().expect("three bars");
        assert_eq!(heaviest, MAX_BAR_CELLS, "the peak row fills the bar column");
        assert!(
            lightest < heaviest,
            "and a lighter row draws a shorter one, got {runs:?}"
        );
    }

    /// The bar and the number it encodes must sit together. A bar drawn beside
    /// some other figure reads as a claim about that figure.
    #[test]
    fn the_ranked_figure_sits_beside_the_bar_that_encodes_it() {
        let rows = vec![ToolUsageView {
            max_result_bytes: 1_024,
            ..tool("fileio__read_file", 10, 2_560)
        }];
        let out = rendered(&state_with(rows), 140, 40);
        let bar_end = out.rfind(BAR_FULL).expect("a bar is drawn") + BAR_FULL.len_utf8();
        let after: String = out[bar_end..].chars().take(24).collect();
        assert!(
            after.trim_start().starts_with("2,560 tokens"),
            "the ranked figure must follow the bar, found {after:?}"
        );
    }

    #[test]
    fn bars_are_proportional_to_the_sorted_axis() {
        let rows = mixed_rows();
        let report = Report::new(&rows, Axis::Tokens);
        let heaviest = report.ranked()[0];
        let lightest = report.ranked()[2];
        assert!((report.fraction(heaviest) - 1.0).abs() < f64::EPSILON);
        assert!((report.fraction(lightest) - 300.0 / 50_000.0).abs() < 1e-9);
        assert!(
            bar_cells(report.fraction(heaviest), 20).chars().count()
                > bar_cells(report.fraction(lightest), 20).chars().count(),
            "a heavier row draws a longer bar"
        );
    }

    #[test]
    fn switching_the_axis_re_ranks_the_bars() {
        let rows = mixed_rows();
        let chatty = &rows[0];
        let by_tokens = Report::new(&rows, Axis::Tokens).fraction(chatty);
        let by_calls = Report::new(&rows, Axis::Calls).fraction(chatty);
        assert!(by_tokens < 0.1, "the chatty tool is small by tokens");
        assert!(
            (by_calls - 1.0).abs() < f64::EPSILON,
            "and is the peak by calls, so its bar must fill"
        );
    }

    #[test]
    fn the_largest_single_result_is_shown_on_every_row() {
        let rows = vec![ToolUsageView {
            max_result_bytes: 262_144,
            result_bytes: 300_000,
            ..tool("web__fetch", 4, 1_000)
        }];
        let out = rendered(&state_with(rows), 140, 40);
        assert!(
            out.contains("largest 256.0 KiB"),
            "one enormous dump must be distinguishable from a steady trickle, got: {out}"
        );
    }

    #[test]
    fn header_totals_report_distinct_tools_total_calls_and_total_tokens() {
        let rows = mixed_rows();
        let report = Report::new(&rows, Axis::Tokens);
        assert_eq!(report.distinct_tools(), 3);
        assert_eq!(report.total_calls(), 47);
        assert_eq!(report.total_tokens(), 51_100);

        let out = rendered(&state_with(rows), 140, 40);
        for expected in ["3 tools", "47 calls", "51,100 tokens"] {
            assert!(out.contains(expected), "header must show {expected}: {out}");
        }
    }

    #[test]
    fn a_group_folds_away_its_rows_and_keeps_its_subtotal() {
        let mut state = state_with(vec![
            tool("fileio__read_file", 40, 800),
            tool("fileio__write_file", 3, 200),
            tool("web__fetch", 2, 50_000),
        ]);
        // Heaviest group first, so the selection starts on the `web` heading.
        assert_eq!(state.selected, 0);
        assert_eq!(state.entries().len(), 5, "two headings plus three tools");

        state.handle_key(key(KeyCode::Enter));
        assert_eq!(
            state.entries().len(),
            4,
            "folding `web` removes its one tool row and nothing else"
        );
        assert!(state.is_collapsed(Some("web")));

        let out = rendered(&state, 140, 40);
        assert!(
            !out.contains("fetch"),
            "a folded group hides its tools, got: {out}"
        );
        assert!(
            out.contains("50,000 tokens"),
            "and keeps its subtotal, so folding never hides what it cost, got: {out}"
        );
    }

    // --- Honest reading of what the daemon did not say -----------------------

    #[test]
    fn a_tool_with_no_resolvable_server_reads_as_unattributed_not_as_a_named_server() {
        let rows = vec![tool("say_this", 5, 300)];
        assert_eq!(resolved_namespace(&rows[0]), None);
        let report = Report::new(&rows, Axis::Tokens);
        assert_eq!(report.groups()[0].namespace, None);
        assert_eq!(report.groups()[0].heading(), UNATTRIBUTED_HEADING);

        let out = rendered(&state_with(rows), 140, 40);
        assert!(out.contains(UNATTRIBUTED_HEADING));
        assert!(
            out.contains(UNATTRIBUTED_NOTE),
            "the heading must read as an absence, not as a server, got: {out}"
        );
        assert!(
            !out.to_lowercase().contains("unknown"),
            "never a server called `unknown`, which does not exist, got: {out}"
        );
    }

    #[test]
    fn a_namespace_is_recovered_from_the_tool_name_when_the_daemon_omits_it() {
        let row = tool("fileio__read_file", 1, 1);
        assert_eq!(
            resolved_namespace(&row),
            Some("fileio"),
            "the daemon's own `<namespace>__<tool>` encoding is not a guess"
        );
    }

    /// The daemon splits the tool off the END of the name. A server name may
    /// itself carry the separator, and splitting at the first one would file
    /// its tools under a server that does not exist and merge its subtotals
    /// with every other server sharing that first segment.
    #[test]
    fn a_server_name_that_carries_the_separator_is_kept_whole() {
        let row = tool("home__assistant__get_state", 1, 1);
        assert_eq!(resolved_namespace(&row), Some("home__assistant"));
        assert_eq!(
            display_name(&row),
            "get_state",
            "and the row shows only the tool part"
        );
    }

    #[test]
    fn the_daemon_reported_namespace_wins_over_the_encoded_one() {
        let row = ToolUsageView {
            namespace: Some("calendar".into()),
            ..tool("fileio__read_file", 1, 1)
        };
        assert_eq!(resolved_namespace(&row), Some("calendar"));
    }

    #[test]
    fn an_empty_namespace_field_does_not_become_an_empty_heading() {
        let row = ToolUsageView {
            namespace: Some(String::new()),
            ..tool("say_this", 1, 1)
        };
        assert_eq!(resolved_namespace(&row), None);
    }

    #[test]
    fn a_leading_separator_in_a_tool_name_does_not_become_an_empty_heading() {
        let row = tool("__read_file", 1, 1);
        assert_eq!(resolved_namespace(&row), None);
    }

    #[test]
    fn bars_are_empty_when_every_row_measures_zero() {
        let rows = vec![tool("a__x", 0, 0), tool("a__y", 0, 0)];
        let report = Report::new(&rows, Axis::Tokens);
        assert_eq!(
            report.fraction(&rows[0]),
            0.0,
            "dividing by nothing must not read as everything"
        );
        assert_eq!(bar_cells(0.0, 20), "");
    }

    #[test]
    fn a_small_but_non_zero_row_still_draws_a_bar() {
        assert!(
            !bar_cells(0.001, 20).is_empty(),
            "a real cost must never render as nothing"
        );
    }

    #[test]
    fn equal_rows_rank_in_a_stable_order() {
        let rows = vec![tool("z__b", 3, 100), tool("a__a", 3, 100)];
        let ranked = names(&Report::new(&rows, Axis::Tokens).ranked());
        assert_eq!(
            ranked,
            vec!["a__a", "z__b"],
            "the same figures must list in the same order every time"
        );
    }

    #[test]
    fn an_unattributed_group_and_a_server_group_do_not_share_a_fold_key() {
        let mut state = state_with(vec![tool("web__fetch", 1, 100), tool("say_this", 1, 50)]);
        state.handle_key(key(KeyCode::Enter));
        assert!(state.is_collapsed(Some("web")));
        assert!(
            !state.is_collapsed(None),
            "folding a server must not fold the unattributed group with it"
        );
    }

    /// A tool name and a server name are declared by the server, so a hostile
    /// one must not carry `ESC` onto the draw path.
    ///
    /// The assertion is against the strings this module builds, not against the
    /// rendered buffer. A buffer-only assertion was measured against a build
    /// with the sanitizer removed and still passed, so it does not hold this
    /// property: the buffer swallows what it cannot place, and cannot tell a
    /// sanitized name from an unsanitized one. The buffer check below is kept
    /// as a second, weaker guard.
    #[test]
    fn a_hostile_tool_name_is_sanitized_before_it_reaches_the_draw_path() {
        let rows = vec![tool("evil\u{1b}[2Jbox__wipe\u{1b}c", 1, 10)];
        let report = Report::new(&rows, Axis::Tokens);
        let groups = report.groups();

        let heading = drawn_heading(&groups[0]);
        assert!(
            !heading.chars().any(char::is_control),
            "a server name reaches the draw path sanitized, got {heading:?}"
        );
        let name = drawn_name(&rows[0]);
        assert!(
            !name.chars().any(char::is_control),
            "a tool name reaches the draw path sanitized, got {name:?}"
        );
        assert!(
            !rendered(&state_with(rows), 140, 40).contains('\u{1b}'),
            "and nothing puts it back on the way to the terminal"
        );
    }

    #[test]
    fn an_over_long_name_is_cut_to_the_column_it_is_drawn_in() {
        let long = "x".repeat(200);
        let row = tool(&long, 1, 1);
        assert!(
            display_width(&drawn_name(&row)) <= NAME_COLUMN,
            "one absurd name must not push the figures off the row"
        );
    }

    /// The column is terminal cells, and a character is the wrong unit for it:
    /// one CJK glyph is one character and two cells, so a name cut by character
    /// count occupies twice the column it was given and pushes the figures
    /// beside it off the row.
    #[test]
    fn a_wide_glyph_name_is_cut_by_terminal_cells_not_by_character_count() {
        let wide = "\u{8bfb}".repeat(40);
        assert_eq!(wide.chars().count(), 40, "40 characters");
        assert_eq!(display_width(&wide), 80, "but 80 terminal cells");

        let drawn = drawn_name(&tool(&wide, 1, 1));
        assert!(
            display_width(&drawn) <= NAME_COLUMN,
            "cut at the column it is drawn in, got {} cells",
            display_width(&drawn)
        );
    }

    #[test]
    fn every_name_is_padded_to_the_same_column_whatever_glyphs_it_uses() {
        assert_eq!(
            display_width(&pad_to_width("read_file", NAME_COLUMN)),
            NAME_COLUMN
        );
        assert_eq!(
            display_width(&pad_to_width("\u{8bfb}\u{53d6}", NAME_COLUMN)),
            NAME_COLUMN,
            "a wide name must not shift the figures beside it"
        );
        assert_eq!(
            display_width(&pad_to_width("read\u{200b}\u{200b}file", NAME_COLUMN)),
            NAME_COLUMN,
            "and neither must a zero-width one"
        );
    }

    /// The bar gives up its width to the figures, never the other way round.
    #[test]
    fn the_bar_yields_its_width_to_the_figures_on_a_narrow_terminal() {
        assert_eq!(
            bar_width_for(140),
            MAX_BAR_CELLS,
            "a wide list draws a full bar"
        );
        assert!(bar_width_for(100) > 0);
        assert_eq!(
            bar_width_for(50),
            0,
            "too narrow for all three, so the bar goes and the figures stay"
        );
    }

    #[test]
    fn a_read_failure_is_shown_as_an_error_not_as_an_empty_conversation() {
        let state = State {
            error: Some("daemon said no".into()),
            ..State::new()
        };
        let out = rendered(&state, 140, 40);
        assert!(out.contains("daemon said no"), "got: {out}");
        assert!(
            !out.contains(EMPTY_STATE),
            "a failure to ask must not read as a quiet zero, got: {out}"
        );
    }

    // --- Keys ----------------------------------------------------------------

    /// Ranking rows in a `Report` built by hand proves nothing about what the
    /// user sees. This drives the switch through `State` and renders it, so
    /// pinning the drawn report to one axis cannot pass.
    #[test]
    fn switching_the_axis_re_ranks_the_drawn_list_and_its_bars() {
        let mut state = state_with(mixed_rows());
        state.handle_key(key(KeyCode::Char('s')));

        let report = state.report();
        assert_eq!(
            report.axis(),
            Axis::Calls,
            "the drawn report follows the switch"
        );

        let entries = state.entries();
        let Entry::Tool(first) = entries[1] else {
            panic!("a tool is drawn under its own heading")
        };
        assert_eq!(
            first.tool_name, "fileio__read_file",
            "the chatty tool leads the list once calls rank it"
        );
        assert!(
            (report.fraction(first) - 1.0).abs() < f64::EPSILON,
            "and its bar fills, because it is now the peak"
        );

        let out = rendered(&state, 140, 40);
        assert!(
            out.contains("Call count"),
            "the header names the axis the list is ranked by, got: {out}"
        );
    }

    #[test]
    fn the_unattributed_group_folds_like_any_other() {
        let mut state = state_with(vec![
            tool("say_this", 5, 300),
            tool("web__fetch", 2, 50_000),
        ]);
        // Ranked by tokens `web` leads, so the unattributed heading is third.
        state.handle_key(key(KeyCode::Char('j')));
        state.handle_key(key(KeyCode::Char('j')));
        assert!(matches!(state.entries()[2], Entry::Group { .. }));

        state.handle_key(key(KeyCode::Enter));
        assert!(
            state.is_collapsed(None),
            "the group the daemon could not attribute must fold like a named one"
        );

        let out = rendered(&state, 140, 40);
        assert!(!out.contains("say_this"), "its rows fold away, got: {out}");
        assert!(
            out.contains("300 tokens"),
            "and its subtotal stays, got: {out}"
        );
    }

    #[test]
    fn a_folded_group_keeps_both_of_its_figures() {
        let mut state = state_with(vec![tool("web__fetch", 2, 50_000)]);
        state.handle_key(key(KeyCode::Char('s')));
        state.handle_key(key(KeyCode::Enter));
        let out = rendered(&state, 140, 40);
        assert!(out.contains("2 calls"), "the ranked figure, got: {out}");
        assert!(
            out.contains("50,000 tokens"),
            "and the one it is not ranked on, which folding would otherwise \
             hide with its rows, got: {out}"
        );
    }

    #[test]
    fn a_reread_clears_the_error_from_the_previous_attempt() {
        let mut state = State {
            error: Some("daemon said no".into()),
            ..State::new()
        };
        state.begin_read();
        assert!(
            state.error.is_none(),
            "a live request must not sit beside a stale failure"
        );
        assert!(state.busy.is_some());
    }

    /// A tool name is declared by a third-party server and nothing on the wire
    /// bounds it. The view sorts on these strings and rebuilds every row on
    /// every draw and every key press, so the bound is applied once, at ingest.
    #[test]
    fn an_unbounded_server_declared_name_is_cut_when_it_is_stored() {
        let mut state = State::new();
        state.install_rows(vec![ToolUsageView {
            namespace: Some("n".repeat(100_000)),
            ..tool(&"x".repeat(1_000_000), 1, 1)
        }]);
        assert!(state.rows[0].tool_name.chars().count() <= MAX_STORED_NAME_CHARS);
        assert!(
            state.rows[0]
                .namespace
                .as_ref()
                .is_some_and(|n| n.chars().count() <= MAX_STORED_NAME_CHARS)
        );
    }

    #[test]
    fn s_switches_the_sort_axis() {
        let mut state = state_with(mixed_rows());
        assert_eq!(
            state.axis,
            Axis::Tokens,
            "token cost is the default reading"
        );
        state.handle_key(key(KeyCode::Char('s')));
        assert_eq!(state.axis, Axis::Calls);
        state.handle_key(key(KeyCode::Char('s')));
        assert_eq!(state.axis, Axis::Tokens);
    }

    #[test]
    fn esc_and_q_close_the_view() {
        let mut state = state_with(mixed_rows());
        state.handle_key(key(KeyCode::Esc));
        assert!(state.closing);

        let mut state = state_with(mixed_rows());
        state.handle_key(key(KeyCode::Char('q')));
        assert!(state.closing);
    }

    #[test]
    fn r_asks_the_caller_for_a_refresh() {
        let mut state = state_with(mixed_rows());
        assert_eq!(
            state.handle_key(key(KeyCode::Char('r'))),
            Some(Effect::Refresh)
        );
        assert_eq!(state.handle_key(key(KeyCode::Char('j'))), None);
    }

    #[test]
    fn j_and_k_walk_the_drawn_list_and_stop_at_its_ends() {
        let mut state = state_with(mixed_rows());
        let last = state.entries().len() - 1;
        for _ in 0..last + 5 {
            state.handle_key(key(KeyCode::Char('j')));
        }
        assert_eq!(
            state.selected, last,
            "the selection stops at the last entry"
        );
        for _ in 0..last + 5 {
            state.handle_key(key(KeyCode::Char('k')));
        }
        assert_eq!(state.selected, 0, "and at the first");
    }

    #[test]
    fn folding_from_a_tool_row_folds_its_own_group_and_selects_the_heading() {
        let mut state = state_with(vec![
            tool("fileio__read_file", 40, 800),
            tool("fileio__write_file", 3, 200),
        ]);
        state.handle_key(key(KeyCode::Char('j')));
        assert!(matches!(state.entries()[1], Entry::Tool(_)));
        state.handle_key(key(KeyCode::Char(' ')));
        assert!(state.is_collapsed(Some("fileio")));
        assert_eq!(
            state.selected, 0,
            "the selection follows the fold up to the heading it left standing"
        );
    }

    #[test]
    fn the_selection_stays_inside_the_list_when_a_group_folds() {
        let mut state = state_with(vec![
            tool("web__fetch", 2, 50_000),
            tool("fileio__read_file", 40, 800),
        ]);
        // Walk to the last entry, then fold the group above it.
        let last = state.entries().len() - 1;
        for _ in 0..last {
            state.handle_key(key(KeyCode::Char('j')));
        }
        state.handle_key(key(KeyCode::Enter));
        assert!(
            state.selected < state.entries().len(),
            "a fold must never leave the selection off the end of the list"
        );
    }

    // --- Formatting ----------------------------------------------------------

    #[test]
    fn counts_carry_thousands_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(51_100), "51,100");
        assert_eq!(format_count(1_234_567), "1,234,567");
    }

    #[test]
    fn bytes_carry_binary_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1_023), "1023 B");
        assert_eq!(format_bytes(1_024), "1.0 KiB");
        assert_eq!(format_bytes(262_144), "256.0 KiB");
        assert_eq!(format_bytes(1_048_576), "1.0 MiB");
        assert_eq!(format_bytes(1_073_741_824), "1.0 GiB");
    }

    /// Rounding to one decimal can carry a value up into the next unit. Picking
    /// the unit before that happens prints a figure in the wrong one.
    #[test]
    fn a_byte_figure_is_never_printed_in_the_unit_below_it() {
        assert_eq!(format_bytes(1_048_575), "1.0 MiB", "not 1024.0 KiB");
        assert_eq!(format_bytes(1_073_741_823), "1.0 GiB", "not 1024.0 MiB");
        assert_eq!(
            format_bytes(1_048_524),
            "1023.9 KiB",
            "just below the carry"
        );
    }

    #[test]
    fn a_gated_tier_is_labelled_in_a_word_a_row_can_carry() {
        assert_eq!(tier_label(ToolTier::Egress), "network");
        assert_eq!(tier_label(ToolTier::Execution), "execute");
        assert_eq!(tier_label(ToolTier::Read), "read");
        assert!(ToolTier::Egress.is_gated());
        assert!(!ToolTier::Read.is_gated());
    }

    #[test]
    fn a_tier_is_rendered_on_the_row_that_carries_one() {
        let rows = vec![ToolUsageView {
            tool_tier: Some(ToolTier::Egress),
            ..tool("web__fetch", 1, 10)
        }];
        let out = rendered(&state_with(rows), 140, 40);
        assert!(out.contains("network"), "got: {out}");
    }
}
