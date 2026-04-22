//! Per-conversation model selector — popup + state store.
//!
//! The popup flattens `ListAvailableModels` into `"<connection> · <model>"`
//! rows across every healthy connection. A disabled `Auto (coming soon)`
//! row is pinned at the top per the ticket. Picking an entry stashes the
//! selection in the per-conversation map, which the chat view's next
//! `SendMessage` threads into `override_selection`.

use std::collections::HashMap;

use desktop_assistant_client_common::api;

/// Sticky per-conversation selection; lives in-app until the daemon starts
/// round-tripping `last_model_selection` on `GetConversation`
/// (see desktop-assistant#18). Independent of the daemon-persisted selection
/// — the daemon is still authoritative; we just track what the user last
/// picked in this session so the popup / status bar can reflect it.
#[derive(Debug, Clone, Default)]
pub struct ConversationSelections {
    inner: HashMap<String, api::SendPromptOverride>,
}

impl ConversationSelections {
    pub fn get(&self, conversation_id: &str) -> Option<&api::SendPromptOverride> {
        self.inner.get(conversation_id)
    }

    pub fn set(&mut self, conversation_id: String, override_sel: api::SendPromptOverride) {
        self.inner.insert(conversation_id, override_sel);
    }

    pub fn clear(&mut self, conversation_id: &str) {
        self.inner.remove(conversation_id);
    }

    pub fn hydrate_from_dangling_fallback(
        &mut self,
        conversation_id: &str,
        fallback: &api::ConversationModelSelectionView,
    ) {
        self.inner.insert(
            conversation_id.to_string(),
            api::SendPromptOverride {
                connection_id: fallback.connection_id.clone(),
                model_id: fallback.model_id.clone(),
                effort: fallback.effort,
            },
        );
    }
}

/// Popup state for picking a model for the active conversation.
#[derive(Debug, Clone, Default)]
pub struct ModelSelector {
    pub open: bool,
    pub loading: bool,
    pub entries: Vec<api::ModelListing>,
    /// Highlighted row. Index 0 is the disabled "Auto" row — we skip past it
    /// on open when possible so Enter doesn't land on a no-op.
    pub highlight: usize,
    pub status: Option<String>,
}

impl ModelSelector {
    pub fn total_rows(&self) -> usize {
        // +1 for the pinned Auto row
        self.entries.len() + 1
    }

    pub fn open(&mut self) {
        self.open = true;
        self.highlight = if self.entries.is_empty() { 0 } else { 1 };
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn set_entries(&mut self, entries: Vec<api::ModelListing>) {
        self.entries = entries;
        self.loading = false;
        if self.highlight >= self.total_rows() {
            self.highlight = self.total_rows().saturating_sub(1);
        }
    }

    pub fn highlight_next(&mut self) {
        let total = self.total_rows();
        if total == 0 {
            return;
        }
        self.highlight = (self.highlight + 1) % total;
    }

    pub fn highlight_previous(&mut self) {
        let total = self.total_rows();
        if total == 0 {
            return;
        }
        self.highlight = (self.highlight + total - 1) % total;
    }

    /// The currently highlighted entry, or `None` when the auto row is selected
    /// (index 0) or the list is empty.
    pub fn selected_entry(&self) -> Option<&api::ModelListing> {
        if self.highlight == 0 {
            return None;
        }
        self.entries.get(self.highlight - 1)
    }

    /// Position the highlight on the entry that matches the given
    /// (connection, model) pair, if any. Otherwise leave it on the first
    /// real entry (after the Auto row).
    pub fn highlight_for(&mut self, sel: Option<&api::SendPromptOverride>) {
        if let Some(sel) = sel {
            for (i, entry) in self.entries.iter().enumerate() {
                if entry.connection_id == sel.connection_id && entry.model.id == sel.model_id {
                    self.highlight = i + 1;
                    return;
                }
            }
        }
        self.highlight = if self.entries.is_empty() { 0 } else { 1 };
    }
}

/// Human-readable representation of an override for the status bar.
pub fn status_bar_label(sel: Option<&api::SendPromptOverride>) -> String {
    match sel {
        Some(sel) => {
            let base = format!("{} · {}", sel.connection_id, sel.model_id);
            match sel.effort {
                Some(api::EffortLevel::Low) => format!("{base} (low)"),
                Some(api::EffortLevel::Medium) => format!("{base} (med)"),
                Some(api::EffortLevel::High) => format!("{base} (high)"),
                None => base,
            }
        }
        None => "Auto (purpose)".into(),
    }
}

/// Human-readable representation of a `ConversationWarning` for the
/// status-bar inline notice.
pub fn warning_notice(warning: &api::ConversationWarning) -> String {
    match warning {
        api::ConversationWarning::DanglingModelSelection {
            previous_selection,
            fallback_to,
        } => format!(
            "previous model {}·{} unavailable → falling back to {}·{}",
            previous_selection.connection_id,
            previous_selection.model_id,
            fallback_to.connection_id,
            fallback_to.model_id,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(conn: &str, model: &str) -> api::ModelListing {
        api::ModelListing {
            connection_id: conn.into(),
            connection_label: conn.into(),
            model: api::ModelInfoView {
                id: model.into(),
                display_name: model.into(),
                context_limit: None,
                capabilities: api::ModelCapabilitiesView::default(),
            },
        }
    }

    #[test]
    fn selector_opens_past_auto_row() {
        let mut s = ModelSelector::default();
        s.set_entries(vec![listing("a", "m1")]);
        s.open();
        assert_eq!(s.highlight, 1); // skip Auto
    }

    #[test]
    fn selector_opens_on_auto_when_empty() {
        let mut s = ModelSelector::default();
        s.open();
        assert_eq!(s.highlight, 0);
    }

    #[test]
    fn selector_wraps_through_auto() {
        let mut s = ModelSelector::default();
        s.set_entries(vec![listing("a", "m1"), listing("a", "m2")]);
        s.open();
        s.highlight_previous(); // from 1 -> 0 (auto)
        assert_eq!(s.highlight, 0);
        s.highlight_previous(); // wrap to last real entry
        assert_eq!(s.highlight, 2);
    }

    #[test]
    fn selector_selected_entry_returns_none_on_auto() {
        let mut s = ModelSelector::default();
        s.set_entries(vec![listing("a", "m1")]);
        s.highlight = 0;
        assert!(s.selected_entry().is_none());
        s.highlight = 1;
        assert_eq!(s.selected_entry().unwrap().model.id, "m1");
    }

    #[test]
    fn conversation_selections_round_trip() {
        let mut store = ConversationSelections::default();
        assert!(store.get("c1").is_none());
        store.set(
            "c1".into(),
            api::SendPromptOverride {
                connection_id: "work".into(),
                model_id: "gpt-5".into(),
                effort: Some(api::EffortLevel::High),
            },
        );
        let got = store.get("c1").unwrap();
        assert_eq!(got.connection_id, "work");
        assert_eq!(got.model_id, "gpt-5");
        assert_eq!(got.effort, Some(api::EffortLevel::High));
    }

    #[test]
    fn hydrate_from_dangling_fallback_stores_override() {
        let mut store = ConversationSelections::default();
        store.hydrate_from_dangling_fallback(
            "c1",
            &api::ConversationModelSelectionView {
                connection_id: "other".into(),
                model_id: "claude-sonnet".into(),
                effort: None,
            },
        );
        let got = store.get("c1").unwrap();
        assert_eq!(got.connection_id, "other");
        assert_eq!(got.model_id, "claude-sonnet");
    }

    #[test]
    fn status_bar_label_auto() {
        assert_eq!(status_bar_label(None), "Auto (purpose)");
    }

    #[test]
    fn status_bar_label_with_effort() {
        let sel = api::SendPromptOverride {
            connection_id: "work".into(),
            model_id: "m1".into(),
            effort: Some(api::EffortLevel::High),
        };
        assert_eq!(status_bar_label(Some(&sel)), "work · m1 (high)");
    }

    #[test]
    fn warning_notice_describes_dangling() {
        let w = api::ConversationWarning::DanglingModelSelection {
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
        };
        let note = warning_notice(&w);
        assert!(note.contains("old·gone"));
        assert!(note.contains("new·ok"));
    }

    #[test]
    fn highlight_for_finds_match() {
        let mut s = ModelSelector::default();
        s.set_entries(vec![
            listing("a", "m1"),
            listing("a", "m2"),
            listing("b", "m3"),
        ]);
        let sel = api::SendPromptOverride {
            connection_id: "a".into(),
            model_id: "m2".into(),
            effort: None,
        };
        s.highlight_for(Some(&sel));
        assert_eq!(s.highlight, 2);
    }
}
