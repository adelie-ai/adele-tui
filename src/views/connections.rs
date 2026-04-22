//! Connections view: list + per-connector-type configure form + delete confirm.
//!
//! Interaction surface mirrors the GitHub issue: Add (`a`), Configure (`c`/enter),
//! Remove (`d`), Back (`q`/esc). The form's field set diverges per connector
//! type (anthropic/openai/bedrock/ollama).

use desktop_assistant_client_common::api;

/// Connector variants the TUI can create/edit. Mirrors the tag on
/// [`api::ConnectionConfigView`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorKind {
    Anthropic,
    OpenAi,
    Bedrock,
    Ollama,
}

impl ConnectorKind {
    pub const ALL: [ConnectorKind; 4] = [
        ConnectorKind::Anthropic,
        ConnectorKind::OpenAi,
        ConnectorKind::Bedrock,
        ConnectorKind::Ollama,
    ];

    pub fn as_label(self) -> &'static str {
        match self {
            ConnectorKind::Anthropic => "anthropic",
            ConnectorKind::OpenAi => "openai",
            ConnectorKind::Bedrock => "bedrock",
            ConnectorKind::Ollama => "ollama",
        }
    }

    pub fn from_api(tag: &str) -> Option<Self> {
        match tag {
            "anthropic" => Some(ConnectorKind::Anthropic),
            "openai" => Some(ConnectorKind::OpenAi),
            "bedrock" => Some(ConnectorKind::Bedrock),
            "ollama" => Some(ConnectorKind::Ollama),
            _ => None,
        }
    }

    /// Editable fields for this connector, in tab order.
    pub fn fields(self) -> &'static [FormField] {
        match self {
            ConnectorKind::Anthropic => &[FormField::ApiKeyEnv, FormField::BaseUrl],
            ConnectorKind::OpenAi => &[FormField::ApiKeyEnv, FormField::BaseUrl],
            ConnectorKind::Bedrock => &[
                FormField::AwsProfile,
                FormField::Region,
                FormField::BaseUrl,
            ],
            ConnectorKind::Ollama => &[FormField::BaseUrl, FormField::OllamaAutoPull],
        }
    }
}

/// Individual editable fields across all connector forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormField {
    /// Name of the env var that holds the API key (we never store the key itself).
    ApiKeyEnv,
    BaseUrl,
    AwsProfile,
    Region,
    /// Ollama-only toggle (true/false).
    OllamaAutoPull,
}

impl FormField {
    pub fn label(self) -> &'static str {
        match self {
            FormField::ApiKeyEnv => "API Key (env var name)",
            FormField::BaseUrl => "Base URL",
            FormField::AwsProfile => "AWS Profile",
            FormField::Region => "AWS Region",
            FormField::OllamaAutoPull => "Auto-pull",
        }
    }

    pub fn is_toggle(self) -> bool {
        matches!(self, FormField::OllamaAutoPull)
    }
}

/// In-memory form state. Each connector uses only a subset of these strings;
/// the `ConnectorKind::fields` slice is the source of truth for rendering and
/// tab order.
#[derive(Debug, Clone)]
pub struct ConnectionForm {
    pub id: String,
    pub kind: ConnectorKind,
    pub api_key_env: String,
    pub base_url: String,
    pub aws_profile: String,
    pub region: String,
    pub ollama_auto_pull: bool,
    /// True when editing an existing connection (id becomes read-only).
    pub existing: bool,
    /// Index into `[FormField::IdField, …kind.fields()]` — 0 targets the id row.
    pub field_cursor: usize,
    /// Last error message from the daemon (e.g. invalid slug, duplicate id).
    pub error: Option<String>,
}

impl ConnectionForm {
    pub fn new_for_add() -> Self {
        Self {
            id: String::new(),
            kind: ConnectorKind::Anthropic,
            api_key_env: String::new(),
            base_url: String::new(),
            aws_profile: String::new(),
            region: String::new(),
            ollama_auto_pull: false,
            existing: false,
            field_cursor: 0,
            error: None,
        }
    }

    pub fn from_existing(view: &api::ConnectionView) -> Self {
        // We can't hydrate secret fields (daemon returns `has_credentials`, not
        // the key). Existing env-var / base-url values aren't on the
        // ConnectionView either — the user re-enters them to update.
        let kind = ConnectorKind::from_api(&view.connector_type).unwrap_or(ConnectorKind::Anthropic);
        Self {
            id: view.id.clone(),
            kind,
            api_key_env: String::new(),
            base_url: String::new(),
            aws_profile: String::new(),
            region: String::new(),
            ollama_auto_pull: false,
            existing: true,
            field_cursor: 0,
            error: None,
        }
    }

    /// Total row count: id + connector-kind picker + fields for the current kind.
    /// Cursor positions:
    ///   0 -> id
    ///   1 -> connector kind picker
    ///   2..(2 + n) -> connector-specific fields
    pub fn row_count(&self) -> usize {
        2 + self.kind.fields().len()
    }

    pub fn next_field(&mut self) {
        self.field_cursor = (self.field_cursor + 1) % self.row_count();
    }

    pub fn previous_field(&mut self) {
        let count = self.row_count();
        self.field_cursor = (self.field_cursor + count - 1) % count;
    }

    pub fn is_on_id(&self) -> bool {
        self.field_cursor == 0
    }

    pub fn is_on_kind(&self) -> bool {
        self.field_cursor == 1
    }

    pub fn current_field(&self) -> Option<FormField> {
        if self.field_cursor < 2 {
            return None;
        }
        self.kind.fields().get(self.field_cursor - 2).copied()
    }

    pub fn cycle_kind_next(&mut self) {
        let idx = ConnectorKind::ALL
            .iter()
            .position(|k| *k == self.kind)
            .unwrap_or(0);
        self.kind = ConnectorKind::ALL[(idx + 1) % ConnectorKind::ALL.len()];
        // Reset kind-specific cursor overrun.
        self.field_cursor = self.field_cursor.min(self.row_count() - 1);
    }

    pub fn cycle_kind_prev(&mut self) {
        let idx = ConnectorKind::ALL
            .iter()
            .position(|k| *k == self.kind)
            .unwrap_or(0);
        let len = ConnectorKind::ALL.len();
        self.kind = ConnectorKind::ALL[(idx + len - 1) % len];
        self.field_cursor = self.field_cursor.min(self.row_count() - 1);
    }

    pub fn toggle_auto_pull(&mut self) {
        self.ollama_auto_pull = !self.ollama_auto_pull;
    }

    pub fn insert_char(&mut self, ch: char) {
        if self.existing && self.is_on_id() {
            return; // id is read-only for updates
        }
        if let Some(field) = self.current_field()
            && field.is_toggle()
        {
            // Single-char yes/no toggle — 'y'/'n'/' '/'t'/'f'
            let lower = ch.to_ascii_lowercase();
            match lower {
                'y' | 't' | '1' => self.ollama_auto_pull = true,
                'n' | 'f' | '0' => self.ollama_auto_pull = false,
                ' ' => self.toggle_auto_pull(),
                _ => {}
            }
            return;
        }
        self.current_string_mut().push(ch);
    }

    pub fn backspace(&mut self) {
        if self.existing && self.is_on_id() {
            return;
        }
        if let Some(field) = self.current_field()
            && field.is_toggle()
        {
            // Backspace on a toggle clears to false.
            self.ollama_auto_pull = false;
            return;
        }
        self.current_string_mut().pop();
    }

    fn current_string_mut(&mut self) -> &mut String {
        if self.is_on_id() {
            return &mut self.id;
        }
        if self.is_on_kind() {
            // Kind rows are cycled, not typed — fall through to a scratch
            // buffer would lose data; just return id which is a harmless no-op
            // since callers only reach here after current_field() returns Some.
            // But just in case, point to api_key_env.
            return &mut self.api_key_env;
        }
        match self.current_field() {
            Some(FormField::ApiKeyEnv) => &mut self.api_key_env,
            Some(FormField::BaseUrl) => &mut self.base_url,
            Some(FormField::AwsProfile) => &mut self.aws_profile,
            Some(FormField::Region) => &mut self.region,
            Some(FormField::OllamaAutoPull) | None => &mut self.api_key_env,
        }
    }

    /// Build the wire-format config to send to the daemon. Returns `None`
    /// when the form is incomplete (e.g. no id).
    pub fn to_api_config(&self) -> Option<api::ConnectionConfigView> {
        if self.id.trim().is_empty() {
            return None;
        }
        let opt = |s: &str| {
            let t = s.trim();
            if t.is_empty() { None } else { Some(t.to_string()) }
        };
        Some(match self.kind {
            ConnectorKind::Anthropic => api::ConnectionConfigView::Anthropic {
                base_url: opt(&self.base_url),
                api_key_env: opt(&self.api_key_env),
            },
            ConnectorKind::OpenAi => api::ConnectionConfigView::OpenAi {
                base_url: opt(&self.base_url),
                api_key_env: opt(&self.api_key_env),
            },
            ConnectorKind::Bedrock => api::ConnectionConfigView::Bedrock {
                aws_profile: opt(&self.aws_profile),
                region: opt(&self.region),
                base_url: opt(&self.base_url),
            },
            ConnectorKind::Ollama => api::ConnectionConfigView::Ollama {
                base_url: opt(&self.base_url),
                // NB: the daemon's ConnectionConfigView::Ollama variant does
                // not carry an auto_pull bit today — it's a TUI-side
                // presentation flag that the daemon can honor in a follow-up.
            },
        })
    }
}

/// Confirmation flow for deleting a connection. First attempt sends
/// `force=false`; if the daemon refuses (conn is referenced by a purpose)
/// we advance to [`DeleteStage::OfferForce`] and show the server message so
/// the user can retry with `force=true`.
#[derive(Debug, Clone)]
pub struct DeletePrompt {
    pub id: String,
    pub stage: DeleteStage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteStage {
    /// Initial yes/no — confirm and send `force=false`.
    Initial,
    /// Server refused; surface its reason and offer `force=true`.
    OfferForce { server_error: String },
}

impl DeletePrompt {
    pub fn initial(id: String) -> Self {
        Self {
            id,
            stage: DeleteStage::Initial,
        }
    }

    pub fn advance_to_force(&mut self, server_error: String) {
        self.stage = DeleteStage::OfferForce { server_error };
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConnectionsView {
    pub connections: Vec<api::ConnectionView>,
    pub selected: Option<usize>,
    pub loading: bool,
    pub status: Option<String>,
    pub form: Option<ConnectionForm>,
    pub delete: Option<DeletePrompt>,
}

impl ConnectionsView {
    pub fn set_connections(&mut self, items: Vec<api::ConnectionView>) {
        self.connections = items;
        self.loading = false;
        if self.connections.is_empty() {
            self.selected = None;
        } else if self.selected.is_none() {
            self.selected = Some(0);
        } else if let Some(idx) = self.selected
            && idx >= self.connections.len()
        {
            self.selected = Some(self.connections.len() - 1);
        }
    }

    pub fn select_next(&mut self) {
        if self.connections.is_empty() {
            return;
        }
        self.selected = Some(match self.selected {
            Some(i) if i + 1 >= self.connections.len() => 0,
            Some(i) => i + 1,
            None => 0,
        });
    }

    pub fn select_previous(&mut self) {
        if self.connections.is_empty() {
            return;
        }
        self.selected = Some(match self.selected {
            Some(0) | None => self.connections.len() - 1,
            Some(i) => i - 1,
        });
    }

    pub fn selected_connection(&self) -> Option<&api::ConnectionView> {
        self.connections.get(self.selected?)
    }

    pub fn start_add(&mut self) {
        self.form = Some(ConnectionForm::new_for_add());
    }

    pub fn start_configure(&mut self) {
        if let Some(conn) = self.selected_connection() {
            self.form = Some(ConnectionForm::from_existing(conn));
        }
    }

    pub fn start_delete(&mut self) {
        if let Some(conn) = self.selected_connection() {
            self.delete = Some(DeletePrompt::initial(conn.id.clone()));
        }
    }

    pub fn close_overlay(&mut self) {
        self.form = None;
        self.delete = None;
    }
}

/// Test-only helper so sibling modules can construct a `ConnectionView`
/// without duplicating the boilerplate.
#[cfg(test)]
pub fn tests_fixture(id: &str, ty: &str) -> api::ConnectionView {
    api::ConnectionView {
        id: id.into(),
        connector_type: ty.into(),
        display_label: format!("{id} ({ty})"),
        availability: api::ConnectionAvailability::Ok,
        has_credentials: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_view(id: &str, ty: &str) -> api::ConnectionView {
        tests_fixture(id, ty)
    }

    #[test]
    fn set_connections_picks_first_by_default() {
        let mut v = ConnectionsView::default();
        v.set_connections(vec![sample_view("a", "openai")]);
        assert_eq!(v.selected, Some(0));
    }

    #[test]
    fn select_wraps() {
        let mut v = ConnectionsView::default();
        v.set_connections(vec![
            sample_view("a", "openai"),
            sample_view("b", "anthropic"),
        ]);
        v.selected = Some(1);
        v.select_next();
        assert_eq!(v.selected, Some(0));
        v.select_previous();
        assert_eq!(v.selected, Some(1));
    }

    #[test]
    fn empty_connections_clears_selection() {
        let mut v = ConnectionsView::default();
        v.set_connections(vec![sample_view("a", "openai")]);
        v.set_connections(Vec::new());
        assert_eq!(v.selected, None);
    }

    #[test]
    fn form_for_add_defaults_to_anthropic() {
        let f = ConnectionForm::new_for_add();
        assert_eq!(f.kind, ConnectorKind::Anthropic);
        assert_eq!(f.field_cursor, 0);
        assert!(!f.existing);
    }

    #[test]
    fn form_cycles_kind_and_clamps_cursor() {
        let mut f = ConnectionForm::new_for_add();
        f.kind = ConnectorKind::Bedrock;
        // bedrock has 3 fields so rows = 5
        f.field_cursor = 4;
        f.cycle_kind_next(); // -> Ollama (2 fields, rows = 4)
        assert_eq!(f.kind, ConnectorKind::Ollama);
        assert!(f.field_cursor < f.row_count());
    }

    #[test]
    fn form_rejects_id_edits_on_existing() {
        let view = sample_view("work", "openai");
        let mut f = ConnectionForm::from_existing(&view);
        assert!(f.existing);
        assert_eq!(f.field_cursor, 0); // on id
        f.insert_char('x');
        assert_eq!(f.id, "work");
    }

    #[test]
    fn form_tabs_through_fields() {
        let mut f = ConnectionForm::new_for_add();
        // Anthropic: rows = 2 + 2 = 4
        assert_eq!(f.row_count(), 4);
        f.next_field(); // 1
        f.next_field(); // 2
        assert_eq!(f.current_field(), Some(FormField::ApiKeyEnv));
        f.next_field(); // 3
        assert_eq!(f.current_field(), Some(FormField::BaseUrl));
        f.next_field(); // 0
        assert_eq!(f.field_cursor, 0);
    }

    #[test]
    fn form_api_key_env_populated_to_api_config() {
        let mut f = ConnectionForm::new_for_add();
        f.id = "work".into();
        // Land on ApiKeyEnv (index 2) and type.
        f.field_cursor = 2;
        for c in "OPENAI_WORK_KEY".chars() {
            f.insert_char(c);
        }
        // Next is BaseUrl (index 3)
        f.field_cursor = 3;
        for c in "https://api.example/v1".chars() {
            f.insert_char(c);
        }
        let cfg = f.to_api_config().unwrap();
        match cfg {
            api::ConnectionConfigView::Anthropic {
                base_url,
                api_key_env,
            } => {
                assert_eq!(api_key_env.as_deref(), Some("OPENAI_WORK_KEY"));
                assert_eq!(base_url.as_deref(), Some("https://api.example/v1"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn form_bedrock_api_config_uses_profile_region() {
        let mut f = ConnectionForm::new_for_add();
        f.id = "aws".into();
        f.kind = ConnectorKind::Bedrock;
        f.field_cursor = 2; // AwsProfile
        for c in "work".chars() {
            f.insert_char(c);
        }
        f.field_cursor = 3; // Region
        for c in "us-west-2".chars() {
            f.insert_char(c);
        }
        let cfg = f.to_api_config().unwrap();
        match cfg {
            api::ConnectionConfigView::Bedrock {
                aws_profile,
                region,
                base_url,
            } => {
                assert_eq!(aws_profile.as_deref(), Some("work"));
                assert_eq!(region.as_deref(), Some("us-west-2"));
                assert_eq!(base_url, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn form_empty_id_fails_to_build() {
        let mut f = ConnectionForm::new_for_add();
        f.kind = ConnectorKind::OpenAi;
        assert!(f.to_api_config().is_none());
    }

    #[test]
    fn ollama_toggle_accepts_yn() {
        let mut f = ConnectionForm::new_for_add();
        f.kind = ConnectorKind::Ollama;
        // Ollama fields: BaseUrl (idx 2), OllamaAutoPull (idx 3)
        f.field_cursor = 3;
        f.insert_char('y');
        assert!(f.ollama_auto_pull);
        f.insert_char('n');
        assert!(!f.ollama_auto_pull);
        f.insert_char(' ');
        assert!(f.ollama_auto_pull);
    }

    #[test]
    fn delete_prompt_stages() {
        let mut p = DeletePrompt::initial("id".into());
        assert_eq!(p.stage, DeleteStage::Initial);
        p.advance_to_force("referenced by interactive".into());
        match p.stage {
            DeleteStage::OfferForce { ref server_error } => {
                assert!(server_error.contains("referenced"));
            }
            _ => panic!("expected force stage"),
        }
    }
}
