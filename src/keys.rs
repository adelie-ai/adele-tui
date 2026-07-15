use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::InputMode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    NextConversation,
    PreviousConversation,
    OpenConversation,
    /// Arm the delete-confirm overlay for the selected conversation (`d` in the
    /// sidebar). Deletion no longer fires on the first keypress — matching the
    /// KB / connections / profile destructive-delete flows, which all confirm
    /// first. The overlay's `y`/`Enter` then dispatches [`Action::DeleteConversation`].
    BeginDeleteConversation,
    /// Perform the conversation delete. Dispatched by the confirm overlay
    /// (`y`/`Enter`), not bound to a key directly.
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
    /// Open the MCP-servers admin panel (`F5`, desktop-assistant#495). Mirrors
    /// `OpenConnections`/`OpenPurposes`: a modal manager over the daemon's MCP
    /// server config.
    OpenMcpServers,
    OpenModelPicker,
    /// Open the per-conversation personality picker (`Ctrl+R`, "peRsonality").
    /// Mirrors `OpenModelPicker`; pins/clears the Expressive-7 traits for the
    /// active conversation via `set_conversation_personality`.
    OpenPersonalityPicker,
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
    /// Push-to-talk dictation (adele-tui#77). Bound to `Ctrl+G` ("Go, voice") —
    /// free across modes and not intercepted by terminals or the textarea.
    /// Prefers the voice daemon when it is running (it routes the whole spoken
    /// turn into the active conversation); otherwise falls back to one-shot
    /// embedded dictation (mic → transcript → prompt input), which is a no-op
    /// unless voice is in `embedded` mode. Available when `You == Enabled`.
    Dictate,
    /// Cycle the per-conversation `Adele:` voice-output level (adele-tui#77),
    /// `Disabled → On Demand → Always → Disabled`. Bound to `Ctrl+S` ("Speech";
    /// Adele's speech). The keyboard-enhancement flags pushed at startup deliver
    /// Ctrl+S as a real key event (not terminal XOFF flow control). `Always`
    /// reads every reply aloud in full (made speakable); `On Demand` reads
    /// replies only while `You == Enabled`, kept brief, and always speaks
    /// `say_this` asides; `Disabled` never speaks. Also set by the model via
    /// `request_voice` (→ On Demand) / `stop_voice` (→ Disabled). Defaults
    /// `Disabled` per conversation.
    CycleAdeleOutput,
    /// Toggle the per-conversation `You:` voice-input control (adele-tui#77),
    /// `Disabled ↔ Enabled`. Bound to `Ctrl+V` ("Voice"; your voice), delivered
    /// as a real key event by the same keyboard-enhancement flags. When Enabled,
    /// push-to-talk dictation is available (Ctrl+G) and — combined with
    /// `Adele == On Demand` — reply narration is on. Defaults `Disabled` (type
    /// only); text input is always available.
    ToggleVoiceIn,
    /// Toggle the keymap help overlay (`?` in Normal mode, or `F1` from anywhere).
    ToggleHelp,
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
    // F5 opens the MCP-servers admin panel.
    if key.code == KeyCode::F(5) && key.modifiers.is_empty() {
        return Some(Action::OpenMcpServers);
    }
    // F1 toggles the keymap help overlay from any mode (`?` also opens it in
    // Normal mode; F1 is offered too since `?` is a literal character in the
    // composer).
    if key.code == KeyCode::F(1) && key.modifiers.is_empty() {
        return Some(Action::ToggleHelp);
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
            // Ctrl+R ("peRsonality") opens the per-conversation personality
            // picker, mirroring Ctrl+M's per-conversation model picker.
            KeyCode::Char('r') => Some(Action::OpenPersonalityPicker),
            // Ctrl+P toggles the process-manager pane. Available from
            // any non-renaming mode so the user can pop it open while
            // editing a prompt to glance at running subagents.
            KeyCode::Char('p') => Some(Action::ToggleTasksPane),
            // Ctrl+G starts embedded dictation (mic → prompt). A no-op when
            // voice isn't in embedded mode; main.rs gates on the session.
            KeyCode::Char('g') => Some(Action::Dictate),
            // Ctrl+S cycles the per-conversation Adele output level
            // (adele-tui#77). A no-op status hint when no conversation is open;
            // main.rs gates.
            KeyCode::Char('s') => Some(Action::CycleAdeleOutput),
            // Ctrl+V toggles the per-conversation You (voice-input) control
            // (adele-tui#77). Like Ctrl+S it is delivered as a real key by the
            // enhancement flags.
            KeyCode::Char('v') => Some(Action::ToggleVoiceIn),
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
                KeyCode::Char('d') => Some(Action::BeginDeleteConversation),
                KeyCode::Char('n') => Some(Action::NewConversation),
                KeyCode::Char('a') => Some(Action::ToggleShowArchived),
                KeyCode::Char('A') => Some(Action::ArchiveConversation),
                KeyCode::Char('r') => Some(Action::BeginRename),
                KeyCode::Char('i') => Some(Action::EnterEditMode),
                KeyCode::Char('?') => Some(Action::ToggleHelp),
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

/// The keymap shown in the `?`/F1 help overlay (rendered by `ui::draw_help_overlay`).
/// Lives next to `handle_key_event` so the help stays the single source of truth
/// for the bindings.
pub fn help_sections() -> &'static [(&'static str, &'static [(&'static str, &'static str)])] {
    &[
        (
            "Conversations (Normal mode)",
            &[
                ("j / k   ↑ / ↓", "move selection"),
                ("Enter", "open conversation"),
                ("i", "compose / edit"),
                ("n", "new conversation"),
                ("r", "rename"),
                ("d", "delete"),
                ("A", "archive"),
                ("a", "show / hide archived"),
                ("q", "quit"),
            ],
        ),
        (
            "Composing (edit mode)",
            &[
                ("Enter", "send"),
                ("Shift+Enter / Ctrl+J", "insert newline"),
                ("Esc", "leave edit mode"),
            ],
        ),
        (
            "Scroll",
            &[
                ("Ctrl+U / Ctrl+D", "page up / down"),
                ("Ctrl+E", "jump to bottom"),
            ],
        ),
        (
            "View",
            &[
                ("Ctrl+B", "toggle sidebar"),
                ("Ctrl+T", "toggle debug messages"),
                ("Ctrl+P", "tasks pane"),
            ],
        ),
        (
            "Open",
            &[
                ("F2", "switch connection"),
                ("F3", "connections manager"),
                ("F4", "purposes"),
                ("F5", "MCP servers"),
                ("Ctrl+K", "knowledge base"),
                ("Ctrl+M", "model picker"),
                ("Ctrl+R", "personality"),
            ],
        ),
        (
            "Voice",
            &[
                ("Ctrl+G", "push-to-talk dictation"),
                ("Ctrl+S", "cycle Adele voice output"),
                ("Ctrl+V", "toggle You (voice input)"),
            ],
        ),
        (
            "Tasks pane (when open)",
            &[
                ("j / k", "move selection"),
                ("c", "cancel task"),
                ("Enter", "open task's conversation"),
                ("Esc", "close pane"),
            ],
        ),
    ]
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
    fn normal_d_arms_delete_confirm() {
        // `d` no longer deletes immediately — it arms the confirm overlay
        // (matching the KB / connections / profile destructive-delete flows).
        // The actual `DeleteConversation` is dispatched by the overlay on
        // `y`/`Enter`, not bound to a key.
        assert_eq!(
            handle_key_event(key(KeyCode::Char('d')), &InputMode::Normal, false),
            Some(Action::BeginDeleteConversation)
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
            handle_key_event(
                key_with_mod(KeyCode::Char('q'), KeyModifiers::CONTROL),
                &InputMode::Normal,
                false
            ),
            None
        );
    }

    #[test]
    fn normal_alt_modifier_ignored() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('j'), KeyModifiers::ALT),
                &InputMode::Normal,
                false
            ),
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
            handle_key_event(
                key_with_mod(KeyCode::Enter, KeyModifiers::SHIFT),
                &InputMode::Editing,
                false
            ),
            Some(Action::InsertNewline)
        );
    }

    #[test]
    fn editing_newline_char_is_forwarded_to_textarea() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('\n'), KeyModifiers::NONE),
                &InputMode::Editing,
                false
            ),
            Some(Action::InsertNewline)
        );
    }

    #[test]
    fn editing_carriage_return_char_is_forwarded_to_textarea() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('\r'), KeyModifiers::NONE),
                &InputMode::Editing,
                false
            ),
            Some(Action::InsertNewline)
        );
    }

    #[test]
    fn editing_ctrl_j_inserts_newline() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('j'), KeyModifiers::CONTROL),
                &InputMode::Editing,
                false
            ),
            Some(Action::InsertNewline)
        );
    }

    #[test]
    fn editing_alt_enter_is_forwarded_to_textarea() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Enter, KeyModifiers::ALT),
                &InputMode::Editing,
                false
            ),
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
            handle_key_event(
                key_with_mod(KeyCode::Char('u'), KeyModifiers::CONTROL),
                &InputMode::Normal,
                false
            ),
            Some(Action::ScrollUp)
        );
    }

    #[test]
    fn ctrl_d_scrolls_down() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('d'), KeyModifiers::CONTROL),
                &InputMode::Normal,
                false
            ),
            Some(Action::ScrollDown)
        );
    }

    #[test]
    fn ctrl_e_scrolls_to_bottom() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('e'), KeyModifiers::CONTROL),
                &InputMode::Normal,
                false
            ),
            Some(Action::ScrollToBottom)
        );
    }

    #[test]
    fn ctrl_u_works_in_editing_mode() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('u'), KeyModifiers::CONTROL),
                &InputMode::Editing,
                false
            ),
            Some(Action::ScrollUp)
        );
    }

    #[test]
    fn ctrl_d_works_in_editing_mode() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('d'), KeyModifiers::CONTROL),
                &InputMode::Editing,
                false
            ),
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
            handle_key_event(
                key_with_mod(KeyCode::Char('u'), KeyModifiers::CONTROL),
                &InputMode::Renaming,
                false
            ),
            None
        );
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('a'), KeyModifiers::CONTROL),
                &InputMode::Renaming,
                false
            ),
            None
        );
    }

    // --- Debug toggle (Ctrl+T) ---

    #[test]
    fn ctrl_t_toggles_debug_in_normal() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('t'), KeyModifiers::CONTROL),
                &InputMode::Normal,
                false
            ),
            Some(Action::ToggleDebug)
        );
    }

    #[test]
    fn ctrl_t_toggles_debug_in_editing() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('t'), KeyModifiers::CONTROL),
                &InputMode::Editing,
                false
            ),
            Some(Action::ToggleDebug)
        );
    }

    // --- Sidebar toggle (Ctrl+B) ---

    #[test]
    fn ctrl_b_toggles_sidebar_in_normal() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('b'), KeyModifiers::CONTROL),
                &InputMode::Normal,
                false
            ),
            Some(Action::ToggleSidebar)
        );
    }

    #[test]
    fn ctrl_b_toggles_sidebar_in_editing() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('b'), KeyModifiers::CONTROL),
                &InputMode::Editing,
                false
            ),
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
            handle_key_event(
                key_with_mod(KeyCode::Char('k'), KeyModifiers::CONTROL),
                &InputMode::Normal,
                false
            ),
            Some(Action::OpenKnowledgeBase)
        );
    }

    #[test]
    fn ctrl_k_opens_kb_in_editing() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('k'), KeyModifiers::CONTROL),
                &InputMode::Editing,
                false
            ),
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
            handle_key_event(
                key_with_mod(KeyCode::Char('m'), KeyModifiers::CONTROL),
                &InputMode::Normal,
                false
            ),
            Some(Action::OpenModelPicker)
        );
    }

    #[test]
    fn ctrl_m_opens_model_picker_in_editing() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('m'), KeyModifiers::CONTROL),
                &InputMode::Editing,
                false
            ),
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

    // --- Ctrl+G starts dictation ---

    #[test]
    fn ctrl_g_dictates_in_normal() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('g'), KeyModifiers::CONTROL),
                &InputMode::Normal,
                false
            ),
            Some(Action::Dictate)
        );
    }

    #[test]
    fn ctrl_g_dictates_in_editing() {
        // Dictation must be reachable while composing a prompt — that's the
        // primary use (mic appends to the input you're typing).
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('g'), KeyModifiers::CONTROL),
                &InputMode::Editing,
                false
            ),
            Some(Action::Dictate)
        );
    }

    // --- Ctrl+S cycles the Adele output level (adele-tui#77) ---

    #[test]
    fn ctrl_s_cycles_adele_output_in_normal() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('s'), KeyModifiers::CONTROL),
                &InputMode::Normal,
                false
            ),
            Some(Action::CycleAdeleOutput)
        );
    }

    #[test]
    fn ctrl_s_cycles_adele_output_in_editing() {
        // Adele output is a per-conversation control the user will change while
        // composing, so it must be reachable from editing mode too.
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('s'), KeyModifiers::CONTROL),
                &InputMode::Editing,
                false
            ),
            Some(Action::CycleAdeleOutput)
        );
    }

    #[test]
    fn ctrl_s_is_not_intercepted_in_renaming() {
        // Renaming forwards all Ctrl combos to the rename textarea; the Adele
        // cycle must not hijack them mid-rename.
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('s'), KeyModifiers::CONTROL),
                &InputMode::Renaming,
                false
            ),
            None
        );
    }

    #[test]
    fn ctrl_g_is_not_intercepted_in_renaming() {
        // Renaming forwards all Ctrl combos to the rename textarea; dictation
        // must not hijack them mid-rename.
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('g'), KeyModifiers::CONTROL),
                &InputMode::Renaming,
                false
            ),
            None
        );
    }

    // --- Ctrl+V toggles the You (voice-input) control (adele-tui#77) ---

    #[test]
    fn ctrl_v_toggles_voice_in_in_normal() {
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('v'), KeyModifiers::CONTROL),
                &InputMode::Normal,
                false
            ),
            Some(Action::ToggleVoiceIn)
        );
    }

    #[test]
    fn ctrl_v_toggles_voice_in_in_editing() {
        // The You control is a per-conversation control the user will flip while
        // composing, so it must be reachable from editing mode too.
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('v'), KeyModifiers::CONTROL),
                &InputMode::Editing,
                false
            ),
            Some(Action::ToggleVoiceIn)
        );
    }

    #[test]
    fn ctrl_v_is_not_intercepted_in_renaming() {
        // Renaming forwards all Ctrl combos to the rename textarea; the You
        // toggle must not hijack them mid-rename.
        assert_eq!(
            handle_key_event(
                key_with_mod(KeyCode::Char('v'), KeyModifiers::CONTROL),
                &InputMode::Renaming,
                false
            ),
            None
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
