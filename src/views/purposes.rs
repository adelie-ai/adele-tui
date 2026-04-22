//! Purposes view: flat (Purpose) (Connection) (Model) (Effort) list.
//!
//! Daemon exposes four purposes: Interactive, Dreaming, Embedding, Titling.
//! Interactive is required and cannot inherit via `"primary"`; the other three
//! accept `"primary"` in either field to inherit from the interactive purpose.

use desktop_assistant_client_common::api;

pub const PURPOSES_ORDER: [api::PurposeKindApi; 4] = [
    api::PurposeKindApi::Interactive,
    api::PurposeKindApi::Dreaming,
    api::PurposeKindApi::Embedding,
    api::PurposeKindApi::Titling,
];

/// Fields a user can edit for a single purpose row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurposeField {
    Connection,
    Model,
    Effort,
}

impl PurposeField {
    #[allow(dead_code)] // documents the field set for future navigation helpers
    pub const ALL: [PurposeField; 3] = [
        PurposeField::Connection,
        PurposeField::Model,
        PurposeField::Effort,
    ];

    #[allow(dead_code)] // exposed for symmetry with `FormField::label`
    pub fn label(self) -> &'static str {
        match self {
            PurposeField::Connection => "Connection",
            PurposeField::Model => "Model",
            PurposeField::Effort => "Effort",
        }
    }
}

/// In-memory edit buffer for one row.
#[derive(Debug, Clone)]
pub struct PurposeEditor {
    pub purpose: api::PurposeKindApi,
    pub connection: String,
    pub model: String,
    pub effort: Option<api::EffortLevel>,
    pub field: PurposeField,
    /// When typing into the connection/model field, we also show a list of
    /// candidate suggestions below. The raw candidate list comes from the
    /// view's model listings.
    pub error: Option<String>,
}

impl PurposeEditor {
    pub fn new(purpose: api::PurposeKindApi, existing: Option<&api::PurposeConfigView>) -> Self {
        match existing {
            Some(cfg) => Self {
                purpose,
                connection: cfg.connection.clone(),
                model: cfg.model.clone(),
                effort: cfg.effort,
                field: PurposeField::Connection,
                error: None,
            },
            None => {
                // Sensible starting defaults: interactive inherits nothing
                // (required both fields), the rest default to "primary".
                let default_inherit = !matches!(purpose, api::PurposeKindApi::Interactive);
                Self {
                    purpose,
                    connection: if default_inherit { "primary".into() } else { String::new() },
                    model: if default_inherit { "primary".into() } else { String::new() },
                    effort: None,
                    field: PurposeField::Connection,
                    error: None,
                }
            }
        }
    }

    pub fn next_field(&mut self) {
        self.field = match self.field {
            PurposeField::Connection => PurposeField::Model,
            PurposeField::Model => PurposeField::Effort,
            PurposeField::Effort => PurposeField::Connection,
        };
    }

    pub fn previous_field(&mut self) {
        self.field = match self.field {
            PurposeField::Connection => PurposeField::Effort,
            PurposeField::Model => PurposeField::Connection,
            PurposeField::Effort => PurposeField::Model,
        };
    }

    pub fn insert_char(&mut self, ch: char) {
        match self.field {
            PurposeField::Connection => self.connection.push(ch),
            PurposeField::Model => self.model.push(ch),
            PurposeField::Effort => {
                // Single-letter shortcut: l/m/h
                match ch.to_ascii_lowercase() {
                    'l' => self.effort = Some(api::EffortLevel::Low),
                    'm' => self.effort = Some(api::EffortLevel::Medium),
                    'h' => self.effort = Some(api::EffortLevel::High),
                    ' ' | 'x' | 'n' => self.effort = None,
                    _ => {}
                }
            }
        }
    }

    pub fn backspace(&mut self) {
        match self.field {
            PurposeField::Connection => {
                self.connection.pop();
            }
            PurposeField::Model => {
                self.model.pop();
            }
            PurposeField::Effort => {
                self.effort = None;
            }
        }
    }

    /// Validate and build the wire config. Interactive may not use `"primary"`.
    pub fn to_api_config(&self) -> Result<api::PurposeConfigView, String> {
        let connection = self.connection.trim().to_string();
        let model = self.model.trim().to_string();

        if connection.is_empty() {
            return Err("Connection is required".into());
        }
        if model.is_empty() {
            return Err("Model is required".into());
        }

        if matches!(self.purpose, api::PurposeKindApi::Interactive)
            && (connection == "primary" || model == "primary")
        {
            return Err("Interactive cannot inherit via \"primary\"".into());
        }

        Ok(api::PurposeConfigView {
            connection,
            model,
            effort: self.effort,
        })
    }
}

/// Purposes view state: purposes snapshot from the daemon plus optional
/// per-row editor popup.
#[derive(Debug, Clone, Default)]
pub struct PurposesView {
    pub purposes: api::PurposesView,
    pub selected: usize,
    pub loading: bool,
    pub editor: Option<PurposeEditor>,
    pub status: Option<String>,
}

impl PurposesView {
    pub fn set_purposes(&mut self, v: api::PurposesView) {
        self.purposes = v;
        self.loading = false;
    }

    pub fn select_next(&mut self) {
        self.selected = (self.selected + 1) % PURPOSES_ORDER.len();
    }

    pub fn select_previous(&mut self) {
        self.selected = (self.selected + PURPOSES_ORDER.len() - 1) % PURPOSES_ORDER.len();
    }

    pub fn current_purpose(&self) -> api::PurposeKindApi {
        PURPOSES_ORDER[self.selected]
    }

    pub fn current_config(&self) -> Option<&api::PurposeConfigView> {
        match self.current_purpose() {
            api::PurposeKindApi::Interactive => self.purposes.interactive.as_ref(),
            api::PurposeKindApi::Dreaming => self.purposes.dreaming.as_ref(),
            api::PurposeKindApi::Embedding => self.purposes.embedding.as_ref(),
            api::PurposeKindApi::Titling => self.purposes.titling.as_ref(),
        }
    }

    pub fn start_edit(&mut self) {
        let purpose = self.current_purpose();
        let existing = self.current_config().cloned();
        self.editor = Some(PurposeEditor::new(purpose, existing.as_ref()));
    }

    pub fn close_editor(&mut self) {
        self.editor = None;
    }
}

pub fn purpose_label(p: api::PurposeKindApi) -> &'static str {
    match p {
        api::PurposeKindApi::Interactive => "interactive",
        api::PurposeKindApi::Dreaming => "dreaming",
        api::PurposeKindApi::Embedding => "embedding",
        api::PurposeKindApi::Titling => "titling",
    }
}

pub fn effort_label(e: Option<api::EffortLevel>) -> &'static str {
    match e {
        Some(api::EffortLevel::Low) => "low",
        Some(api::EffortLevel::Medium) => "medium",
        Some(api::EffortLevel::High) => "high",
        None => "—",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_wraps() {
        let mut v = PurposesView {
            selected: PURPOSES_ORDER.len() - 1,
            ..Default::default()
        };
        v.select_next();
        assert_eq!(v.selected, 0);
        v.select_previous();
        assert_eq!(v.selected, PURPOSES_ORDER.len() - 1);
    }

    #[test]
    fn editor_defaults_for_non_interactive_inherit() {
        let ed = PurposeEditor::new(api::PurposeKindApi::Dreaming, None);
        assert_eq!(ed.connection, "primary");
        assert_eq!(ed.model, "primary");
    }

    #[test]
    fn editor_defaults_for_interactive_blank() {
        let ed = PurposeEditor::new(api::PurposeKindApi::Interactive, None);
        assert_eq!(ed.connection, "");
        assert_eq!(ed.model, "");
    }

    #[test]
    fn editor_validation_rejects_primary_on_interactive() {
        let ed = PurposeEditor {
            purpose: api::PurposeKindApi::Interactive,
            connection: "primary".into(),
            model: "gpt-5".into(),
            effort: None,
            field: PurposeField::Connection,
            error: None,
        };
        assert!(ed.to_api_config().is_err());
    }

    #[test]
    fn editor_validation_accepts_primary_elsewhere() {
        let ed = PurposeEditor {
            purpose: api::PurposeKindApi::Titling,
            connection: "primary".into(),
            model: "primary".into(),
            effort: Some(api::EffortLevel::Low),
            field: PurposeField::Connection,
            error: None,
        };
        let cfg = ed.to_api_config().unwrap();
        assert_eq!(cfg.connection, "primary");
        assert_eq!(cfg.effort, Some(api::EffortLevel::Low));
    }

    #[test]
    fn editor_effort_shortcut_keys() {
        let mut ed = PurposeEditor::new(api::PurposeKindApi::Dreaming, None);
        ed.field = PurposeField::Effort;
        ed.insert_char('h');
        assert_eq!(ed.effort, Some(api::EffortLevel::High));
        ed.insert_char('m');
        assert_eq!(ed.effort, Some(api::EffortLevel::Medium));
        ed.insert_char('x'); // clear
        assert_eq!(ed.effort, None);
    }

    #[test]
    fn from_existing_round_trips_effort() {
        let existing = api::PurposeConfigView {
            connection: "work".into(),
            model: "gpt-5".into(),
            effort: Some(api::EffortLevel::High),
        };
        let ed = PurposeEditor::new(api::PurposeKindApi::Interactive, Some(&existing));
        assert_eq!(ed.effort, Some(api::EffortLevel::High));
        assert_eq!(ed.connection, "work");
    }
}
