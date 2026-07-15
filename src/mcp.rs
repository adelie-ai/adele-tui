//! MCP-servers admin panel (issue desktop-assistant#495).
//!
//! Modal screen reachable from the chat with `F5`. Lists the daemon's Model
//! Context Protocol servers with an honest status, and lets the user
//! enable/disable, add, edit, and remove them via the typed command surface the
//! daemon already exposes: `ListMcpServers`, `SetMcpServerEnabled`,
//! `RemoveMcpServer`, `UpsertMcpServer` (transport-aware add/edit), `SetMcpSecret`
//! (bearer token), and `ListServiceAccounts` (the OAuth account picker). No
//! daemon / protocol / `client-ui-common` change: this is a client-local panel
//! that issues those commands through the same `Connector`/command path the
//! [`crate::connections`] and [`crate::purposes`] panels use, mirroring the web
//! panel that shipped in adele-web-ui (`crates/web/src/mcp.rs`) and the KCM tab.
//!
//! The transport-aware add/edit rides `UpsertMcpServer { config_json }`, a JSON
//! string of the daemon's `McpServerConfig`. This module builds that JSON from a
//! small local [`McpConfigDto`] mirroring only the fields the form surfaces, so
//! the TUI never pulls the process-spawning `desktop-assistant-mcp-client`.
//!
//! **Bearer secrets are write-only.** A bearer token is never echoed by the
//! daemon (the view carries only refs/kinds), never pre-filled on edit, rendered
//! masked, and only sent — via [`Command::SetMcpSecret`] under the `{name}_token`
//! ref, *before* the `UpsertMcpServer` that references it — when the user types
//! one. OAuth carries only the service-account *ref*; secret values never leave
//! via this panel.
//!
//! **Honest OAuth degradation.** Interactive OAuth sign-in (`configure_command`)
//! spawns a browser *on the daemon host*. The TUI may be driving a remote daemon
//! over a socket, so it cannot spawn that browser meaningfully. An OAuth server
//! that is not yet authorized renders honestly (`Sign in required`); pressing `c`
//! prints the exact command to run on the daemon host, rather than a dead button.
//!
//! Keys
//! ----
//!
//! List mode:
//! - `j`/`k` or arrows: navigate
//! - `Enter` / `e`: edit selected
//! - `a`: add new
//! - `Space` / `t`: enable/disable selected
//! - `c`: show the sign-in command for a server that needs OAuth sign-in
//! - `d`: remove selected (with confirm)
//! - `r`: refresh from daemon
//! - `Esc` / `q`: close
//!
//! Edit mode:
//! - `Tab` / `Shift+Tab`: cycle fields
//! - `←` / `→` / `Space`: cycle the focused selector (Transport / Enabled / Auth /
//!   Account); on text fields these edit the text instead
//! - `Ctrl+S`: save
//! - `Esc`: cancel
//!
//! Remove-confirm / sign-in overlay:
//! - `y` / `Enter`: confirm remove (remove overlay)
//! - `n` / `Esc`: cancel (any other key is ignored in the remove overlay)

use std::collections::BTreeMap;
use std::io;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use desktop_assistant_api_model::{
    Command, CommandResult, McpServerView, Secret, ServiceAccountView,
};
use desktop_assistant_client_common::{SignalEvent, TransportClient};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use ratatui_textarea::{CursorMove, TextArea};

use crate::in_flight::InFlight;
use crate::screen::Screen;
use crate::theme::theme;

// ===========================================================================
// Pure logic (host-testable) — mirrors adele-web-ui `crates/web/src/mcp.rs`
// ===========================================================================

/// A minimal `#[derive(Serialize)]` mirror of the daemon's `McpServerConfig`,
/// carrying only the fields the form surfaces. Building the `config_json` from
/// this DTO (rather than depending on `desktop-assistant-mcp-client`) keeps the
/// TUI free of that crate's process-spawn code. The daemon's `McpServerConfig`
/// uses serde defaults for every field this omits, so omit-empty is safe; `env`
/// is a `BTreeMap` so its JSON is key-sorted and the wire form is deterministic.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct McpConfigDto {
    name: String,
    enabled: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    command: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    http: Option<HttpDto>,
}

/// The `http` sub-table of [`McpConfigDto`] — mirrors the daemon's
/// `HttpTransportConfig` for the two auth modes the form drives: a static bearer
/// token (by secret ref) or a reference to an OAuth service account (epic #477).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct HttpDto {
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_bearer_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oauth_account: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    scopes: Vec<String>,
}

/// The transport a server speaks. Selects which set of form fields is shown and
/// which shape [`McpForm::build`] emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpTransport {
    /// Local process spawned over stdio (`command`/`args`/`env`).
    Stdio,
    /// Remote streamable-HTTP endpoint (`url` + auth).
    Http,
}

impl McpTransport {
    const ALL: [McpTransport; 2] = [McpTransport::Stdio, McpTransport::Http];

    /// Label for the transport selector.
    pub fn label(self) -> &'static str {
        match self {
            McpTransport::Stdio => "Local (stdio)",
            McpTransport::Http => "Remote (HTTP)",
        }
    }

    /// Cycle to the next/previous transport (`delta` is +1 / -1).
    pub fn cycle(self, delta: i32) -> Self {
        let pos = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[wrap_index(pos as i32 + delta, Self::ALL.len())]
    }
}

/// How a remote (HTTP) server authenticates. Mirrors the daemon's `auth_kind`
/// (`"none"` | `"bearer"` | `"oauth"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAuthKind {
    /// No authentication.
    None,
    /// A static `Authorization: Bearer` token, stored write-only under the
    /// `{name}_token` secret ref.
    Bearer,
    /// OAuth 2.0 via a reusable service account (epic #477) referenced by id.
    OAuth,
}

impl McpAuthKind {
    const ALL: [McpAuthKind; 3] = [McpAuthKind::None, McpAuthKind::Bearer, McpAuthKind::OAuth];

    /// Label for the auth selector (http only).
    pub fn label(self) -> &'static str {
        match self {
            McpAuthKind::None => "None",
            McpAuthKind::Bearer => "Bearer token",
            McpAuthKind::OAuth => "OAuth account",
        }
    }

    /// Cycle to the next/previous auth kind (`delta` is +1 / -1).
    pub fn cycle(self, delta: i32) -> Self {
        let pos = Self::ALL.iter().position(|a| *a == self).unwrap_or(0);
        Self::ALL[wrap_index(pos as i32 + delta, Self::ALL.len())]
    }
}

/// The colour tone a server's status renders in. Kept theme-independent (and so
/// unit-testable) — [`StatusTone::color`] maps it to a concrete theme colour at
/// draw time. Mirrors the web panel's `(css-class, label)` pair, adapted to the
/// terminal's semantic palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTone {
    /// Healthy / running — `theme().ok` (green).
    Ok,
    /// Needs attention (sign-in) — `theme().warn` (amber).
    Warn,
    /// Failed — `theme().error` (red).
    Error,
    /// Idle / off — `theme().text_dim` (neutral).
    Neutral,
}

impl StatusTone {
    /// The concrete theme colour for this tone. Called only on the draw path, so
    /// the pure mapping ([`status_display`]) stays theme-free and testable.
    fn color(self) -> Color {
        match self {
            StatusTone::Ok => theme().ok,
            StatusTone::Warn => theme().warn,
            StatusTone::Error => theme().error,
            StatusTone::Neutral => theme().text_dim,
        }
    }
}

/// Map the coarse daemon status string to a `(tone, human label)` pair. Covers
/// the six states the daemon reports; any unrecognized future state renders as a
/// neutral "Unknown" rather than panicking, so an older client degrades honestly
/// against a newer daemon.
pub fn status_display(status: &str) -> (StatusTone, &'static str) {
    match status {
        "running" => (StatusTone::Ok, "Running"),
        "stopped" => (StatusTone::Neutral, "Stopped"),
        "disabled" => (StatusTone::Neutral, "Disabled"),
        "needs_auth" => (StatusTone::Warn, "Sign in required"),
        "auth_expired" => (StatusTone::Warn, "Sign in expired"),
        "error" => (StatusTone::Error, "Error"),
        _ => (StatusTone::Neutral, "Unknown"),
    }
}

/// The transport chip label: an HTTP server is `"remote"`, anything else (stdio)
/// is `"local"`.
pub fn transport_chip(transport: &str) -> &'static str {
    if transport == "http" {
        "remote"
    } else {
        "local"
    }
}

/// Parse an env textarea into ordered `(KEY, value)` pairs. Each non-blank line
/// is `KEY=value`; the key is trimmed and the value is everything after the first
/// `=` (values may themselves contain `=`), also trimmed. Lines without a `=`, or
/// with a blank key, are skipped — a malformed line is dropped, never turned into
/// a half-entry.
pub fn parse_env(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            if key.is_empty() {
                return None;
            }
            Some((key.to_string(), value.trim().to_string()))
        })
        .collect()
}

/// Split a space-separated args string into argv tokens. Any run of whitespace
/// separates; empty tokens are dropped.
pub fn split_args(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

/// Split an OAuth scopes string on whitespace and/or commas into individual
/// scopes, dropping empties.
pub fn split_scopes(text: &str) -> Vec<String> {
    text.split([',', ' ', '\t', '\n', '\r'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The secrets.toml ref a server's bearer token is stored under. Convention:
/// `{name}_token`, so a server's config can reference its token by a stable id
/// the user never has to hand-edit.
pub fn bearer_secret_ref(name: &str) -> String {
    format!("{name}_token")
}

/// Build the [`Command::SetMcpSecret`] that stores a bearer token value under
/// `id`. The value is wrapped in [`Secret`] so it can't leak via `Debug`.
pub fn mcp_secret_command(id: String, value: String) -> Command {
    Command::SetMcpSecret {
        id,
        value: Secret(value),
    }
}

/// Replace ASCII/Unicode control characters (newlines, tabs, and — critically —
/// `ESC`) in daemon-provided text with a space, so a hostile server name /
/// target / error string can't inject terminal control sequences (cursor moves,
/// screen clears) into the rendered TUI. Applied to every daemon-sourced string
/// on the draw path; static labels are trusted and skip it.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Validate a server name on create: non-empty and only letters, digits, `-`,
/// `_` (mirrors the connections slug contract — the name is a config table key
/// and a tool-namespace prefix).
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Server name is required.".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("Name may only contain letters, digits, '-', and '_'.".to_string());
    }
    Ok(())
}

/// Trim `s`; `None` when the trimmed result is empty (so an empty optional is
/// omitted from the JSON rather than sent as `""`).
fn opt(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// The view-free model of the add/edit form. The TUI form widget
/// ([`FormState`]) splats this in on open and reads it back on submit, keeping
/// the validation/mapping here (tested) rather than in the widget. Mirrors the
/// web panel's `McpForm` so both clients emit byte-identical `config_json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpForm {
    /// `true` when editing an existing server — the name is immutable.
    pub editing: bool,
    pub transport: McpTransport,
    pub name: String,
    pub enabled: bool,
    // --- stdio ---
    pub command: String,
    /// Space-separated argv (split on save).
    pub args: String,
    pub namespace: String,
    /// `KEY=value` lines (parsed on save).
    pub env: String,
    // --- http ---
    pub url: String,
    pub auth: McpAuthKind,
    /// Write-only bearer token; never populated from a view.
    pub bearer_token: String,
    /// Referenced service-account id (OAuth).
    pub oauth_account: String,
    /// Space/comma-separated OAuth scopes.
    pub scopes: String,
}

impl McpForm {
    /// A blank create form for `transport`.
    pub fn blank(transport: McpTransport) -> Self {
        Self {
            editing: false,
            transport,
            name: String::new(),
            enabled: true,
            command: String::new(),
            args: String::new(),
            namespace: String::new(),
            env: String::new(),
            url: String::new(),
            auth: McpAuthKind::None,
            bearer_token: String::new(),
            oauth_account: String::new(),
            scopes: String::new(),
        }
    }

    /// Pre-fill an edit form from a server view: name + transport, the surfaced
    /// non-secret config fields, and (for http) the auth kind + oauth ref/scopes.
    /// Secret material (the bearer token) stays blank — the daemon never echoes
    /// it. The `env` box also stays blank: the view does not carry env.
    pub fn from_view(view: &McpServerView) -> Self {
        let transport = if view.transport == "http" {
            McpTransport::Http
        } else {
            McpTransport::Stdio
        };
        let auth = match view.auth_kind.as_deref() {
            Some("bearer") => McpAuthKind::Bearer,
            Some("oauth") => McpAuthKind::OAuth,
            _ => McpAuthKind::None,
        };
        let url = if transport == McpTransport::Http {
            view.target.clone()
        } else {
            String::new()
        };
        Self {
            editing: true,
            transport,
            name: view.name.clone(),
            enabled: view.enabled,
            command: view.command.clone(),
            args: view.args.join(" "),
            namespace: view.namespace.clone().unwrap_or_default(),
            // The view carries no env — it can't be pre-filled.
            env: String::new(),
            url,
            auth,
            // Write-only: the bearer token is never echoed / pre-filled.
            bearer_token: String::new(),
            oauth_account: view.oauth_account_ref.clone().unwrap_or_default(),
            scopes: view.oauth_scopes.join(" "),
        }
    }

    /// Validate + assemble the form into the command inputs: the target name
    /// (typed + validated on create, immutable on edit), the `config_json` string
    /// [`Command::UpsertMcpServer`] receives, and the optional bearer secret
    /// `(ref, value)` to write *first*. `Err` carries a human-readable reason.
    pub fn build(&self) -> Result<BuiltMcpServer, String> {
        let name = self.name.trim().to_string();
        // The name is immutable on edit (already daemon-validated); only a
        // freshly-typed create name is checked.
        if !self.editing {
            validate_name(&name)?;
        }

        let (dto, secret) = match self.transport {
            McpTransport::Stdio => {
                let command = self.command.trim().to_string();
                if command.is_empty() {
                    return Err("Command is required for a local (stdio) server.".to_string());
                }
                let dto = McpConfigDto {
                    name: name.clone(),
                    enabled: self.enabled,
                    command,
                    args: split_args(&self.args),
                    namespace: opt(&self.namespace),
                    env: parse_env(&self.env).into_iter().collect(),
                    http: None,
                };
                (dto, None)
            }
            McpTransport::Http => {
                let url = self.url.trim().to_string();
                if url.is_empty() {
                    return Err("URL is required for a remote (HTTP) server.".to_string());
                }
                let (auth_bearer_secret, oauth_account, scopes, secret) = match self.auth {
                    McpAuthKind::None => (None, None, Vec::new(), None),
                    McpAuthKind::Bearer => {
                        let secret_ref = bearer_secret_ref(&name);
                        let token = self.bearer_token.trim();
                        // Write-only: only write a secret when the user typed one;
                        // a blank field leaves any stored token untouched. The
                        // config still references the ref so the server stays
                        // "bearer" rather than silently going unauthenticated.
                        let secret = if token.is_empty() {
                            None
                        } else {
                            Some((secret_ref.clone(), token.to_string()))
                        };
                        (Some(secret_ref), None, Vec::new(), secret)
                    }
                    McpAuthKind::OAuth => {
                        let account = self.oauth_account.trim().to_string();
                        if account.is_empty() {
                            return Err(
                                "Choose a service account for OAuth authentication.".to_string()
                            );
                        }
                        (None, Some(account), split_scopes(&self.scopes), None)
                    }
                };
                let dto = McpConfigDto {
                    name: name.clone(),
                    enabled: self.enabled,
                    command: String::new(),
                    args: Vec::new(),
                    namespace: None,
                    env: BTreeMap::new(),
                    http: Some(HttpDto {
                        url,
                        auth_bearer_secret,
                        oauth_account,
                        scopes,
                    }),
                };
                (dto, secret)
            }
        };

        let config_json = serde_json::to_string(&dto)
            .map_err(|e| format!("Failed to encode the server config: {e}"))?;
        Ok(BuiltMcpServer {
            editing: self.editing,
            name,
            config_json,
            secret,
        })
    }
}

/// The assembled inputs for an upsert (+ optional bearer secret) round-trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltMcpServer {
    /// `true` ⇒ the name already existed (edit); `false` ⇒ create. Both go
    /// through `UpsertMcpServer`, which is add-or-replace.
    pub editing: bool,
    /// The target server name (immutable on edit, validated on create).
    pub name: String,
    /// The JSON `McpServerConfig` string for `UpsertMcpServer { config_json }`.
    pub config_json: String,
    /// `(secret_ref, value)` to store via `SetMcpSecret` *before* the upsert,
    /// when the user typed a bearer token. `None` leaves any stored secret
    /// untouched (write-only: a blank field never wipes a token).
    pub secret: Option<(String, String)>,
}

/// Wrap `i` into `0..len` (handles negatives), mirroring the purposes cycler.
fn wrap_index(i: i32, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let len = len as i32;
    (((i % len) + len) % len) as usize
}

// ===========================================================================
// TUI screen (mirrors `crate::connections`)
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    List,
    Edit,
    /// Remove-confirm overlay.
    RemoveConfirm,
    /// Read-only overlay printing the OAuth sign-in command for the daemon host.
    SignInInfo,
}

/// One editable field in the add/edit form. Some are text inputs; the rest are
/// selectors cycled with `←`/`→`/`Space`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Transport,
    Enabled,
    // stdio
    Command,
    Args,
    Namespace,
    Env,
    // http
    Url,
    Auth,
    BearerToken,
    OAuthAccount,
    Scopes,
}

impl Field {
    /// Whether this field is a `←`/`→`/`Space` selector (vs a text input).
    fn is_selector(self) -> bool {
        matches!(
            self,
            Field::Transport | Field::Enabled | Field::Auth | Field::OAuthAccount
        )
    }
}

/// The fields to show (in order) for a transport + auth combination.
fn fields_for(transport: McpTransport, auth: McpAuthKind) -> Vec<Field> {
    let mut fields = vec![Field::Name, Field::Transport, Field::Enabled];
    match transport {
        McpTransport::Stdio => {
            fields.extend([Field::Command, Field::Args, Field::Namespace, Field::Env]);
        }
        McpTransport::Http => {
            fields.extend([Field::Url, Field::Auth]);
            match auth {
                McpAuthKind::None => {}
                McpAuthKind::Bearer => fields.push(Field::BearerToken),
                McpAuthKind::OAuth => fields.extend([Field::OAuthAccount, Field::Scopes]),
            }
        }
    }
    fields
}

/// The TUI add/edit form widget: a [`TextArea`] per text field plus the selector
/// state ([`McpForm`] is the pure model it splats to/from).
struct FormState {
    editing: bool,
    transport: McpTransport,
    auth: McpAuthKind,
    enabled: bool,
    focus: Field,
    name: TextArea<'static>,
    command: TextArea<'static>,
    args: TextArea<'static>,
    namespace: TextArea<'static>,
    env: TextArea<'static>,
    url: TextArea<'static>,
    /// Rendered masked (`set_mask_char`) — the token is never shown in plaintext.
    bearer_token: TextArea<'static>,
    scopes: TextArea<'static>,
    /// Selected service-account id for OAuth (cycled against the account list).
    oauth_account: String,
}

impl FormState {
    /// Build the widget from the pure [`McpForm`]. The masked bearer field and
    /// the multi-line env field are set up here.
    fn from_pure(f: McpForm) -> Self {
        let mut bearer = single_line_textarea();
        bearer.set_mask_char('\u{25cf}'); // ● — never echo the token
        insert(&mut bearer, &f.bearer_token);
        Self {
            editing: f.editing,
            transport: f.transport,
            auth: f.auth,
            enabled: f.enabled,
            focus: Field::Name,
            name: text_field(&f.name),
            command: text_field(&f.command),
            args: text_field(&f.args),
            namespace: text_field(&f.namespace),
            env: multiline_field(&f.env),
            url: text_field(&f.url),
            bearer_token: bearer,
            scopes: text_field(&f.scopes),
            oauth_account: f.oauth_account,
        }
    }

    fn blank() -> Self {
        Self::from_pure(McpForm::blank(McpTransport::Stdio))
    }

    /// Read the widget back into a pure [`McpForm`] for validation/build.
    fn snapshot(&self) -> McpForm {
        McpForm {
            editing: self.editing,
            transport: self.transport,
            name: single(&self.name),
            enabled: self.enabled,
            command: single(&self.command),
            args: spaced(&self.args),
            namespace: single(&self.namespace),
            env: self.env.lines().join("\n"),
            url: single(&self.url),
            auth: self.auth,
            bearer_token: single(&self.bearer_token),
            oauth_account: self.oauth_account.clone(),
            scopes: spaced(&self.scopes),
        }
    }

    fn fields(&self) -> Vec<Field> {
        fields_for(self.transport, self.auth)
    }

    fn next_field(&mut self) {
        let fields = self.fields();
        let pos = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        self.focus = fields[(pos + 1) % fields.len()];
    }

    fn prev_field(&mut self) {
        let fields = self.fields();
        let pos = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        self.focus = fields[(pos + fields.len() - 1) % fields.len()];
    }

    /// Keep focus on a field the current transport/auth combination actually
    /// shows (after a transport or auth switch strands it on a hidden field).
    fn clamp_focus(&mut self) {
        let fields = self.fields();
        if !fields.contains(&self.focus) {
            self.focus = fields[0];
        }
    }

    /// The `TextArea` for the focused field, if it is a text input.
    fn focused_textarea(&mut self) -> Option<&mut TextArea<'static>> {
        match self.focus {
            Field::Name => Some(&mut self.name),
            Field::Command => Some(&mut self.command),
            Field::Args => Some(&mut self.args),
            Field::Namespace => Some(&mut self.namespace),
            Field::Env => Some(&mut self.env),
            Field::Url => Some(&mut self.url),
            Field::BearerToken => Some(&mut self.bearer_token),
            Field::Scopes => Some(&mut self.scopes),
            Field::Transport | Field::Enabled | Field::Auth | Field::OAuthAccount => None,
        }
    }
}

fn single_line_textarea() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_cursor_line_style(Style::default());
    ta
}

fn text_field(value: &str) -> TextArea<'static> {
    let mut ta = single_line_textarea();
    insert(&mut ta, value);
    ta
}

fn multiline_field(value: &str) -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_cursor_line_style(Style::default());
    insert(&mut ta, value);
    ta
}

fn insert(ta: &mut TextArea<'static>, value: &str) {
    if !value.is_empty() {
        ta.insert_str(value);
        ta.move_cursor(CursorMove::End);
    }
}

/// Collapse a (nominally single-line) text field to one string.
fn single(ta: &TextArea<'static>) -> String {
    ta.lines().join("")
}

/// Join a whitespace-delimited field with single spaces, so a stray newline
/// still separates tokens rather than merging them.
fn spaced(ta: &TextArea<'static>) -> String {
    ta.lines()
        .iter()
        .flat_map(|l| l.split_whitespace())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Resolved outcome of an off-loop RPC (modal-freeze fix). Each variant carries
/// the daemon result (stringified error); `apply_outcome` chains a `refresh_all`
/// after a successful mutation.
enum RpcOutcome {
    Refreshed {
        servers: Result<Vec<McpServerView>, String>,
        accounts: Result<Vec<ServiceAccountView>, String>,
    },
    Saved(Result<(), String>),
    Toggled(Result<(), String>),
    Removed(Result<(), String>),
}

struct State {
    servers: Vec<McpServerView>,
    /// OAuth service accounts (for the editor picker). Best-effort — a failure
    /// to list them leaves the picker empty rather than blocking the panel.
    accounts: Vec<ServiceAccountView>,
    selected: usize,
    mode: Mode,
    form: FormState,
    error: Option<String>,
    busy: Option<String>,
    closing: bool,
}

/// The MCP-servers manager as a [`Screen`]: its [`State`] plus the borrowed
/// client. The shared driver supplies the loop and drains daemon signals while
/// the screen is open (TUI-12).
struct McpScreen<'a> {
    state: State,
    client: &'a TransportClient,
    pending: InFlight<'a, RpcOutcome>,
}

impl Screen for McpScreen<'_> {
    type Outcome = ();

    fn draw(&mut self, frame: &mut Frame) {
        draw(frame, &self.state);
    }

    fn handle_key(&mut self, key: KeyEvent) -> impl std::future::Future<Output = ()> {
        match self.state.mode {
            Mode::List => handle_list_key(&mut self.state, key, self.client, &mut self.pending),
            Mode::Edit => handle_edit_key(&mut self.state, key, self.client, &mut self.pending),
            Mode::RemoveConfirm => {
                handle_remove_key(&mut self.state, key, self.client, &mut self.pending)
            }
            Mode::SignInInfo => {
                // Any key dismisses the read-only sign-in overlay.
                self.state.mode = Mode::List;
            }
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
            apply_outcome(&mut self.state, &mut self.pending, self.client, outcome);
        }
    }
}

/// Run the MCP-servers screen. Returns when the user closes it.
pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    client: &TransportClient,
    signal_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SignalEvent>,
    sink: &mut impl crate::screen::SignalSink,
) -> anyhow::Result<()> {
    let mut screen = McpScreen {
        state: State {
            servers: Vec::new(),
            accounts: Vec::new(),
            selected: 0,
            mode: Mode::List,
            form: FormState::blank(),
            error: None,
            busy: Some("Loading MCP servers...".into()),
            closing: false,
        },
        client,
        pending: InFlight::new(),
    };

    // Kick the initial load off-loop so "Loading…" shows and the screen is
    // responsive while it lands.
    refresh_all(&mut screen.state, &mut screen.pending, client);

    crate::screen::run_screen(terminal, &mut screen, signal_rx, sink).await
}

fn handle_list_key<'a>(
    state: &mut State,
    key: KeyEvent,
    client: &'a TransportClient,
    pending: &mut InFlight<'a, RpcOutcome>,
) {
    match (key.code, key.modifiers) {
        (KeyCode::Esc | KeyCode::Char('q'), m) if m.is_empty() => state.closing = true,
        (KeyCode::Char('j') | KeyCode::Down, m) if m.is_empty() => advance_selection(state, 1),
        (KeyCode::Char('k') | KeyCode::Up, m) if m.is_empty() => advance_selection(state, -1),
        (KeyCode::Enter | KeyCode::Char('e'), m) if m.is_empty() => {
            if let Some(view) = state.servers.get(state.selected).cloned() {
                state.form = FormState::from_pure(McpForm::from_view(&view));
                state.error = None;
                state.mode = Mode::Edit;
            }
        }
        (KeyCode::Char('a'), m) if m.is_empty() => {
            state.form = FormState::blank();
            state.error = None;
            state.mode = Mode::Edit;
        }
        (KeyCode::Char(' ') | KeyCode::Char('t'), m) if m.is_empty() => {
            do_toggle(state, pending, client)
        }
        (KeyCode::Char('c'), m) if m.is_empty() => show_signin(state),
        (KeyCode::Char('d'), m) if m.is_empty() && state.servers.get(state.selected).is_some() => {
            state.mode = Mode::RemoveConfirm;
        }
        (KeyCode::Char('r'), m) if m.is_empty() => refresh_all(state, pending, client),
        _ => {}
    }
}

fn handle_edit_key<'a>(
    state: &mut State,
    key: KeyEvent,
    client: &'a TransportClient,
    pending: &mut InFlight<'a, RpcOutcome>,
) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if ctrl && key.code == KeyCode::Char('s') {
        save_edit(state, pending, client);
        return;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            state.error = None;
            state.mode = Mode::List;
        }
        (KeyCode::Tab, _) => state.form.next_field(),
        (KeyCode::BackTab, _) => state.form.prev_field(),
        // ←/→/Space cycle the focused selector; on a text field they edit text.
        (KeyCode::Left | KeyCode::Right | KeyCode::Char(' '), _)
            if state.form.focus.is_selector() =>
        {
            let delta = if key.code == KeyCode::Left { -1 } else { 1 };
            cycle_selector(state, delta);
        }
        _ => {
            // Forward all other keys to the focused text field. Name is
            // immutable on edit; ignore edits with a clear message.
            if state.form.focus == Field::Name && state.form.editing {
                state.error = Some("Name is immutable on edit.".into());
            } else if let Some(ta) = state.form.focused_textarea() {
                ta.input(key);
            }
        }
    }
}

fn cycle_selector(state: &mut State, delta: i32) {
    match state.form.focus {
        Field::Transport => {
            if state.form.editing {
                state.error =
                    Some("Transport is locked on edit — remove and re-add to change it.".into());
                return;
            }
            state.form.transport = state.form.transport.cycle(delta);
            state.form.clamp_focus();
        }
        Field::Enabled => state.form.enabled = !state.form.enabled,
        Field::Auth => {
            state.form.auth = state.form.auth.cycle(delta);
            state.form.clamp_focus();
        }
        Field::OAuthAccount => cycle_account(state, delta),
        _ => {}
    }
}

fn cycle_account(state: &mut State, delta: i32) {
    let options: Vec<String> = state.accounts.iter().map(|a| a.id.clone()).collect();
    if options.is_empty() {
        return;
    }
    let pos = options
        .iter()
        .position(|o| o == &state.form.oauth_account)
        .unwrap_or(0);
    let next = wrap_index(pos as i32 + delta, options.len());
    state.form.oauth_account = options[next].clone();
}

fn handle_remove_key<'a>(
    state: &mut State,
    key: KeyEvent,
    client: &'a TransportClient,
    pending: &mut InFlight<'a, RpcOutcome>,
) {
    match (key.code, key.modifiers) {
        (KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter, _) => {
            do_remove(state, pending, client);
        }
        // A destructive confirm is dismissed only by an explicit cancel (n/Esc);
        // any other key is ignored rather than silently closing it.
        (KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc, _) => {
            state.mode = Mode::List;
        }
        _ => {}
    }
}

/// Show the OAuth sign-in command for the selected server (honest degradation):
/// the TUI can't spawn the daemon-host browser, so it prints the command to run.
fn show_signin(state: &mut State) {
    let Some(server) = state.servers.get(state.selected) else {
        return;
    };
    if server.configure_command.is_empty() {
        state.error = Some("This server doesn't need an interactive sign-in.".into());
        return;
    }
    state.error = None;
    state.mode = Mode::SignInInfo;
}

fn advance_selection(state: &mut State, delta: i32) {
    let len = state.servers.len();
    if len == 0 {
        return;
    }
    state.selected = wrap_index(state.selected as i32 + delta, len);
}

fn refresh_all<'a>(
    state: &mut State,
    pending: &mut InFlight<'a, RpcOutcome>,
    client: &'a TransportClient,
) {
    state.busy = Some("Loading MCP servers...".into());
    pending.push(async move {
        let servers = match send(client, Command::ListMcpServers).await {
            Ok(CommandResult::McpServers(v)) => Ok(v),
            Ok(other) => Err(format!("Unexpected response: {other:?}")),
            Err(e) => Err(e.to_string()),
        };
        let accounts = match send(client, Command::ListServiceAccounts).await {
            Ok(CommandResult::ServiceAccounts(v)) => Ok(v),
            Ok(other) => Err(format!("Unexpected response: {other:?}")),
            Err(e) => Err(e.to_string()),
        };
        RpcOutcome::Refreshed { servers, accounts }
    });
}

fn save_edit<'a>(
    state: &mut State,
    pending: &mut InFlight<'a, RpcOutcome>,
    client: &'a TransportClient,
) {
    let built = match state.form.snapshot().build() {
        Ok(b) => b,
        Err(e) => {
            state.error = Some(e);
            return;
        }
    };

    state.busy = Some("Saving...".into());
    pending.push(async move {
        // SECURITY / ordering: write the bearer secret BEFORE the upsert that
        // references it, so the config never points at a missing token.
        if let Some((id, value)) = built.secret
            && let Err(e) = send(client, mcp_secret_command(id, value)).await
        {
            return RpcOutcome::Saved(Err(format!("Saving the token failed: {e}")));
        }
        RpcOutcome::Saved(
            send(
                client,
                Command::UpsertMcpServer {
                    config_json: built.config_json,
                },
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string()),
        )
    });
}

fn do_toggle<'a>(
    state: &mut State,
    pending: &mut InFlight<'a, RpcOutcome>,
    client: &'a TransportClient,
) {
    let Some(server) = state.servers.get(state.selected) else {
        return;
    };
    let name = server.name.clone();
    let enabled = server.enabled;
    state.busy = Some(
        if enabled {
            "Disabling..."
        } else {
            "Enabling..."
        }
        .into(),
    );
    pending.push(async move {
        RpcOutcome::Toggled(
            send(
                client,
                Command::SetMcpServerEnabled {
                    name,
                    enabled: !enabled,
                },
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string()),
        )
    });
}

fn do_remove<'a>(
    state: &mut State,
    pending: &mut InFlight<'a, RpcOutcome>,
    client: &'a TransportClient,
) {
    let Some(server) = state.servers.get(state.selected) else {
        state.mode = Mode::List;
        return;
    };
    let name = server.name.clone();
    state.busy = Some("Removing...".into());
    pending.push(async move {
        RpcOutcome::Removed(
            send(client, Command::RemoveMcpServer { name })
                .await
                .map(|_| ())
                .map_err(|e| e.to_string()),
        )
    });
}

/// Apply a resolved RPC; chains a `refresh_all` after a successful mutation.
fn apply_outcome<'a>(
    state: &mut State,
    pending: &mut InFlight<'a, RpcOutcome>,
    client: &'a TransportClient,
    outcome: RpcOutcome,
) {
    state.busy = None;
    match outcome {
        RpcOutcome::Refreshed { servers, accounts } => {
            match servers {
                Ok(list) => {
                    state.servers = list;
                    if state.selected >= state.servers.len() {
                        state.selected = state.servers.len().saturating_sub(1);
                    }
                    state.error = None;
                }
                Err(e) => state.error = Some(format!("Failed to load MCP servers: {e}")),
            }
            // Accounts are best-effort: a failure just leaves the OAuth picker
            // empty (it shows a note), never clobbering a server-list error.
            match accounts {
                Ok(list) => state.accounts = list,
                Err(_) => state.accounts.clear(),
            }
        }
        RpcOutcome::Saved(result) => match result {
            Ok(()) => {
                state.error = None;
                state.mode = Mode::List;
                refresh_all(state, pending, client);
            }
            Err(e) => state.error = Some(format!("Save failed: {e}")),
        },
        RpcOutcome::Toggled(result) => match result {
            Ok(()) => refresh_all(state, pending, client),
            Err(e) => state.error = Some(format!("Toggle failed: {e}")),
        },
        RpcOutcome::Removed(result) => match result {
            Ok(()) => {
                state.mode = Mode::List;
                refresh_all(state, pending, client);
            }
            Err(e) => {
                state.error = Some(format!("Remove failed: {e}"));
                state.mode = Mode::List;
            }
        },
    }
}

/// Send a `Command` over the transport. The shared command channel
/// (`as_commands`) exposes a generic `send_command` over both socket transports
/// (UDS + WS); D-Bus speaks a fixed set of typed methods and has no command
/// channel, so we surface a clear error there rather than silently no-op'ing.
async fn send(client: &TransportClient, command: Command) -> anyhow::Result<CommandResult> {
    if let Some(commands) = client.as_commands() {
        commands.send_command(command).await
    } else {
        anyhow::bail!(
            "MCP-server management isn't available over D-Bus — switch transport with --transport ws or the local socket"
        )
    }
}

// --- Rendering ---

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

    draw_header(f, chunks[0]);
    match state.mode {
        Mode::Edit => draw_edit_form(f, state, chunks[1]),
        _ => draw_list(f, state, chunks[1]),
    }
    draw_status(f, state, chunks[2]);
    draw_hints(f, state, chunks[3]);

    match state.mode {
        Mode::RemoveConfirm => draw_remove_overlay(f, state, area),
        Mode::SignInInfo => draw_signin_overlay(f, state, area),
        _ => {}
    }
}

fn draw_header(f: &mut Frame, area: Rect) {
    let line = Line::from(vec![
        Span::styled(
            "MCP servers",
            Style::default()
                .fg(theme().title)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  —  Esc to return to chat",
            Style::default().fg(theme().text_dim),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_list(f: &mut Frame, state: &State, area: Rect) {
    let items: Vec<ListItem> = if state.servers.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "(no MCP servers — press 'a' to add one)",
            Style::default().fg(theme().text_dim),
        )))]
    } else {
        state.servers.iter().map(server_item).collect()
    };

    let title = if state.servers.is_empty() {
        "Servers".to_string()
    } else {
        format!("Servers ({})", state.servers.len())
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme().border))
                .title(Line::from(Span::styled(
                    title,
                    Style::default()
                        .fg(theme().title)
                        .add_modifier(Modifier::BOLD),
                ))),
        )
        .highlight_style(
            Style::default()
                .bg(theme().list_highlight)
                .fg(theme().list_highlight_fg)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");

    let mut list_state = ListState::default();
    if !state.servers.is_empty() {
        list_state.select(Some(state.selected));
    }
    f.render_stateful_widget(list, area, &mut list_state);
}

/// One server row: a status line (dot + name + chip + status/tools) plus a dim
/// target subtitle and, when present, an error-styled detail line. All
/// daemon-provided text is [`sanitize`]d before it reaches the terminal.
fn server_item(server: &McpServerView) -> ListItem<'static> {
    let (tone, status_label) = status_display(&server.status);
    let chip = transport_chip(&server.transport);

    let mut head: Vec<Span<'static>> = vec![
        Span::styled("●", Style::default().fg(tone.color())),
        Span::raw(" "),
        Span::styled(
            sanitize(&server.name),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  [{chip}]"), Style::default().fg(theme().text_dim)),
        Span::styled(
            format!("  {status_label}"),
            Style::default().fg(tone.color()),
        ),
    ];
    if server.status == "running" && server.tool_count > 0 {
        let n = server.tool_count;
        head.push(Span::styled(
            format!(" · {n} tool{}", if n == 1 { "" } else { "s" }),
            Style::default().fg(theme().text_dim),
        ));
    }

    let mut lines = vec![Line::from(head)];
    if !server.target.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("    {}", sanitize(&server.target)),
            Style::default().fg(theme().text_dim),
        )));
    }
    if let Some(detail) = &server.detail {
        lines.push(Line::from(Span::styled(
            format!("    {}", sanitize(detail)),
            Style::default()
                .fg(theme().error)
                .add_modifier(Modifier::ITALIC),
        )));
    }
    if !server.configure_command.is_empty()
        && matches!(server.status.as_str(), "needs_auth" | "auth_expired")
    {
        lines.push(Line::from(Span::styled(
            "    Sign-in required — press 'c' for the command to run on the daemon host",
            Style::default().fg(theme().warn),
        )));
    }
    ListItem::new(lines)
}

fn draw_edit_form(f: &mut Frame, state: &State, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme().border))
        .title(Line::from(Span::styled(
            if state.form.editing {
                "Edit MCP server"
            } else {
                "New MCP server"
            },
            Style::default()
                .fg(theme().title)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let fields = state.form.fields();
    let mut constraints: Vec<Constraint> = Vec::with_capacity(fields.len() * 2 + 1);
    for field in &fields {
        constraints.push(Constraint::Length(1)); // label
        constraints.push(Constraint::Length(field_input_height(*field)));
    }
    constraints.push(Constraint::Min(0));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    for (idx, field) in fields.iter().enumerate() {
        let label_row = rows[idx * 2];
        let input_row = rows[idx * 2 + 1];
        let focused = state.form.focus == *field;
        draw_field(f, state, *field, label_row, input_row, focused);
    }
}

fn field_input_height(field: Field) -> u16 {
    if field.is_selector() {
        1
    } else if field == Field::Env {
        5
    } else {
        3
    }
}

fn draw_field(
    f: &mut Frame,
    state: &State,
    field: Field,
    label_row: Rect,
    input_row: Rect,
    focused: bool,
) {
    let form = &state.form;
    match field {
        Field::Name => {
            let label = if form.editing {
                "Name (locked)"
            } else {
                "Name"
            };
            draw_field_label(f, label_row, label, focused);
            draw_text_field(f, input_row, &form.name, focused);
        }
        Field::Transport => {
            let suffix = if form.editing {
                " — locked on edit"
            } else {
                ""
            };
            draw_field_label(f, label_row, &format!("Transport{suffix}"), focused);
            draw_chip_line(
                f,
                input_row,
                &McpTransport::ALL
                    .iter()
                    .map(|t| (t.label(), *t == form.transport))
                    .collect::<Vec<_>>(),
                focused,
            );
        }
        Field::Enabled => {
            draw_field_label(f, label_row, "Enabled", focused);
            draw_chip_line(
                f,
                input_row,
                &[("Enabled", form.enabled), ("Disabled", !form.enabled)],
                focused,
            );
        }
        Field::Command => {
            draw_field_label(f, label_row, "Command (e.g. fileio-mcp)", focused);
            draw_text_field(f, input_row, &form.command, focused);
        }
        Field::Args => {
            draw_field_label(f, label_row, "Arguments (space-separated)", focused);
            draw_text_field(f, input_row, &form.args, focused);
        }
        Field::Namespace => {
            draw_field_label(f, label_row, "Namespace (optional)", focused);
            draw_text_field(f, input_row, &form.namespace, focused);
        }
        Field::Env => {
            draw_field_label(f, label_row, "Environment (KEY=value per line)", focused);
            draw_text_field(f, input_row, &form.env, focused);
        }
        Field::Url => {
            draw_field_label(f, label_row, "URL (https://host/mcp)", focused);
            draw_text_field(f, input_row, &form.url, focused);
        }
        Field::Auth => {
            draw_field_label(f, label_row, "Authentication", focused);
            draw_chip_line(
                f,
                input_row,
                &McpAuthKind::ALL
                    .iter()
                    .map(|a| (a.label(), *a == form.auth))
                    .collect::<Vec<_>>(),
                focused,
            );
        }
        Field::BearerToken => {
            draw_field_label(
                f,
                label_row,
                "Bearer token (write-only; blank keeps current)",
                focused,
            );
            draw_text_field(f, input_row, &form.bearer_token, focused);
        }
        Field::OAuthAccount => {
            draw_field_label(f, label_row, "Service account", focused);
            draw_account_line(f, input_row, state, focused);
        }
        Field::Scopes => {
            draw_field_label(f, label_row, "Scopes (space or comma-separated)", focused);
            draw_text_field(f, input_row, &form.scopes, focused);
        }
    }
}

fn draw_field_label(f: &mut Frame, area: Rect, label: &str, focused: bool) {
    let style = if focused {
        Style::default()
            .fg(theme().border_active)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme().text_dim)
    };
    f.render_widget(Paragraph::new(Span::styled(label.to_string(), style)), area);
}

fn draw_text_field(f: &mut Frame, area: Rect, textarea: &TextArea<'static>, focused: bool) {
    let mut ta = textarea.clone();
    let border_color = if focused {
        theme().border_active
    } else {
        theme().border
    };
    ta.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color)),
    );
    f.render_widget(&ta, area);
}

/// Render a selector as a row of chips, the active one highlighted.
fn draw_chip_line(f: &mut Frame, area: Rect, chips: &[(&str, bool)], focused: bool) {
    let mut spans: Vec<Span> = Vec::new();
    if focused {
        spans.push(Span::styled(
            "‹ ",
            Style::default().fg(theme().border_active),
        ));
    } else {
        spans.push(Span::raw("  "));
    }
    for (i, (label, active)) in chips.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let style = if *active {
            Style::default()
                .fg(Color::Black)
                .bg(theme().border_active)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme().text_dim)
        };
        spans.push(Span::styled(format!(" {label} "), style));
    }
    if focused {
        spans.push(Span::styled(
            " ›  (←/→)",
            Style::default().fg(theme().border_active),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Render the OAuth account selector: the current account (display name / id),
/// or an honest note when none are configured.
fn draw_account_line(f: &mut Frame, area: Rect, state: &State, focused: bool) {
    let line = if state.accounts.is_empty() {
        Line::from(Span::styled(
            "  (no service accounts — add one on the daemon host)",
            Style::default().fg(theme().warn),
        ))
    } else {
        let current = state
            .accounts
            .iter()
            .find(|a| a.id == state.form.oauth_account);
        let label = match current {
            Some(a) if !a.display_name.is_empty() => sanitize(&a.display_name),
            Some(a) => sanitize(&a.id),
            None => "(choose account)".to_string(),
        };
        let value_style = if focused {
            Style::default()
                .fg(Color::Black)
                .bg(theme().border_active)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme().text_dim)
        };
        let mut spans = vec![Span::raw("  ")];
        if focused {
            spans.push(Span::styled(
                "‹ ",
                Style::default().fg(theme().border_active),
            ));
        }
        spans.push(Span::styled(format!(" {label} "), value_style));
        if focused {
            spans.push(Span::styled(
                " ›  (←/→)",
                Style::default().fg(theme().border_active),
            ));
        }
        Line::from(spans)
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_remove_overlay(f: &mut Frame, state: &State, area: Rect) {
    let label = state
        .servers
        .get(state.selected)
        .map(|s| sanitize(&s.name))
        .unwrap_or_else(|| "this server".to_string());
    let popup = centered_rect(64, 6, area);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme().error))
        .title(Line::from(Span::styled(
            "Remove MCP server",
            Style::default()
                .fg(theme().error_text)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let body = Paragraph::new(vec![
        Line::from(Span::styled(
            format!("Remove \"{label}\"?"),
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            "y/Enter = confirm · n/Esc = cancel",
            Style::default().fg(theme().text_dim),
        )),
    ])
    .wrap(Wrap { trim: true });
    f.render_widget(body, inner);
}

/// The read-only OAuth sign-in overlay: prints the exact command to run on the
/// daemon host (honest degradation — the TUI can't spawn a remote browser).
fn draw_signin_overlay(f: &mut Frame, state: &State, area: Rect) {
    let Some(server) = state.servers.get(state.selected) else {
        return;
    };
    let command = server
        .configure_command
        .iter()
        .map(|part| sanitize(part))
        .collect::<Vec<_>>()
        .join(" ");
    let popup = centered_rect(72, 9, area);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme().warn))
        .title(Line::from(Span::styled(
            "Sign in to this MCP server",
            Style::default()
                .fg(theme().warn)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let body = Paragraph::new(vec![
        Line::from(Span::styled(
            "Sign-in opens a browser on the daemon host. Run this there:",
            Style::default().fg(theme().text_dim),
        )),
        Line::from(""),
        Line::from(Span::styled(
            command,
            Style::default()
                .fg(theme().code_fg)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Press any key to close.",
            Style::default().fg(theme().text_dim),
        )),
    ])
    .wrap(Wrap { trim: true });
    f.render_widget(body, inner);
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let popup_width = width.min(area.width.saturating_sub(4));
    let popup_height = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(popup_width)) / 2,
        y: area.y + (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    }
}

fn draw_status(f: &mut Frame, state: &State, area: Rect) {
    if let Some(busy) = &state.busy {
        let style = Style::default()
            .fg(theme().assistant_indicator)
            .add_modifier(Modifier::ITALIC);
        f.render_widget(
            Paragraph::new(Span::styled(format!(" ● {busy}"), style)),
            area,
        );
    } else if let Some(err) = &state.error {
        let style = Style::default().fg(theme().error);
        f.render_widget(
            Paragraph::new(Span::styled(format!(" • {err}"), style)),
            area,
        );
    }
}

fn draw_hints(f: &mut Frame, state: &State, area: Rect) {
    let hints: &[(&str, &str)] = match state.mode {
        Mode::List => &[
            ("Enter", "edit"),
            ("a", "add"),
            ("Space", "enable/disable"),
            ("c", "sign-in cmd"),
            ("d", "remove"),
            ("r", "refresh"),
            ("Esc", "back to chat"),
        ],
        Mode::Edit => &[
            ("Tab", "next field"),
            ("←/→", "cycle selector"),
            ("Ctrl+S", "save"),
            ("Esc", "cancel"),
        ],
        Mode::RemoveConfirm => &[("y/Enter", "confirm"), ("n/Esc", "cancel")],
        Mode::SignInInfo => &[("any key", "close")],
    };
    let mut spans: Vec<Span> = Vec::new();
    for (idx, (key, desc)) in hints.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled("  ·  ", Style::default().fg(theme().hint_sep)));
        }
        spans.push(Span::styled(
            (*key).to_string(),
            Style::default()
                .fg(theme().hint_key)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            (*desc).to_string(),
            Style::default().fg(theme().text_dim),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio(name: &str) -> McpForm {
        McpForm {
            name: name.into(),
            command: "fileio-mcp".into(),
            ..McpForm::blank(McpTransport::Stdio)
        }
    }

    fn http(name: &str) -> McpForm {
        McpForm {
            name: name.into(),
            url: "https://x.example/mcp".into(),
            ..McpForm::blank(McpTransport::Http)
        }
    }

    // --- status_display -------------------------------------------------------

    #[test]
    fn status_display_covers_all_six_states() {
        assert_eq!(status_display("running"), (StatusTone::Ok, "Running"));
        assert_eq!(status_display("stopped"), (StatusTone::Neutral, "Stopped"));
        assert_eq!(
            status_display("disabled"),
            (StatusTone::Neutral, "Disabled")
        );
        assert_eq!(
            status_display("needs_auth"),
            (StatusTone::Warn, "Sign in required")
        );
        assert_eq!(
            status_display("auth_expired"),
            (StatusTone::Warn, "Sign in expired")
        );
        assert_eq!(status_display("error"), (StatusTone::Error, "Error"));
    }

    #[test]
    fn status_display_unknown_is_neutral() {
        assert_eq!(
            status_display("teleporting"),
            (StatusTone::Neutral, "Unknown")
        );
        assert_eq!(status_display(""), (StatusTone::Neutral, "Unknown"));
    }

    // --- transport_chip -------------------------------------------------------

    #[test]
    fn transport_chip_http_is_remote_else_local() {
        assert_eq!(transport_chip("http"), "remote");
        assert_eq!(transport_chip("stdio"), "local");
        assert_eq!(transport_chip("something-new"), "local");
    }

    // --- parse_env ------------------------------------------------------------

    #[test]
    fn parse_env_reads_key_value_lines_in_order() {
        assert_eq!(
            parse_env("TOKEN=abc\nDEBUG=1"),
            vec![
                ("TOKEN".to_string(), "abc".to_string()),
                ("DEBUG".to_string(), "1".to_string()),
            ]
        );
    }

    #[test]
    fn parse_env_skips_blank_and_malformed_lines() {
        assert_eq!(
            parse_env("\n  \nNOVALUE\n=novalue\nOK=1\n"),
            vec![("OK".to_string(), "1".to_string())]
        );
    }

    #[test]
    fn parse_env_value_may_contain_equals() {
        assert_eq!(
            parse_env("QUERY=a=b=c"),
            vec![("QUERY".to_string(), "a=b=c".to_string())]
        );
    }

    #[test]
    fn parse_env_trims_key_and_value() {
        assert_eq!(
            parse_env("  KEY = val \n"),
            vec![("KEY".to_string(), "val".to_string())]
        );
    }

    // --- split_args / split_scopes -------------------------------------------

    #[test]
    fn split_args_splits_on_whitespace_runs() {
        assert_eq!(
            split_args("serve   --root  /data"),
            vec!["serve", "--root", "/data"]
        );
    }

    #[test]
    fn split_args_empty_is_empty() {
        assert!(split_args("   ").is_empty());
        assert!(split_args("").is_empty());
    }

    #[test]
    fn split_scopes_splits_on_whitespace_and_commas() {
        assert_eq!(split_scopes("a b,c ,  d"), vec!["a", "b", "c", "d"]);
        assert!(split_scopes("").is_empty());
    }

    // --- bearer_secret_ref ----------------------------------------------------

    #[test]
    fn bearer_secret_ref_appends_token_suffix() {
        assert_eq!(bearer_secret_ref("gmail"), "gmail_token");
    }

    // --- sanitize (control-char stripping) ------------------------------------

    #[test]
    fn sanitize_replaces_control_chars_with_space() {
        // An embedded ESC + newline + tab must not survive to the terminal.
        assert_eq!(sanitize("a\x1b[2Jb\nc\td"), "a [2Jb c d");
        assert_eq!(sanitize("plain-name_1"), "plain-name_1");
    }

    // --- mcp_secret_command (wire shape + redaction) --------------------------

    #[test]
    fn mcp_secret_command_wire_shape() {
        let cmd = mcp_secret_command("gmail_token".into(), "ya29.tok".into());
        let json = serde_json::to_string(&cmd).expect("serializes");
        assert_eq!(
            json,
            r#"{"set_mcp_secret":{"id":"gmail_token","value":"ya29.tok"}}"#
        );
    }

    #[test]
    fn mcp_secret_command_redacts_value_in_debug() {
        let cmd = mcp_secret_command("gmail_token".into(), "ya29.supersecret".into());
        let dump = format!("{cmd:?}");
        assert!(!dump.contains("ya29.supersecret"), "token leaked: {dump}");
    }

    // --- build: stdio ---------------------------------------------------------

    #[test]
    fn build_stdio_emits_exact_config_json() {
        let form = McpForm {
            args: "serve --root /data".into(),
            namespace: "files".into(),
            env: "TOKEN=abc\nDEBUG=1".into(),
            ..stdio("files")
        };
        let built = form.build().expect("builds");
        assert!(!built.editing);
        assert_eq!(built.name, "files");
        assert_eq!(built.secret, None);
        // env is a BTreeMap in the DTO → keys sorted (DEBUG before TOKEN).
        assert_eq!(
            built.config_json,
            r#"{"name":"files","enabled":true,"command":"fileio-mcp","args":["serve","--root","/data"],"namespace":"files","env":{"DEBUG":"1","TOKEN":"abc"}}"#
        );
    }

    #[test]
    fn build_stdio_omits_empty_optionals() {
        let built = stdio("bare").build().expect("builds");
        assert_eq!(
            built.config_json,
            r#"{"name":"bare","enabled":true,"command":"fileio-mcp"}"#
        );
    }

    #[test]
    fn build_carries_disabled_flag() {
        let form = McpForm {
            enabled: false,
            ..stdio("x")
        };
        let built = form.build().expect("builds");
        assert!(built.config_json.contains(r#""enabled":false"#));
    }

    // --- build: http bearer ---------------------------------------------------

    #[test]
    fn build_http_bearer_emits_config_and_secret() {
        let form = McpForm {
            url: "https://gmailmcp.googleapis.com/mcp/v1".into(),
            auth: McpAuthKind::Bearer,
            bearer_token: "  ya29.token \n".into(),
            ..http("gmail")
        };
        let built = form.build().expect("builds");
        assert_eq!(
            built.config_json,
            r#"{"name":"gmail","enabled":true,"http":{"url":"https://gmailmcp.googleapis.com/mcp/v1","auth_bearer_secret":"gmail_token"}}"#
        );
        // The token is trimmed and written under the `{name}_token` ref.
        assert_eq!(
            built.secret,
            Some(("gmail_token".to_string(), "ya29.token".to_string()))
        );
    }

    #[test]
    fn build_http_bearer_blank_token_writes_no_secret() {
        // Write-only: a blank token never wipes a stored token — but the config
        // still references the ref so the server stays honestly "bearer".
        let form = McpForm {
            auth: McpAuthKind::Bearer,
            bearer_token: "   ".into(),
            ..http("gmail")
        };
        let built = form.build().expect("builds");
        assert_eq!(built.secret, None);
        assert!(
            built
                .config_json
                .contains(r#""auth_bearer_secret":"gmail_token""#)
        );
    }

    // --- build: http oauth ----------------------------------------------------

    #[test]
    fn build_http_oauth_emits_account_ref_and_scopes() {
        let form = McpForm {
            url: "https://cal.example/mcp".into(),
            auth: McpAuthKind::OAuth,
            oauth_account: "work-google".into(),
            scopes: "calendar.read, calendar.write".into(),
            ..http("cal")
        };
        let built = form.build().expect("builds");
        // OAuth carries only the account ref + scopes — never a secret value.
        assert_eq!(built.secret, None);
        assert_eq!(
            built.config_json,
            r#"{"name":"cal","enabled":true,"http":{"url":"https://cal.example/mcp","oauth_account":"work-google","scopes":["calendar.read","calendar.write"]}}"#
        );
    }

    // --- build: validation ----------------------------------------------------

    #[test]
    fn build_requires_command_for_stdio() {
        let form = McpForm {
            command: "   ".into(),
            ..stdio("x")
        };
        assert!(form.build().is_err());
    }

    #[test]
    fn build_requires_url_for_http() {
        let form = McpForm {
            url: "".into(),
            ..http("x")
        };
        assert!(form.build().is_err());
    }

    #[test]
    fn build_requires_account_for_oauth() {
        let form = McpForm {
            auth: McpAuthKind::OAuth,
            oauth_account: "  ".into(),
            ..http("x")
        };
        assert!(form.build().is_err());
    }

    #[test]
    fn build_requires_valid_name_on_create() {
        assert!(stdio("").build().is_err());
        assert!(stdio("has space").build().is_err());
        assert!(stdio("ok-name_1").build().is_ok());
    }

    #[test]
    fn build_edit_does_not_revalidate_locked_name() {
        // On edit the name is the already-stored (locked) one, so build trusts
        // it rather than re-running the create-time slug check.
        let form = McpForm {
            editing: true,
            name: "already.there".into(),
            ..stdio("already.there")
        };
        let built = form.build().expect("builds");
        assert!(built.editing);
        assert_eq!(built.name, "already.there");
    }

    // --- from_view (edit prefill) --------------------------------------------

    #[test]
    fn from_view_prefills_stdio_editor() {
        let view = McpServerView {
            name: "files".into(),
            command: "fileio-mcp".into(),
            args: vec!["serve".into(), "--root".into(), "/data".into()],
            namespace: Some("files".into()),
            enabled: true,
            status: "running".into(),
            transport: "stdio".into(),
            target: "fileio-mcp".into(),
            ..Default::default()
        };
        let f = McpForm::from_view(&view);
        assert!(f.editing);
        assert_eq!(f.transport, McpTransport::Stdio);
        assert_eq!(f.name, "files");
        assert_eq!(f.command, "fileio-mcp");
        assert_eq!(f.args, "serve --root /data");
        assert_eq!(f.namespace, "files");
        // The view carries no env — never pre-filled.
        assert_eq!(f.env, "");
    }

    #[test]
    fn from_view_prefills_http_bearer_editor() {
        let view = McpServerView {
            name: "gh".into(),
            enabled: true,
            status: "running".into(),
            transport: "http".into(),
            target: "https://gh.example/mcp".into(),
            auth_kind: Some("bearer".into()),
            ..Default::default()
        };
        let f = McpForm::from_view(&view);
        assert_eq!(f.transport, McpTransport::Http);
        assert_eq!(f.auth, McpAuthKind::Bearer);
        assert_eq!(f.url, "https://gh.example/mcp");
        // Write-only: the token is never echoed / pre-filled.
        assert_eq!(f.bearer_token, "");
    }

    #[test]
    fn from_view_prefills_http_oauth_editor() {
        let view = McpServerView {
            name: "cal".into(),
            enabled: true,
            status: "needs_auth".into(),
            transport: "http".into(),
            target: "https://cal.example/mcp".into(),
            auth_kind: Some("oauth".into()),
            oauth_account_ref: Some("work-google".into()),
            oauth_scopes: vec!["calendar.read".into()],
            oauth_authorized: Some(false),
            ..Default::default()
        };
        let f = McpForm::from_view(&view);
        assert_eq!(f.transport, McpTransport::Http);
        assert_eq!(f.auth, McpAuthKind::OAuth);
        assert_eq!(f.url, "https://cal.example/mcp");
        assert_eq!(f.oauth_account, "work-google");
        assert_eq!(f.scopes, "calendar.read");
    }

    // --- transport / auth cyclers --------------------------------------------

    #[test]
    fn transport_cycle_wraps_both_ways() {
        assert_eq!(McpTransport::Stdio.cycle(1), McpTransport::Http);
        assert_eq!(McpTransport::Http.cycle(1), McpTransport::Stdio);
        assert_eq!(McpTransport::Stdio.cycle(-1), McpTransport::Http);
    }

    #[test]
    fn auth_cycle_wraps_three_ways() {
        assert_eq!(McpAuthKind::None.cycle(1), McpAuthKind::Bearer);
        assert_eq!(McpAuthKind::Bearer.cycle(1), McpAuthKind::OAuth);
        assert_eq!(McpAuthKind::OAuth.cycle(1), McpAuthKind::None);
        assert_eq!(McpAuthKind::None.cycle(-1), McpAuthKind::OAuth);
    }

    // --- fields_for (transport/auth-divergent field sets) --------------------

    #[test]
    fn fields_for_stdio_has_command_and_env_not_url() {
        let fields = fields_for(McpTransport::Stdio, McpAuthKind::None);
        assert!(fields.contains(&Field::Command));
        assert!(fields.contains(&Field::Env));
        assert!(!fields.contains(&Field::Url));
        assert!(!fields.contains(&Field::Auth));
    }

    #[test]
    fn fields_for_http_bearer_has_token_not_account() {
        let fields = fields_for(McpTransport::Http, McpAuthKind::Bearer);
        assert!(fields.contains(&Field::Url));
        assert!(fields.contains(&Field::BearerToken));
        assert!(!fields.contains(&Field::OAuthAccount));
        assert!(!fields.contains(&Field::Command));
    }

    #[test]
    fn fields_for_http_oauth_has_account_and_scopes() {
        let fields = fields_for(McpTransport::Http, McpAuthKind::OAuth);
        assert!(fields.contains(&Field::OAuthAccount));
        assert!(fields.contains(&Field::Scopes));
        assert!(!fields.contains(&Field::BearerToken));
    }

    // --- FormState round-trip (widget ⇄ pure model) --------------------------

    #[test]
    fn form_state_round_trips_stdio() {
        let pure = McpForm {
            args: "serve --root /data".into(),
            namespace: "files".into(),
            env: "TOKEN=abc\nDEBUG=1".into(),
            ..stdio("files")
        };
        let round = FormState::from_pure(pure.clone()).snapshot();
        assert_eq!(round, pure);
    }

    #[test]
    fn form_state_round_trips_http_bearer_including_masked_token() {
        // The bearer field is rendered masked but must still round-trip its
        // value back out for the build step.
        let pure = McpForm {
            url: "https://gmail.example/mcp".into(),
            auth: McpAuthKind::Bearer,
            bearer_token: "ya29.secret".into(),
            ..http("gmail")
        };
        let round = FormState::from_pure(pure.clone()).snapshot();
        assert_eq!(round, pure);
        assert_eq!(round.bearer_token, "ya29.secret");
    }

    #[test]
    fn form_state_round_trips_http_oauth() {
        let pure = McpForm {
            url: "https://cal.example/mcp".into(),
            auth: McpAuthKind::OAuth,
            oauth_account: "work-google".into(),
            scopes: "calendar.read calendar.write".into(),
            ..http("cal")
        };
        let round = FormState::from_pure(pure.clone()).snapshot();
        assert_eq!(round, pure);
    }

    #[test]
    fn form_state_blank_defaults_to_stdio_enabled() {
        let f = FormState::blank().snapshot();
        assert_eq!(f.transport, McpTransport::Stdio);
        assert!(f.enabled);
        assert!(!f.editing);
    }

    // --- headless render smoke (draw path, no terminal) -----------------------

    use ratatui::{Terminal, backend::TestBackend};

    /// Render `state` into an off-screen buffer and return its text content.
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

    /// A server whose name / target / detail / configure command all carry
    /// terminal control sequences, to prove the draw path sanitizes them.
    fn hostile_server() -> McpServerView {
        McpServerView {
            name: "evil\u{1b}[2Jbox".into(),
            enabled: true,
            status: "error".into(),
            transport: "http".into(),
            target: "https://h\u{1b}]0;x\u{07}/mcp".into(),
            detail: Some("kaboom\u{1b}c".into()),
            configure_command: vec![
                "adele-daemon".into(),
                "--mcp-oauth-login".into(),
                "cal".into(),
            ],
            ..Default::default()
        }
    }

    fn state_with(servers: Vec<McpServerView>, accounts: Vec<ServiceAccountView>) -> State {
        State {
            servers,
            accounts,
            selected: 0,
            mode: Mode::List,
            form: FormState::blank(),
            error: None,
            busy: None,
            closing: false,
        }
    }

    #[test]
    fn draw_list_sanitizes_hostile_daemon_text() {
        let state = state_with(vec![hostile_server()], Vec::new());
        let text = rendered(&state, 120, 20);
        // The ESC / BEL control bytes must never reach the terminal buffer.
        assert!(!text.contains('\u{1b}'), "ESC reached the buffer");
        assert!(!text.contains('\u{07}'), "BEL reached the buffer");
        // The visible (sanitized) name still renders.
        assert!(text.contains("evil"), "server name missing: {text}");
    }

    #[test]
    fn draw_all_modes_render_without_panicking() {
        let mut state = state_with(
            vec![hostile_server()],
            vec![ServiceAccountView {
                id: "work-google".into(),
                display_name: "Work Google".into(),
                ..Default::default()
            }],
        );
        state.error = Some("something failed".into());
        // List, then the http/oauth edit form, then both overlays — including a
        // cramped terminal (overlay centering must not underflow).
        let _ = rendered(&state, 120, 30);
        state.mode = Mode::Edit;
        state.form = FormState::from_pure(McpForm {
            auth: McpAuthKind::OAuth,
            oauth_account: "work-google".into(),
            ..http("cal")
        });
        let _ = rendered(&state, 120, 30);
        state.mode = Mode::RemoveConfirm;
        let _ = rendered(&state, 30, 8);
        state.mode = Mode::SignInInfo;
        let text = rendered(&state, 120, 30);
        assert!(
            text.contains("--mcp-oauth-login"),
            "sign-in command missing: {text}"
        );
    }

    #[test]
    fn draw_empty_list_shows_add_hint() {
        let state = state_with(Vec::new(), Vec::new());
        let text = rendered(&state, 120, 20);
        assert!(
            text.contains("no MCP servers"),
            "empty hint missing: {text}"
        );
    }
}
