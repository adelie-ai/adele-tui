//! Pre-chat profile picker.
//!
//! Renders a small modal-style screen listing saved profiles with shortcuts
//! to add or delete entries. Returns the chosen `Profile` (or `None` if the
//! user quit). Lives outside the chat UI so it doesn't widen the chat
//! state machine.

use std::io;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use desktop_assistant_client_common::TransportMode;
use futures::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use ratatui_textarea::{CursorMove, DataCursor, TextArea};

use crate::{
    credentials::{self, CredentialKind},
    profile::{Profile, ProfileStore},
};

use crate::theme::theme;

/// Outcome of running the picker.
pub enum PickerOutcome {
    /// User picked or created a profile to connect to.
    Selected(Profile),
    /// User pressed quit before selecting anything.
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    List,
    Form,
    DeleteConfirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Transport,
    Url,
    Subject,
    Username,
    Password,
}

const FIELD_ORDER: [Field; 6] = [
    Field::Name,
    Field::Transport,
    Field::Url,
    Field::Subject,
    Field::Username,
    Field::Password,
];

struct PickerState {
    store: ProfileStore,
    selected: usize,
    mode: Mode,
    error: Option<String>,
    form: FormState,
    /// Set when the user triggers OAuth from the form. The outer event
    /// loop drains this flag and awaits the async flow.
    oauth_pending: bool,
    /// Transient status line shown during OAuth flows.
    busy: Option<String>,
}

struct FormState {
    /// `Some(id)` when editing an existing profile; `None` for a new one.
    editing_id: Option<String>,
    /// Stable id for this form. For new profiles this is generated up
    /// front so OAuth flows can store tokens against it before save.
    form_id: String,
    focus: Field,
    name: TextArea<'static>,
    url: TextArea<'static>,
    subject: TextArea<'static>,
    username: TextArea<'static>,
    /// Password buffer — masked in the UI. Empty on edit means "leave the
    /// stored secret untouched"; non-empty replaces the stored value.
    password: TextArea<'static>,
    /// Whether this profile already has a stored password (set when
    /// editing). Drives the placeholder hint and decides whether to
    /// preserve or clear.
    had_stored_password: bool,
    /// Whether OAuth tokens are currently stored for this form's id.
    has_oauth_tokens: bool,
    transport: TransportMode,
}

impl FormState {
    fn empty() -> Self {
        let mut name = single_line_textarea();
        let mut url = single_line_textarea();
        url.insert_str("ws://127.0.0.1:11339/ws");
        url.move_cursor(CursorMove::End);
        let mut subject = single_line_textarea();
        subject.insert_str("desktop-tui");
        subject.move_cursor(CursorMove::End);
        let username = single_line_textarea();
        let mut password = single_line_textarea();
        password.set_mask_char('•');
        // Default focus on Name so users can just start typing.
        name.move_cursor(CursorMove::Head);
        // Pre-allocate an id so OAuth tokens (if any) can be written before
        // the user presses save.
        let seed = crate::profile::Profile::new(
            String::new(),
            TransportMode::Ws,
            String::new(),
            String::new(),
        );
        Self {
            editing_id: None,
            form_id: seed.id,
            focus: Field::Name,
            name,
            url,
            subject,
            username,
            password,
            had_stored_password: false,
            has_oauth_tokens: false,
            // UDS is the default connector for local use.
            transport: TransportMode::Uds,
        }
    }

    fn from_profile(profile: &Profile) -> Self {
        let mut form = Self::empty();
        form.editing_id = Some(profile.id.clone());
        form.form_id = profile.id.clone();
        form.name = single_line_textarea();
        form.name.insert_str(&profile.name);
        form.name.move_cursor(CursorMove::End);
        form.url = single_line_textarea();
        form.url.insert_str(&profile.ws_url);
        form.url.move_cursor(CursorMove::End);
        form.subject = single_line_textarea();
        form.subject.insert_str(&profile.ws_subject);
        form.subject.move_cursor(CursorMove::End);
        form.username = single_line_textarea();
        if let Some(user) = &profile.username {
            form.username.insert_str(user);
            form.username.move_cursor(CursorMove::End);
        }
        // Don't reveal stored password — leave the field empty and let the
        // submit logic preserve the existing secret unless the user types
        // a replacement.
        form.had_stored_password = profile.has_password;
        form.has_oauth_tokens = profile.has_jwt;
        form.transport = profile.transport;
        form
    }

    fn next_field(&mut self) {
        let pos = FIELD_ORDER
            .iter()
            .position(|f| *f == self.focus)
            .unwrap_or(0);
        self.focus = FIELD_ORDER[(pos + 1) % FIELD_ORDER.len()];
    }

    fn prev_field(&mut self) {
        let pos = FIELD_ORDER
            .iter()
            .position(|f| *f == self.focus)
            .unwrap_or(0);
        self.focus = FIELD_ORDER[(pos + FIELD_ORDER.len() - 1) % FIELD_ORDER.len()];
    }

    fn submit(&self) -> Result<SubmittedProfile, String> {
        let name = self.name.lines().join(" ").trim().to_string();
        if name.is_empty() {
            return Err("Name is required".into());
        }
        let ws_url = self.url.lines().join(" ").trim().to_string();
        if matches!(self.transport, TransportMode::Ws) && ws_url.is_empty() {
            return Err("URL is required for WebSocket transport".into());
        }
        let ws_subject = self.subject.lines().join(" ").trim().to_string();
        let username_raw = self.username.lines().join(" ").trim().to_string();
        let username = if username_raw.is_empty() {
            None
        } else {
            Some(username_raw)
        };
        let password_raw = self.password.lines().join("");
        // Empty password on edit = preserve the stored secret. On a fresh
        // profile (no editing_id), empty just means "no password set".
        let password_action = if password_raw.is_empty() {
            if self.editing_id.is_some() && self.had_stored_password {
                PasswordAction::PreserveExisting
            } else {
                PasswordAction::None
            }
        } else {
            PasswordAction::Set(password_raw)
        };

        Ok(SubmittedProfile {
            id: self.form_id.clone(),
            name,
            transport: self.transport,
            ws_url,
            ws_subject,
            username,
            password_action,
            has_oauth_tokens: self.has_oauth_tokens,
        })
    }
}

#[derive(Debug, Clone)]
enum PasswordAction {
    /// No password — clear any existing stored secret.
    None,
    /// Replace stored secret with this value.
    Set(String),
    /// Leave the stored secret as-is (only meaningful on edit).
    PreserveExisting,
}

#[derive(Debug, Clone)]
struct SubmittedProfile {
    id: String,
    name: String,
    transport: TransportMode,
    ws_url: String,
    ws_subject: String,
    username: Option<String>,
    password_action: PasswordAction,
    has_oauth_tokens: bool,
}

fn single_line_textarea() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_cursor_line_style(Style::default());
    ta
}

/// Forward cycle for the transport toggle: Local → WebSocket → D-Bus → Local.
fn transport_next(t: TransportMode) -> TransportMode {
    match t {
        TransportMode::Uds => TransportMode::Ws,
        TransportMode::Ws => TransportMode::Dbus,
        TransportMode::Dbus => TransportMode::Uds,
    }
}

/// Backward cycle (the reverse of [`transport_next`]).
fn transport_prev(t: TransportMode) -> TransportMode {
    match t {
        TransportMode::Uds => TransportMode::Dbus,
        TransportMode::Ws => TransportMode::Uds,
        TransportMode::Dbus => TransportMode::Ws,
    }
}

/// Run the picker until the user selects, creates, or quits.
pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    store: ProfileStore,
) -> io::Result<(PickerOutcome, ProfileStore)> {
    let initial_mode = if store.profiles.is_empty() {
        Mode::Form
    } else {
        Mode::List
    };
    let initial_selected = store.last_used_index().unwrap_or(0);
    let mut state = PickerState {
        selected: initial_selected,
        store,
        mode: initial_mode,
        error: None,
        form: FormState::empty(),
        oauth_pending: false,
        busy: None,
    };
    if state.selected >= state.store.profiles.len() {
        state.selected = state.store.profiles.len().saturating_sub(1);
    }

    let mut events = crossterm::event::EventStream::new();
    loop {
        terminal.draw(|f| draw(f, &state))?;

        if state.oauth_pending {
            state.oauth_pending = false;
            run_oauth_for_form(&mut state, terminal).await;
            continue;
        }

        let evt = match events.next().await {
            Some(Ok(e)) => e,
            Some(Err(_)) | None => return Ok((PickerOutcome::Cancelled, state.store)),
        };
        let Event::Key(key) = evt else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }

        match state.mode {
            Mode::List => {
                if let Some(outcome) = handle_list_key(&mut state, key) {
                    return Ok((outcome, state.store));
                }
            }
            Mode::Form => handle_form_key(&mut state, key),
            Mode::DeleteConfirm => handle_delete_key(&mut state, key),
        }
    }
}

fn handle_list_key(state: &mut PickerState, key: KeyEvent) -> Option<PickerOutcome> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('q'), m) if m.is_empty() => Some(PickerOutcome::Cancelled),
        (KeyCode::Esc, _) => Some(PickerOutcome::Cancelled),
        (KeyCode::Char('j') | KeyCode::Down, m) if m.is_empty() => {
            advance_selection(state, 1);
            None
        }
        (KeyCode::Char('k') | KeyCode::Up, m) if m.is_empty() => {
            advance_selection(state, -1);
            None
        }
        (KeyCode::Enter, m) if m.is_empty() => {
            if let Some(profile) = state.store.profiles.get(state.selected).cloned() {
                state.store.mark_used(&profile.id);
                let _ = state.store.save();
                Some(PickerOutcome::Selected(profile))
            } else {
                None
            }
        }
        (KeyCode::Char('a') | KeyCode::Char('+'), m) if m.is_empty() => {
            state.form = FormState::empty();
            state.error = None;
            state.mode = Mode::Form;
            None
        }
        (KeyCode::Char('e'), m) if m.is_empty() => {
            if let Some(profile) = state.store.profiles.get(state.selected) {
                state.form = FormState::from_profile(profile);
                state.error = None;
                state.mode = Mode::Form;
            }
            None
        }
        (KeyCode::Char('d'), m) if m.is_empty() => {
            if state.store.profiles.get(state.selected).is_some() {
                state.mode = Mode::DeleteConfirm;
            }
            None
        }
        _ => None,
    }
}

fn advance_selection(state: &mut PickerState, delta: i32) {
    let len = state.store.profiles.len();
    if len == 0 {
        return;
    }
    let mut idx = state.selected as i32 + delta;
    if idx < 0 {
        idx = (len as i32) - 1;
    }
    if idx >= len as i32 {
        idx = 0;
    }
    state.selected = idx as usize;
}

fn handle_form_key(state: &mut PickerState, key: KeyEvent) {
    // Ctrl+L: launch OAuth flow against the URL currently in the form.
    if key.code == KeyCode::Char('l') && key.modifiers.contains(KeyModifiers::CONTROL) {
        let url = state.form.url.lines().join(" ").trim().to_string();
        if matches!(state.form.transport, TransportMode::Ws) && !url.is_empty() {
            state.oauth_pending = true;
            state.error = None;
        } else {
            state.error =
                Some("OAuth requires a WebSocket URL — fill in the URL field first".into());
        }
        return;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => {
            state.error = None;
            state.mode = if state.store.profiles.is_empty() {
                // No way back to a list — Esc with nothing to go to means
                // exit the picker entirely; we surface that on the next
                // iteration via the outer loop. Simpler: treat as a noop
                // so the user stays in the form. Add a dedicated quit hint.
                Mode::Form
            } else {
                Mode::List
            };
            // Clear form on cancel of edit so a re-entry starts fresh.
            state.form = FormState::empty();
        }
        (KeyCode::Tab, _) => state.form.next_field(),
        (KeyCode::BackTab, _) => state.form.prev_field(),
        (KeyCode::Up, m) if m.is_empty() => state.form.prev_field(),
        (KeyCode::Down, m) if m.is_empty() => state.form.next_field(),
        (KeyCode::Enter, m) if m.is_empty() => match state.form.submit() {
            Ok(submitted) => {
                if let Err(e) = apply_submission(state, submitted) {
                    state.error = Some(e);
                } else {
                    state.error = None;
                    state.mode = Mode::List;
                    state.form = FormState::empty();
                }
            }
            Err(msg) => state.error = Some(msg),
        },
        // Transport field cycles Local → WebSocket → D-Bus with left/right
        // (space steps forward).
        (KeyCode::Right | KeyCode::Char(' '), _) if state.form.focus == Field::Transport => {
            state.form.transport = transport_next(state.form.transport);
        }
        (KeyCode::Left, _) if state.form.focus == Field::Transport => {
            state.form.transport = transport_prev(state.form.transport);
        }
        _ => match state.form.focus {
            Field::Name => {
                state.form.name.input(key);
            }
            Field::Url => {
                state.form.url.input(key);
            }
            Field::Subject => {
                state.form.subject.input(key);
            }
            Field::Username => {
                state.form.username.input(key);
            }
            Field::Password => {
                state.form.password.input(key);
            }
            Field::Transport => {}
        },
    }
}

/// Run the OAuth flow for the form currently being edited. Updates the
/// busy status line as it progresses; the outer loop redraws each step.
async fn run_oauth_for_form(
    state: &mut PickerState,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) {
    let url = state.form.url.lines().join(" ").trim().to_string();

    state.busy = Some("Discovering auth config...".into());
    let _ = terminal.draw(|f| draw(f, state));

    let discovery = match crate::oauth::discover_auth_config(&url).await {
        Ok(d) => d,
        Err(e) => {
            state.busy = None;
            state.error = Some(format!("Auth discovery failed: {e}"));
            return;
        }
    };

    let oidc = match discovery.oidc {
        Some(o)
            if crate::oauth::supports_oauth(&crate::oauth::AuthDiscovery {
                methods: discovery.methods.clone(),
                oidc: Some(o.clone()),
            }) =>
        {
            o
        }
        _ => {
            state.busy = None;
            state.error = Some("Server does not advertise OAuth/OIDC support".into());
            return;
        }
    };

    state.busy = Some("Browser opened — complete sign-in to continue...".into());
    let _ = terminal.draw(|f| draw(f, state));

    let tokens = match crate::oauth::run_oauth_flow(&oidc).await {
        Ok(t) => t,
        Err(e) => {
            state.busy = None;
            state.error = Some(format!("Sign-in failed: {e}"));
            return;
        }
    };

    let id = state.form.form_id.clone();
    if let Err(e) = credentials::store(&id, CredentialKind::Jwt, &tokens.access_token) {
        state.busy = None;
        state.error = Some(format!("Could not store access token: {e}"));
        return;
    }
    if let Some(refresh) = tokens.refresh_token.as_deref() {
        if let Err(e) = credentials::store(&id, CredentialKind::OauthRefresh, refresh) {
            // Non-fatal — access token works for now, but warn so the user
            // knows refresh is unavailable.
            state.error = Some(format!("Stored access token but refresh failed: {e}"));
        }
    } else {
        // No refresh token offered — clear any stale entry.
        let _ = credentials::delete(&id, CredentialKind::OauthRefresh);
    }

    state.form.has_oauth_tokens = true;
    state.busy = None;
    if state.error.is_none() {
        state.error = Some("Signed in — save the profile to keep these tokens".into());
    }
}

/// Persist a submitted profile: update keyring, write profile, save store,
/// reselect. Returns an error string suitable for the UI on failure.
fn apply_submission(state: &mut PickerState, submitted: SubmittedProfile) -> Result<(), String> {
    let editing = state.store.profiles.iter().any(|p| p.id == submitted.id);

    let mut has_password = match &submitted.password_action {
        PasswordAction::Set(_) => true,
        PasswordAction::PreserveExisting => true,
        PasswordAction::None => false,
    };

    // Apply password change first; if keyring fails, surface it before we
    // touch the on-disk store so state stays consistent.
    match &submitted.password_action {
        PasswordAction::Set(secret) => {
            if let Err(e) = credentials::store(&submitted.id, CredentialKind::Password, secret) {
                return Err(format!("Could not store password: {e}"));
            }
        }
        PasswordAction::PreserveExisting => {
            // Keep the keyring as-is; nothing to do.
        }
        PasswordAction::None => {
            let _ = credentials::delete(&submitted.id, CredentialKind::Password);
            has_password = false;
        }
    }

    let new_profile = Profile {
        id: submitted.id.clone(),
        name: submitted.name,
        transport: submitted.transport,
        ws_url: submitted.ws_url,
        ws_subject: submitted.ws_subject,
        // Saved Local profiles use the daemon's default socket; custom socket
        // paths are a CLI-only concern (`adele --socket <path>`).
        socket_path: None,
        username: submitted.username,
        has_password,
        has_jwt: submitted.has_oauth_tokens,
    };

    if editing {
        if let Some(existing) = state
            .store
            .profiles
            .iter_mut()
            .find(|p| p.id == new_profile.id)
        {
            *existing = new_profile.clone();
        }
    } else {
        state.store.add(new_profile.clone());
    }
    state
        .store
        .save()
        .map_err(|e| format!("Save failed: {e}"))?;
    state.selected = state
        .store
        .profiles
        .iter()
        .position(|p| p.id == new_profile.id)
        .unwrap_or(state.selected);
    Ok(())
}

fn handle_delete_key(state: &mut PickerState, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter, _) => {
            if let Some(profile) = state.store.profiles.get(state.selected).cloned() {
                state.store.remove(&profile.id);
                let _ = state.store.save();
                if state.selected >= state.store.profiles.len() {
                    state.selected = state.store.profiles.len().saturating_sub(1);
                }
            }
            state.mode = if state.store.profiles.is_empty() {
                Mode::Form
            } else {
                Mode::List
            };
            if matches!(state.mode, Mode::Form) {
                state.form = FormState::empty();
            }
        }
        // A destructive confirm is dismissed only by an explicit cancel
        // (n/Esc); any other key is ignored rather than silently closing it.
        (KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc, _) => {
            state.mode = Mode::List;
        }
        _ => {}
    }
}

fn draw(f: &mut Frame, state: &PickerState) {
    let area = f.area();
    f.render_widget(Clear, area);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(f, layout[0]);
    match state.mode {
        Mode::List => draw_list(f, state, layout[1]),
        Mode::Form => draw_form(f, state, layout[1]),
        Mode::DeleteConfirm => {
            draw_list(f, state, layout[1]);
            draw_delete_overlay(f, state, area);
        }
    }
    draw_error(f, state, layout[2]);
    draw_hints(f, state, layout[3]);

    if matches!(state.mode, Mode::Form) {
        // Keep cursor blinking in the focused text field.
        position_cursor_in_form(f, state, layout[1]);
    }
}

fn draw_header(f: &mut Frame, area: Rect) {
    let title = Paragraph::new(vec![
        Line::from(Span::styled(
            "Adele connection profiles",
            Style::default()
                .fg(theme().title)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Pick a daemon to connect to, or create a new profile.",
            Style::default().fg(theme().text_dim),
        )),
    ]);
    f.render_widget(title, area);
}

fn draw_list(f: &mut Frame, state: &PickerState, area: Rect) {
    let items: Vec<ListItem> = if state.store.profiles.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "(no saved profiles — press 'a' to add one)",
            Style::default().fg(theme().text_dim),
        )))]
    } else {
        state
            .store
            .profiles
            .iter()
            .enumerate()
            .map(|(idx, p)| {
                let mut spans: Vec<Span<'static>> = Vec::new();
                if state.store.last_used_index() == Some(idx) {
                    spans.push(Span::styled("★ ", Style::default().fg(theme().pinned)));
                }
                spans.push(Span::styled(p.display_label(), Style::default()));
                ListItem::new(Line::from(spans))
            })
            .collect()
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme().border))
                .title(Line::from(Span::styled(
                    "Profiles",
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
    if !state.store.profiles.is_empty() {
        list_state.select(Some(state.selected));
    }
    f.render_stateful_widget(list, area, &mut list_state);
}

fn draw_form(f: &mut Frame, state: &PickerState, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme().border))
        .title(Line::from(Span::styled(
            if state.form.editing_id.is_some() {
                "Edit profile"
            } else {
                "New profile"
            },
            Style::default()
                .fg(theme().title)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(inner);

    draw_field_label(f, rows[0], "Name", state.form.focus == Field::Name);
    draw_text_field(
        f,
        rows[1],
        &state.form.name,
        state.form.focus == Field::Name,
    );

    draw_field_label(
        f,
        rows[2],
        "Transport (←/→ or Space to toggle)",
        state.form.focus == Field::Transport,
    );
    draw_transport_toggle(f, rows[3], state);

    draw_field_label(f, rows[4], "URL", state.form.focus == Field::Url);
    draw_text_field(f, rows[5], &state.form.url, state.form.focus == Field::Url);

    draw_field_label(
        f,
        rows[6],
        "JWT subject",
        state.form.focus == Field::Subject,
    );
    draw_text_field(
        f,
        rows[7],
        &state.form.subject,
        state.form.focus == Field::Subject,
    );

    draw_field_label(
        f,
        rows[8],
        "Username (optional)",
        state.form.focus == Field::Username,
    );
    draw_text_field(
        f,
        rows[9],
        &state.form.username,
        state.form.focus == Field::Username,
    );

    let password_label = if state.form.had_stored_password {
        "Password (•••• stored — leave blank to keep)"
    } else {
        "Password (optional, masked)"
    };
    draw_field_label(
        f,
        rows[10],
        password_label,
        state.form.focus == Field::Password,
    );
    draw_text_field(
        f,
        rows[11],
        &state.form.password,
        state.form.focus == Field::Password,
    );
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

fn draw_transport_toggle(f: &mut Frame, area: Rect, state: &PickerState) {
    let focused = state.form.focus == Field::Transport;
    let border_color = if focused {
        theme().border_active
    } else {
        theme().border
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let render_chip = |label: &str, selected: bool| {
        let style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(theme().border_active)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme().text_dim)
        };
        Span::styled(format!(" {label} "), style)
    };

    let line = Line::from(vec![
        render_chip("Local", state.form.transport == TransportMode::Uds),
        Span::styled("  ", Style::default()),
        render_chip("WebSocket", state.form.transport == TransportMode::Ws),
        Span::styled("  ", Style::default()),
        render_chip("D-Bus", state.form.transport == TransportMode::Dbus),
    ]);
    f.render_widget(Paragraph::new(line), inner);
}

fn draw_delete_overlay(f: &mut Frame, state: &PickerState, area: Rect) {
    let label = state
        .store
        .profiles
        .get(state.selected)
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "this profile".to_string());
    let popup_width = 60.min(area.width.saturating_sub(4));
    let popup_height = 5.min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_width)) / 2,
        y: area.y + (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    };
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme().error))
        .title(Line::from(Span::styled(
            "Delete profile",
            Style::default()
                .fg(theme().error_text)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let body = Paragraph::new(vec![
        Line::from(Span::styled(
            format!("Delete \"{label}\"?"),
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

fn draw_error(f: &mut Frame, state: &PickerState, area: Rect) {
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

fn draw_hints(f: &mut Frame, state: &PickerState, area: Rect) {
    let hints: &[(&str, &str)] = match state.mode {
        Mode::List => &[
            ("Enter", "connect"),
            ("a", "add"),
            ("e", "edit"),
            ("d", "delete"),
            ("q/Esc", "quit"),
        ],
        Mode::Form => &[
            ("Tab", "next field"),
            ("Enter", "save"),
            ("Ctrl+L", "OAuth sign-in"),
            ("Esc", "back"),
        ],
        Mode::DeleteConfirm => &[("y/Enter", "confirm"), ("n/Esc", "cancel")],
    };

    let mut spans: Vec<Span> = Vec::with_capacity(hints.len() * 4);
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

fn position_cursor_in_form(f: &mut Frame, state: &PickerState, area: Rect) {
    // Re-derive the inner box of the focused field's text area. The form
    // layout is mirrored from `draw_form`; we use the same constraints.
    let inner = Block::default().borders(Borders::ALL).inner(area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(inner);

    let (textarea, row_idx) = match state.form.focus {
        Field::Name => (&state.form.name, 1),
        Field::Url => (&state.form.url, 5),
        Field::Subject => (&state.form.subject, 7),
        Field::Username => (&state.form.username, 9),
        Field::Password => (&state.form.password, 11),
        Field::Transport => return,
    };

    let row = rows[row_idx];
    let inner_row = Block::default().borders(Borders::ALL).inner(row);
    let DataCursor(cursor_row, cursor_col) = textarea.cursor();
    let x = inner_row.x + cursor_col.min(inner_row.width.saturating_sub(1) as usize) as u16;
    let y = inner_row.y + cursor_row.min(inner_row.height.saturating_sub(1) as usize) as u16;
    f.set_cursor_position((x, y));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn make_state(profiles: Vec<Profile>) -> PickerState {
        let mut store = ProfileStore::default();
        for p in profiles {
            store.add(p);
        }
        PickerState {
            selected: 0,
            store,
            mode: Mode::List,
            error: None,
            form: FormState::empty(),
            oauth_pending: false,
            busy: None,
        }
    }

    #[test]
    fn list_q_returns_cancelled() {
        let mut state = make_state(vec![]);
        let outcome = handle_list_key(&mut state, key(KeyCode::Char('q')));
        assert!(matches!(outcome, Some(PickerOutcome::Cancelled)));
    }

    #[test]
    fn list_enter_returns_selected_profile() {
        let p = Profile::new(
            "Local".into(),
            TransportMode::Ws,
            "ws://x".into(),
            "s".into(),
        );
        let id = p.id.clone();
        let mut state = make_state(vec![p]);
        let outcome = handle_list_key(&mut state, key(KeyCode::Enter));
        match outcome {
            Some(PickerOutcome::Selected(picked)) => assert_eq!(picked.id, id),
            _ => panic!("expected Selected outcome"),
        }
    }

    #[test]
    fn list_a_enters_form_mode() {
        let mut state = make_state(vec![]);
        handle_list_key(&mut state, key(KeyCode::Char('a')));
        assert_eq!(state.mode, Mode::Form);
        assert!(state.form.editing_id.is_none());
    }

    #[test]
    fn form_tab_cycles_focus() {
        let mut state = make_state(vec![]);
        state.mode = Mode::Form;
        assert_eq!(state.form.focus, Field::Name);
        handle_form_key(&mut state, key(KeyCode::Tab));
        assert_eq!(state.form.focus, Field::Transport);
        handle_form_key(&mut state, key(KeyCode::Tab));
        assert_eq!(state.form.focus, Field::Url);
    }

    #[test]
    fn new_form_defaults_to_local_transport() {
        let state = make_state(vec![]);
        assert_eq!(state.form.transport, TransportMode::Uds);
    }

    #[test]
    fn form_transport_cycles_through_all_three_with_arrows() {
        let mut state = make_state(vec![]);
        state.mode = Mode::Form;
        state.form.focus = Field::Transport;
        // Default is Local (UDS); Right steps forward through the cycle.
        assert_eq!(state.form.transport, TransportMode::Uds);
        handle_form_key(&mut state, key(KeyCode::Right));
        assert_eq!(state.form.transport, TransportMode::Ws);
        handle_form_key(&mut state, key(KeyCode::Right));
        assert_eq!(state.form.transport, TransportMode::Dbus);
        handle_form_key(&mut state, key(KeyCode::Right));
        assert_eq!(state.form.transport, TransportMode::Uds);
        // Left steps backward.
        handle_form_key(&mut state, key(KeyCode::Left));
        assert_eq!(state.form.transport, TransportMode::Dbus);
    }

    #[test]
    fn form_submit_with_blank_name_records_error() {
        let mut state = make_state(vec![]);
        state.mode = Mode::Form;
        // Name is empty by default.
        handle_form_key(&mut state, key(KeyCode::Enter));
        assert!(state.error.is_some());
        assert_eq!(state.mode, Mode::Form);
    }

    #[test]
    fn delete_confirm_y_removes_profile() {
        let p = Profile::new(
            "Local".into(),
            TransportMode::Ws,
            "ws://x".into(),
            "s".into(),
        );
        let mut state = make_state(vec![p]);
        state.mode = Mode::DeleteConfirm;
        handle_delete_key(&mut state, key(KeyCode::Char('y')));
        assert!(state.store.profiles.is_empty());
        // Falls back to Form when nothing left.
        assert_eq!(state.mode, Mode::Form);
    }

    #[test]
    fn delete_confirm_n_cancels() {
        let p = Profile::new(
            "Local".into(),
            TransportMode::Ws,
            "ws://x".into(),
            "s".into(),
        );
        let mut state = make_state(vec![p]);
        state.mode = Mode::DeleteConfirm;
        handle_delete_key(&mut state, key(KeyCode::Char('n')));
        assert_eq!(state.store.profiles.len(), 1);
        assert_eq!(state.mode, Mode::List);
    }

    #[test]
    fn delete_confirm_esc_cancels() {
        let p = Profile::new(
            "Local".into(),
            TransportMode::Ws,
            "ws://x".into(),
            "s".into(),
        );
        let mut state = make_state(vec![p]);
        state.mode = Mode::DeleteConfirm;
        handle_delete_key(&mut state, key(KeyCode::Esc));
        assert_eq!(state.store.profiles.len(), 1);
        assert_eq!(state.mode, Mode::List);
    }

    #[test]
    fn delete_confirm_stray_key_is_ignored() {
        // A key that is neither confirm (y/Enter) nor cancel (n/Esc) must
        // leave the overlay up rather than silently dismissing it.
        let p = Profile::new(
            "Local".into(),
            TransportMode::Ws,
            "ws://x".into(),
            "s".into(),
        );
        let mut state = make_state(vec![p]);
        state.mode = Mode::DeleteConfirm;
        handle_delete_key(&mut state, key(KeyCode::Char('x')));
        assert_eq!(state.store.profiles.len(), 1);
        assert_eq!(state.mode, Mode::DeleteConfirm);
    }
}
