use std::rc::Rc;

use desktop_assistant_api_model::TaskId;
use desktop_assistant_client_common::mcp_host::McpHost;
pub use desktop_assistant_client_common::{
    ChatMessage, ConversationDetail, ConversationSummary, MessageKind,
};
use ratatui::style::Style;
use ratatui_textarea::{CursorMove, DataCursor, TextArea};

use crate::tasks::TaskPane;

// The shared, view-agnostic model+controller (client-ui-common). `App` embeds a
// `WindowState` as `core` and is migrating its chat-core state onto it slice by
// slice (CC-3). First slice: per-conversation voice state lives in `core`;
// `App`'s voice methods are thin wrappers that read core's accessors and route
// writes through `core.apply(...)`.
use client_ui_common::{Effect, UiMessage, WindowState};

/// Context-window fill view + colour bucket (#341), shared via client-ui-common;
/// re-exported so existing crate::app::ContextUsageView paths in ui.rs/main.rs resolve.
pub use client_ui_common::{ContextFillLevel, ContextUsageView};
fn new_textarea() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_cursor_line_style(Style::default());
    ta
}

/// Display width of a single character in terminal columns, treating
/// control/zero-width characters as one column so they still advance the
/// wrap cursor and never panic the width budget.
fn char_display_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c)
        .unwrap_or(0)
        .max(1)
}

/// Split `line` into display rows no wider than `width` terminal columns,
/// preferring whitespace break points. Measurement is by Unicode display
/// width (`unicode-width`) so wide glyphs (CJK, many emoji) wrap at the
/// column they actually occupy rather than at a raw char count.
///
/// This is purely presentational: the joined segments reproduce `line`
/// exactly, so callers must never treat the segment boundaries as logical
/// newlines (issue #84).
fn wrap_line_for_width(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }

    let chars: Vec<char> = line.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut start = 0usize;

    while start < chars.len() {
        // Walk forward accumulating display width until we'd exceed `width`.
        let mut end = start;
        let mut used = 0usize;
        while end < chars.len() {
            let w = char_display_width(chars[end]);
            if used + w > width && end > start {
                break;
            }
            used += w;
            end += 1;
        }

        if end >= chars.len() {
            out.push(chars[start..].iter().collect());
            break;
        }

        // Prefer breaking just after the last whitespace in [start, end).
        let mut split_at = end;
        for i in (start..end).rev() {
            if chars[i].is_whitespace() {
                split_at = i + 1;
                break;
            }
        }
        // No whitespace to break on (or it's at the very start): hard-break
        // at the column limit so we always make progress.
        if split_at == start {
            split_at = end;
        }

        out.push(chars[start..split_at].iter().collect());
        start = split_at;
    }

    out
}

fn map_cursor_col_to_wrapped_segments(segments: &[String], cursor_col: usize) -> (usize, usize) {
    let mut remaining = cursor_col;
    for (idx, segment) in segments.iter().enumerate() {
        let seg_len = segment.chars().count();
        if remaining <= seg_len {
            return (idx, remaining);
        }
        remaining = remaining.saturating_sub(seg_len);
    }

    let last_idx = segments.len().saturating_sub(1);
    let last_len = segments[last_idx].chars().count();
    (last_idx, last_len)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Editing,
    Renaming,
}

/// A modal sub-screen the user has asked the run loop to open. Each maps to one
/// `*::run` driver invocation in the loop's single dispatch point. Replaces the
/// old independent `*_requested` bools so the request is mutually exclusive by
/// construction (CC-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenRequest {
    KnowledgeBase,
    Connections,
    Purposes,
    /// The MCP-servers admin panel (desktop-assistant#495).
    McpServers,
    ModelPicker,
    PersonalityPicker,
}

/// The `Adele:` voice-output level for a conversation. Decides reply narration
/// (with `You`), the `say_this` aside gate, and the send-time
/// `system_refinement`. Defaults to `Disabled`.
///
/// Now owned by the shared `adele-voice-client-common` crate
/// (desktop-assistant#274) so the GTK and TUI clients share one definition + the
/// narration gate (`next`/`label`/`narrates_reply`/`speaks_aside`/
/// `send_refinement`); re-exported here so existing `crate::app::AdeleOutput`
/// paths keep resolving unchanged.
pub use adele_voice_client_common::AdeleOutput;

/// What a `Down`-key queue step resolves to given the outbox length and the
/// currently checked-out edit slot. Keeps the walk decision pure and testable,
/// separate from the `apply_core` dispatch it drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecallNext {
    /// Check out the queued item at this index for editing.
    Edit(usize),
    /// Past the newest queued item: return the checked-out one and clear.
    Cancel,
    /// Not editing a queued message — nothing to step through.
    None,
}

/// Target index for an `Up`-key recall: step one toward the oldest from the
/// checked-out slot, or the newest queued item when composing fresh. `None` at
/// the front of the queue (`Some(0)`) or when the queue is empty — a no-op that
/// preserves any in-composer edits of the front item.
fn recall_prev_index(queued_len: usize, editing: Option<usize>) -> Option<usize> {
    match editing {
        Some(0) => None,
        Some(i) => Some(i - 1),
        None => queued_len.checked_sub(1),
    }
}

/// Resolve a `Down`-key queue step. Only meaningful while editing (`editing` is
/// `Some`): step to the next queued item when one exists after the checked-out
/// slot, otherwise cancel the edit (the checked-out message returns to the
/// queue). `queued_len` is the outbox length *excluding* the checked-out item.
fn recall_next_action(queued_len: usize, editing: Option<usize>) -> RecallNext {
    match editing {
        None => RecallNext::None,
        Some(i) if i < queued_len => RecallNext::Edit(i + 1),
        Some(_) => RecallNext::Cancel,
    }
}

pub struct App {
    pub selected_conversation: Option<usize>,
    pub textarea: TextArea<'static>,
    // The conversation list, the open conversation's transcript, and the
    // in-flight streaming state (buffer, pending request/conversation id,
    // external-turn flag, ack sentinel, narration gate) all now live in the
    // shared `core` (CC-3): the reducer owns those state machines and the view
    // reads them back through `conversations()` / `current_conversation()` /
    // `streaming_buffer()` / `streaming_is_active_for_view()` / `is_streaming()`.
    // `App` keeps only TUI view state (selection, scroll, mode, status lines).
    pub mode: InputMode,
    pub status_message: String,
    /// Whether the daemon connection is currently live. The run loop projects
    /// its `ReconnectState` into this each frame (it owns the socket); the view
    /// reads it to render disconnect chrome — a warn-colored input border plus
    /// an `offline` tag. Defaults `true`; the loop overwrites it before the
    /// first draw. (CC-3 moves this into the shared `core` once signals route
    /// through the reducer.)
    pub connected: bool,
    /// Client-side MCP host (`client-mcp.toml`): local MCP servers whose tools
    /// are exposed to the daemon as client-side tools. Created once at startup,
    /// shared (`Rc`) between the register site and client-tool dispatch; `None`
    /// when no servers are configured for this surface.
    pub mcp_host: Option<Rc<McpHost>>,
    pub should_quit: bool,
    /// Whether the `?`/F1 keymap help overlay is shown. Any key closes it.
    pub show_help: bool,
    /// Title of the conversation awaiting a delete confirmation, or `None` when
    /// no confirm overlay is up. Set when `d` is pressed in the sidebar (it no
    /// longer deletes immediately — matching the KB / connections / profile
    /// destructive-delete convention); the overlay names this title. `y`/`Enter`
    /// confirms (runs the delete against the still-selected row), `n`/`Esc`
    /// cancels; any other key is ignored. Held only for display — the delete
    /// itself operates on `selected_conversation`, which the overlay blocks the
    /// user from moving.
    pub pending_delete_conversation: Option<String>,
    /// Lines scrolled up from the bottom. 0 = auto-scroll to bottom.
    pub scroll_offset: u16,
    /// Whether to include archived conversations in the list.
    pub show_archived: bool,
    /// Single-line input used during InputMode::Renaming.
    pub rename_textarea: TextArea<'static>,
    /// Conversation id being renamed; `None` outside InputMode::Renaming.
    pub renaming_id: Option<String>,
    /// When true, render tool/system/empty-assistant messages dimly inline
    /// instead of filtering them out. Persisted via `settings.json`.
    pub show_debug: bool,
    /// When true, share basic device context (name, username, home folder,
    /// hostname, timezone, OS) with the assistant at connect so it can
    /// personalize. On by default; toggled with `Ctrl+O` and persisted via
    /// `settings.json` (da#549). Read into the `ConnectionConfig` when the
    /// connection is built, so a toggle applies on the next (re)connect.
    pub share_client_context: bool,
    /// Transient indicator from AssistantStatus events ("Searching knowledge
    /// base…", tool-call progress). Cleared when streaming completes or
    /// errors. Distinct from `status_message`, which is sticky user-facing
    /// feedback.
    pub assistant_status: Option<String>,
    /// Most recent context-window fill for the open conversation (#341).
    /// Updated by `SignalEvent::ContextUsage` each turn; cleared when the
    /// open conversation changes so a stale reading never bleeds across
    /// conversations. `None` = no reading yet (budget unknown / pre-first-turn),
    /// rendered as nothing.
    pub context_usage: Option<ContextUsageView>,
    /// Whether the conversation list pane is visible. When `false`, the chat
    /// panel takes the full window width.
    pub show_sidebar: bool,
    /// Set when the user asks to switch to a different connection. Causes
    /// the chat loop to exit cleanly so the picker can run again.
    pub switch_requested: bool,
    /// Set when a `SignalEvent::ConversationListChanged` arrives while a modal
    /// sub-screen is open (#1). The sub-screen sink can't own the loop-local
    /// `InFlight` RPC driver, so it records the request here; the chat loop
    /// drains it on the next iteration (once the modal has closed and the
    /// sidebar is visible again) by pushing a conversation-list refetch.
    pub pending_conversation_refresh: bool,
    /// The modal sub-screen the user has asked to open, serviced (and cleared)
    /// by the run loop on its next iteration. Replaces the old set of independent
    /// `*_requested` bools (CC-3): only ONE screen can be pending at a time, so a
    /// fresh request can't silently race a not-yet-serviced one, and the run loop
    /// dispatches them from a single point instead of five near-identical guards.
    pub pending_screen: Option<ScreenRequest>,
    /// One-shot override staged by the model picker. Applied to the next
    /// `SendPrompt` and then cleared — the daemon persists it as the
    /// conversation's `last_model_selection`, so subsequent prompts pick
    /// it up automatically.
    pub pending_model_override: Option<desktop_assistant_api_model::SendPromptOverride>,
    /// Process-manager (background tasks) state. Always present but
    /// only rendered when `tasks.visible == true`. Populated at connect
    /// time via `ListBackgroundTasks` + `SubscribeBackgroundTasks` and
    /// kept fresh by `SignalEvent::Task*` variants in the main loop.
    pub tasks: TaskPane,
    /// Pending request id of an in-flight `CancelBackgroundTask`. The
    /// terminal `TaskCompleted { status: Cancelled }` event is what
    /// actually closes the loop; this is purely so the status bar can
    /// say "cancelling t-1..." while we wait.
    pub pending_task_cancel: Option<TaskId>,
    /// The shared, view-agnostic model+controller (client-ui-common). `App` is
    /// migrating its chat-core state onto it slice by slice (CC-3). It owns the
    /// per-conversation voice state (`You:` input enablement and `Adele:` output
    /// level, adele-tui#77 / adele-gtk#80) and the **in-flight streaming state**
    /// — the buffer, the pending request/conversation ids, the external-turn
    /// flag, the ack sentinel, and the reply-narration gate. Daemon stream events
    /// route through `core.apply(...)` (see [`App::apply_core`]); `App` mirrors
    /// the resulting view-effects onto its own rendered transcript and surfaces
    /// `core`'s streaming state to the view via `streaming_buffer()` /
    /// `streaming_is_active_for_view()` / `is_streaming()`. The open
    /// conversation's id is dual-written into `core` on load so its
    /// originating-conversation checks (TUI-4 / GTK-2) judge against the
    /// conversation actually in view.
    core: WindowState,
}

impl App {
    pub fn new() -> Self {
        Self {
            selected_conversation: None,
            textarea: new_textarea(),
            mode: InputMode::Normal,
            status_message: "Connected".to_string(),
            connected: true,
            should_quit: false,
            show_help: false,
            pending_delete_conversation: None,
            scroll_offset: 0,
            show_archived: false,
            rename_textarea: new_textarea(),
            renaming_id: None,
            show_debug: false,
            share_client_context: true,
            assistant_status: None,
            context_usage: None,
            show_sidebar: true,
            switch_requested: false,
            pending_conversation_refresh: false,
            pending_screen: None,
            pending_model_override: None,
            tasks: TaskPane::new(),
            pending_task_cancel: None,
            core: WindowState::default(),
            mcp_host: None,
        }
    }

    /// Toggle the keymap help overlay (`?`/F1). Any key closes it (handled in the
    /// event loop), so the action only ever opens it.
    pub fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
    }

    /// Ask the run loop to open `screen` as a modal on its next iteration.
    /// Mutually exclusive: a newer request supersedes any not-yet-serviced one
    /// (the loop services at most one modal per turn, then redraws).
    pub fn request_screen(&mut self, screen: ScreenRequest) {
        self.pending_screen = Some(screen);
    }

    /// Take (and clear) the pending modal-screen request, if any. The run loop
    /// calls this once per iteration to drive its single modal-dispatch point.
    pub fn take_pending_screen(&mut self) -> Option<ScreenRequest> {
        self.pending_screen.take()
    }

    // --- Per-conversation You/Adele voice controls (adele-tui#77) ---
    //
    // Two independent per-conversation controls mirroring adele-gtk#80:
    //   * `You` (voice input)  — `voice_in`: Disabled (type only) | Enabled (PTT).
    //   * `Adele` (voice output) — `adele_output`: Disabled | OnDemand | Always.
    // Text input is always available regardless. The narration gate, the
    // `say_this` aside gate, and the send-time refinement all derive from these
    // two as pure functions so they are unit-testable without a transport or
    // audio device.

    /// Whether `You:` (voice input) is Enabled for `conversation_id`. A
    /// conversation absent from the map is Disabled (the default).
    pub fn voice_in_for(&self, conversation_id: &str) -> bool {
        self.core.voice_in_for(conversation_id)
    }

    /// Whether `You:` is Enabled for the currently-open conversation. `false`
    /// when no conversation is open.
    pub fn current_voice_in(&self) -> bool {
        self.core
            .current_conversation()
            .is_some_and(|c| self.voice_in_for(&c.id))
    }

    /// Flip `You:` for the currently-open conversation and return the new state
    /// (used by the `Ctrl+V` keybind). `None` when no conversation is open.
    pub fn toggle_current_voice_in(&mut self) -> Option<bool> {
        let conv_id = self.core.current_conversation()?.id.clone();
        let next = !self.core.voice_in_for(&conv_id);
        self.set_voice_in(&conv_id, next);
        Some(next)
    }

    /// Set `You:` (voice input) for an explicit `conversation_id`.
    /// Per-conversation: only the named conversation is affected. Mirrors
    /// [`Self::set_adele_output`]; routes the write through `core.apply`.
    pub fn set_voice_in(&mut self, conversation_id: &str, enabled: bool) {
        self.core.apply(UiMessage::SetVoiceIn {
            conversation_id: conversation_id.to_string(),
            enabled,
        });
    }

    /// The `Adele:` (voice output) level for `conversation_id`. `Disabled` when
    /// the conversation was never set (the default).
    pub fn adele_output_for(&self, conversation_id: &str) -> AdeleOutput {
        self.core.adele_output_for(conversation_id)
    }

    /// The `Adele:` level for the currently-open conversation. `Disabled` when
    /// no conversation is open.
    pub fn current_adele_output(&self) -> AdeleOutput {
        self.core
            .current_conversation()
            .map(|c| self.adele_output_for(&c.id))
            .unwrap_or_default()
    }

    /// Set `Adele:` for an explicit `conversation_id` (used by the model's
    /// `request_voice` → OnDemand / `stop_voice` → Disabled tools, which carry
    /// their own conversation). Per-conversation: only the named conversation is
    /// affected.
    pub fn set_adele_output(&mut self, conversation_id: &str, level: AdeleOutput) {
        self.core.apply(UiMessage::SetAdeleOutput {
            conversation_id: conversation_id.to_string(),
            level,
        });
    }

    /// Cycle `Adele:` for the currently-open conversation
    /// (`Disabled → OnDemand → Always → Disabled`) and return the new level
    /// (used by the `Ctrl+S` keybind). `None` when no conversation is open.
    pub fn cycle_current_adele_output(&mut self) -> Option<AdeleOutput> {
        let conv_id = self.core.current_conversation()?.id.clone();
        let next = self.core.adele_output_for(&conv_id).next();
        self.set_adele_output(&conv_id, next);
        Some(next)
    }

    /// Whether a *reply* is spoken for `conversation_id` (adele-tui#77, the
    /// narration gate): `Adele == Always` OR (`Adele == OnDemand` AND
    /// `You == Enabled`). `Disabled` never narrates. Delegates to the shared
    /// gate (desktop-assistant#274).
    pub fn narrate_for(&self, conversation_id: &str) -> bool {
        self.core.narrate_for(conversation_id)
    }

    /// Whether a `say_this` aside is spoken for `conversation_id` (adele-tui#77):
    /// spoken iff `Adele ∈ {OnDemand, Always}` (independent of `You`). `Disabled`
    /// downgrades the aside to inline text. Delegates to the shared gate
    /// (desktop-assistant#274).
    pub fn say_this_spoken_for(&self, conversation_id: &str) -> bool {
        self.core.say_this_spoken_for(conversation_id)
    }

    /// Render a `say_this` call whose aside is NOT spoken (Adele ≠ OnDemand, or
    /// no speech backend) as a transcript line instead (adele-tui#77). Appended
    /// to `conversation_id` only when that is the open conversation, so a call
    /// from a stale/other conversation never bleeds into the visible chat.
    /// Returns whether the note was shown. The line carries clean `content` and
    /// `MessageKind::SpeechDisabled`; the "(speech mode disabled)" marker is
    /// added at render time from the metadata, not baked into the text
    /// (voice#126).
    pub fn push_speech_disabled_note(&mut self, conversation_id: &str, text: &str) -> bool {
        self.push_local_say_this(conversation_id, text, MessageKind::SpeechDisabled)
    }

    /// Render a `say_this` aside that WAS spoken as a transcript line tagged
    /// `MessageKind::Spoken` (voice#126), so the user sees what Adele voiced.
    /// Same open-conversation guard as [`Self::push_speech_disabled_note`].
    pub fn push_spoken_note(&mut self, conversation_id: &str, text: &str) -> bool {
        self.push_local_say_this(conversation_id, text, MessageKind::Spoken)
    }

    /// Shared body for the two `say_this` transcript lines: push a client-local
    /// assistant message with clean `content` and an explicit presentation
    /// `kind`, only when `conversation_id` is the open conversation.
    fn push_local_say_this(
        &mut self,
        conversation_id: &str,
        text: &str,
        kind: MessageKind,
    ) -> bool {
        let Some(conv) = self.core.current_conversation_mut() else {
            return false;
        };
        if conv.id != conversation_id {
            return false;
        }
        conv.messages.push(ChatMessage {
            // Local-only line (never persisted daemon-side), so it has no
            // daemon-assigned message id (#1): the dedupe/ordering cursor is only
            // meaningful for daemon-sourced messages.
            id: String::new(),
            role: "assistant".to_string(),
            content: text.to_string(),
            kind,
        });
        self.scroll_offset = 0;
        true
    }

    // --- Tasks-pane glue ---
    //
    // The actual mutation logic lives in `TaskPane`; these methods exist
    // so main.rs can drive UI actions ("toggle the pane", "switch to the
    // conversation linked to the selected task") without having to reach
    // through `app.tasks` for every operation.

    /// Toggle the process-manager overlay.
    pub fn toggle_tasks_pane(&mut self) {
        self.tasks.toggle();
    }

    /// Select the conversation linked to the currently-highlighted task,
    /// closing the pane on success. Returns the conversation id so the
    /// caller can fetch the conversation detail from the daemon. `None`
    /// when the selected task has no linked conversation (or nothing is
    /// selected).
    pub fn jump_to_selected_task_conversation(&mut self) -> Option<String> {
        let row = self.tasks.selected_row()?;
        let conv_id = row.conversation_id.clone()?;
        // Move sidebar selection to the linked conversation if it's
        // present in the local list. Loading the detail itself is
        // main's responsibility (it needs the WS client).
        if let Some(idx) = self.core.conversations.iter().position(|c| c.id == conv_id) {
            self.selected_conversation = Some(idx);
        }
        self.tasks.visible = false;
        Some(conv_id)
    }

    /// Stage a `CancelBackgroundTask` for the highlighted task. Returns
    /// the task id so the caller can fire the command. `None` when
    /// nothing is selected — terminal rows are evicted from the pane,
    /// so a selected row is always cancellable.
    pub fn request_cancel_selected_task(&mut self) -> Option<TaskId> {
        let row = self.tasks.selected_row()?;
        let id = row.id.clone();
        self.pending_task_cancel = Some(id.clone());
        self.status_message = format!("Cancelling {id}...");
        Some(id)
    }

    /// Apply the user's model picker selection to local state so the
    /// chat title/status reflects it immediately. The override is staged
    /// for the next prompt; once that prompt fires the daemon persists it
    /// as `last_model_selection`.
    pub fn apply_model_override(
        &mut self,
        override_selection: desktop_assistant_api_model::SendPromptOverride,
    ) {
        if let Some(conv) = self.core.current_conversation_mut() {
            conv.model_selection = Some(
                desktop_assistant_api_model::ConversationModelSelectionView {
                    connection_id: override_selection.connection_id.clone(),
                    model_id: override_selection.model_id.clone(),
                    effort: override_selection.effort,
                },
            );
        }
        self.pending_model_override = Some(override_selection);
    }

    /// Take and clear the pending override. Called once per `SendPrompt`.
    pub fn take_pending_override(
        &mut self,
    ) -> Option<desktop_assistant_api_model::SendPromptOverride> {
        self.pending_model_override.take()
    }

    pub fn set_assistant_status(&mut self, message: impl Into<String>) {
        let msg = message.into();
        if msg.trim().is_empty() {
            self.assistant_status = None;
        } else {
            self.assistant_status = Some(msg);
        }
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    // --- Navigation ---

    pub fn next_conversation(&mut self) {
        if self.core.conversations.is_empty() {
            return;
        }
        self.selected_conversation = Some(match self.selected_conversation {
            Some(i) => {
                if i >= self.core.conversations.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        });
    }

    pub fn previous_conversation(&mut self) {
        if self.core.conversations.is_empty() {
            return;
        }
        self.selected_conversation = Some(match self.selected_conversation {
            Some(i) => {
                if i == 0 {
                    self.core.conversations.len() - 1
                } else {
                    i - 1
                }
            }
            None => self.core.conversations.len() - 1,
        });
    }

    // --- Scrolling ---

    pub fn scroll_up(&mut self, lines: u16) {
        self.scroll_offset = self.scroll_offset.saturating_add(lines);
    }

    pub fn scroll_down(&mut self, lines: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    // --- Input ---

    /// Returns the textarea content as a single string (lines joined with newlines).
    pub fn textarea_content(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Empty the composer (after an accepted send). The submission gate, the
    /// optimistic user-bubble append, and the streaming block (TUI-2 / TUI-7) all
    /// live in the shared core now, reached via `UiMessage::SubmitPrompt`; the
    /// composer is the one piece of pure view state the client still owns.
    pub fn clear_composer(&mut self) {
        self.textarea = new_textarea();
    }

    /// Refill the composer with `text` (after a failed send, so the user can
    /// retry without retyping). The matching transcript rollback runs in the core
    /// via `UiMessage::SendFailed`.
    pub fn set_composer(&mut self, text: &str) {
        self.textarea = new_textarea();
        self.textarea.insert_str(text);
    }

    /// Apply a bracketed paste (TUI-3 / `Event::Paste`) to whichever input is
    /// focused. Pasted text is inserted **verbatim** into the composer — each
    /// newline becomes a real line of the prompt instead of firing
    /// `SubmitPrompt` per line (the pre-bracketed-paste failure mode). The
    /// rename input is single-line, so newlines collapse to spaces there.
    /// Normal mode has no focused input; the paste is ignored.
    pub fn apply_paste(&mut self, text: &str) {
        match self.mode {
            InputMode::Editing => {
                // `insert_str` splits on '\n' and strips a trailing '\r' per
                // line, so CRLF pastes normalize to real composer lines.
                self.textarea.insert_str(text);
            }
            InputMode::Renaming => {
                let single_line = text
                    .split(['\n', '\r'])
                    .filter(|piece| !piece.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                self.rename_textarea.insert_str(single_line);
            }
            InputMode::Normal => {}
        }
    }

    /// Build a **display-only** word-wrapped copy of the composer for the
    /// given editor width (in terminal columns).
    ///
    /// This is the fix for issue #84 (TUI-6). The backing `ratatui_textarea`
    /// widget has no soft-wrap, so to show long lines wrapped we used to
    /// *rewrite* `self.textarea` with the wrapped lines — which baked
    /// terminal-width newlines into `textarea_content()`/`submit_prompt`,
    /// mutating the prompt actually sent. Instead, `self.textarea` remains the
    /// untouched logical source of truth (exactly what the user typed) and
    /// this returns a throwaway `TextArea` to render. The wrap is purely
    /// presentational; the segments concatenate back to the original lines, so
    /// no wrap point can ever reach the wire.
    ///
    /// The live cursor is mapped from its logical (row, col) into the wrapped
    /// coordinate space so editing still tracks correctly on screen. Wrapping
    /// is measured by Unicode display width, so CJK/emoji wrap at the column
    /// they occupy.
    pub fn wrapped_display_textarea(&self, width: usize) -> TextArea<'static> {
        let original_lines = self.textarea.lines().to_vec();
        if width == 0 {
            let mut display = TextArea::from(original_lines);
            display.set_cursor_line_style(Style::default());
            return display;
        }

        let DataCursor(cursor_row, cursor_col) = self.textarea.cursor();
        let mut wrapped_lines: Vec<String> = Vec::new();
        let mut wrapped_cursor_row = 0usize;
        let mut wrapped_cursor_col = 0usize;

        for (row_idx, line) in original_lines.iter().enumerate() {
            let segments = wrap_line_for_width(line, width);
            if row_idx < cursor_row {
                wrapped_cursor_row += segments.len();
            } else if row_idx == cursor_row {
                let (segment_idx, segment_col) =
                    map_cursor_col_to_wrapped_segments(&segments, cursor_col);
                wrapped_cursor_row += segment_idx;
                wrapped_cursor_col = segment_col;
            }
            wrapped_lines.extend(segments);
        }

        if wrapped_lines.is_empty() {
            wrapped_lines.push(String::new());
        }

        if wrapped_cursor_row >= wrapped_lines.len() {
            wrapped_cursor_row = wrapped_lines.len().saturating_sub(1);
            wrapped_cursor_col = wrapped_lines[wrapped_cursor_row].chars().count();
        } else {
            wrapped_cursor_col =
                wrapped_cursor_col.min(wrapped_lines[wrapped_cursor_row].chars().count());
        }

        let mut display = TextArea::from(wrapped_lines);
        display.set_cursor_line_style(Style::default());
        display.move_cursor(CursorMove::Jump(
            wrapped_cursor_row.min(u16::MAX as usize) as u16,
            wrapped_cursor_col.min(u16::MAX as usize) as u16,
        ));
        display
    }

    // --- Mode transitions ---

    pub fn enter_editing_mode(&mut self) {
        self.mode = InputMode::Editing;
    }

    pub fn enter_normal_mode(&mut self) {
        self.mode = InputMode::Normal;
    }

    // --- Streaming (delegated to the shared core, CC-3) ---
    //
    // The stream knows its conversation (TUI-4 / GTK-2): the reducer records the
    // conversation a prompt was sent to and keys chunk rendering, completion, and
    // reply narration off THAT conversation, not whichever one is open when an
    // event arrives. `App` holds none of that state — it feeds daemon stream
    // events into `core.apply` via `apply_core` and mirrors the resulting
    // view-effects onto its own transcript.

    /// Feed a daemon-derived [`UiMessage`] into the shared core, apply the
    /// *view-level* effects to `App`'s own state in place, and return the
    /// *controller-level* effects the view can't perform itself (today:
    /// [`Effect::Speak`] narration and [`Effect::FetchScratchpad`], which need
    /// handles `App` doesn't hold). The caller (`main`'s signal loop) runs those.
    ///
    /// View-effects are applied here because the TUI redraws from state every
    /// frame: a streamed chunk is already on screen once it lands in
    /// `core.streaming_buffer()`, so [`Effect::ReceiveChunk`] is a no-op — and
    /// since the reducer now owns the open transcript and the conversation list
    /// (CC-3 slice 4), the transcript-finalizing and `ClearChat` effects are
    /// no-ops too. Only the TUI-only view state (status lines, context-usage
    /// readout, positional selection) still needs a write here.
    pub fn apply_core(&mut self, msg: UiMessage) -> Vec<Effect> {
        let effects = self.core.apply(msg);
        effects
            .into_iter()
            .filter_map(|effect| self.run_view_effect(effect))
            .collect()
    }

    /// Apply one effect's view-level part to `App`, returning `Some(effect)` for
    /// the controller-level effects the caller must still run (narration, RPCs).
    fn run_view_effect(&mut self, effect: Effect) -> Option<Effect> {
        match effect {
            // The transcript view re-reads `core.streaming_buffer()` each frame,
            // so the chunk is already visible — nothing to do. (Scroll follows
            // only at the bottom, TUI-10, which needs no write either.)
            Effect::ReceiveChunk(_) => None,
            // The reducer now owns the open conversation's transcript
            // (`core.current_conversation`, CC-3 slice 4) and pushes the finalized
            // reply / adopted external user bubble into it itself — guarded so it
            // only ever writes when the originating conversation is the one in view
            // (TUI-4). The view re-reads that transcript each frame, so these
            // effects carry no view-level work.
            Effect::CompleteStreaming(_) | Effect::AddUserMessage(_) => None,
            Effect::SetChatStatus(message) => {
                self.set_assistant_status(message);
                None
            }
            Effect::ClearChatStatus => {
                self.assistant_status = None;
                None
            }
            // The reducer owns the composer for the message-queue flows: it
            // clears it (`""`) when a submitted prompt is enqueued or an edit is
            // cancelled, and loads a recalled queued message into it for editing.
            // The live textarea is pure view state, so apply it here — routing it
            // through `apply_core` means every dispatch (submit, recall, cancel,
            // and the queue flush a StreamComplete emits) keeps the composer in
            // sync without the caller having to special-case it.
            Effect::SetComposerText(text) => {
                if text.is_empty() {
                    self.clear_composer();
                } else {
                    self.set_composer(&text);
                }
                None
            }
            // The TUI redraws the "N queued" indicator from `core` state each
            // frame (`queued_messages_for_view` / `editing_queued_index`), so the
            // render-ready snapshot carries no view-level work here.
            Effect::SetQueuedMessages { .. } => None,
            Effect::SetContextUsage(usage) => {
                self.context_usage = usage;
                None
            }
            Effect::SetStatusText(text) => {
                self.status_message = text;
                None
            }
            // Repaint the sidebar list. `set_conversations` re-clamps the
            // positional selection and keeps core's list in sync.
            Effect::SetConversations(convs) => {
                self.set_conversations(convs);
                None
            }
            // The TUI does not auto-open a conversation (unlike gtk): the sidebar
            // selection is already clamped by `SetConversations`, and the open
            // conversation is whatever the user last opened. So "ensure active"
            // is a no-op here — the auto-load/create that gtk's executor performs
            // is deliberately omitted (CC-3: behavior-preserving).
            Effect::EnsureActiveConversation => None,
            // The active conversation was deleted. The reducer already cleared
            // `core.current_conversation` (and its id) before emitting this, so the
            // view — which reads that field — needs no further write (CC-3 slice 4).
            Effect::ClearChat => None,
            // The TUI has no side pane (the scratchpad + per-conversation task
            // list live in the gtk/kde clients), so these are inert here.
            Effect::SidePaneSetScratchpad(_)
            | Effect::RefreshSidePaneTasks
            | Effect::FetchScratchpad(_) => None,
            // Controller-level effects (narration, and — in later CC-3 slices —
            // the open-conversation RPC effects) need handles the view doesn't
            // hold; bubble them up to `main`'s executor.
            other => Some(other),
        }
    }

    /// The live streaming partial for the open conversation, read back from the
    /// shared core. Empty when nothing is buffering, or when the in-flight stream
    /// belongs to a backgrounded conversation.
    pub fn streaming_buffer(&self) -> &str {
        self.core.streaming_buffer()
    }

    /// Whether the in-flight stream (if any) belongs to the open conversation
    /// (TUI-4) — the render guard the transcript view consults before painting
    /// the live partial.
    pub fn streaming_is_active_for_view(&self) -> bool {
        self.core.streaming_is_active_for_view()
    }

    /// Whether a streamed reply is currently in flight. The submit path gates on
    /// this so a second prompt can't be sent mid-stream (TUI-7).
    pub fn is_streaming(&self) -> bool {
        self.core.is_streaming()
    }

    /// The messages queued for the open conversation (submit order), read back
    /// from the shared core. The draw path renders the "N queued" indicator from
    /// this each frame; the key dispatch consults it to decide whether an empty
    /// composer's `Up`/`Down` should recall a queued message.
    pub fn queued_messages_for_view(&self) -> &[String] {
        self.core.queued_messages_for_view()
    }

    /// The queue index currently checked out into the composer for editing, or
    /// `None` when composing a fresh message. Drives the up/down queue walk.
    pub fn editing_queued_index(&self) -> Option<usize> {
        self.core.editing_queued_index()
    }

    /// Recall the previous queued message into the composer for editing (`Up`).
    /// Walks one step toward the oldest from the checked-out item, or recalls the
    /// newest when composing fresh. A no-op at the front of the queue or when the
    /// queue is empty. The reducer loads the text via `SetComposerText`, applied
    /// in `run_view_effect`.
    pub fn recall_prev_queued(&mut self) {
        let queued_len = self.core.queued_messages_for_view().len();
        if let Some(index) = recall_prev_index(queued_len, self.core.editing_queued_index()) {
            let _ = self.apply_core(UiMessage::EditQueued { index });
        }
    }

    /// Step forward through the queue while editing (`Down`): recall the next
    /// queued message, or — once past the newest — return the checked-out message
    /// to the queue and clear the composer. A no-op when not editing a queued
    /// message.
    pub fn recall_next_queued(&mut self) {
        let queued_len = self.core.queued_messages_for_view().len();
        match recall_next_action(queued_len, self.core.editing_queued_index()) {
            RecallNext::Edit(index) => {
                let _ = self.apply_core(UiMessage::EditQueued { index });
            }
            RecallNext::Cancel => {
                let _ = self.apply_core(UiMessage::CancelQueuedEdit);
            }
            RecallNext::None => {}
        }
    }

    /// Return the currently checked-out queued message to the queue and clear the
    /// composer, if an edit is in progress (`Esc` while editing a recalled item).
    /// A no-op otherwise. Distinct from `enter_normal_mode`, which the caller runs
    /// alongside this to leave edit mode.
    pub fn cancel_queued_edit_if_active(&mut self) {
        if self.core.editing_queued_index().is_some() {
            let _ = self.apply_core(UiMessage::CancelQueuedEdit);
        }
    }

    /// Record a `send_prompt` ack: register the in-flight turn (and its
    /// originating conversation, TUI-4) in the shared core. The wire value is a
    /// `task_id` (post-desktop-assistant#114 `SendMessageAck`) or an empty string
    /// (legacy `Ack`) — neither is the chunk-stream `request_id` (server-generated,
    /// arriving in the first `AssistantDelta`), so the reducer seeds a sentinel the
    /// first stream event claims (#52). `PromptSent` emits no effects.
    pub fn apply_prompt_ack(&mut self, task_id: String, conversation_id: String) {
        let _ = self.core.apply(UiMessage::PromptSent {
            task_id,
            conversation_id,
        });
        // Immediate feedback in the gap between Enter and the first streamed
        // token. The daemon's own `Status` events (e.g. "Searching…") overwrite
        // this, and completion/error clears it.
        self.set_assistant_status("Adele is thinking…");
    }

    /// Drop all in-flight streaming state on a connection teardown (TUI-8): the
    /// stream died with the link, so the frozen `▌` buffer must not linger and the
    /// ack sentinel must not mis-claim the first post-reconnect stream. Delegates
    /// to the core (which owns the streaming state); also clears the TUI-only
    /// transient assistant-status line.
    pub fn clear_streaming_state(&mut self) {
        self.core.reset_streaming_state();
        self.assistant_status = None;
    }

    /// Move the sidebar selection to the conversation with `id`, returning
    /// whether it was found (TUI-8: selection is positional, so after a
    /// reconnect's list refresh we reselect by id, not index).
    pub fn select_conversation_by_id(&mut self, id: &str) -> bool {
        match self.core.conversations.iter().position(|c| c.id == id) {
            Some(idx) => {
                self.selected_conversation = Some(idx);
                true
            }
            None => false,
        }
    }

    // --- Conversation management ---

    /// The conversation list, owned by the shared core (CC-3 slice 4): the
    /// reducer is authoritative for it (its conversation arms read + mutate
    /// `core.conversations`), and the view reads it back through here. The TUI
    /// keeps only the *positional* selection (`selected_conversation`) as view
    /// state.
    pub fn conversations(&self) -> &[ConversationSummary] {
        &self.core.conversations
    }

    /// The open conversation's detail — transcript, title, and model selection —
    /// owned by the shared core (CC-3 slice 4). The reducer maintains it: it
    /// finalizes streamed replies into it, adopts external turns, and clears it
    /// when the active conversation is deleted (`App::load_conversation` seeds it
    /// on open). The view + controller read it back through here. `None` when no
    /// conversation is open.
    pub fn current_conversation(&self) -> Option<&ConversationDetail> {
        self.core.current_conversation()
    }

    pub fn set_conversations(&mut self, conversations: Vec<ConversationSummary>) {
        // The shared core owns the list (CC-3 slice 4): store it there directly.
        // The reducer's conversation arms read `core.conversations` (e.g. to
        // decide reconnect reload vs. fresh load), and paths that don't route
        // through `core.apply` (connect-time load, create/delete) funnel here, so
        // this keeps core authoritative regardless of the caller.
        self.core.conversations = conversations;
        // Fix selection if out of bounds (positional selection is TUI view state).
        let len = self.core.conversations.len();
        if let Some(sel) = self.selected_conversation
            && sel >= len
        {
            self.selected_conversation = if len == 0 { None } else { Some(len - 1) };
        }
    }

    pub fn load_conversation(&mut self, detail: ConversationDetail) {
        let incoming_id = detail.id.clone();
        let outgoing_id = self.core.current_conversation().map(|c| c.id.clone());
        let switching = outgoing_id.as_deref() != Some(incoming_id.as_str());
        // Save the conversation we're leaving: snapshot its unsent composer text
        // into the shared model so switching back restores it (#2). The textarea
        // stays the live editor; the model owns the saved draft (empty drops it).
        if switching && let Some(id) = outgoing_id.as_deref() {
            let draft = self.textarea_content();
            self.core.set_composer_draft(id, draft);
        }
        // Seed the shared core's open conversation (CC-3 slice 4): it owns both the
        // rendered transcript (the reducer finalizes streamed replies into it) and
        // the id its streaming originating-conversation checks (TUI-4 / GTK-2)
        // judge against, so the stream events route to the conversation in view.
        self.core.open_conversation(detail);
        if switching {
            // Drop a stale context-fill reading when the visible conversation
            // changes; the next turn re-establishes it (#341).
            self.context_usage = None;
            // Restore the conversation we're entering: load its saved draft into
            // the composer (empty if none), cursor at end.
            let draft = self.core.composer_draft(&incoming_id).to_string();
            self.set_composer(&draft);
        }
    }

    /// Apply a live `TitleChanged` signal: the sidebar list rename flows through
    /// the shared core (which owns the list and repaints it via `SetConversations`),
    /// and the open chat's cached title is refreshed locally — the reducer owns
    /// only the list, but the TUI's chat header reads `current_conversation.title`.
    pub fn update_conversation_title(&mut self, conversation_id: &str, title: &str) {
        let _ = self.apply_core(UiMessage::TitleChanged {
            conversation_id: conversation_id.to_string(),
            title: title.to_string(),
        });
        self.set_open_conversation_title(conversation_id, title);
    }

    /// Refresh the open conversation's cached title if it is the one named (the
    /// chat header reads it). No-op when a different conversation is open.
    fn set_open_conversation_title(&mut self, conversation_id: &str, title: &str) {
        if let Some(current) = self.core.current_conversation_mut()
            && current.id == conversation_id
        {
            current.title = title.to_string();
        }
    }

    /// Optimistically update the open conversation's per-conversation personality
    /// after the picker saves it, so the change shows without a re-fetch. The
    /// picker owns the screen while open, so the open conversation is still the
    /// one it was launched for; mirrors [`Self::apply_model_override`]. No-op when
    /// no conversation is open.
    pub fn set_open_conversation_personality(
        &mut self,
        personality: desktop_assistant_api_model::ConversationPersonalityView,
    ) {
        if let Some(conv) = self.core.current_conversation_mut() {
            conv.conversation_personality = Some(personality);
        }
    }

    pub fn selected_conversation_id(&self) -> Option<&str> {
        let idx = self.selected_conversation?;
        self.core.conversations.get(idx).map(|c| c.id.as_str())
    }

    // --- Rename ---

    /// Enter rename mode for the selected conversation, prepopulating the
    /// rename buffer with its current title. No-op if nothing is selected.
    pub fn begin_rename(&mut self) {
        let Some(idx) = self.selected_conversation else {
            return;
        };
        let Some(conv) = self.core.conversations.get(idx) else {
            return;
        };
        let mut ta = new_textarea();
        ta.insert_str(&conv.title);
        ta.move_cursor(CursorMove::End);
        self.rename_textarea = ta;
        self.renaming_id = Some(conv.id.clone());
        self.mode = InputMode::Renaming;
    }

    /// Trim the rename buffer and return `(id, new_title)` if the title is
    /// non-empty and differs from the current one. Always exits rename mode.
    pub fn submit_rename(&mut self) -> Option<(String, String)> {
        let new_title = self.rename_textarea.lines().join(" ").trim().to_string();
        let id = self.renaming_id.take();
        self.rename_textarea = new_textarea();
        self.mode = InputMode::Normal;

        let id = id?;
        if new_title.is_empty() {
            return None;
        }
        let unchanged = self
            .core
            .conversations
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.title == new_title)
            .unwrap_or(false);
        if unchanged {
            return None;
        }
        Some((id, new_title))
    }

    pub fn cancel_rename(&mut self) {
        self.renaming_id = None;
        self.rename_textarea = new_textarea();
        self.mode = InputMode::Normal;
    }

    /// Apply a renamed title (call after the daemon confirms). Routes the sidebar
    /// rename through the shared core and refreshes the open chat's cached title.
    pub fn apply_rename(&mut self, conversation_id: &str, title: &str) {
        let _ = self.apply_core(UiMessage::ConversationRenamed {
            id: conversation_id.to_string(),
            title: title.to_string(),
        });
        self.set_open_conversation_title(conversation_id, title);
    }

    // --- Delete confirmation ---

    /// Arm the delete-confirm overlay for the selected conversation, stashing its
    /// title for the prompt. No-op when nothing is selected (so a stray `d` on an
    /// empty list does nothing). The overlay is dismissed by [`Self::confirm_delete`]
    /// or [`Self::cancel_delete_confirm`]; the actual removal still goes through
    /// [`Self::delete_selected_conversation`] on confirm.
    pub fn begin_delete_confirm(&mut self) {
        let Some(idx) = self.selected_conversation else {
            return;
        };
        let Some(conv) = self.core.conversations.get(idx) else {
            return;
        };
        self.pending_delete_conversation = Some(conv.title.clone());
    }

    /// Whether the delete-confirm overlay is currently up.
    pub fn delete_confirm_pending(&self) -> bool {
        self.pending_delete_conversation.is_some()
    }

    /// Dismiss the delete-confirm overlay on confirmation, returning `true` when
    /// it was actually up (the caller then runs the delete). The removal itself is
    /// left to [`Self::delete_selected_conversation`] so the existing delete path
    /// is unchanged.
    pub fn confirm_delete(&mut self) -> bool {
        self.pending_delete_conversation.take().is_some()
    }

    /// Dismiss the delete-confirm overlay without deleting.
    pub fn cancel_delete_confirm(&mut self) {
        self.pending_delete_conversation = None;
    }

    pub fn delete_selected_conversation(&mut self) -> Option<String> {
        let id = self.selected_conversation_id()?.to_string();
        // Route the removal through the shared core: the reducer drops the row,
        // prunes the conversation's per-conversation voice state (GTK-9 — the TUI
        // previously leaked it, growing the maps unbounded), and clears the open
        // chat when the deleted conversation was the active one. The emitted
        // effects are all view-effects (SetConversations re-clamps the positional
        // selection; ClearChat blanks the chat; the side-pane + EnsureActive
        // effects are TUI no-ops), so `apply_core` fully handles them.
        let effects = self.apply_core(UiMessage::ConversationDeleted { id: id.clone() });
        debug_assert!(
            effects.is_empty(),
            "ConversationDeleted must emit only view-effects: {effects:?}"
        );
        Some(id)
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Context-usage indicator (#341) ---

    fn usage(used: u64, budget: u64, compaction: bool) -> ContextUsageView {
        ContextUsageView {
            used_tokens: used,
            budget_tokens: budget,
            compaction_active: compaction,
        }
    }

    #[test]
    fn context_usage_readout_formats_used_over_budget_with_percent() {
        assert_eq!(usage(12_000, 32_000, false).readout(), "12k / 32k (38%)");
        // Small exact counts are not abbreviated.
        assert_eq!(usage(500, 8_000, false).readout(), "500 / 8k (6%)");
    }

    #[test]
    fn context_usage_readout_marks_active_compaction() {
        assert!(usage(30_000, 32_000, true).readout().ends_with(" ⟳"));
        assert!(!usage(30_000, 32_000, false).readout().contains('⟳'));
    }

    #[test]
    fn context_usage_zero_used_at_turn_start_is_green_zero_percent() {
        let u = usage(0, 32_000, false);
        assert_eq!(u.level(), ContextFillLevel::Green);
        assert_eq!(u.readout(), "0 / 32k (0%)");
    }

    #[test]
    fn context_usage_below_threshold_is_green() {
        // 0.84 of budget — just under the 0.85 line.
        assert_eq!(
            usage(26_880, 32_000, false).level(),
            ContextFillLevel::Green
        );
    }

    #[test]
    fn context_usage_exactly_at_0_85_is_amber_inclusive() {
        // 0.85 * 32_000 == 27_200. The amber threshold is inclusive at 0.85.
        assert_eq!(
            usage(27_200, 32_000, false).level(),
            ContextFillLevel::Amber
        );
    }

    #[test]
    fn context_usage_between_threshold_and_budget_is_amber() {
        assert_eq!(
            usage(30_000, 32_000, false).level(),
            ContextFillLevel::Amber
        );
    }

    #[test]
    fn context_usage_at_budget_is_red() {
        assert_eq!(usage(32_000, 32_000, false).level(), ContextFillLevel::Red);
    }

    #[test]
    fn context_usage_over_budget_overflow_is_red() {
        let u = usage(40_000, 32_000, false);
        assert_eq!(u.level(), ContextFillLevel::Red);
        // Percent reads over 100 — honest overflow, not clamped.
        assert_eq!(u.readout(), "40k / 32k (125%)");
    }

    #[test]
    fn context_usage_zero_budget_renders_neutrally_without_panic() {
        // 200K-fallback / unknown budget degenerate guard: no divide-by-zero,
        // green, 0%.
        let u = usage(5_000, 0, false);
        assert_eq!(u.fraction(), 0.0);
        assert_eq!(u.level(), ContextFillLevel::Green);
        assert_eq!(u.readout(), "5k / 0 (0%)");
    }

    #[test]
    fn context_usage_signal_updates_the_indicator_only_for_the_open_conversation() {
        // The reducer gates the `ContextUsage` signal on the open conversation
        // (#341); apply_core wires the resulting `SetContextUsage` effect onto
        // App's indicator. A background turn's reading must not paint.
        let mut app = App::new();
        app.load_conversation(detail("c1"));

        app.apply_core(UiMessage::ContextUsage {
            conversation_id: "c2".into(),
            used_tokens: 10_000,
            budget_tokens: 32_000,
            compaction_active: false,
        });
        assert_eq!(
            app.context_usage, None,
            "a background reading must not paint"
        );

        app.apply_core(UiMessage::ContextUsage {
            conversation_id: "c1".into(),
            used_tokens: 10_000,
            budget_tokens: 32_000,
            compaction_active: false,
        });
        assert_eq!(app.context_usage, Some(usage(10_000, 32_000, false)));
    }

    #[test]
    fn switching_conversation_clears_stale_context_usage() {
        let mut app = App::new();
        app.load_conversation(detail("c1"));
        app.apply_core(UiMessage::ContextUsage {
            conversation_id: "c1".into(),
            used_tokens: 10_000,
            budget_tokens: 32_000,
            compaction_active: false,
        });
        assert!(app.context_usage.is_some());
        app.load_conversation(detail("c2"));
        assert_eq!(
            app.context_usage, None,
            "stale reading must not bleed across"
        );
    }

    #[test]
    fn composer_draft_is_saved_and_restored_across_a_switch() {
        // The composer draft is per-conversation (#2): switching away snapshots
        // the unsent text into the shared model, switching back restores it.
        let mut app = App::new();
        app.load_conversation(detail("c1"));
        app.textarea.insert_str("half-written c1 message");

        app.load_conversation(detail("c2"));
        assert_eq!(
            app.textarea_content(),
            "",
            "a fresh conversation opens with an empty composer"
        );
        app.textarea.insert_str("c2 thoughts");

        app.load_conversation(detail("c1"));
        assert_eq!(
            app.textarea_content(),
            "half-written c1 message",
            "switching back restores c1's saved draft"
        );

        app.load_conversation(detail("c2"));
        assert_eq!(
            app.textarea_content(),
            "c2 thoughts",
            "and c2's draft is preserved independently"
        );
    }

    #[test]
    fn reopening_the_same_conversation_keeps_the_in_progress_draft() {
        // A reload (same id — e.g. after a rename or reconnect) is not a switch,
        // so it must not wipe what the user is mid-way through typing.
        let mut app = App::new();
        app.load_conversation(detail("c1"));
        app.textarea.insert_str("still typing");
        app.load_conversation(detail("c1"));
        assert_eq!(app.textarea_content(), "still typing");
    }

    fn sample_conversations() -> Vec<ConversationSummary> {
        vec![
            ConversationSummary {
                id: "1".into(),
                title: "First".into(),
                message_count: 2,
                archived: false,
            },
            ConversationSummary {
                id: "2".into(),
                title: "Second".into(),
                message_count: 0,
                archived: false,
            },
            ConversationSummary {
                id: "3".into(),
                title: "Third".into(),
                message_count: 5,
                archived: false,
            },
        ]
    }

    fn app_with_conversations() -> App {
        let mut app = App::new();
        app.set_conversations(sample_conversations());
        app
    }

    // --- Navigation tests ---

    #[test]
    fn next_on_empty_list_does_nothing() {
        let mut app = App::new();
        app.next_conversation();
        assert_eq!(app.selected_conversation, None);
    }

    #[test]
    fn next_from_none_selects_first() {
        let mut app = app_with_conversations();
        app.next_conversation();
        assert_eq!(app.selected_conversation, Some(0));
    }

    #[test]
    fn next_wraps_around() {
        let mut app = app_with_conversations();
        app.selected_conversation = Some(2);
        app.next_conversation();
        assert_eq!(app.selected_conversation, Some(0));
    }

    #[test]
    fn next_advances() {
        let mut app = app_with_conversations();
        app.selected_conversation = Some(0);
        app.next_conversation();
        assert_eq!(app.selected_conversation, Some(1));
    }

    #[test]
    fn previous_on_empty_list_does_nothing() {
        let mut app = App::new();
        app.previous_conversation();
        assert_eq!(app.selected_conversation, None);
    }

    #[test]
    fn previous_from_none_selects_last() {
        let mut app = app_with_conversations();
        app.previous_conversation();
        assert_eq!(app.selected_conversation, Some(2));
    }

    #[test]
    fn previous_wraps_around() {
        let mut app = app_with_conversations();
        app.selected_conversation = Some(0);
        app.previous_conversation();
        assert_eq!(app.selected_conversation, Some(2));
    }

    #[test]
    fn previous_goes_back() {
        let mut app = app_with_conversations();
        app.selected_conversation = Some(2);
        app.previous_conversation();
        assert_eq!(app.selected_conversation, Some(1));
    }

    #[test]
    fn single_item_next_stays() {
        let mut app = App::new();
        app.set_conversations(vec![ConversationSummary {
            id: "1".into(),
            title: "Only".into(),
            message_count: 0,
            archived: false,
        }]);
        app.selected_conversation = Some(0);
        app.next_conversation();
        assert_eq!(app.selected_conversation, Some(0));
    }

    // --- Input tests ---

    #[test]
    fn textarea_insert_and_content() {
        let mut app = App::new();
        // Type into textarea using its input method
        app.textarea.insert_char('h');
        app.textarea.insert_char('i');
        assert_eq!(app.textarea_content(), "hi");
        app.textarea.delete_char();
        assert_eq!(app.textarea_content(), "h");
        app.textarea.delete_char();
        assert_eq!(app.textarea_content(), "");
        app.textarea.delete_char(); // no panic on empty
        assert_eq!(app.textarea_content(), "");
    }

    #[test]
    fn textarea_input_newline_char_creates_new_line() {
        let mut app = App::new();
        app.textarea.insert_str("alpha");

        app.textarea.input(crossterm::event::KeyEvent {
            code: crossterm::event::KeyCode::Char('\n'),
            modifiers: crossterm::event::KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        app.textarea.insert_str("beta");

        assert_eq!(app.textarea.lines(), ["alpha", "beta"]);
        assert_eq!(app.textarea_content(), "alpha\nbeta");
    }

    #[test]
    fn textarea_input_carriage_return_char_creates_new_line() {
        let mut app = App::new();
        app.textarea.insert_str("alpha");

        app.textarea.input(crossterm::event::KeyEvent {
            code: crossterm::event::KeyCode::Char('\r'),
            modifiers: crossterm::event::KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        app.textarea.insert_str("beta");

        assert_eq!(app.textarea.lines(), ["alpha", "beta"]);
        assert_eq!(app.textarea_content(), "alpha\nbeta");
    }

    #[test]
    fn wrapped_display_textarea_wraps_long_lines_on_word_boundaries() {
        let mut app = App::new();
        app.textarea.insert_str("hello world again");

        let display = app.wrapped_display_textarea(8);

        assert_eq!(display.lines(), ["hello ", "world ", "again"]);
    }

    #[test]
    fn wrapped_display_textarea_preserves_explicit_newlines() {
        let mut app = App::new();
        app.textarea.insert_str("alpha beta\ngamma delta");

        let display = app.wrapped_display_textarea(7);

        assert_eq!(display.lines(), ["alpha ", "beta", "gamma ", "delta"]);
    }

    // --- TUI-6: display wrap must NOT mutate the logical prompt -----------

    /// The core bug (issue #84): a long line that the composer must visually
    /// wrap is still sent verbatim — no terminal-width-dependent newlines get
    /// baked into the outgoing payload.
    #[test]
    fn display_wrap_does_not_inject_newlines_into_sent_prompt() {
        let mut app = App::new();
        let typed = "the quick brown fox jumps over the lazy dog again and again";
        app.textarea.insert_str(typed);

        // Simulate several render frames at a narrow width (this is what
        // `draw_input` does every frame).
        let _ = app.wrapped_display_textarea(8);
        let _ = app.wrapped_display_textarea(8);

        // Logical content is untouched: exactly what was typed, no '\n'.
        assert_eq!(app.textarea_content(), typed);
        assert!(!app.textarea_content().contains('\n'));
    }

    /// End-to-end: type a long line, render-wrap, then submit; the outgoing
    /// payload equals the original typed text.
    #[test]
    fn submit_prompt_sends_unwrapped_text_after_display_wrap() {
        let mut app = App::new();
        app.load_conversation(ConversationDetail {
            id: "c1".into(),
            title: "t".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });
        let typed = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        app.textarea.insert_str(typed);

        // A render frame wraps for display only.
        let _ = app.wrapped_display_textarea(10);

        // The prompt that reaches the send effect is the logical (unwrapped) text,
        // not the display-wrapped copy.
        let prompt = app.textarea_content();
        let effects = app.apply_core(UiMessage::SubmitPrompt { prompt });
        let sent = sent_prompt(&effects).expect("an accepted send emits SendPrompt");
        assert_eq!(sent, typed);
        assert!(!sent.contains('\n'));
    }

    /// Shrinking then regrowing the terminal must not leave wrap newlines
    /// baked in: the logical content is identical before and after.
    #[test]
    fn display_wrap_shrink_then_grow_preserves_logical_content() {
        let mut app = App::new();
        let typed = "one two three four five six seven eight nine ten";
        app.textarea.insert_str(typed);

        let _ = app.wrapped_display_textarea(60); // wide: no wrap
        let _ = app.wrapped_display_textarea(8); // narrow: wraps for display
        let _ = app.wrapped_display_textarea(60); // wide again

        assert_eq!(app.textarea_content(), typed);
    }

    /// User-entered (explicit) newlines survive a submit unchanged — only
    /// wrap-injected ones are forbidden.
    #[test]
    fn submit_prompt_preserves_explicit_user_newlines() {
        let mut app = App::new();
        app.load_conversation(ConversationDetail {
            id: "c1".into(),
            title: "t".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });
        let typed = "first paragraph here\nsecond paragraph here";
        app.textarea.insert_str(typed);

        let _ = app.wrapped_display_textarea(8);

        let prompt = app.textarea_content();
        let effects = app.apply_core(UiMessage::SubmitPrompt { prompt });
        assert_eq!(
            sent_prompt(&effects).expect("an accepted send emits SendPrompt"),
            typed
        );
    }

    /// CJK (wide) glyphs are measured by display width, not char count, so a
    /// run of full-width characters wraps at the column it actually occupies.
    #[test]
    fn wrapped_display_textarea_measures_wide_glyphs_by_display_width() {
        let mut app = App::new();
        // Each CJK glyph is 2 display columns wide.
        app.textarea.insert_str("一二三四五");

        // Width 6 columns = room for 3 wide glyphs per display row.
        let display = app.wrapped_display_textarea(6);

        assert_eq!(display.lines(), ["一二三", "四五"]);
        // And the logical content is still the full string.
        assert_eq!(app.textarea_content(), "一二三四五");
    }

    // The submit gate, the optimistic user-bubble append, and the empty/
    // no-conversation rejections moved to the shared core (`UiMessage::SubmitPrompt`,
    // Phase-2) and are spec'd by client-ui-common's reducer tests. The App tests
    // below cover only the TUI's own send-path wiring (composer text → effect).

    // --- Message queuing (feat/queue-messages) ---
    //
    // These exercise the TUI's wiring of the queue reducer contract onto App's
    // own composer widget + view accessors: enqueue-clears-composer, the
    // stream-complete flush, and the up/down recall walk. The queue state machine
    // itself is spec'd by client-ui-common's reducer tests; here we assert the
    // `SetComposerText` effect reaches `App::textarea` and the recall index walk
    // dispatches the right `EditQueued`/`CancelQueuedEdit`.

    /// Enter a live stream on `c1` so a following `SubmitPrompt` queues (busy).
    fn app_streaming_on_c1() -> App {
        let mut app = App::new();
        app.load_conversation(detail("c1"));
        app.apply_prompt_ack("task-1".into(), "c1".into());
        app
    }

    /// Type `text` into the composer and submit it through the shared core.
    fn submit(app: &mut App, text: &str) -> Vec<Effect> {
        app.textarea.insert_str(text);
        let prompt = app.textarea_content();
        app.apply_core(UiMessage::SubmitPrompt { prompt })
    }

    #[test]
    fn submitting_while_streaming_queues_and_clears_the_composer() {
        let mut app = app_streaming_on_c1();
        let effects = submit(&mut app, "hello while busy");

        assert!(
            sent_prompt(&effects).is_none(),
            "a busy submit must not send"
        );
        assert_eq!(
            app.queued_messages_for_view(),
            &["hello while busy".to_string()]
        );
        // The reducer cleared the live composer via SetComposerText, applied in
        // run_view_effect.
        assert!(
            app.textarea_content().is_empty(),
            "composer clears when a message is queued"
        );
    }

    #[test]
    fn queued_messages_flush_as_one_combined_send_on_stream_complete() {
        let mut app = app_streaming_on_c1();
        submit(&mut app, "first");
        submit(&mut app, "second");
        assert_eq!(app.queued_messages_for_view().len(), 2);

        let effects = app.apply_core(UiMessage::StreamComplete {
            request_id: "srv-1".into(),
            full_response: "reply".into(),
        });
        assert_eq!(
            sent_prompt(&effects).expect("stream completion flushes the queue as a send"),
            "first\nsecond",
            "the queued burst flushes as ONE combined turn joined with \\n"
        );
        assert!(
            app.queued_messages_for_view().is_empty(),
            "the queue drains on flush"
        );
    }

    #[test]
    fn recall_prev_loads_the_newest_queued_message_then_walks_to_the_front() {
        let mut app = app_streaming_on_c1();
        submit(&mut app, "alpha");
        submit(&mut app, "bravo");

        app.recall_prev_queued(); // Up -> newest
        assert_eq!(app.textarea_content(), "bravo");
        assert_eq!(app.editing_queued_index(), Some(1));
        assert_eq!(app.queued_messages_for_view(), &["alpha".to_string()]);

        app.recall_prev_queued(); // Up -> older
        assert_eq!(app.textarea_content(), "alpha");
        assert_eq!(app.editing_queued_index(), Some(0));

        app.recall_prev_queued(); // Up at the front -> no-op, stays on alpha
        assert_eq!(app.textarea_content(), "alpha");
        assert_eq!(app.editing_queued_index(), Some(0));
    }

    #[test]
    fn recall_next_steps_forward_then_cancels_past_the_newest() {
        let mut app = app_streaming_on_c1();
        submit(&mut app, "alpha");
        submit(&mut app, "bravo");
        app.recall_prev_queued(); // bravo
        app.recall_prev_queued(); // alpha
        assert_eq!(app.textarea_content(), "alpha");

        app.recall_next_queued(); // Down -> bravo
        assert_eq!(app.textarea_content(), "bravo");
        assert_eq!(app.editing_queued_index(), Some(1));

        app.recall_next_queued(); // Down past newest -> cancel, restore + clear
        assert!(app.textarea_content().is_empty());
        assert_eq!(app.editing_queued_index(), None);
        assert_eq!(
            app.queued_messages_for_view(),
            &["alpha".to_string(), "bravo".to_string()]
        );
    }

    #[test]
    fn editing_a_recalled_message_reinserts_it_in_place_on_submit() {
        let mut app = app_streaming_on_c1();
        submit(&mut app, "alpha");
        submit(&mut app, "bravo");
        app.recall_prev_queued(); // check out bravo (slot 1)
        app.clear_composer();
        let effects = submit(&mut app, "bravo-edited");

        assert!(
            sent_prompt(&effects).is_none(),
            "still streaming: the edited message re-queues, not sends"
        );
        assert!(app.textarea_content().is_empty());
        assert_eq!(
            app.queued_messages_for_view(),
            &["alpha".to_string(), "bravo-edited".to_string()]
        );
        assert_eq!(app.editing_queued_index(), None);
    }

    #[test]
    fn cancelling_a_recalled_edit_returns_the_message_and_clears_composer() {
        let mut app = app_streaming_on_c1();
        submit(&mut app, "solo");
        app.recall_prev_queued();
        assert_eq!(app.textarea_content(), "solo");
        assert_eq!(app.editing_queued_index(), Some(0));

        app.cancel_queued_edit_if_active(); // Esc while editing a queued item
        assert!(app.textarea_content().is_empty());
        assert_eq!(app.editing_queued_index(), None);
        assert_eq!(app.queued_messages_for_view(), &["solo".to_string()]);
    }

    #[test]
    fn cancel_queued_edit_is_a_no_op_when_not_editing() {
        let mut app = app_streaming_on_c1();
        submit(&mut app, "queued");
        // Not checked out for editing: Esc must not disturb the queue.
        app.cancel_queued_edit_if_active();
        assert_eq!(app.queued_messages_for_view(), &["queued".to_string()]);
        assert_eq!(app.editing_queued_index(), None);
    }

    #[test]
    fn idle_send_emits_send_without_a_composer_effect() {
        // The idle single-send path emits SendPrompt but NO SetComposerText, so
        // apply_core leaves the live composer untouched — the submit path clears
        // it itself for this case.
        let mut app = App::new();
        app.load_conversation(detail("c1"));
        let effects = submit(&mut app, "just send it");
        assert_eq!(sent_prompt(&effects).as_deref(), Some("just send it"));
        assert_eq!(
            app.textarea_content(),
            "just send it",
            "apply_core does not clear the composer on the idle-send path"
        );
    }

    #[test]
    fn recall_prev_index_walks_backward_and_stops_at_the_front() {
        assert_eq!(
            recall_prev_index(0, None),
            None,
            "empty queue: nothing to recall"
        );
        assert_eq!(
            recall_prev_index(3, None),
            Some(2),
            "fresh compose -> newest"
        );
        assert_eq!(recall_prev_index(2, Some(2)), Some(1));
        assert_eq!(recall_prev_index(2, Some(1)), Some(0));
        assert_eq!(recall_prev_index(2, Some(0)), None, "at the front -> no-op");
    }

    #[test]
    fn recall_next_action_steps_forward_then_cancels_at_the_end() {
        assert_eq!(recall_next_action(2, None), RecallNext::None, "not editing");
        assert_eq!(recall_next_action(2, Some(0)), RecallNext::Edit(1));
        assert_eq!(recall_next_action(2, Some(1)), RecallNext::Edit(2));
        assert_eq!(
            recall_next_action(2, Some(2)),
            RecallNext::Cancel,
            "past newest"
        );
    }

    // --- Bracketed paste (TUI-3) ---

    #[test]
    fn paste_in_editing_inserts_multiline_text_verbatim() {
        // Acceptance: a multi-line paste lands in the composer as-is — no
        // per-newline submit, no dropped lines.
        let mut app = App::new();
        app.enter_editing_mode();
        app.apply_paste("line one\nline two\nline three");
        assert_eq!(app.textarea_content(), "line one\nline two\nline three");
    }

    #[test]
    fn paste_in_editing_normalizes_crlf_to_lf() {
        let mut app = App::new();
        app.enter_editing_mode();
        app.apply_paste("alpha\r\nbeta");
        assert_eq!(app.textarea_content(), "alpha\nbeta");
    }

    #[test]
    fn paste_then_submit_sends_one_prompt_containing_the_newlines() {
        // Acceptance (TUI-3): a 3-line paste yields ONE submitted prompt with
        // the newlines intact, not three partial prompts.
        let mut app = App::new();
        app.load_conversation(ConversationDetail {
            id: "c1".into(),
            title: "Test".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });
        app.enter_editing_mode();
        app.apply_paste("first\nsecond\nthird");
        let prompt = app.textarea_content();
        let effects = app.apply_core(UiMessage::SubmitPrompt { prompt });
        assert_eq!(
            sent_prompt(&effects).expect("an accepted send emits SendPrompt"),
            "first\nsecond\nthird"
        );
        let msgs = &app.current_conversation().unwrap().messages;
        assert_eq!(msgs.len(), 1, "exactly one user message appended");
    }

    #[test]
    fn paste_in_renaming_collapses_newlines_to_spaces() {
        // The rename input is single-line; a pasted title with newlines must
        // not create phantom lines.
        let mut app = app_with_conversations();
        app.selected_conversation = Some(0);
        app.begin_rename();
        app.rename_textarea = TextArea::default();
        app.apply_paste("New\nTitle");
        assert_eq!(app.rename_textarea.lines().join(""), "New Title");
    }

    #[test]
    fn paste_in_normal_mode_is_ignored() {
        let mut app = App::new();
        app.apply_paste("stray paste");
        assert_eq!(app.textarea_content(), "");
    }

    // --- Send-path wiring (TUI-2 / TUI-7) ---
    //
    // The send *decision* (gate, optimistic append, refinement, rollback) now
    // lives in the shared core (`UiMessage::SubmitPrompt` / `SendFailed`) and is
    // spec'd by client-ui-common's reducer tests. What stays here is the TUI's
    // own wiring: the composer text becomes the prompt, and an accepted send
    // surfaces as a `SendPrompt` effect for `send_prompt_from_input` to run. The
    // connection gate + the transport RPC live in `main.rs` (not unit-testable
    // without a daemon), so they're covered by the live smoke test.

    /// The prompt carried by the `SendPrompt` effect when a submission was
    /// accepted (the send path runs this effect as the RPC); `None` when the core
    /// rejected the submit.
    fn sent_prompt(effects: &[Effect]) -> Option<String> {
        effects.iter().find_map(|e| match e {
            Effect::SendPrompt { prompt, .. } => Some(prompt.clone()),
            _ => None,
        })
    }

    // --- Conversation-id threading (TUI-4) + disconnect reset (TUI-8) ---

    fn detail(id: &str) -> ConversationDetail {
        ConversationDetail {
            id: id.into(),
            title: format!("Conv {id}"),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        }
    }

    // These exercise the TUI's *integration* with the shared core — that
    // `apply_core` applies the reducer's streaming effects onto `App`'s own
    // transcript / buffer / status, and routes the `clear_streaming_state`
    // teardown through core. The streaming *state machine itself* (request-id
    // claiming, originating-conversation targeting, external-turn handling,
    // error/disconnect finalization) is spec'd by client-ui-common's reducer
    // tests; we don't re-test that logic here, only the wiring.

    #[test]
    fn apply_core_finalizes_an_open_streams_reply_into_the_transcript() {
        // The ack carries a task id; the chunks carry a DIFFERENT server request
        // id (#52). They must still be accepted, buffer live, and finalize into
        // the open conversation's transcript on completion.
        let mut app = App::new();
        app.load_conversation(detail("c1"));
        app.apply_prompt_ack("task-abc".into(), "c1".into());

        app.apply_core(UiMessage::StreamChunk {
            request_id: "server-xyz".into(),
            chunk: "Hello ".into(),
        });
        app.apply_core(UiMessage::StreamChunk {
            request_id: "server-xyz".into(),
            chunk: "world!".into(),
        });
        assert_eq!(
            app.streaming_buffer(),
            "Hello world!",
            "the partial buffers live"
        );

        app.apply_core(UiMessage::StreamComplete {
            request_id: "server-xyz".into(),
            full_response: "Hello world!".into(),
        });

        assert!(!app.is_streaming(), "the slot clears on completion");
        assert_eq!(app.streaming_buffer(), "", "the live partial is gone");
        let msgs = &app.current_conversation().unwrap().messages;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "assistant");
        assert_eq!(msgs[0].content, "Hello world!");
    }

    #[test]
    fn apply_core_does_not_bleed_a_backgrounded_completion_into_the_open_chat() {
        // TUI-4: a Complete arriving after the user switched conversations must
        // NOT append to the newly opened conversation, emits no narration, and
        // still clears the slot.
        let mut app = App::new();
        app.load_conversation(detail("c1"));
        app.apply_prompt_ack("task-1".into(), "c1".into());
        app.apply_core(UiMessage::StreamChunk {
            request_id: "req-1".into(),
            chunk: "partial ".into(),
        });

        app.load_conversation(detail("c2")); // switch away mid-stream

        let controller = app.apply_core(UiMessage::StreamComplete {
            request_id: "req-1".into(),
            full_response: "done".into(),
        });
        assert!(
            controller.is_empty(),
            "a backgrounded completion emits no controller effect (no narration)"
        );
        assert!(
            app.current_conversation().unwrap().messages.is_empty(),
            "the reply must not bleed into the switched-to conversation"
        );
        assert!(!app.is_streaming(), "the slot still clears");
    }

    #[test]
    fn backgrounded_stream_chunks_do_not_reset_the_open_conversations_scroll() {
        // Scroll belongs to the OPEN conversation; a backgrounded stream's chunks
        // must neither yank it nor paint into the open chat (TUI-4 / TUI-10).
        let mut app = App::new();
        app.load_conversation(detail("c1"));
        app.apply_prompt_ack("task-1".into(), "c1".into());
        app.load_conversation(detail("c2"));
        app.scroll_up(7);

        app.apply_core(UiMessage::StreamChunk {
            request_id: "req-1".into(),
            chunk: "background chunk".into(),
        });

        assert_eq!(
            app.scroll_offset, 7,
            "scroll is untouched by a backgrounded chunk"
        );
        assert!(
            !app.streaming_is_active_for_view(),
            "and it is not painted into the open chat"
        );
    }

    #[test]
    fn clear_streaming_state_resets_everything_on_disconnect() {
        // Acceptance (TUI-8): after a disconnect there is no frozen ▌ buffer, no
        // stale pending slot, and the transient assistant-status line is cleared.
        let mut app = App::new();
        app.load_conversation(detail("c1"));
        app.apply_prompt_ack("task-1".into(), "c1".into());
        app.apply_core(UiMessage::StreamChunk {
            request_id: "req-1".into(),
            chunk: "now-dead partial".into(),
        });
        app.set_assistant_status("Calling tool…");

        app.clear_streaming_state();

        assert!(!app.is_streaming());
        assert_eq!(app.streaming_buffer(), "");
        assert!(app.assistant_status.is_none());
    }

    #[test]
    fn cleared_sentinel_cannot_misclaim_the_next_stream() {
        // Unhappy path (TUI-8): a leftover ack sentinel from before the
        // disconnect must not claim the first post-reconnect stream.
        let mut app = App::new();
        app.load_conversation(detail("c1"));
        app.apply_prompt_ack("task-1".into(), "c1".into()); // sentinel armed
        app.clear_streaming_state();

        app.apply_core(UiMessage::StreamChunk {
            request_id: "post-reconnect-req".into(),
            chunk: "someone else's chunk".into(),
        });
        assert_eq!(app.streaming_buffer(), "", "chunk must be ignored");
        assert!(!app.is_streaming());
    }

    #[test]
    fn select_conversation_by_id_moves_selection() {
        let mut app = app_with_conversations();
        assert!(app.select_conversation_by_id("3"));
        assert_eq!(app.selected_conversation, Some(2));
    }

    #[test]
    fn select_conversation_by_id_missing_keeps_selection() {
        let mut app = app_with_conversations();
        app.selected_conversation = Some(1);
        assert!(!app.select_conversation_by_id("nope"));
        assert_eq!(app.selected_conversation, Some(1));
    }

    // --- Streaming tests ---
    //
    // These cover the TUI's wiring of the shared core's streaming effects. The
    // happy-path lifecycle (claim → buffer → finalize) is exercised by
    // `apply_core_finalizes_an_open_streams_reply_into_the_transcript` above; the
    // reducer's state machine itself is spec'd in client-ui-common.

    #[test]
    fn streaming_error_surfaces_in_the_status_line() {
        // apply_core wires StreamError's `SetStatusText` onto App's status line
        // and `ClearChatStatus` onto the transient assistant-status, and clears
        // the slot + buffer.
        let mut app = App::new();
        app.load_conversation(detail("c1"));
        app.apply_prompt_ack("task-1".into(), "c1".into());
        app.apply_core(UiMessage::StreamChunk {
            request_id: "req1".into(),
            chunk: "half a thought".into(),
        });
        app.set_assistant_status("Calling tool…");

        app.apply_core(UiMessage::StreamError {
            request_id: "req1".into(),
            error: "LLM timeout".into(),
        });

        assert_eq!(app.status_message, "Error: LLM timeout");
        assert!(
            app.assistant_status.is_none(),
            "the transient status clears"
        );
        assert!(!app.is_streaming());
        assert_eq!(app.streaming_buffer(), "");
    }

    // --- Live external-turn rendering (#1) --------------------------------

    #[test]
    fn external_user_message_renders_and_adopts_into_the_open_conversation() {
        // A `UserMessageAdded` for the open conversation with nothing in flight
        // is an external turn (voice / another client): apply_core renders the
        // user bubble (the `AddUserMessage` effect) and adopts the turn, so a
        // following chunk for it streams live into the open chat.
        let mut app = app_with_open_conversation("c1");

        let controller = app.apply_core(UiMessage::UserMessageAdded {
            conversation_id: "c1".into(),
            request_id: "voice-req".into(),
            content: "what's the weather?".into(),
        });
        assert!(
            controller.is_empty(),
            "rendering the bubble needs no controller effect"
        );

        let msgs = &app.current_conversation().unwrap().messages;
        assert_eq!(msgs.len(), 1, "the external user message must be rendered");
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content, "what's the weather?");

        // Adopted: its reply now streams live into the open chat.
        app.apply_core(UiMessage::StreamChunk {
            request_id: "voice-req".into(),
            chunk: "Sunny.".into(),
        });
        assert_eq!(app.streaming_buffer(), "Sunny.");
        assert!(app.streaming_is_active_for_view());
    }

    #[test]
    fn adopted_external_turn_is_not_narrated_by_this_client() {
        // An adopted external turn is narrated by its originator (the voice
        // daemon); even with the conversation's gate wide open (Adele=Always),
        // completing it must emit NO `Speak` controller effect.
        let mut app = app_with_open_conversation("c1");
        app.set_adele_output("c1", AdeleOutput::Always);
        assert!(
            app.narrate_for("c1"),
            "precondition: the gate alone would narrate"
        );

        app.apply_core(UiMessage::UserMessageAdded {
            conversation_id: "c1".into(),
            request_id: "voice-req".into(),
            content: "a question".into(),
        });
        let controller = app.apply_core(UiMessage::StreamComplete {
            request_id: "voice-req".into(),
            full_response: "the spoken answer".into(),
        });

        assert!(
            !controller.iter().any(|e| matches!(e, Effect::Speak(_))),
            "an external turn must not be narrated here: {controller:?}"
        );
    }

    #[test]
    fn own_narrating_turn_completion_returns_a_speak_effect() {
        // The flip side: this client's OWN turn, in a narrating conversation
        // (Adele=Always), must bubble up a `Speak` controller effect on
        // completion for `main` to enqueue.
        let mut app = app_with_open_conversation("c1");
        app.set_adele_output("c1", AdeleOutput::Always);
        app.apply_prompt_ack("task-1".into(), "c1".into());
        app.apply_core(UiMessage::StreamChunk {
            request_id: "req1".into(),
            chunk: "hi".into(),
        });

        let controller = app.apply_core(UiMessage::StreamComplete {
            request_id: "req1".into(),
            full_response: "the spoken answer".into(),
        });

        assert!(
            controller
                .iter()
                .any(|e| matches!(e, Effect::Speak(text) if text == "the spoken answer")),
            "an own narrating turn must bubble up a Speak effect: {controller:?}"
        );
    }

    #[test]
    fn assistant_status_set_and_cleared_through_apply_core() {
        // A daemon `Status` event paints the transient indicator (SetChatStatus);
        // completion clears it (ClearChatStatus) — both via apply_core.
        let mut app = App::new();
        app.load_conversation(detail("c1"));
        app.apply_prompt_ack("task-1".into(), "c1".into());
        app.apply_core(UiMessage::AssistantStatus {
            request_id: "req1".into(),
            message: "Searching knowledge base...".into(),
        });
        assert_eq!(
            app.assistant_status.as_deref(),
            Some("Searching knowledge base...")
        );

        app.apply_core(UiMessage::StreamComplete {
            request_id: "req1".into(),
            full_response: "done".into(),
        });
        assert!(app.assistant_status.is_none());
    }

    #[test]
    fn prompt_ack_shows_thinking_status() {
        // The send was accepted but no token has arrived yet — show an immediate
        // "thinking" cue so there's no dead air after pressing Enter.
        let mut app = App::new();
        app.load_conversation(ConversationDetail {
            id: "c1".into(),
            title: "Test".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });
        app.apply_prompt_ack("t-1".into(), "c1".into());
        assert_eq!(app.assistant_status.as_deref(), Some("Adele is thinking…"));
    }

    #[test]
    fn assistant_status_empty_string_clears() {
        let mut app = App::new();
        app.set_assistant_status("something");
        app.set_assistant_status("");
        assert!(app.assistant_status.is_none());
    }

    // The pending-sentinel claim, unrelated-request rejection, and the issue #52
    // distinct-ack-vs-server-request-id regression are all spec'd by
    // client-ui-common's reducer tests (and exercised end-to-end through the TUI
    // by `apply_core_finalizes_an_open_streams_reply_into_the_transcript`, which
    // uses a distinct ack task id and server request id).

    // --- Mode transition tests ---

    #[test]
    fn mode_transitions() {
        let mut app = App::new();
        assert_eq!(app.mode, InputMode::Normal);

        app.enter_editing_mode();
        assert_eq!(app.mode, InputMode::Editing);

        app.enter_normal_mode();
        assert_eq!(app.mode, InputMode::Normal);
    }

    // --- Conversation management tests ---

    #[test]
    fn set_conversations_fixes_out_of_bounds_selection() {
        let mut app = app_with_conversations();
        app.selected_conversation = Some(2);
        app.set_conversations(vec![ConversationSummary {
            id: "1".into(),
            title: "Only".into(),
            message_count: 0,
            archived: false,
        }]);
        assert_eq!(app.selected_conversation, Some(0));
    }

    #[test]
    fn set_empty_conversations_clears_selection() {
        let mut app = app_with_conversations();
        app.selected_conversation = Some(1);
        app.set_conversations(vec![]);
        assert_eq!(app.selected_conversation, None);
    }

    #[test]
    fn list_refetch_replaces_sidebar_without_disturbing_open_conversation() {
        // A `ConversationListChanged` / archive refetch (#1) routes through the
        // reducer's `ConversationListRefetched`: apply_core repaints the sidebar
        // (SetConversations) and re-syncs selection (EnsureActiveConversation, a
        // TUI no-op) — emitting only view-effects — while the OPEN conversation
        // and its transcript stay intact, even when rows were renamed/removed.
        let mut app = app_with_conversations();
        app.selected_conversation = Some(1);
        app.load_conversation(ConversationDetail {
            id: "2".into(),
            title: "Second".into(),
            messages: vec![
                ChatMessage {
                    id: "m1".into(),
                    role: "user".into(),
                    content: "hello".into(),
                    kind: crate::app::MessageKind::Normal,
                },
                ChatMessage {
                    id: "m2".into(),
                    role: "assistant".into(),
                    content: "hi there".into(),
                    kind: crate::app::MessageKind::Normal,
                },
            ],
            model_selection: None,
            conversation_personality: None,
        });

        // Refetched list: "1" was renamed and "3" was deleted elsewhere; "2"
        // (the open one) survives.
        let effects = app.apply_core(UiMessage::ConversationListRefetched(vec![
            ConversationSummary {
                id: "1".into(),
                title: "First (renamed elsewhere)".into(),
                message_count: 3,
                archived: false,
            },
            ConversationSummary {
                id: "2".into(),
                title: "Second".into(),
                message_count: 2,
                archived: false,
            },
        ]));
        assert!(
            effects.is_empty(),
            "a list refetch must emit only view-effects: {effects:?}"
        );

        // Sidebar reflects the new list (and core stays authoritative for it)...
        assert_eq!(app.conversations().len(), 2);
        assert_eq!(app.conversations()[0].title, "First (renamed elsewhere)");
        // ...but the open conversation + its transcript are byte-for-byte intact.
        let open = app.current_conversation().expect("still open");
        assert_eq!(open.id, "2");
        assert_eq!(open.messages.len(), 2);
        assert_eq!(open.messages[1].content, "hi there");
    }

    #[test]
    fn selected_conversation_id() {
        let mut app = app_with_conversations();
        assert_eq!(app.selected_conversation_id(), None);

        app.selected_conversation = Some(1);
        assert_eq!(app.selected_conversation_id(), Some("2"));
    }

    #[test]
    fn delete_selected_conversation() {
        let mut app = app_with_conversations();
        app.selected_conversation = Some(1);
        // `load_conversation` (not a raw field write) so core's open-conversation
        // id is set — the reducer's delete keys its "clear the open chat" on it.
        app.load_conversation(ConversationDetail {
            id: "2".into(),
            title: "Second".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });

        let deleted = app.delete_selected_conversation();
        assert_eq!(deleted, Some("2".to_string()));
        assert_eq!(app.conversations().len(), 2);
        assert!(
            app.current_conversation().is_none(),
            "deleting the active conversation clears the open chat"
        );
        assert_eq!(app.selected_conversation, Some(1)); // stays at 1 (now "Third")
    }

    // --- Delete-confirm gate (mirrors picker.rs's confirm tests; the event loop
    // routes y/Enter → confirm_delete + delete, n/Esc → cancel_delete_confirm). ---

    #[test]
    fn begin_delete_confirm_arms_overlay_with_selected_title() {
        let mut app = app_with_conversations();
        assert!(!app.delete_confirm_pending(), "starts disarmed");
        app.selected_conversation = Some(1); // "Second"
        app.begin_delete_confirm();
        assert!(app.delete_confirm_pending());
        assert_eq!(app.pending_delete_conversation.as_deref(), Some("Second"));
    }

    #[test]
    fn begin_delete_confirm_without_selection_is_noop() {
        let mut app = app_with_conversations();
        assert!(app.selected_conversation.is_none());
        app.begin_delete_confirm();
        assert!(
            !app.delete_confirm_pending(),
            "a stray `d` on nothing selected does not arm the overlay"
        );
    }

    #[test]
    fn confirm_delete_then_delete_removes_the_row() {
        // The loop's y/Enter path: confirm_delete() clears the overlay and
        // reports it was up, then delete_selected_conversation() removes the row.
        let mut app = app_with_conversations();
        app.selected_conversation = Some(1); // "Second"
        app.begin_delete_confirm();
        assert!(app.confirm_delete(), "reports the overlay was up");
        assert!(!app.delete_confirm_pending(), "overlay cleared on confirm");
        let deleted = app.delete_selected_conversation();
        assert_eq!(deleted, Some("2".to_string()));
        assert_eq!(app.conversations().len(), 2);
        assert!(app.conversations().iter().all(|c| c.id != "2"));
    }

    #[test]
    fn confirm_delete_returns_false_when_not_armed() {
        // Guards the loop's gate: the confirm branch only runs the delete when
        // the overlay was actually up (it checks `delete_confirm_pending` first,
        // and confirm_delete() reports false otherwise).
        let mut app = app_with_conversations();
        assert!(!app.confirm_delete());
    }

    #[test]
    fn cancel_delete_confirm_dismisses_without_deleting() {
        // The loop's n/Esc path: cancel clears the overlay and the row survives.
        let mut app = app_with_conversations();
        app.selected_conversation = Some(1); // "Second"
        app.begin_delete_confirm();
        app.cancel_delete_confirm();
        assert!(!app.delete_confirm_pending());
        assert_eq!(app.conversations().len(), 3, "nothing was deleted");
        assert!(app.conversations().iter().any(|c| c.id == "2"));
    }

    #[test]
    fn deleting_a_conversation_prunes_its_per_conversation_voice_state() {
        // GTK-9 / bug fix: the pre-flip TUI delete left the per-conversation
        // voice maps untouched, leaking state (and risking a later id-reuse
        // inheriting a stale setting). Routing delete through the core prunes it.
        let mut app = app_with_conversations();
        app.set_adele_output("2", AdeleOutput::Always);
        app.set_voice_in("2", true);
        assert!(app.narrate_for("2"), "precondition: voice state is set");

        app.selected_conversation = Some(1); // id "2"
        app.delete_selected_conversation();

        assert!(
            !app.narrate_for("2"),
            "voice state must be pruned on delete"
        );
        assert_eq!(app.adele_output_for("2"), AdeleOutput::Disabled);
        assert!(!app.voice_in_for("2"));
    }

    #[test]
    fn rename_routes_through_core_and_refreshes_the_open_title() {
        let mut app = app_with_conversations();
        app.load_conversation(ConversationDetail {
            id: "2".into(),
            title: "Second".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });

        app.apply_rename("2", "Renamed Second");

        // The sidebar row is updated via the core-owned list...
        let row = app.conversations().iter().find(|c| c.id == "2").unwrap();
        assert_eq!(row.title, "Renamed Second");
        // ...and the open chat's cached title (the header reads it) is refreshed.
        assert_eq!(app.current_conversation().unwrap().title, "Renamed Second");
    }

    #[test]
    fn delete_last_item_adjusts_selection() {
        let mut app = app_with_conversations();
        app.selected_conversation = Some(2);

        let deleted = app.delete_selected_conversation();
        assert_eq!(deleted, Some("3".to_string()));
        assert_eq!(app.selected_conversation, Some(1));
    }

    #[test]
    fn delete_only_item_clears_selection() {
        let mut app = App::new();
        app.set_conversations(vec![ConversationSummary {
            id: "1".into(),
            title: "Only".into(),
            message_count: 0,
            archived: false,
        }]);
        app.selected_conversation = Some(0);

        let deleted = app.delete_selected_conversation();
        assert_eq!(deleted, Some("1".to_string()));
        assert_eq!(app.selected_conversation, None);
    }

    #[test]
    fn delete_with_no_selection_returns_none() {
        let mut app = app_with_conversations();
        assert!(app.delete_selected_conversation().is_none());
    }

    #[test]
    fn quit_sets_flag() {
        let mut app = App::new();
        assert!(!app.should_quit);
        app.quit();
        assert!(app.should_quit);
    }

    // --- Rename tests ---

    #[test]
    fn begin_rename_without_selection_is_noop() {
        let mut app = app_with_conversations();
        assert!(app.selected_conversation.is_none());
        app.begin_rename();
        assert_eq!(app.mode, InputMode::Normal);
        assert!(app.renaming_id.is_none());
    }

    #[test]
    fn begin_rename_prepopulates_buffer_and_enters_mode() {
        let mut app = app_with_conversations();
        app.selected_conversation = Some(1); // "Second"
        app.begin_rename();
        assert_eq!(app.mode, InputMode::Renaming);
        assert_eq!(app.renaming_id.as_deref(), Some("2"));
        assert_eq!(app.rename_textarea.lines(), ["Second"]);
    }

    #[test]
    fn submit_rename_returns_id_and_trimmed_title() {
        let mut app = app_with_conversations();
        app.selected_conversation = Some(0);
        app.begin_rename();
        // Replace contents
        app.rename_textarea = TextArea::from(vec!["  Renamed Chat  ".to_string()]);

        let result = app.submit_rename();
        assert_eq!(result, Some(("1".to_string(), "Renamed Chat".to_string())));
        assert_eq!(app.mode, InputMode::Normal);
        assert!(app.renaming_id.is_none());
    }

    #[test]
    fn submit_rename_with_unchanged_title_returns_none() {
        let mut app = app_with_conversations();
        app.selected_conversation = Some(0); // "First"
        app.begin_rename();
        // Buffer still equals current title
        let result = app.submit_rename();
        assert_eq!(result, None);
        assert_eq!(app.mode, InputMode::Normal);
    }

    #[test]
    fn submit_rename_with_empty_title_returns_none() {
        let mut app = app_with_conversations();
        app.selected_conversation = Some(0);
        app.begin_rename();
        app.rename_textarea = TextArea::from(vec!["   ".to_string()]);

        let result = app.submit_rename();
        assert_eq!(result, None);
        assert_eq!(app.mode, InputMode::Normal);
    }

    #[test]
    fn cancel_rename_clears_state() {
        let mut app = app_with_conversations();
        app.selected_conversation = Some(0);
        app.begin_rename();
        app.rename_textarea = TextArea::from(vec!["scratch".to_string()]);

        app.cancel_rename();
        assert_eq!(app.mode, InputMode::Normal);
        assert!(app.renaming_id.is_none());
    }

    #[test]
    fn apply_rename_updates_sidebar_and_current() {
        let mut app = app_with_conversations();
        app.load_conversation(ConversationDetail {
            id: "2".into(),
            title: "Second".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });
        app.apply_rename("2", "Renamed");
        assert_eq!(app.conversations()[1].title, "Renamed");
        assert_eq!(app.current_conversation().unwrap().title, "Renamed");
    }

    #[test]
    fn switch_requested_default_false() {
        assert!(!App::new().switch_requested);
    }

    #[test]
    fn pending_screen_request_round_trips_and_is_taken_once() {
        let mut app = App::new();
        assert_eq!(app.take_pending_screen(), None, "none pending by default");

        app.request_screen(ScreenRequest::KnowledgeBase);
        assert_eq!(
            app.take_pending_screen(),
            Some(ScreenRequest::KnowledgeBase)
        );
        assert_eq!(
            app.take_pending_screen(),
            None,
            "taking clears it so the loop services it exactly once"
        );
    }

    #[test]
    fn requesting_a_screen_supersedes_an_unserviced_one() {
        // The invariant the `ScreenRequest` enum buys over the old set of
        // independent `*_requested` bools: only ONE screen can be pending, so a
        // second request can't leave two queued to open in arbitrary order.
        let mut app = App::new();
        app.request_screen(ScreenRequest::Connections);
        app.request_screen(ScreenRequest::Purposes);
        assert_eq!(app.take_pending_screen(), Some(ScreenRequest::Purposes));
        assert_eq!(app.take_pending_screen(), None);
    }

    #[test]
    fn apply_model_override_updates_current_conversation_and_stages_pending() {
        let mut app = App::new();
        app.load_conversation(ConversationDetail {
            id: "c1".into(),
            title: "Chat".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });
        let ovr = desktop_assistant_api_model::SendPromptOverride {
            connection_id: "work".into(),
            model_id: "claude-3-5".into(),
            effort: None,
        };
        app.apply_model_override(ovr.clone());
        assert!(app.pending_model_override.is_some());
        let sel = app
            .current_conversation()
            .unwrap()
            .model_selection
            .as_ref()
            .unwrap();
        assert_eq!(sel.connection_id, "work");
        assert_eq!(sel.model_id, "claude-3-5");
    }

    #[test]
    fn take_pending_override_clears_after_first_take() {
        let mut app = App::new();
        let ovr = desktop_assistant_api_model::SendPromptOverride {
            connection_id: "a".into(),
            model_id: "b".into(),
            effort: None,
        };
        app.pending_model_override = Some(ovr);
        assert!(app.take_pending_override().is_some());
        assert!(app.take_pending_override().is_none());
    }

    // --- Scroll tests ---

    #[test]
    fn scroll_up_and_down() {
        let mut app = App::new();
        assert_eq!(app.scroll_offset, 0);
        app.scroll_up(5);
        assert_eq!(app.scroll_offset, 5);
        app.scroll_up(3);
        assert_eq!(app.scroll_offset, 8);
        app.scroll_down(3);
        assert_eq!(app.scroll_offset, 5);
        app.scroll_down(100);
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn scroll_to_bottom_resets() {
        let mut app = App::new();
        app.scroll_up(10);
        app.scroll_to_bottom();
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn receive_chunk_at_bottom_keeps_following() {
        // TUI-10: when the user is at the bottom (offset 0), streaming keeps
        // auto-following — applying a chunk through the core never moves scroll.
        let mut app = App::new();
        app.load_conversation(detail("c1"));
        app.apply_prompt_ack("task1".into(), "c1".into());
        app.apply_core(UiMessage::StreamChunk {
            request_id: "req1".into(),
            chunk: "data".into(),
        });
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn receive_chunk_while_scrolled_up_preserves_position() {
        // Acceptance (TUI-10): the user can read scrollback during a long
        // reply — chunks must not yank the view back to the bottom.
        let mut app = App::new();
        app.load_conversation(detail("c1"));
        app.apply_prompt_ack("task1".into(), "c1".into());
        app.scroll_up(10);
        app.apply_core(UiMessage::StreamChunk {
            request_id: "req1".into(),
            chunk: "data".into(),
        });
        assert_eq!(app.scroll_offset, 10);
        assert_eq!(app.streaming_buffer(), "data", "chunk still buffered");
    }

    // --- Tasks pane integration ---

    fn standalone_view(
        id: &str,
        conv_id: &str,
        title: &str,
    ) -> desktop_assistant_api_model::TaskView {
        desktop_assistant_api_model::TaskView {
            id: desktop_assistant_api_model::TaskId(id.into()),
            kind: desktop_assistant_api_model::TaskKind::Standalone {
                name: title.into(),
                conversation_id: conv_id.into(),
            },
            status: desktop_assistant_api_model::TaskStatus::Running,
            started_at: 1,
            ended_at: None,
            last_error: None,
            parent: None,
            children: Vec::new(),
            title: title.into(),
            progress_hint: None,
        }
    }

    #[test]
    fn keybind_toggles_pane_visibility() {
        let mut app = App::new();
        assert!(!app.tasks.visible);
        app.toggle_tasks_pane();
        assert!(app.tasks.visible);
        app.toggle_tasks_pane();
        assert!(!app.tasks.visible);
    }

    #[test]
    fn pressing_enter_on_selection_switches_to_linked_conversation() {
        let mut app = App::new();
        app.set_conversations(vec![
            ConversationSummary {
                id: "conv-1".into(),
                title: "Other".into(),
                message_count: 0,
                archived: false,
            },
            ConversationSummary {
                id: "conv-x".into(),
                title: "Linked".into(),
                message_count: 0,
                archived: false,
            },
        ]);
        app.tasks
            .apply_task_started(standalone_view("t-1", "conv-x", "Researcher"));
        app.tasks.visible = true;
        app.tasks.selected = Some(desktop_assistant_api_model::TaskId("t-1".into()));

        let conv = app.jump_to_selected_task_conversation();
        assert_eq!(conv.as_deref(), Some("conv-x"));
        assert_eq!(app.selected_conversation, Some(1));
        assert!(!app.tasks.visible);
    }

    #[test]
    fn jump_to_selected_task_conversation_returns_id_even_when_not_in_sidebar() {
        // The conversation may have been archived out of the visible
        // sidebar list. We still return its id so main.rs can fetch the
        // detail; the sidebar selection just stays where it was.
        let mut app = App::new();
        app.tasks
            .apply_task_started(standalone_view("t-1", "archived-conv", "Researcher"));
        app.tasks.selected = Some(desktop_assistant_api_model::TaskId("t-1".into()));

        let conv = app.jump_to_selected_task_conversation();
        assert_eq!(conv.as_deref(), Some("archived-conv"));
        assert_eq!(app.selected_conversation, None);
    }

    #[test]
    fn pressing_c_on_selection_requests_cancel_command() {
        let mut app = App::new();
        app.tasks
            .apply_task_started(standalone_view("t-1", "conv-x", "Researcher"));
        app.tasks.selected = Some(desktop_assistant_api_model::TaskId("t-1".into()));

        let id = app.request_cancel_selected_task();
        assert_eq!(id, Some(desktop_assistant_api_model::TaskId("t-1".into())));
        assert_eq!(
            app.pending_task_cancel,
            Some(desktop_assistant_api_model::TaskId("t-1".into()))
        );
        assert!(app.status_message.starts_with("Cancelling"));
    }

    #[test]
    fn cancelling_after_task_completes_finds_nothing_because_row_was_evicted() {
        // With terminal-row eviction, a completed task disappears from
        // the pane. A subsequent cancel request finds no selected row
        // and returns silently — there is no "already terminal" branch
        // to hit because terminal rows can't exist.
        let mut app = App::new();
        app.tasks
            .apply_task_started(standalone_view("t-1", "conv-x", "Researcher"));
        app.tasks.selected = Some(desktop_assistant_api_model::TaskId("t-1".into()));
        app.tasks.apply_task_completed("t-1");

        assert!(
            !app.tasks
                .tasks
                .contains_key(&desktop_assistant_api_model::TaskId("t-1".into()))
        );
        let id = app.request_cancel_selected_task();
        assert!(id.is_none());
        assert!(app.pending_task_cancel.is_none());
    }

    #[test]
    fn cancel_with_no_selection_is_a_noop() {
        let mut app = App::new();
        let id = app.request_cancel_selected_task();
        assert!(id.is_none());
    }

    // --- Per-conversation You/Adele voice controls (adele-tui#77) ---

    fn app_with_open_conversation(id: &str) -> App {
        let mut app = App::new();
        // Via `load_conversation` so core's open-conversation id is dual-written.
        app.load_conversation(ConversationDetail {
            id: id.into(),
            title: "Chat".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });
        app
    }

    #[test]
    fn adele_output_next_cycles_disabled_on_demand_always() {
        assert_eq!(AdeleOutput::Disabled.next(), AdeleOutput::OnDemand);
        assert_eq!(AdeleOutput::OnDemand.next(), AdeleOutput::Always);
        assert_eq!(AdeleOutput::Always.next(), AdeleOutput::Disabled);
    }

    #[test]
    fn defaults_are_you_disabled_and_adele_disabled_silent() {
        // The default gate must be closed: nothing is spoken, asides go inline.
        let app = app_with_open_conversation("c1");
        assert!(!app.current_voice_in(), "You defaults Disabled");
        assert_eq!(
            app.current_adele_output(),
            AdeleOutput::Disabled,
            "Adele defaults Disabled"
        );
        assert!(
            !app.narrate_for("c1"),
            "default gate is closed (no narration)"
        );
        assert!(
            !app.say_this_spoken_for("c1"),
            "default say_this is not spoken (Disabled)"
        );
        assert!(!app.narrate_for("never-seen"));
    }

    #[test]
    fn toggle_current_voice_in_flips_and_returns_new_state() {
        let mut app = app_with_open_conversation("c1");
        assert_eq!(app.toggle_current_voice_in(), Some(true));
        assert!(app.current_voice_in());
        assert_eq!(app.toggle_current_voice_in(), Some(false));
        assert!(!app.current_voice_in());
    }

    #[test]
    fn toggle_current_voice_in_without_conversation_is_none() {
        let mut app = App::new();
        assert_eq!(app.toggle_current_voice_in(), None);
        assert!(!app.current_voice_in());
    }

    #[test]
    fn cycle_current_adele_output_advances_and_returns_new_level() {
        let mut app = app_with_open_conversation("c1");
        assert_eq!(
            app.cycle_current_adele_output(),
            Some(AdeleOutput::OnDemand)
        );
        assert_eq!(app.current_adele_output(), AdeleOutput::OnDemand);
        assert_eq!(app.cycle_current_adele_output(), Some(AdeleOutput::Always));
        assert_eq!(
            app.cycle_current_adele_output(),
            Some(AdeleOutput::Disabled)
        );
    }

    #[test]
    fn cycle_current_adele_output_without_conversation_is_none() {
        let mut app = App::new();
        assert_eq!(app.cycle_current_adele_output(), None);
        assert_eq!(app.current_adele_output(), AdeleOutput::Disabled);
    }

    #[test]
    fn adele_always_narrates_regardless_of_you() {
        // Always reads every reply aloud whether or not You is Enabled; say_this
        // is NOT its spoken channel (voice#126) — the whole reply already is.
        for you in [false, true] {
            let mut app = app_with_open_conversation("c1");
            app.set_voice_in("c1", you);
            app.set_adele_output("c1", AdeleOutput::Always);
            assert!(app.narrate_for("c1"), "Always must narrate (You={you})");
            assert!(
                !app.say_this_spoken_for("c1"),
                "Always does not separately speak say_this (You={you})"
            );
        }
    }

    #[test]
    fn adele_on_demand_never_narrates_but_say_this_speaks() {
        // On-demand's spoken channel is say_this, not auto-narration (voice#126);
        // decoupled from You — neither value narrates, both speak say_this.
        let mut app = app_with_open_conversation("c1");
        app.set_adele_output("c1", AdeleOutput::OnDemand);
        for you in [false, true] {
            app.set_voice_in("c1", you);
            assert!(
                !app.narrate_for("c1"),
                "OnDemand must not auto-narrate (You={you})"
            );
            assert!(
                app.say_this_spoken_for("c1"),
                "OnDemand say_this aside is spoken (You={you})"
            );
        }
    }

    #[test]
    fn adele_disabled_never_narrates_and_say_this_goes_inline() {
        let mut app = app_with_open_conversation("c1");
        app.set_adele_output("c1", AdeleOutput::Disabled);
        for you in [false, true] {
            app.set_voice_in("c1", you);
            assert!(
                !app.narrate_for("c1"),
                "Disabled never narrates (You={you})"
            );
            assert!(
                !app.say_this_spoken_for("c1"),
                "Disabled never speaks say_this (You={you})"
            );
        }
    }

    #[test]
    fn set_voice_in_targets_an_explicit_conversation() {
        // The model's tools / the streaming path carry their own conversation id.
        let mut app = app_with_open_conversation("c1");
        app.set_voice_in("c2", true);
        assert!(app.voice_in_for("c2"));
        assert!(!app.voice_in_for("c1")); // open conversation untouched
    }

    #[test]
    fn set_adele_output_targets_an_explicit_conversation() {
        let mut app = app_with_open_conversation("c1");
        app.set_adele_output("c2", AdeleOutput::OnDemand);
        assert_eq!(app.adele_output_for("c2"), AdeleOutput::OnDemand);
        assert_eq!(app.adele_output_for("c1"), AdeleOutput::Disabled);
    }

    #[test]
    fn voice_controls_are_per_conversation_isolated() {
        // Enabling You / raising Adele in one conversation must NOT bleed.
        let mut app = app_with_open_conversation("c1");
        assert_eq!(app.toggle_current_voice_in(), Some(true)); // c1 You ON
        app.set_adele_output("c1", AdeleOutput::Always);
        assert!(app.voice_in_for("c1"));
        assert_eq!(app.adele_output_for("c1"), AdeleOutput::Always);
        // c2 untouched → both defaults.
        assert!(!app.voice_in_for("c2"));
        assert_eq!(app.adele_output_for("c2"), AdeleOutput::Disabled);
        assert!(!app.narrate_for("c2"));
    }

    #[test]
    fn backgrounded_turn_completion_is_not_narrated_into_the_open_conversation() {
        // TUI-4 / GTK-2 convergence: when the originating conversation (c1,
        // Adele=Always) is backgrounded — the user switched to c2 mid-stream —
        // its completion touches NOTHING in the open chat and emits no narration.
        // The reply is persisted daemon-side and re-appears when c1 is reopened.
        //
        // This is a deliberate behavior change adopting the shared reducer's
        // semantics: the pre-migration TUI narrated a backgrounded reply per its
        // origin's gate even while another conversation was on screen.
        let mut app = app_with_open_conversation("c1");
        app.set_adele_output("c1", AdeleOutput::Always);
        app.apply_prompt_ack("task-1".into(), "c1".into());
        app.apply_core(UiMessage::StreamChunk {
            request_id: "req-1".into(),
            chunk: "partial".into(),
        });
        app.load_conversation(detail("c2")); // switch away mid-stream

        let controller = app.apply_core(UiMessage::StreamComplete {
            request_id: "req-1".into(),
            full_response: "spoken reply".into(),
        });

        assert!(
            !controller.iter().any(|e| matches!(e, Effect::Speak(_))),
            "a backgrounded turn must not be narrated into the open chat: {controller:?}"
        );
        assert!(
            app.current_conversation().unwrap().messages.is_empty(),
            "and it must not append to the open (c2) transcript"
        );
        // The gate predicate itself is unchanged and still origin-keyed.
        assert!(app.narrate_for("c1"), "origin's gate still holds");
        assert!(!app.narrate_for("c2"), "open conversation stays silent");
    }

    #[test]
    fn push_speech_disabled_note_appends_inline_to_open_conversation() {
        let mut app = app_with_open_conversation("c1");
        assert!(app.push_speech_disabled_note("c1", "the kettle is on"));
        let msgs = &app.current_conversation().unwrap().messages;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "assistant");
        // Content is clean; the "(speech mode disabled)" marker is added at
        // render time from the metadata, not baked in (voice#126).
        assert_eq!(msgs[0].content, "the kettle is on");
        assert_eq!(msgs[0].kind, MessageKind::SpeechDisabled);
    }

    #[test]
    fn push_spoken_note_tags_the_line_spoken() {
        let mut app = app_with_open_conversation("c1");
        assert!(app.push_spoken_note("c1", "the kettle is on"));
        let msgs = &app.current_conversation().unwrap().messages;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "assistant");
        assert_eq!(msgs[0].content, "the kettle is on");
        assert_eq!(msgs[0].kind, MessageKind::Spoken);
    }

    #[test]
    fn push_speech_disabled_note_ignores_other_conversation() {
        // A say_this call referencing a conversation that isn't the open one
        // must NOT bleed text into the visible transcript.
        let mut app = app_with_open_conversation("c1");
        assert!(!app.push_speech_disabled_note("c2", "wrong conversation"));
        assert!(app.current_conversation().unwrap().messages.is_empty());
    }

    #[test]
    fn push_speech_disabled_note_with_no_conversation_is_noop() {
        let mut app = App::new();
        assert!(!app.push_speech_disabled_note("c1", "anything"));
    }

    #[test]
    fn business_outcome_user_sees_their_spawned_standalone_agent_in_the_list_with_correct_title() {
        // End-to-end happy path: the daemon emits a `TaskStarted` event
        // for a user-initiated standalone agent. The TUI's pane must
        // contain a row with the agent's title, its conversation id,
        // and Running status — without any further round-trip.
        let mut app = App::new();
        let view = standalone_view("t-42", "spawned-conv", "Researcher: pricing data");
        app.tasks.apply_task_started(view);

        let row = app
            .tasks
            .tasks
            .get(&desktop_assistant_api_model::TaskId("t-42".into()))
            .expect("row should be present after TaskStarted");
        assert_eq!(row.title, "Researcher: pricing data");
        assert_eq!(row.conversation_id.as_deref(), Some("spawned-conv"));
        assert!(matches!(
            row.status,
            desktop_assistant_api_model::TaskStatus::Running
        ));
        // The running badge should now reflect the count.
        assert_eq!(crate::tasks::running_badge(&app.tasks), "(1 running)");
    }
}
