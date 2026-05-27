use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::InputMode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    NextConversation,
    PreviousConversation,
    OpenConversation,
    DeleteConversation,
    ArchiveConversation,
    NewConversation,
    EnterEditMode,
    ExitEditMode,
    SubmitPrompt,
    InsertNewline,
    ScrollUp,
    ScrollDown,
    ScrollToBottom,
    ToggleShowArchived,
    BeginRename,
    SubmitRename,
    CancelRename,
    ToggleDebug,
    ToggleSidebar,
    SwitchConnection,
    OpenKnowledgeBase,
    OpenConnections,
    OpenPurposes,
    OpenModelPicker,
    /// Toggle the process-manager (tasks) overlay. Currently bound to
    /// `Ctrl+P` ("process manager") since `Ctrl+T` is already used for
    /// the debug-view toggle and a chord-style `g t` would require a
    /// new key-state machine that none of the existing bindings use.
    ToggleTasksPane,
    /// Tasks-pane navigation: move highlighted task selection. Only
    /// active when the pane is visible.
    NextTask,
    PreviousTask,
    /// Cancel the highlighted task (sends `CancelBackgroundTask`).
    CancelSelectedTask,
    /// Jump to the conversation linked to the highlighted task.
    OpenSelectedTaskConversation,
}

/// Handle key events that we intercept before passing to textarea.
/// Returns None for keys that should be forwarded to textarea.input().
///
/// `tasks_pane_visible` modifies behavior when the process-manager overlay
/// is up: `j`/`k`/`c`/`Enter` route to task navigation/cancel/open-conv
/// instead of the normal-mode conversation actions. The pane is opened
/// and closed by `Ctrl+P`, which is honored from any mode.
pub fn handle_key_event(
    key: KeyEvent,
    mode: &InputMode,
    tasks_pane_visible: bool,
) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // F2 opens the connection picker from any mode. Chosen over Ctrl+Shift+C
    // since terminal emulators commonly intercept that for clipboard copy.
    if key.code == KeyCode::F(2) && key.modifiers.is_empty() {
        return Some(Action::SwitchConnection);
    }
    // F3 opens the LLM-provider connections manager.
    if key.code == KeyCode::F(3) && key.modifiers.is_empty() {
        return Some(Action::OpenConnections);
    }
    // F4 opens the purposes manager.
    if key.code == KeyCode::F(4) && key.modifiers.is_empty() {
        return Some(Action::OpenPurposes);
    }

    // Ctrl combos. Most apply across modes, but in Renaming we forward
    // all Ctrl combos to the rename textarea (Ctrl+a/e/u/k word/line edits).
    // Note: this shadows tui-textarea's emacs-style Ctrl+B (back one char)
    // in Editing mode — arrow keys cover that case.
    if ctrl && !matches!(mode, InputMode::Renaming) {
        if matches!(mode, InputMode::Editing) && matches!(key.code, KeyCode::Char('j')) {
            return Some(Action::InsertNewline);
        }
        return match key.code {
            KeyCode::Char('u') => Some(Action::ScrollUp),
            KeyCode::Char('d') => Some(Action::ScrollDown),
            KeyCode::Char('e') => Some(Action::ScrollToBottom),
            KeyCode::Char('t') => Some(Action::ToggleDebug),
            KeyCode::Char('b') => Some(Action::ToggleSidebar),
            KeyCode::Char('k') => Some(Action::OpenKnowledgeBase),
            KeyCode::Char('m') => Some(Action::OpenModelPicker),
            // Ctrl+P toggles the process-manager pane. Available from
            // any non-renaming mode so the user can pop it open while
            // editing a prompt to glance at running subagents.
            KeyCode::Char('p') => Some(Action::ToggleTasksPane),
            _ => None,
        };
    }

    // When the tasks pane is open, intercept normal-mode-style keys for
    // pane navigation regardless of editing/normal mode. Esc and the
    // toggle shortcut close it; we let the toggle through above.
    if tasks_pane_visible {
        if key.code == KeyCode::Esc && key.modifiers.is_empty() {
            return Some(Action::ToggleTasksPane);
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('j') | KeyCode::Down, m) if m.is_empty() => {
                return Some(Action::NextTask);
            }
            (KeyCode::Char('k') | KeyCode::Up, m) if m.is_empty() => {
                return Some(Action::PreviousTask);
            }
            (KeyCode::Char('c'), m) if m.is_empty() => {
                return Some(Action::CancelSelectedTask);
            }
            (KeyCode::Enter, m) if m.is_empty() => {
                return Some(Action::OpenSelectedTaskConversation);
            }
            _ => {}
        }
        // While the pane is open, other keys are swallowed (we don't
        // want `i` to drop you into edit mode, etc.).
        return None;
    }

    match mode {
        InputMode::Normal => {
            // Ignore Alt/Meta combos in Normal mode
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
                KeyCode::Char('r') => Some(Action::BeginRename),
                KeyCode::Char('i') => Some(Action::EnterEditMode),
                KeyCode::PageUp => Some(Action::ScrollUp),
                KeyCode::PageDown => Some(Action::ScrollDown),
                KeyCode::End => Some(Action::ScrollToBottom),
                _ => None,
            }
        }
        InputMode::Renaming => match key.code {
            KeyCode::Enter => Some(Action::SubmitRename),
            KeyCode::Esc => Some(Action::CancelRename),
            // All other keys (chars, backspace, arrows, home/end, ctrl-a/e/u/k)
            // forward to the rename textarea.
            _ => None,
        },
        InputMode::Editing => {
            // Shift+Enter inserts a newline while plain Enter submits.
            match key.code {
                KeyCode::Enter => {
                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                        return Some(Action::InsertNewline);
                    }
                    if key.modifiers.is_empty() {
                        return Some(Action::SubmitPrompt);
                    }
                    None
                }
                // Preserve terminal-provided newline chars by forwarding them
                // to textarea.input(...), which keeps composer and payload in sync.
                KeyCode::Char('\n') | KeyCode::Char('\r') => Some(Action::InsertNewline),
                KeyCode::Esc => Some(Action::ExitEditMode),
                KeyCode::PageUp => Some(Action::ScrollUp),
                KeyCode::PageDown => Some(Action::ScrollDown),
                KeyCode::End if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    Some(Action::ScrollToBottom)
                }
                // All other keys: return None so they get forwarded to textarea
                _ => None,
            }
        }
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

    // --- Normal mode tests ---

    #[test]
    fn normal_q_quits() {
        assert_eq!(
            handle_key_event(key(KeyCode::Char('q')), &InputMode::Normal, false),
            Some(Action::Quit)
        );
    }

    #[test]
    fn normal_j_next() {
        assert_eq!(
            handle_key_event(key(KeyCode::Char('j')), &InputMode::Normal, false),
            Some(Action::NextConversation)
        );
    }

    #[test]
    fn normal_down_next() {
        assert_eq!(
            handle_key_event(key(KeyCode::Down), &InputMode::Normal, false),
            Some(Action::NextConversation)
        );
    }

    #[test]
    fn normal_k_previous() {
        assert_eq!(
            handle_key_event(key(KeyCode::Char('k')), &InputMode::Normal, false),
            Some(Action::PreviousConversation)
        );
    }

    #[test]
    fn normal_up_previous() {
        assert_eq!(
            handle_key_event(key(KeyCode::Up), &InputMode::Normal, false),
            Some(Action::PreviousConversation)
        );
    }

    #[test]
    fn normal_enter_opens() {
        assert_eq!(
            handle_key_event(key(KeyCode::Enter), &InputMode::Normal, false),
            Some(Action::OpenConversation)
        );
    }

    #[test]
    fn normal_char_newline_is_ignored() {
        assert_eq!(
            handle_key_event(key(KeyCode::Char('\n')), &InputMode::Normal, false),
            None
        );
    }

    #[test]
    fn normal_d_deletes() {
        assert_eq!(
            handle_key_event(key(KeyCode::Char('d')), &InputMode::Normal, false),
            Some(Action::DeleteConversation)
        );
    }

    #[test]
    fn normal_n_new_conversation() {
        assert_eq!(
            handle_key_event(key(KeyCode::Char('n')), &InputMode::Normal, false),
            Some(Action::NewConversation)
        );
    }

    #[test]
    fn normal_i_enter_edit() {
        assert_eq!(
            handle_key_event(key(KeyCode::Char('i')), &InputMode::Normal, false),
            Some(Action::EnterEditMode)
        );
    }

    #[test]
    fn normal_unknown_key_ignored() {
        assert_eq!(
            handle_key_event(key(KeyCode::Char('x')), &InputMode::Normal, false),
            None
        );
    }

    #[test]
    fn normal_ctrl_modifier_ignored() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('q'), KeyModifiers::CONTROL), &InputMode::Normal, false),
            None
        );
    }

    #[test]
    fn normal_alt_modifier_ignored() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('j'), KeyModifiers::ALT), &InputMode::Normal, false),
            None
        );
    }

    // --- Editing mode tests ---

    #[test]
    fn editing_escape_exits() {
        assert_eq!(
            handle_key_event(key(KeyCode::Esc), &InputMode::Editing, false),
            Some(Action::ExitEditMode)
        );
    }

    #[test]
    fn editing_enter_submits_prompt() {
        assert_eq!(
            handle_key_event(key(KeyCode::Enter), &InputMode::Editing, false),
            Some(Action::SubmitPrompt)
        );
    }

    #[test]
    fn editing_shift_enter_inserts_newline() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Enter, KeyModifiers::SHIFT), &InputMode::Editing, false),
            Some(Action::InsertNewline)
        );
    }

    #[test]
    fn editing_newline_char_is_forwarded_to_textarea() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('\n'), KeyModifiers::NONE), &InputMode::Editing, false),
            Some(Action::InsertNewline)
        );
    }

    #[test]
    fn editing_carriage_return_char_is_forwarded_to_textarea() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('\r'), KeyModifiers::NONE), &InputMode::Editing, false),
            Some(Action::InsertNewline)
        );
    }

    #[test]
    fn editing_ctrl_j_inserts_newline() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('j'), KeyModifiers::CONTROL), &InputMode::Editing, false),
            Some(Action::InsertNewline)
        );
    }

    #[test]
    fn editing_alt_enter_is_forwarded_to_textarea() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Enter, KeyModifiers::ALT), &InputMode::Editing, false),
            None
        );
    }

    #[test]
    fn editing_char_forwarded_to_textarea() {
        // Regular chars should return None so they get forwarded to textarea
        assert_eq!(
            handle_key_event(key(KeyCode::Char('a')), &InputMode::Editing, false),
            None
        );
    }

    #[test]
    fn editing_backspace_forwarded_to_textarea() {
        assert_eq!(
            handle_key_event(key(KeyCode::Backspace), &InputMode::Editing, false),
            None
        );
    }

    #[test]
    fn editing_arrows_forwarded_to_textarea() {
        assert_eq!(
            handle_key_event(key(KeyCode::Left), &InputMode::Editing, false),
            None
        );
        assert_eq!(
            handle_key_event(key(KeyCode::Right), &InputMode::Editing, false),
            None
        );
        assert_eq!(
            handle_key_event(key(KeyCode::Up), &InputMode::Editing, false),
            None
        );
        assert_eq!(
            handle_key_event(key(KeyCode::Down), &InputMode::Editing, false),
            None
        );
    }

    // --- Scroll tests (Ctrl+u/d/e work in all modes) ---

    #[test]
    fn ctrl_u_scrolls_up() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('u'), KeyModifiers::CONTROL), &InputMode::Normal, false),
            Some(Action::ScrollUp)
        );
    }

    #[test]
    fn ctrl_d_scrolls_down() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('d'), KeyModifiers::CONTROL), &InputMode::Normal, false),
            Some(Action::ScrollDown)
        );
    }

    #[test]
    fn ctrl_e_scrolls_to_bottom() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('e'), KeyModifiers::CONTROL), &InputMode::Normal, false),
            Some(Action::ScrollToBottom)
        );
    }

    #[test]
    fn ctrl_u_works_in_editing_mode() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('u'), KeyModifiers::CONTROL), &InputMode::Editing, false),
            Some(Action::ScrollUp)
        );
    }

    #[test]
    fn ctrl_d_works_in_editing_mode() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('d'), KeyModifiers::CONTROL), &InputMode::Editing, false),
            Some(Action::ScrollDown)
        );
    }

    #[test]
    fn normal_pageup_scrolls_up() {
        assert_eq!(
            handle_key_event(key(KeyCode::PageUp), &InputMode::Normal, false),
            Some(Action::ScrollUp)
        );
    }

    #[test]
    fn normal_pagedown_scrolls_down() {
        assert_eq!(
            handle_key_event(key(KeyCode::PageDown), &InputMode::Normal, false),
            Some(Action::ScrollDown)
        );
    }

    // --- Renaming mode tests ---

    #[test]
    fn normal_r_begins_rename() {
        assert_eq!(
            handle_key_event(key(KeyCode::Char('r')), &InputMode::Normal, false),
            Some(Action::BeginRename)
        );
    }

    #[test]
    fn renaming_enter_submits() {
        assert_eq!(
            handle_key_event(key(KeyCode::Enter), &InputMode::Renaming, false),
            Some(Action::SubmitRename)
        );
    }

    #[test]
    fn renaming_esc_cancels() {
        assert_eq!(
            handle_key_event(key(KeyCode::Esc), &InputMode::Renaming, false),
            Some(Action::CancelRename)
        );
    }

    #[test]
    fn renaming_chars_forwarded_to_textarea() {
        // Regular chars and editing keys must reach the rename textarea.
        assert_eq!(
            handle_key_event(key(KeyCode::Char('a')), &InputMode::Renaming, false),
            None
        );
        assert_eq!(
            handle_key_event(key(KeyCode::Backspace), &InputMode::Renaming, false),
            None
        );
    }

    #[test]
    fn renaming_ctrl_combos_forwarded_to_textarea() {
        // Ctrl+a/e/u/k are textarea editing shortcuts in rename mode and
        // must NOT be intercepted as scroll commands.
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('u'), KeyModifiers::CONTROL), &InputMode::Renaming, false),
            None
        );
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('a'), KeyModifiers::CONTROL), &InputMode::Renaming, false),
            None
        );
    }

    // --- Debug toggle (Ctrl+T) ---

    #[test]
    fn ctrl_t_toggles_debug_in_normal() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('t'), KeyModifiers::CONTROL), &InputMode::Normal, false),
            Some(Action::ToggleDebug)
        );
    }

    #[test]
    fn ctrl_t_toggles_debug_in_editing() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('t'), KeyModifiers::CONTROL), &InputMode::Editing, false),
            Some(Action::ToggleDebug)
        );
    }

    // --- Sidebar toggle (Ctrl+B) ---

    #[test]
    fn ctrl_b_toggles_sidebar_in_normal() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('b'), KeyModifiers::CONTROL), &InputMode::Normal, false),
            Some(Action::ToggleSidebar)
        );
    }

    #[test]
    fn ctrl_b_toggles_sidebar_in_editing() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('b'), KeyModifiers::CONTROL), &InputMode::Editing, false),
            Some(Action::ToggleSidebar)
        );
    }

    // --- F2 switch connection ---

    #[test]
    fn f2_triggers_switch_in_normal() {
        assert_eq!(
            handle_key_event(key(KeyCode::F(2)), &InputMode::Normal, false),
            Some(Action::SwitchConnection)
        );
    }

    #[test]
    fn f2_triggers_switch_in_editing() {
        assert_eq!(
            handle_key_event(key(KeyCode::F(2)), &InputMode::Editing, false),
            Some(Action::SwitchConnection)
        );
    }

    // --- Ctrl+K opens KB ---

    #[test]
    fn ctrl_k_opens_kb_in_normal() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('k'), KeyModifiers::CONTROL), &InputMode::Normal, false),
            Some(Action::OpenKnowledgeBase)
        );
    }

    #[test]
    fn ctrl_k_opens_kb_in_editing() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('k'), KeyModifiers::CONTROL), &InputMode::Editing, false),
            Some(Action::OpenKnowledgeBase)
        );
    }

    // --- F3 opens connections ---

    #[test]
    fn f3_opens_connections_in_normal() {
        assert_eq!(
            handle_key_event(key(KeyCode::F(3)), &InputMode::Normal, false),
            Some(Action::OpenConnections)
        );
    }

    #[test]
    fn f3_opens_connections_in_editing() {
        assert_eq!(
            handle_key_event(key(KeyCode::F(3)), &InputMode::Editing, false),
            Some(Action::OpenConnections)
        );
    }

    #[test]
    fn f4_opens_purposes_in_normal() {
        assert_eq!(
            handle_key_event(key(KeyCode::F(4)), &InputMode::Normal, false),
            Some(Action::OpenPurposes)
        );
    }

    #[test]
    fn f4_opens_purposes_in_editing() {
        assert_eq!(
            handle_key_event(key(KeyCode::F(4)), &InputMode::Editing, false),
            Some(Action::OpenPurposes)
        );
    }

    // --- Ctrl+M opens model picker ---

    #[test]
    fn ctrl_m_opens_model_picker_in_normal() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('m'), KeyModifiers::CONTROL), &InputMode::Normal, false),
            Some(Action::OpenModelPicker)
        );
    }

    #[test]
    fn ctrl_m_opens_model_picker_in_editing() {
        assert_eq!(
            handle_key_event(key_with_mod(KeyCode::Char('m'), KeyModifiers::CONTROL), &InputMode::Editing, false),
            Some(Action::OpenModelPicker)
        );
    }

    // --- Ctrl+P toggles tasks pane (process-manager) ---

    #[test]
    fn ctrl_p_toggles_tasks_pane_in_normal() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('p'), KeyModifiers::CONTROL),
                &InputMode::Normal,
                false
            ),
            Some(Action::ToggleTasksPane)
        );
    }

    #[test]
    fn ctrl_p_toggles_tasks_pane_in_editing() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('p'), KeyModifiers::CONTROL),
                &InputMode::Editing,
                false
            ),
            Some(Action::ToggleTasksPane)
        );
    }

    #[test]
    fn ctrl_p_also_closes_pane_when_open() {
        // Toggle is symmetric: the same shortcut closes the pane.
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('p'), KeyModifiers::CONTROL),
                &InputMode::Normal,
                true
            ),
            Some(Action::ToggleTasksPane)
        );
    }

    // --- Tasks-pane key routing when visible ---

    #[test]
    fn tasks_pane_visible_routes_j_to_next_task() {
        assert_eq!(
            handle_key_event(key(KeyCode::Char('j')), &InputMode::Normal, true),
            Some(Action::NextTask)
        );
    }

    #[test]
    fn tasks_pane_visible_routes_k_to_previous_task() {
        assert_eq!(
            handle_key_event(key(KeyCode::Char('k')), &InputMode::Normal, true),
            Some(Action::PreviousTask)
        );
    }

    #[test]
    fn tasks_pane_visible_routes_down_to_next_task() {
        assert_eq!(
            handle_key_event(key(KeyCode::Down), &InputMode::Normal, true),
            Some(Action::NextTask)
        );
    }

    #[test]
    fn tasks_pane_visible_routes_up_to_previous_task() {
        assert_eq!(
            handle_key_event(key(KeyCode::Up), &InputMode::Normal, true),
            Some(Action::PreviousTask)
        );
    }

    #[test]
    fn tasks_pane_visible_routes_c_to_cancel_task() {
        // When the pane is open, `c` cancels the highlighted task
        // rather than being passed through (and `c` does nothing in
        // normal mode today, so this isn't shadowing anything).
        assert_eq!(
            handle_key_event(key(KeyCode::Char('c')), &InputMode::Normal, true),
            Some(Action::CancelSelectedTask)
        );
    }

    #[test]
    fn tasks_pane_visible_routes_enter_to_open_conversation() {
        assert_eq!(
            handle_key_event(key(KeyCode::Enter), &InputMode::Normal, true),
            Some(Action::OpenSelectedTaskConversation)
        );
    }

    #[test]
    fn tasks_pane_visible_routes_esc_to_close_pane() {
        // Esc is a discoverable second way to close the pane.
        assert_eq!(
            handle_key_event(key(KeyCode::Esc), &InputMode::Normal, true),
            Some(Action::ToggleTasksPane)
        );
    }

    #[test]
    fn tasks_pane_visible_swallows_unmapped_keys_in_editing_mode() {
        // While the pane is open, regular typing in editing mode is
        // suppressed — otherwise `i` would still drop into edit and
        // start typing into a hidden textarea.
        assert_eq!(
            handle_key_event(key(KeyCode::Char('x')), &InputMode::Editing, true),
            None
        );
    }

    #[test]
    fn tasks_pane_visible_still_lets_f2_through() {
        // Function keys for global navigation should still work; only
        // mode-level keys are intercepted.
        assert_eq!(
            handle_key_event(key(KeyCode::F(2)), &InputMode::Normal, true),
            Some(Action::SwitchConnection)
        );
    }
}
