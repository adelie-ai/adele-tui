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

use crate::settings::Settings;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The user changed something; the caller should apply + persist these.
    Changed(Settings),
    /// Nothing changed — the caller can skip the write entirely.
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

#[cfg(test)]
mod tests {
    use super::*;

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
