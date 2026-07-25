//! The global-settings screen (#135).
//!
//! A modal list of the TUI's **global** preferences — the ones persisted to
//! `settings.json` — so they are visible, self-describing, and changeable
//! without knowing a chord. Before this, every preference was a hotkey you had
//! to already know (`Ctrl+T`, `Ctrl+O`), which does not scale: each new setting
//! cost another binding in an already-crowded keymap.
//!
//! Deliberately scoped to **global** settings. Per-conversation controls (voice
//! input, Adele output level) are keyed by conversation id and stay as on-the-fly
//! bindings — showing them here would imply they apply everywhere.
//!
//! The state machine below is pure: no terminal, no IO. [`State`] is what the
//! tests drive; the [`Screen`] impl is a thin shell over it.

use std::io;

use crossterm::event::{KeyCode, KeyEvent};
use desktop_assistant_client_common::SignalEvent;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::screen::Screen;
use crate::settings::Settings;
use crate::theme::theme;

/// Which global setting a row edits.
///
/// Adding a setting means adding a variant plus its entry in [`ALL`] — the label,
/// help text, getter and setter all live in one place, so a new preference costs
/// no keybinding and no screen surgery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingId {
    /// Render tool/system/empty-assistant messages dimly inline.
    ShowDebug,
    /// Share device context (name, username, home, hostname, timezone, OS).
    ShareClientContext,
}

/// Every setting the screen shows, in display order.
pub const ALL: &[SettingId] = &[SettingId::ShowDebug, SettingId::ShareClientContext];

impl SettingId {
    /// Short label shown in the list.
    pub fn label(self) -> &'static str {
        match self {
            Self::ShowDebug => "Show debug messages",
            Self::ShareClientContext => "Share device info",
        }
    }

    /// One-line explanation. This is the whole point of the screen over a
    /// keybinding: `Ctrl+O` could never convey what it actually shares.
    pub fn help(self) -> &'static str {
        match self {
            Self::ShowDebug => {
                "Show tool, system and empty assistant messages inline, dimmed, \
                 instead of hiding them."
            }
            Self::ShareClientContext => {
                "Send your real name, username, home folder, hostname, timezone \
                 and OS to the assistant so it can personalise replies."
            }
        }
    }

    /// Read this setting's current value.
    pub fn get(self, settings: &Settings) -> bool {
        match self {
            Self::ShowDebug => settings.show_debug,
            Self::ShareClientContext => settings.share_client_context,
        }
    }

    /// Write this setting's value.
    pub fn set(self, settings: &mut Settings, value: bool) {
        match self {
            Self::ShowDebug => settings.show_debug = value,
            Self::ShareClientContext => settings.share_client_context = value,
        }
    }
}

/// What the screen hands back when it closes.
///
/// `Default` is `Unchanged` because [`crate::screen::run_screen`] requires it:
/// if the driver ever has to bail without the screen settling (a terminal error),
/// the safe reading is "the user changed nothing", which skips the write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Outcome {
    /// The user changed something; the caller should apply + persist these.
    Changed(Settings),
    /// Nothing changed — the caller can skip the write entirely.
    #[default]
    Unchanged,
}

/// Pure screen state: the working copy of the settings, the cursor, and whether
/// anything has actually changed.
#[derive(Debug, Clone)]
pub struct State {
    settings: Settings,
    original: Settings,
    selected: usize,
    closing: bool,
}

impl State {
    /// Open the screen over a copy of the current settings.
    pub fn new(settings: Settings) -> Self {
        Self {
            original: settings.clone(),
            settings,
            selected: 0,
            closing: false,
        }
    }

    /// Index of the highlighted row.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The working settings, reflecting any toggles made so far.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Whether the working copy differs from what the screen opened with. Drives
    /// [`Outcome`], so an open-and-escape never rewrites `settings.json`.
    pub fn is_dirty(&self) -> bool {
        self.settings != self.original
    }

    /// Current value of the row at `index`, or `None` when out of range.
    pub fn value_at(&self, index: usize) -> Option<bool> {
        ALL.get(index).map(|id| id.get(&self.settings))
    }

    /// Move the cursor down one row, stopping at the last (no wrap: a list this
    /// short reads better clamped than cycling).
    pub fn move_down(&mut self) {
        if self.selected + 1 < ALL.len() {
            self.selected += 1;
        }
    }

    /// Move the cursor up one row, stopping at the first.
    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Flip the highlighted setting.
    pub fn toggle_selected(&mut self) {
        let Some(&id) = ALL.get(self.selected) else {
            return;
        };
        let next = !id.get(&self.settings);
        id.set(&mut self.settings, next);
    }

    /// Ask the screen to close.
    pub fn close(&mut self) {
        self.closing = true;
    }

    /// The outcome once closed, or `None` while still open.
    pub fn take_outcome(&mut self) -> Option<Outcome> {
        if !self.closing {
            return None;
        }
        Some(if self.is_dirty() {
            Outcome::Changed(self.settings.clone())
        } else {
            Outcome::Unchanged
        })
    }
}

/// Apply one key press to the screen state.
///
/// Pure and synchronous — no RPC, because every setting here is client-local.
/// Split out from the [`Screen`] impl so the tests drive real key events rather
/// than the state API.
pub fn handle_key(state: &mut State, key: KeyEvent) {
    match key.code {
        KeyCode::Down | KeyCode::Char('j') => state.move_down(),
        KeyCode::Up | KeyCode::Char('k') => state.move_up(),
        // Both, because muscle memory splits: Space reads as "tick a checkbox",
        // Enter as "activate the thing".
        KeyCode::Char(' ') | KeyCode::Enter => state.toggle_selected(),
        KeyCode::Esc | KeyCode::Char('q') => state.close(),
        _ => {}
    }
}

/// The settings screen as a [`Screen`]. No client and no `pending`: unlike the
/// other screens this one issues no RPCs, so it never needs the off-loop
/// machinery.
struct SettingsScreen {
    state: State,
}

impl Screen for SettingsScreen {
    type Outcome = Outcome;

    fn draw(&mut self, frame: &mut Frame) {
        draw(frame, &self.state);
    }

    fn handle_key(&mut self, key: KeyEvent) -> impl std::future::Future<Output = ()> {
        handle_key(&mut self.state, key);
        std::future::ready(())
    }

    fn take_outcome(&mut self) -> Option<Outcome> {
        State::take_outcome(&mut self.state)
    }
}

/// Run the settings screen until the user closes it.
///
/// Takes the current settings by value and returns whether they changed; the
/// caller owns persisting the result (there is one write path, in the run loop).
pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    settings: Settings,
    signal_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SignalEvent>,
    sink: &mut impl crate::screen::SignalSink,
) -> anyhow::Result<Outcome> {
    let mut screen = SettingsScreen {
        state: State::new(settings),
    };
    crate::screen::run_screen(terminal, &mut screen, signal_rx, sink).await
}

fn draw(f: &mut Frame, state: &State) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            // Help for the highlighted row: the reason this screen beats a
            // keybinding, so it gets fixed space rather than whatever is left.
            Constraint::Length(5),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_header(f, chunks[0]);
    draw_list(f, state, chunks[1]);
    draw_help(f, state, chunks[2]);
    draw_footer(f, chunks[3]);
}

fn draw_header(f: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme().border));
    let text = Line::from(vec![Span::styled(
        "Settings",
        Style::default()
            .fg(theme().title)
            .add_modifier(Modifier::BOLD),
    )]);
    f.render_widget(Paragraph::new(text).block(block), area);
}

fn draw_list(f: &mut Frame, state: &State, area: Rect) {
    let label_w = ALL.iter().map(|s| s.label().len()).max().unwrap_or(0);
    let items: Vec<ListItem> = ALL
        .iter()
        .map(|&id| {
            let on = id.get(state.settings());
            // A filled/empty box reads at a glance; the on/off word disambiguates
            // it for anyone whose terminal renders the glyphs poorly.
            let (mark, word, style) = if on {
                (
                    "[x]",
                    "on",
                    Style::default()
                        .fg(theme().pinned)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("[ ]", "off", Style::default().fg(theme().text_dim))
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{mark} "), style),
                Span::styled(
                    format!("{:<width$}", id.label(), width = label_w),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled("   ", Style::default()),
                Span::styled(word, style),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme().border)),
        )
        .highlight_style(
            Style::default()
                .bg(theme().list_highlight)
                .fg(theme().list_highlight_fg)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");

    let mut list_state = ListState::default();
    list_state.select(Some(state.selected()));
    f.render_stateful_widget(list, area, &mut list_state);
}

fn draw_help(f: &mut Frame, state: &State, area: Rect) {
    let help = ALL.get(state.selected()).map(|id| id.help()).unwrap_or("");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme().border))
        .title(Line::from(Span::styled(
            "What this does",
            Style::default().fg(theme().title),
        )));
    f.render_widget(
        Paragraph::new(help)
            .style(Style::default().fg(theme().text_dim))
            .wrap(Wrap { trim: true })
            .block(block),
        area,
    );
}

fn draw_footer(f: &mut Frame, area: Rect) {
    let hints = [("↑/↓", "move"), ("Space/Enter", "toggle"), ("Esc", "close")];
    let mut spans = Vec::new();
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ·  ", Style::default().fg(theme().hint_sep)));
        }
        spans.push(Span::styled(*key, Style::default().fg(theme().hint_key)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*desc, Style::default().fg(theme().text_dim)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn state() -> State {
        State::new(Settings::default())
    }

    /// Every setting in `ALL` renders with a non-empty label and help string.
    /// The help text is the screen's entire reason to exist over a hotkey, so an
    /// unexplained setting is a defect, not a cosmetic gap.
    #[test]
    fn every_setting_has_a_label_and_help_text() {
        for &id in ALL {
            assert!(!id.label().is_empty(), "{id:?} has no label");
            assert!(
                id.help().len() > 20,
                "{id:?} help text is too thin to explain the setting: {:?}",
                id.help()
            );
        }
    }

    /// Toggling flips only the highlighted setting, and leaves its neighbours
    /// alone — the obvious off-by-one this screen could have.
    #[test]
    fn toggle_flips_only_the_selected_row() {
        let mut s = state();
        let before: Vec<bool> = (0..ALL.len()).map(|i| s.value_at(i).unwrap()).collect();

        s.toggle_selected();

        assert_eq!(
            s.value_at(0).unwrap(),
            !before[0],
            "the selected row must flip"
        );
        for (i, &was) in before.iter().enumerate().skip(1) {
            assert_eq!(
                s.value_at(i).unwrap(),
                was,
                "row {i} must be untouched when row 0 is toggled"
            );
        }
    }

    /// The cursor clamps at both ends rather than wrapping or running past the
    /// list (which would panic on `value_at` or silently toggle nothing).
    #[test]
    fn cursor_clamps_at_both_ends() {
        let mut s = state();
        s.move_up();
        assert_eq!(s.selected(), 0, "up from the first row stays at the first");

        for _ in 0..ALL.len() * 2 {
            s.move_down();
        }
        assert_eq!(
            s.selected(),
            ALL.len() - 1,
            "down past the end stays on the last row"
        );
        assert!(
            s.value_at(s.selected()).is_some(),
            "the clamped cursor must still address a real row"
        );
    }

    /// Opening and closing without touching anything reports `Unchanged`, so the
    /// caller skips the file write. A screen that always saved would rewrite
    /// settings.json on every glance.
    #[test]
    fn untouched_screen_closes_unchanged() {
        let mut s = state();
        s.close();
        assert_eq!(s.take_outcome(), Some(Outcome::Unchanged));
    }

    /// A real change is reported with the updated settings.
    #[test]
    fn changed_screen_returns_the_new_settings() {
        let mut s = state();
        let before = s.settings().show_debug;
        s.toggle_selected();
        s.close();

        match s.take_outcome() {
            Some(Outcome::Changed(settings)) => {
                assert_eq!(settings.show_debug, !before, "the flip must be carried out");
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    /// Toggling a setting and toggling it back is not a change — the outcome is
    /// value-based, not "did the user press anything".
    #[test]
    fn toggling_back_to_the_original_is_not_a_change() {
        let mut s = state();
        s.toggle_selected();
        s.toggle_selected();
        s.close();
        assert_eq!(
            s.take_outcome(),
            Some(Outcome::Unchanged),
            "a net-zero edit must not trigger a save"
        );
    }

    /// While open, there is no outcome — the driver keeps looping.
    #[test]
    fn no_outcome_until_closed() {
        let mut s = state();
        s.toggle_selected();
        assert_eq!(s.take_outcome(), None, "an open screen has no outcome");
    }

    /// Every setting is individually reachable and toggleable, so none is
    /// stranded by a cursor that cannot reach it.
    #[test]
    fn every_setting_is_reachable_and_toggleable() {
        for target in 0..ALL.len() {
            let mut s = state();
            for _ in 0..target {
                s.move_down();
            }
            assert_eq!(s.selected(), target);
            let before = s.value_at(target).unwrap();
            s.toggle_selected();
            assert_eq!(
                s.value_at(target).unwrap(),
                !before,
                "row {target} must be toggleable"
            );
        }
    }

    /// Both toggle keys work. Space and Enter are both bound because muscle
    /// memory splits between "tick the checkbox" and "activate the row"; a
    /// screen that honoured only one would feel broken to half its users.
    #[test]
    fn space_and_enter_both_toggle() {
        for code in [KeyCode::Char(' '), KeyCode::Enter] {
            let mut s = state();
            let before = s.value_at(0).unwrap();
            handle_key(&mut s, key(code));
            assert_eq!(
                s.value_at(0).unwrap(),
                !before,
                "{code:?} must toggle the selected row"
            );
        }
    }

    /// Arrow keys and vim keys both navigate, matching the rest of the TUI.
    #[test]
    fn arrows_and_vim_keys_both_navigate() {
        for (down, up) in [
            (KeyCode::Down, KeyCode::Up),
            (KeyCode::Char('j'), KeyCode::Char('k')),
        ] {
            let mut s = state();
            handle_key(&mut s, key(down));
            assert_eq!(s.selected(), 1, "{down:?} must move down");
            handle_key(&mut s, key(up));
            assert_eq!(s.selected(), 0, "{up:?} must move up");
        }
    }

    /// Esc and q both close.
    #[test]
    fn esc_and_q_both_close() {
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            let mut s = state();
            handle_key(&mut s, key(code));
            assert!(
                State::take_outcome(&mut s).is_some(),
                "{code:?} must close the screen"
            );
        }
    }

    /// An unbound key is inert — it must not toggle, move, or close. The screen
    /// mutates persisted user settings, so a stray keystroke silently flipping
    /// something is the failure mode worth pinning.
    #[test]
    fn unbound_keys_do_nothing() {
        let mut s = state();
        let before = s.settings().clone();
        for code in [KeyCode::Char('x'), KeyCode::Tab, KeyCode::Backspace] {
            handle_key(&mut s, key(code));
        }
        assert_eq!(s.selected(), 0, "an unbound key must not move the cursor");
        assert_eq!(
            s.settings(),
            &before,
            "an unbound key must not change values"
        );
        assert!(
            State::take_outcome(&mut s).is_none(),
            "an unbound key must not close the screen"
        );
    }

    /// The screen renders without panicking, at a normal size and squeezed down
    /// to a height where the fixed-size sections cannot all fit — the classic
    /// ratatui layout panic.
    #[test]
    fn draw_does_not_panic_at_any_size() {
        for (w, h) in [(80u16, 24u16), (40, 12), (20, 6), (10, 3)] {
            let backend = ratatui::backend::TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let s = state();
            terminal
                .draw(|f| draw(f, &s))
                .unwrap_or_else(|e| panic!("draw failed at {w}x{h}: {e}"));
        }
    }

    /// The rendered screen actually shows each setting's label and its on/off
    /// state — a draw that silently rendered an empty list would still pass a
    /// no-panic test.
    #[test]
    fn draw_shows_labels_and_values() {
        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let s = state();
        terminal.draw(|f| draw(f, &s)).expect("draw");

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        for &id in ALL {
            assert!(
                rendered.contains(id.label()),
                "rendered screen is missing the label {:?}",
                id.label()
            );
        }
        assert!(
            rendered.contains("[x]") || rendered.contains("[ ]"),
            "rendered screen shows no on/off markers"
        );
    }

    /// The screen's settings round-trip through the on-disk format, so a toggle
    /// made here survives a restart.
    #[test]
    fn changed_settings_round_trip_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");

        let mut s = state();
        s.toggle_selected();
        s.close();
        let Some(Outcome::Changed(changed)) = s.take_outcome() else {
            panic!("expected a change");
        };
        changed.save_to(&path).expect("save");

        let loaded = Settings::load_from(&path);
        assert_eq!(loaded, changed, "settings must survive a save/load cycle");
    }
}
