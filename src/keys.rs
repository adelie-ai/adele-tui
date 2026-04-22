use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, InputMode, Screen};

/// High-level actions the main loop can dispatch. The dispatch table lives in
/// `handle_action` (main.rs); this module only decides "what semantic action
/// does this keystroke mean *right now*". The `right now` depends on the
/// active [`Screen`], any overlay popup, and the textarea's [`InputMode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,

    // Chat-list navigation
    NextConversation,
    PreviousConversation,
    OpenConversation,
    DeleteConversation,
    ArchiveConversation,
    NewConversation,

    // Chat input / streaming
    EnterEditMode,
    ExitEditMode,
    SubmitPrompt,
    InsertNewline,
    ScrollUp,
    ScrollDown,
    ScrollToBottom,
    ToggleShowArchived,

    // Screen switches
    OpenConnectionsView,
    OpenPurposesView,
    BackToChat,

    // Connections view
    ConnectionsNext,
    ConnectionsPrevious,
    ConnectionsAdd,
    ConnectionsConfigure,
    ConnectionsRemove,
    ConnectionsRefreshModels,
    ConnectionsFormSubmit,
    ConnectionsFormCancel,
    ConnectionsFormNextField,
    ConnectionsFormPreviousField,
    ConnectionsFormCycleKindNext,
    ConnectionsFormCycleKindPrev,
    ConnectionsFormInsertChar(char),
    ConnectionsFormBackspace,
    ConnectionsFormToggleAutoPull,
    ConnectionsDeleteConfirm,
    ConnectionsDeleteForce,
    ConnectionsDeleteCancel,

    // Purposes view
    PurposesNext,
    PurposesPrevious,
    PurposesEdit,
    PurposesEditorSubmit,
    PurposesEditorCancel,
    PurposesEditorNextField,
    PurposesEditorPreviousField,
    PurposesEditorInsertChar(char),
    PurposesEditorBackspace,

    // Model selector popup
    OpenModelSelector,
    ModelSelectorNext,
    ModelSelectorPrevious,
    ModelSelectorConfirm,
    ModelSelectorCancel,
    ModelSelectorRefresh,
}

/// Route a key event to an action, given the app's current screen / overlay
/// / input mode. Returns `None` for keys that should be forwarded to the
/// underlying widget (textarea) or ignored outright.
pub fn route_key(key: KeyEvent, app: &App) -> Option<Action> {
    // Ctrl-M always opens the model selector from the chat screen.
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && key.code == KeyCode::Char('m') && matches!(app.screen, Screen::Chat) {
        return Some(Action::OpenModelSelector);
    }

    // Model selector popup grabs every key when it's open.
    if app.model_selector.open {
        return route_model_selector(key);
    }

    match app.screen {
        Screen::Chat => route_chat(key, app),
        Screen::Connections => route_connections(key, app),
        Screen::Purposes => route_purposes(key, app),
    }
}

fn route_chat(key: KeyEvent, app: &App) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // Scroll shortcuts work in any chat sub-mode
    if ctrl {
        if matches!(app.mode, InputMode::Editing) && matches!(key.code, KeyCode::Char('j')) {
            return Some(Action::InsertNewline);
        }
        return match key.code {
            KeyCode::Char('u') => Some(Action::ScrollUp),
            KeyCode::Char('d') => Some(Action::ScrollDown),
            KeyCode::Char('e') => Some(Action::ScrollToBottom),
            _ => None,
        };
    }

    match app.mode {
        InputMode::Normal => {
            if alt || key.modifiers.intersects(KeyModifiers::META) {
                return None;
            }
            if key.code == KeyCode::Enter {
                return Some(Action::OpenConversation);
            }
            match key.code {
                KeyCode::Char('q') => Some(Action::Quit),
                KeyCode::Char('j') | KeyCode::Down => Some(Action::NextConversation),
                KeyCode::Char('k') | KeyCode::Up => Some(Action::PreviousConversation),
                KeyCode::Char('d') => Some(Action::DeleteConversation),
                KeyCode::Char('n') => Some(Action::NewConversation),
                KeyCode::Char('a') => Some(Action::ToggleShowArchived),
                KeyCode::Char('A') => Some(Action::ArchiveConversation),
                KeyCode::Char('i') => Some(Action::EnterEditMode),
                KeyCode::Char('S') => Some(Action::OpenConnectionsView),
                KeyCode::Char('P') => Some(Action::OpenPurposesView),
                KeyCode::PageUp => Some(Action::ScrollUp),
                KeyCode::PageDown => Some(Action::ScrollDown),
                KeyCode::End => Some(Action::ScrollToBottom),
                _ => None,
            }
        }
        InputMode::Editing => match key.code {
            KeyCode::Enter => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    return Some(Action::InsertNewline);
                }
                if key.modifiers.is_empty() {
                    return Some(Action::SubmitPrompt);
                }
                None
            }
            KeyCode::Char('\n') | KeyCode::Char('\r') => Some(Action::InsertNewline),
            KeyCode::Esc => Some(Action::ExitEditMode),
            KeyCode::PageUp => Some(Action::ScrollUp),
            KeyCode::PageDown => Some(Action::ScrollDown),
            KeyCode::End if key.modifiers.contains(KeyModifiers::SHIFT) => {
                Some(Action::ScrollToBottom)
            }
            _ => None,
        },
    }
}

fn route_connections(key: KeyEvent, app: &App) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // Delete-confirm overlay takes priority.
    if app.connections_view.delete.is_some() {
        return match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                Some(Action::ConnectionsDeleteConfirm)
            }
            KeyCode::Char('f') | KeyCode::Char('F') => Some(Action::ConnectionsDeleteForce),
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                Some(Action::ConnectionsDeleteCancel)
            }
            _ => None,
        };
    }

    // Configure form.
    if app.connections_view.form.is_some() {
        if ctrl {
            return None;
        }
        return route_connection_form(key, app);
    }

    // List mode.
    if ctrl {
        return None;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Some(Action::BackToChat),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::ConnectionsNext),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::ConnectionsPrevious),
        KeyCode::Char('a') => Some(Action::ConnectionsAdd),
        KeyCode::Char('c') | KeyCode::Enter => Some(Action::ConnectionsConfigure),
        KeyCode::Char('d') => Some(Action::ConnectionsRemove),
        KeyCode::Char('r') => Some(Action::ConnectionsRefreshModels),
        _ => None,
    }
}

fn route_connection_form(key: KeyEvent, app: &App) -> Option<Action> {
    let form = app.connections_view.form.as_ref()?;
    match key.code {
        KeyCode::Esc => Some(Action::ConnectionsFormCancel),
        KeyCode::Tab => Some(Action::ConnectionsFormNextField),
        KeyCode::BackTab => Some(Action::ConnectionsFormPreviousField),
        KeyCode::Enter => Some(Action::ConnectionsFormSubmit),
        KeyCode::Backspace => Some(Action::ConnectionsFormBackspace),
        // On the kind picker row, left/right cycle through connector types.
        KeyCode::Left if form.is_on_kind() => Some(Action::ConnectionsFormCycleKindPrev),
        KeyCode::Right if form.is_on_kind() => Some(Action::ConnectionsFormCycleKindNext),
        KeyCode::Char(' ') if form.current_field().is_some_and(|f| f.is_toggle()) => {
            Some(Action::ConnectionsFormToggleAutoPull)
        }
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(Action::ConnectionsFormInsertChar(ch))
        }
        _ => None,
    }
}

fn route_purposes(key: KeyEvent, app: &App) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if app.purposes_view.editor.is_some() {
        if ctrl {
            return None;
        }
        return match key.code {
            KeyCode::Esc => Some(Action::PurposesEditorCancel),
            KeyCode::Tab => Some(Action::PurposesEditorNextField),
            KeyCode::BackTab => Some(Action::PurposesEditorPreviousField),
            KeyCode::Enter => Some(Action::PurposesEditorSubmit),
            KeyCode::Backspace => Some(Action::PurposesEditorBackspace),
            KeyCode::Char(ch) => Some(Action::PurposesEditorInsertChar(ch)),
            _ => None,
        };
    }
    if ctrl {
        return None;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Some(Action::BackToChat),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::PurposesNext),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::PurposesPrevious),
        KeyCode::Char('c') | KeyCode::Enter => Some(Action::PurposesEdit),
        _ => None,
    }
}

fn route_model_selector(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => Some(Action::ModelSelectorCancel),
        KeyCode::Enter => Some(Action::ModelSelectorConfirm),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::ModelSelectorNext),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::ModelSelectorPrevious),
        KeyCode::Char('r') => Some(Action::ModelSelectorRefresh),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn key_with_mod(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn chat_normal() -> App {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.mode = InputMode::Normal;
        app
    }

    fn chat_editing() -> App {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.mode = InputMode::Editing;
        app
    }

    #[test]
    fn chat_normal_q_quits() {
        assert_eq!(
            route_key(key(KeyCode::Char('q')), &chat_normal()),
            Some(Action::Quit)
        );
    }

    #[test]
    fn chat_normal_capital_s_opens_connections() {
        assert_eq!(
            route_key(key(KeyCode::Char('S')), &chat_normal()),
            Some(Action::OpenConnectionsView)
        );
    }

    #[test]
    fn chat_normal_capital_p_opens_purposes() {
        assert_eq!(
            route_key(key(KeyCode::Char('P')), &chat_normal()),
            Some(Action::OpenPurposesView)
        );
    }

    #[test]
    fn chat_ctrl_m_opens_selector() {
        assert_eq!(
            route_key(
                key_with_mod(KeyCode::Char('m'), KeyModifiers::CONTROL),
                &chat_normal()
            ),
            Some(Action::OpenModelSelector)
        );
    }

    #[test]
    fn ctrl_m_from_editing_mode_also_opens_selector() {
        assert_eq!(
            route_key(
                key_with_mod(KeyCode::Char('m'), KeyModifiers::CONTROL),
                &chat_editing()
            ),
            Some(Action::OpenModelSelector)
        );
    }

    #[test]
    fn connections_a_adds() {
        let mut app = App::new();
        app.screen = Screen::Connections;
        assert_eq!(
            route_key(key(KeyCode::Char('a')), &app),
            Some(Action::ConnectionsAdd)
        );
    }

    #[test]
    fn connections_d_prompts_remove() {
        let mut app = App::new();
        app.screen = Screen::Connections;
        assert_eq!(
            route_key(key(KeyCode::Char('d')), &app),
            Some(Action::ConnectionsRemove)
        );
    }

    #[test]
    fn connections_form_tab_next_field() {
        let mut app = App::new();
        app.screen = Screen::Connections;
        app.connections_view.start_add();
        assert_eq!(
            route_key(key(KeyCode::Tab), &app),
            Some(Action::ConnectionsFormNextField)
        );
    }

    #[test]
    fn connections_delete_prompt_force_key() {
        let mut app = App::new();
        app.screen = Screen::Connections;
        app.connections_view.connections = vec![crate::views::connections::tests_fixture(
            "a", "openai",
        )];
        app.connections_view.selected = Some(0);
        app.connections_view.start_delete();
        assert_eq!(
            route_key(key(KeyCode::Char('f')), &app),
            Some(Action::ConnectionsDeleteForce)
        );
    }

    #[test]
    fn model_selector_captures_all_keys_when_open() {
        let mut app = App::new();
        app.screen = Screen::Chat;
        app.model_selector.open = true;
        assert_eq!(
            route_key(key(KeyCode::Down), &app),
            Some(Action::ModelSelectorNext)
        );
        assert_eq!(
            route_key(key(KeyCode::Esc), &app),
            Some(Action::ModelSelectorCancel)
        );
    }

    #[test]
    fn purposes_normal_c_edits() {
        let mut app = App::new();
        app.screen = Screen::Purposes;
        assert_eq!(
            route_key(key(KeyCode::Char('c')), &app),
            Some(Action::PurposesEdit)
        );
    }

    #[test]
    fn purposes_editor_backspace() {
        let mut app = App::new();
        app.screen = Screen::Purposes;
        app.purposes_view.start_edit();
        assert_eq!(
            route_key(key(KeyCode::Backspace), &app),
            Some(Action::PurposesEditorBackspace)
        );
    }

    #[test]
    fn chat_editing_enter_submits() {
        assert_eq!(
            route_key(key(KeyCode::Enter), &chat_editing()),
            Some(Action::SubmitPrompt)
        );
    }

    #[test]
    fn chat_editing_shift_enter_newline() {
        assert_eq!(
            route_key(
                key_with_mod(KeyCode::Enter, KeyModifiers::SHIFT),
                &chat_editing()
            ),
            Some(Action::InsertNewline)
        );
    }

    #[test]
    fn chat_editing_esc_exits() {
        assert_eq!(
            route_key(key(KeyCode::Esc), &chat_editing()),
            Some(Action::ExitEditMode)
        );
    }
}
