use std::collections::HashMap;

use desktop_assistant_api_model::TaskId;
pub use desktop_assistant_client_common::{ChatMessage, ConversationDetail, ConversationSummary};
use ratatui::style::Style;
use ratatui_textarea::{CursorMove, DataCursor, TextArea};

use crate::tasks::TaskPane;

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

/// The `Adele:` voice-output level for a conversation (adele-tui#77, mirroring
/// adele-gtk#80's `AdeleOutput`). Decides reply narration (with `You`), the
/// `say_this` aside gate, and the send-time `system_refinement`. Defaults to
/// [`AdeleOutput::Disabled`].
///
/// * `Disabled` — never speaks; a `say_this` aside downgrades to inline text.
/// * `OnDemand` — speaks replies only while `You == Enabled` (shaped for the ear,
///   brief and conversational) and always speaks `say_this` asides. Selected by
///   the model's `request_voice`.
/// * `Always` — reads every reply aloud, in full but made speakable (not
///   shortened).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AdeleOutput {
    /// Never speaks (the default). `say_this` → inline note.
    #[default]
    Disabled,
    /// Speaks replies when `You == Enabled`; always speaks `say_this` asides.
    OnDemand,
    /// Reads every reply aloud, in full, made speakable.
    Always,
}

impl AdeleOutput {
    /// The next level when the user cycles the control
    /// (`Disabled → OnDemand → Always → Disabled`).
    pub fn next(self) -> Self {
        match self {
            Self::Disabled => Self::OnDemand,
            Self::OnDemand => Self::Always,
            Self::Always => Self::Disabled,
        }
    }

    /// Short label for the status line / chat title cue.
    pub fn label(self) -> &'static str {
        match self {
            Self::Disabled => "Disabled",
            Self::OnDemand => "On Demand",
            Self::Always => "Always",
        }
    }
}

pub struct App {
    pub conversations: Vec<ConversationSummary>,
    pub selected_conversation: Option<usize>,
    pub current_conversation: Option<ConversationDetail>,
    pub textarea: TextArea<'static>,
    pub streaming_buffer: String,
    pub pending_request_id: Option<String>,
    /// The conversation the in-flight stream was sent to (TUI-4). `Some` iff
    /// `pending_request_id` is `Some`. Completion appends to — and narration
    /// gates on — THIS conversation, not whichever one is open at the time.
    pub streaming_conversation_id: Option<String>,
    pub mode: InputMode,
    pub status_message: String,
    pub should_quit: bool,
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
    /// Transient indicator from AssistantStatus events ("Searching knowledge
    /// base…", tool-call progress). Cleared when streaming completes or
    /// errors. Distinct from `status_message`, which is sticky user-facing
    /// feedback.
    pub assistant_status: Option<String>,
    /// Whether the conversation list pane is visible. When `false`, the chat
    /// panel takes the full window width.
    pub show_sidebar: bool,
    /// Set when the user asks to switch to a different connection. Causes
    /// the chat loop to exit cleanly so the picker can run again.
    pub switch_requested: bool,
    /// Set when the user asks to open the knowledge base. The chat loop
    /// hands the screen to `kb::run` and resets this flag when KB exits.
    pub kb_requested: bool,
    /// Set when the user asks to open the LLM-provider connections manager.
    /// Mirrors `kb_requested`'s screen-handoff pattern.
    pub connections_requested: bool,
    /// Set when the user asks to open the purposes manager.
    pub purposes_requested: bool,
    /// Set when the user asks to open the per-conversation model picker.
    pub model_picker_requested: bool,
    /// Set when the user asks to open the per-conversation personality picker.
    /// Mirrors `model_picker_requested`'s screen-handoff pattern.
    pub personality_picker_requested: bool,
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
    /// Per-conversation `You:` (voice input) state (adele-tui#77, mirroring
    /// adele-gtk#80). Keyed by conversation id; a conversation absent from the
    /// map is **Disabled** (type only). When `true` (Enabled), a push-to-talk
    /// control is available and — combined with `Adele == OnDemand` — gates
    /// reply narration. Toggled in-app with `Ctrl+V`. Per-conversation, so
    /// enabling it in one conversation never affects another.
    voice_in: HashMap<String, bool>,
    /// Per-conversation `Adele:` (voice output) level (adele-tui#77, mirroring
    /// adele-gtk#80). Keyed by conversation id; a conversation absent from the
    /// map is `Disabled` (never speaks). Set by the user (`Ctrl+S` cycles it) or
    /// the model (`request_voice` → OnDemand, `stop_voice` → Disabled). Decides
    /// reply narration (with `You`), the `say_this` aside gate, and the send-time
    /// `system_refinement`. Replaces phase-1/2's `speech_enabled` (read-aloud ==
    /// Always) and `voice_mode` (== OnDemand) toggles.
    adele_output: HashMap<String, AdeleOutput>,
}

impl App {
    const PENDING_STREAM_REQUEST_ID: &str = "__pending_stream_request_id__";

    pub fn new() -> Self {
        Self {
            conversations: Vec::new(),
            selected_conversation: None,
            current_conversation: None,
            textarea: new_textarea(),
            streaming_buffer: String::new(),
            pending_request_id: None,
            streaming_conversation_id: None,
            mode: InputMode::Normal,
            status_message: "Connected".to_string(),
            should_quit: false,
            scroll_offset: 0,
            show_archived: false,
            rename_textarea: new_textarea(),
            renaming_id: None,
            show_debug: false,
            assistant_status: None,
            show_sidebar: true,
            switch_requested: false,
            kb_requested: false,
            connections_requested: false,
            purposes_requested: false,
            model_picker_requested: false,
            personality_picker_requested: false,
            pending_model_override: None,
            tasks: TaskPane::new(),
            pending_task_cancel: None,
            voice_in: HashMap::new(),
            adele_output: HashMap::new(),
        }
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
        self.voice_in.get(conversation_id).copied().unwrap_or(false)
    }

    /// Whether `You:` is Enabled for the currently-open conversation. `false`
    /// when no conversation is open.
    pub fn current_voice_in(&self) -> bool {
        self.current_conversation
            .as_ref()
            .is_some_and(|c| self.voice_in_for(&c.id))
    }

    /// Flip `You:` for the currently-open conversation and return the new state
    /// (used by the `Ctrl+V` keybind). `None` when no conversation is open.
    pub fn toggle_current_voice_in(&mut self) -> Option<bool> {
        let conv_id = self.current_conversation.as_ref()?.id.clone();
        let next = !self.voice_in_for(&conv_id);
        self.voice_in.insert(conv_id, next);
        Some(next)
    }

    /// The `Adele:` (voice output) level for `conversation_id`. `Disabled` when
    /// the conversation was never set (the default).
    pub fn adele_output_for(&self, conversation_id: &str) -> AdeleOutput {
        self.adele_output
            .get(conversation_id)
            .copied()
            .unwrap_or_default()
    }

    /// The `Adele:` level for the currently-open conversation. `Disabled` when
    /// no conversation is open.
    pub fn current_adele_output(&self) -> AdeleOutput {
        self.current_conversation
            .as_ref()
            .map(|c| self.adele_output_for(&c.id))
            .unwrap_or_default()
    }

    /// Set `Adele:` for an explicit `conversation_id` (used by the model's
    /// `request_voice` → OnDemand / `stop_voice` → Disabled tools, which carry
    /// their own conversation). Per-conversation: only the named conversation is
    /// affected.
    pub fn set_adele_output(&mut self, conversation_id: &str, level: AdeleOutput) {
        self.adele_output.insert(conversation_id.to_string(), level);
    }

    /// Cycle `Adele:` for the currently-open conversation
    /// (`Disabled → OnDemand → Always → Disabled`) and return the new level
    /// (used by the `Ctrl+S` keybind). `None` when no conversation is open.
    pub fn cycle_current_adele_output(&mut self) -> Option<AdeleOutput> {
        let conv_id = self.current_conversation.as_ref()?.id.clone();
        let next = self.adele_output_for(&conv_id).next();
        self.adele_output.insert(conv_id, next);
        Some(next)
    }

    /// Whether a *reply* is spoken for `conversation_id` (adele-tui#77, the
    /// narration gate): `Adele == Always` OR (`Adele == OnDemand` AND
    /// `You == Enabled`). `Disabled` never narrates.
    pub fn narrate_for(&self, conversation_id: &str) -> bool {
        match self.adele_output_for(conversation_id) {
            AdeleOutput::Always => true,
            AdeleOutput::OnDemand => self.voice_in_for(conversation_id),
            AdeleOutput::Disabled => false,
        }
    }

    /// Whether a `say_this` aside is spoken for `conversation_id` (adele-tui#77):
    /// spoken iff `Adele ∈ {OnDemand, Always}` (independent of `You`). `Disabled`
    /// downgrades the aside to inline text.
    pub fn say_this_spoken_for(&self, conversation_id: &str) -> bool {
        !matches!(
            self.adele_output_for(conversation_id),
            AdeleOutput::Disabled
        )
    }

    /// Render a `say_this` call whose aside is NOT spoken (Adele == Disabled) as
    /// an inline note in the transcript instead (adele-tui#77). Appended to
    /// `conversation_id` only when that is the open conversation, so a call from
    /// a stale/other conversation never bleeds into the visible chat. Returns
    /// whether the note was shown.
    pub fn push_speech_disabled_note(&mut self, conversation_id: &str, text: &str) -> bool {
        let Some(conv) = self.current_conversation.as_mut() else {
            return false;
        };
        if conv.id != conversation_id {
            return false;
        }
        conv.messages.push(ChatMessage {
            role: "assistant".to_string(),
            content: format!("(speech mode disabled) {text}"),
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
        if let Some(idx) = self.conversations.iter().position(|c| c.id == conv_id) {
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
        if let Some(conv) = self.current_conversation.as_mut() {
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
        if self.conversations.is_empty() {
            return;
        }
        self.selected_conversation = Some(match self.selected_conversation {
            Some(i) => {
                if i >= self.conversations.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        });
    }

    pub fn previous_conversation(&mut self) {
        if self.conversations.is_empty() {
            return;
        }
        self.selected_conversation = Some(match self.selected_conversation {
            Some(i) => {
                if i == 0 {
                    self.conversations.len() - 1
                } else {
                    i - 1
                }
            }
            None => self.conversations.len() - 1,
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

    /// Gate + mutate for a prompt submission (TUI-2 / TUI-7). Checks the
    /// preconditions BEFORE touching any state, so a refused submission leaves
    /// the composer and transcript untouched:
    ///
    /// * not connected → refuse with a status message (TUI-2: previously the
    ///   message was appended to the transcript and the composer cleared, then
    ///   silently never sent);
    /// * a reply is still streaming (`pending_request_id` is claimed or the
    ///   ack sentinel) → refuse with a status message (TUI-7 policy: **block
    ///   concurrent sends**. The TUI renders a single streaming buffer for the
    ///   open conversation; interleaving a second stream would cross-wire the
    ///   request-id claim and drop one reply. Blocking keeps the composer text
    ///   so nothing is lost — the user sends again once the reply lands).
    ///
    /// Only when both gates pass does it delegate to [`App::submit_prompt`].
    pub fn prepare_submission(&mut self, connected: bool) -> Option<(String, String)> {
        if !connected {
            self.status_message =
                "Not connected — message not sent (your text is preserved)".into();
            return None;
        }
        if self.pending_request_id.is_some() {
            self.status_message =
                "A reply is still streaming — wait for it to finish (your text is preserved)"
                    .into();
            return None;
        }
        self.submit_prompt()
    }

    /// Roll back a submission whose send RPC failed (TUI-2): remove the
    /// optimistically appended user message (only when the originating
    /// conversation is still open and the tail message matches) and put the
    /// prompt text back into the composer so the user can retry without
    /// retyping. The caller sets the status message with the send error.
    pub fn restore_failed_submission(&mut self, conversation_id: &str, prompt: &str) {
        if let Some(conv) = self
            .current_conversation
            .as_mut()
            .filter(|c| c.id == conversation_id)
            && conv
                .messages
                .last()
                .is_some_and(|m| m.role == "user" && m.content == prompt)
        {
            conv.messages.pop();
        }
        self.textarea = new_textarea();
        self.textarea.insert_str(prompt);
    }

    /// Returns (conversation_id, prompt) if valid, None otherwise.
    pub fn submit_prompt(&mut self) -> Option<(String, String)> {
        let content = self.textarea_content();
        if content.is_empty() {
            return None;
        }
        let conv = self.current_conversation.as_mut()?;
        conv.messages.push(ChatMessage {
            role: "user".to_string(),
            content: content.clone(),
        });
        self.textarea = new_textarea();
        self.scroll_offset = 0;
        Some((conv.id.clone(), content))
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

    // --- Streaming ---
    //
    // The stream knows its conversation (TUI-4): `start_streaming` records the
    // conversation the prompt was sent to, and completion/rendering/narration
    // all target THAT conversation rather than whichever one is open when the
    // event arrives.

    pub fn start_streaming(&mut self, request_id: String, conversation_id: String) {
        self.pending_request_id = Some(request_id);
        self.streaming_conversation_id = Some(conversation_id);
        self.streaming_buffer.clear();
    }

    pub fn start_streaming_without_request_id(&mut self, conversation_id: String) {
        self.start_streaming(Self::PENDING_STREAM_REQUEST_ID.to_string(), conversation_id);
    }

    /// Apply the result of a `send_prompt` ack from the daemon, recording the
    /// conversation the prompt targeted (TUI-4).
    ///
    /// The wire value is either a `task_id` (post-desktop-assistant #114
    /// `SendMessageAck`) or an empty string (legacy `Ack`). Neither is the
    /// chunk-stream `request_id` — that is server-generated and arrives
    /// embedded in the first `AssistantDelta`. We therefore ignore the
    /// ack payload and seed the sentinel; the first chunk claims it via
    /// `stream_matches_or_claims_request_id`. See issue #52.
    pub fn apply_prompt_ack(&mut self, _task_id: String, conversation_id: String) {
        self.start_streaming_without_request_id(conversation_id);
    }

    /// Whether the in-flight stream belongs to the open conversation (TUI-4).
    /// Gates rendering of the live streaming buffer so a backgrounded turn's
    /// chunks never paint into a conversation the user switched to.
    pub fn streaming_is_for_current(&self) -> bool {
        self.pending_request_id.is_some()
            && self.streaming_conversation_id.as_deref()
                == self.current_conversation.as_ref().map(|c| c.id.as_str())
    }

    /// Reset all in-flight streaming state (TUI-8). Called on `Disconnected`:
    /// the stream is dead, so the frozen `▌` buffer must not linger and the
    /// ack sentinel must not mis-claim the first stream after reconnecting.
    pub fn clear_streaming_state(&mut self) {
        self.pending_request_id = None;
        self.streaming_conversation_id = None;
        self.streaming_buffer.clear();
        self.assistant_status = None;
    }

    /// Move the sidebar selection to the conversation with `id`, returning
    /// whether it was found (TUI-8: selection is positional, so after a
    /// reconnect's list refresh we reselect by id, not index).
    pub fn select_conversation_by_id(&mut self, id: &str) -> bool {
        match self.conversations.iter().position(|c| c.id == id) {
            Some(idx) => {
                self.selected_conversation = Some(idx);
                true
            }
            None => false,
        }
    }

    fn stream_matches_or_claims_request_id(&mut self, request_id: &str) -> bool {
        match self.pending_request_id.as_deref() {
            Some(Self::PENDING_STREAM_REQUEST_ID) => {
                self.pending_request_id = Some(request_id.to_string());
                true
            }
            Some(current) => current == request_id,
            None => false,
        }
    }

    pub fn receive_chunk(&mut self, request_id: &str, chunk: &str) {
        if !self.stream_matches_or_claims_request_id(request_id) {
            return;
        }
        self.streaming_buffer.push_str(chunk);
        // Follow-only-at-bottom (TUI-10): the transcript renders anchored to
        // the bottom, so `scroll_offset == 0` keeps following new chunks by
        // itself. A non-zero offset means the user scrolled up to read —
        // never yank them back down (and a backgrounded stream's chunks must
        // never touch the OPEN conversation's scroll at all, TUI-4).
    }

    /// Finish the in-flight stream. Returns the ORIGINATING conversation id
    /// when the event matched the pending stream (TUI-4) — the caller gates
    /// narration on that conversation — or `None` when the event was unrelated.
    /// The reply is appended to the transcript only when the originating
    /// conversation is the open one; otherwise the daemon already persisted it
    /// and it appears when that conversation is next opened.
    pub fn complete_streaming(&mut self, request_id: &str, full_response: &str) -> Option<String> {
        if !self.stream_matches_or_claims_request_id(request_id) {
            return None;
        }
        let origin = self.streaming_conversation_id.take();
        if let Some(conv) = self
            .current_conversation
            .as_mut()
            .filter(|c| origin.as_deref() == Some(c.id.as_str()))
        {
            conv.messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: full_response.to_string(),
            });
        }
        self.streaming_buffer.clear();
        self.pending_request_id = None;
        self.assistant_status = None;
        origin
    }

    pub fn streaming_error(&mut self, request_id: &str, error: &str) {
        if !self.stream_matches_or_claims_request_id(request_id) {
            return;
        }
        self.status_message = format!("Error: {error}");
        self.streaming_buffer.clear();
        self.pending_request_id = None;
        self.streaming_conversation_id = None;
        self.assistant_status = None;
    }

    // --- Conversation management ---

    pub fn set_conversations(&mut self, conversations: Vec<ConversationSummary>) {
        self.conversations = conversations;
        // Fix selection if out of bounds
        if let Some(sel) = self.selected_conversation
            && sel >= self.conversations.len()
        {
            self.selected_conversation = if self.conversations.is_empty() {
                None
            } else {
                Some(self.conversations.len() - 1)
            };
        }
    }

    pub fn load_conversation(&mut self, detail: ConversationDetail) {
        self.current_conversation = Some(detail);
    }

    pub fn update_conversation_title(&mut self, conversation_id: &str, title: &str) {
        for conv in &mut self.conversations {
            if conv.id == conversation_id {
                conv.title = title.to_string();
            }
        }
        if let Some(current) = self.current_conversation.as_mut()
            && current.id == conversation_id
        {
            current.title = title.to_string();
        }
    }

    pub fn selected_conversation_id(&self) -> Option<&str> {
        let idx = self.selected_conversation?;
        self.conversations.get(idx).map(|c| c.id.as_str())
    }

    // --- Rename ---

    /// Enter rename mode for the selected conversation, prepopulating the
    /// rename buffer with its current title. No-op if nothing is selected.
    pub fn begin_rename(&mut self) {
        let Some(idx) = self.selected_conversation else {
            return;
        };
        let Some(conv) = self.conversations.get(idx) else {
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

    /// Apply a renamed title locally (call after the daemon confirms).
    pub fn apply_rename(&mut self, conversation_id: &str, title: &str) {
        self.update_conversation_title(conversation_id, title);
    }

    pub fn delete_selected_conversation(&mut self) -> Option<String> {
        let idx = self.selected_conversation?;
        if idx >= self.conversations.len() {
            return None;
        }
        let id = self.conversations[idx].id.clone();
        self.conversations.remove(idx);

        // Clear current conversation if it was the deleted one
        if self
            .current_conversation
            .as_ref()
            .is_some_and(|c| c.id == id)
        {
            self.current_conversation = None;
        }

        // Fix selection
        if self.conversations.is_empty() {
            self.selected_conversation = None;
        } else if idx >= self.conversations.len() {
            self.selected_conversation = Some(self.conversations.len() - 1);
        }

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
        app.current_conversation = Some(ConversationDetail {
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

        let (conv_id, payload) = app.submit_prompt().expect("submission");
        assert_eq!(conv_id, "c1");
        assert_eq!(payload, typed);
        assert!(!payload.contains('\n'));
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
        app.current_conversation = Some(ConversationDetail {
            id: "c1".into(),
            title: "t".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });
        let typed = "first paragraph here\nsecond paragraph here";
        app.textarea.insert_str(typed);

        let _ = app.wrapped_display_textarea(8);

        let (_id, payload) = app.submit_prompt().expect("submission");
        assert_eq!(payload, typed);
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

    #[test]
    fn submit_prompt_without_conversation_returns_none() {
        let mut app = App::new();
        app.textarea.insert_str("hello");
        assert!(app.submit_prompt().is_none());
        assert_eq!(app.textarea_content(), "hello"); // input preserved
    }

    #[test]
    fn submit_prompt_with_empty_input_returns_none() {
        let mut app = App::new();
        app.current_conversation = Some(ConversationDetail {
            id: "1".into(),
            title: "Test".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });
        assert!(app.submit_prompt().is_none());
    }

    #[test]
    fn submit_prompt_appends_user_message_and_clears_input() {
        let mut app = App::new();
        app.current_conversation = Some(ConversationDetail {
            id: "conv1".into(),
            title: "Test".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });
        app.textarea.insert_str("What is Rust?");

        let result = app.submit_prompt();
        assert_eq!(
            result,
            Some(("conv1".to_string(), "What is Rust?".to_string()))
        );
        assert_eq!(app.textarea_content(), "");

        let msgs = &app.current_conversation.as_ref().unwrap().messages;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content, "What is Rust?");
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
        app.current_conversation = Some(ConversationDetail {
            id: "c1".into(),
            title: "Test".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });
        app.enter_editing_mode();
        app.apply_paste("first\nsecond\nthird");
        let result = app.submit_prompt();
        assert_eq!(
            result,
            Some(("c1".to_string(), "first\nsecond\nthird".to_string()))
        );
        let msgs = &app.current_conversation.as_ref().unwrap().messages;
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

    // --- Send-path integrity (TUI-2 / TUI-7) ---

    fn app_ready_to_send(conv_id: &str, prompt: &str) -> App {
        let mut app = App::new();
        app.current_conversation = Some(ConversationDetail {
            id: conv_id.into(),
            title: "Test".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });
        app.enter_editing_mode();
        app.textarea.insert_str(prompt);
        app
    }

    #[test]
    fn submit_while_disconnected_preserves_composer_and_appends_nothing() {
        // Acceptance (TUI-2): disconnected submit → composer text preserved,
        // status message set, nothing appended to the transcript.
        let mut app = app_ready_to_send("c1", "important prompt");
        app.status_message.clear();

        let result = app.prepare_submission(false);

        assert!(result.is_none(), "nothing must be sent while disconnected");
        assert_eq!(
            app.textarea_content(),
            "important prompt",
            "composer text must be preserved"
        );
        assert!(
            app.current_conversation
                .as_ref()
                .unwrap()
                .messages
                .is_empty(),
            "transcript must not gain a user message"
        );
        assert!(
            !app.status_message.is_empty(),
            "the refusal must be surfaced in the status line"
        );
    }

    #[test]
    fn submit_while_streaming_is_blocked_and_composer_preserved() {
        // Acceptance (TUI-7, chosen policy = block concurrent sends): a second
        // send while a reply streams is refused with a status message and the
        // composer keeps its text.
        let mut app = app_ready_to_send("c1", "second question");
        app.start_streaming("req-in-flight".into(), "c1".into());
        app.status_message.clear();

        let result = app.prepare_submission(true);

        assert!(result.is_none(), "second send must be blocked mid-stream");
        assert_eq!(app.textarea_content(), "second question");
        assert!(
            app.current_conversation
                .as_ref()
                .unwrap()
                .messages
                .is_empty()
        );
        assert!(!app.status_message.is_empty());
    }

    #[test]
    fn submit_while_awaiting_ack_sentinel_is_also_blocked() {
        // Unhappy path: the window between send and the first chunk uses the
        // pending sentinel; a send in that window must be blocked too.
        let mut app = app_ready_to_send("c1", "rapid second send");
        app.start_streaming_without_request_id("c1".into());

        assert!(app.prepare_submission(true).is_none());
        assert_eq!(app.textarea_content(), "rapid second send");
    }

    #[test]
    fn submit_when_connected_and_idle_goes_through() {
        let mut app = app_ready_to_send("c1", "hello");
        let result = app.prepare_submission(true);
        assert_eq!(result, Some(("c1".to_string(), "hello".to_string())));
        assert_eq!(app.textarea_content(), "");
        assert_eq!(app.current_conversation.as_ref().unwrap().messages.len(), 1);
    }

    #[test]
    fn restore_failed_submission_pops_message_and_refills_composer() {
        // Acceptance (TUI-2): a failed send RPC rolls back the optimistic
        // transcript append and puts the prompt back in the composer.
        let mut app = app_ready_to_send("c1", "doomed prompt");
        let (conv_id, prompt) = app.prepare_submission(true).unwrap();

        app.restore_failed_submission(&conv_id, &prompt);

        assert!(
            app.current_conversation
                .as_ref()
                .unwrap()
                .messages
                .is_empty(),
            "optimistic user message must be rolled back"
        );
        assert_eq!(app.textarea_content(), "doomed prompt");
    }

    #[test]
    fn restore_failed_submission_after_switching_conversations_keeps_other_transcript() {
        // Unhappy path: the user switched conversations between submit and the
        // send failure. The other conversation's tail message must NOT be
        // popped; the composer still gets the text back.
        let mut app = app_ready_to_send("c1", "prompt for c1");
        let (conv_id, prompt) = app.prepare_submission(true).unwrap();

        // Switch to a different conversation that already has a user message.
        app.load_conversation(ConversationDetail {
            id: "c2".into(),
            title: "Other".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "prompt for c1".into(), // same text, different conv
            }],
            model_selection: None,
            conversation_personality: None,
        });

        app.restore_failed_submission(&conv_id, &prompt);

        assert_eq!(
            app.current_conversation.as_ref().unwrap().messages.len(),
            1,
            "the other conversation's transcript must be untouched"
        );
        assert_eq!(app.textarea_content(), "prompt for c1");
    }

    #[test]
    fn restore_failed_submission_does_not_pop_a_non_matching_tail() {
        // Unhappy path: something else (e.g. an inline note) landed after the
        // optimistic append; only an exact matching tail is rolled back.
        let mut app = app_ready_to_send("c1", "prompt");
        let (conv_id, prompt) = app.prepare_submission(true).unwrap();
        app.current_conversation
            .as_mut()
            .unwrap()
            .messages
            .push(ChatMessage {
                role: "assistant".into(),
                content: "(speech mode disabled) aside".into(),
            });

        app.restore_failed_submission(&conv_id, &prompt);

        let msgs = &app.current_conversation.as_ref().unwrap().messages;
        assert_eq!(msgs.len(), 2, "non-matching tail must not be popped");
        assert_eq!(app.textarea_content(), "prompt");
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

    #[test]
    fn complete_after_switching_conversations_targets_the_originating_one() {
        // Acceptance (TUI-4): a Complete arriving after the user switched
        // conversations must NOT append to the newly opened conversation; it
        // reports the ORIGINATING conversation id so narration gates there.
        let mut app = App::new();
        app.current_conversation = Some(detail("c1"));
        app.apply_prompt_ack("task-1".into(), "c1".into());
        app.receive_chunk("req-1", "partial ");

        // User switches to c2 mid-stream.
        app.load_conversation(detail("c2"));

        let origin = app.complete_streaming("req-1", "partial reply done");
        assert_eq!(origin.as_deref(), Some("c1"), "origin must be reported");
        assert!(
            app.current_conversation
                .as_ref()
                .unwrap()
                .messages
                .is_empty(),
            "the reply must not bleed into the switched-to conversation"
        );
        assert_eq!(app.pending_request_id, None, "stream state must clear");
        assert_eq!(app.streaming_buffer, "");
    }

    #[test]
    fn complete_appends_when_user_switched_away_and_back() {
        // Switching away and back re-opens the originating conversation; the
        // completion then lands in its transcript.
        let mut app = App::new();
        app.current_conversation = Some(detail("c1"));
        app.apply_prompt_ack("task-1".into(), "c1".into());
        app.load_conversation(detail("c2"));
        app.load_conversation(detail("c1")); // back again (fresh fetch)

        let origin = app.complete_streaming("req-1", "the reply");
        assert_eq!(origin.as_deref(), Some("c1"));
        let msgs = &app.current_conversation.as_ref().unwrap().messages;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "the reply");
    }

    #[test]
    fn unmatched_complete_reports_no_origin() {
        // An unrelated Complete (wrong request id) must not be narrated or
        // appended anywhere — no origin is reported.
        let mut app = App::new();
        app.current_conversation = Some(detail("c1"));
        app.start_streaming("req-1".into(), "c1".into());
        assert_eq!(app.complete_streaming("other-req", "noise"), None);
        assert!(app.pending_request_id.is_some(), "stream still pending");
    }

    #[test]
    fn streaming_buffer_renders_only_into_the_originating_conversation() {
        // The UI gate (TUI-4): mid-stream chunks must not paint into a
        // conversation the user switched to.
        let mut app = App::new();
        app.current_conversation = Some(detail("c1"));
        app.start_streaming("req-1".into(), "c1".into());
        app.receive_chunk("req-1", "partial");
        assert!(app.streaming_is_for_current());

        app.load_conversation(detail("c2"));
        assert!(
            !app.streaming_is_for_current(),
            "stream belongs to c1, not the open c2"
        );
    }

    #[test]
    fn chunks_for_a_backgrounded_stream_do_not_reset_scroll() {
        // Scroll position belongs to the OPEN conversation; a backgrounded
        // stream's chunks must not yank it.
        let mut app = App::new();
        app.current_conversation = Some(detail("c1"));
        app.start_streaming("req-1".into(), "c1".into());
        app.load_conversation(detail("c2"));
        app.scroll_up(7);
        app.receive_chunk("req-1", "background chunk");
        assert_eq!(app.scroll_offset, 7);
    }

    #[test]
    fn clear_streaming_state_resets_everything_on_disconnect() {
        // Acceptance (TUI-8): after a disconnect there is no frozen ▌ buffer
        // and no stale pending id.
        let mut app = App::new();
        app.current_conversation = Some(detail("c1"));
        app.apply_prompt_ack("task-1".into(), "c1".into());
        app.receive_chunk("req-1", "now-dead partial");
        app.set_assistant_status("Calling tool…");

        app.clear_streaming_state();

        assert_eq!(app.pending_request_id, None);
        assert_eq!(app.streaming_buffer, "");
        assert!(app.streaming_conversation_id.is_none());
        assert!(app.assistant_status.is_none());
    }

    #[test]
    fn cleared_sentinel_cannot_misclaim_the_next_stream() {
        // Unhappy path (TUI-8): a leftover ack sentinel from before the
        // disconnect must not claim the first post-reconnect stream.
        let mut app = App::new();
        app.current_conversation = Some(detail("c1"));
        app.apply_prompt_ack("task-1".into(), "c1".into()); // sentinel armed
        app.clear_streaming_state();

        app.receive_chunk("post-reconnect-req", "someone else's chunk");
        assert_eq!(app.streaming_buffer, "", "chunk must be ignored");
        assert_eq!(app.pending_request_id, None);
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

    #[test]
    fn streaming_lifecycle() {
        let mut app = App::new();
        app.current_conversation = Some(ConversationDetail {
            id: "c1".into(),
            title: "Test".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });

        app.start_streaming("req1".into(), "c1".into());
        assert_eq!(app.pending_request_id, Some("req1".to_string()));

        app.receive_chunk("req1", "Hello ");
        app.receive_chunk("req1", "world!");
        assert_eq!(app.streaming_buffer, "Hello world!");

        app.complete_streaming("req1", "Hello world!");
        assert_eq!(app.streaming_buffer, "");
        assert_eq!(app.pending_request_id, None);

        let msgs = &app.current_conversation.as_ref().unwrap().messages;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "assistant");
        assert_eq!(msgs[0].content, "Hello world!");
    }

    #[test]
    fn wrong_request_id_ignored() {
        let mut app = App::new();
        app.current_conversation = Some(ConversationDetail {
            id: "c1".into(),
            title: "Test".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });

        app.start_streaming("req1".into(), "c1".into());
        app.receive_chunk("wrong_id", "bad data");
        assert_eq!(app.streaming_buffer, "");

        app.complete_streaming("wrong_id", "bad");
        assert!(app.pending_request_id.is_some()); // not cleared
    }

    #[test]
    fn streaming_error_sets_status() {
        let mut app = App::new();
        app.start_streaming("req1".into(), "c1".into());
        app.streaming_error("req1", "LLM timeout");
        assert_eq!(app.status_message, "Error: LLM timeout");
        assert_eq!(app.pending_request_id, None);
        assert_eq!(app.streaming_buffer, "");
    }

    #[test]
    fn assistant_status_set_and_cleared_on_complete() {
        let mut app = App::new();
        app.current_conversation = Some(ConversationDetail {
            id: "c1".into(),
            title: "Test".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });
        app.start_streaming("req1".into(), "c1".into());
        app.set_assistant_status("Searching knowledge base...");
        assert_eq!(
            app.assistant_status.as_deref(),
            Some("Searching knowledge base...")
        );

        app.complete_streaming("req1", "done");
        assert!(app.assistant_status.is_none());
    }

    #[test]
    fn assistant_status_cleared_on_error() {
        let mut app = App::new();
        app.start_streaming("req1".into(), "c1".into());
        app.set_assistant_status("Calling tool...");
        app.streaming_error("req1", "boom");
        assert!(app.assistant_status.is_none());
    }

    #[test]
    fn assistant_status_empty_string_clears() {
        let mut app = App::new();
        app.set_assistant_status("something");
        app.set_assistant_status("");
        assert!(app.assistant_status.is_none());
    }

    #[test]
    fn pending_stream_claims_first_request_id_from_chunk() {
        let mut app = App::new();
        app.current_conversation = Some(ConversationDetail {
            id: "c1".into(),
            title: "Test".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });

        app.start_streaming_without_request_id("c1".into());
        app.receive_chunk("ws-req-1", "Hello ");
        app.receive_chunk("ws-req-1", "world");
        app.complete_streaming("ws-req-1", "Hello world");

        assert_eq!(app.pending_request_id, None);
        let msgs = &app.current_conversation.as_ref().unwrap().messages;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "Hello world");
    }

    #[test]
    fn pending_stream_rejects_unrelated_request_after_claim() {
        let mut app = App::new();
        app.start_streaming_without_request_id("c1".into());

        app.receive_chunk("ws-req-1", "good");
        app.receive_chunk("ws-req-2", "ignored");

        assert_eq!(app.streaming_buffer, "good");
        assert_eq!(app.pending_request_id, Some("ws-req-1".to_string()));
    }

    /// Regression test for issue #52.
    ///
    /// `WsClient::send_prompt` returns the daemon's `task_id`
    /// (post-desktop-assistant #114 `SendMessageAck`). That value is
    /// distinct from the chunk-stream `request_id` embedded in
    /// `AssistantDelta` events — the latter is server-generated per
    /// streaming response. Treating the ack value as the stream's
    /// request_id makes every chunk get filtered out by
    /// `stream_matches_or_claims_request_id`, so the assistant's
    /// streaming reply never appears.
    ///
    /// After `apply_prompt_ack(task_id)` the next chunk — carrying the
    /// real, different server request_id — must still be accepted.
    #[test]
    fn apply_prompt_ack_accepts_chunks_with_distinct_server_request_id() {
        let mut app = App::new();
        app.current_conversation = Some(ConversationDetail {
            id: "c1".into(),
            title: "Test".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });

        // Daemon ack carries a task_id (post-#114 wire protocol).
        app.apply_prompt_ack("task-abc123".to_string(), "c1".to_string());

        // Streaming chunks then arrive with a *different* server-generated
        // request_id. They must be accepted, not filtered out.
        app.receive_chunk("server-req-xyz789", "Hello ");
        app.receive_chunk("server-req-xyz789", "world!");

        assert_eq!(app.streaming_buffer, "Hello world!");

        app.complete_streaming("server-req-xyz789", "Hello world!");
        assert_eq!(app.pending_request_id, None);
        let msgs = &app.current_conversation.as_ref().unwrap().messages;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "assistant");
        assert_eq!(msgs[0].content, "Hello world!");
    }

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
        app.current_conversation = Some(ConversationDetail {
            id: "2".into(),
            title: "Second".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });

        let deleted = app.delete_selected_conversation();
        assert_eq!(deleted, Some("2".to_string()));
        assert_eq!(app.conversations.len(), 2);
        assert!(app.current_conversation.is_none());
        assert_eq!(app.selected_conversation, Some(1)); // stays at 1 (now "Third")
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
        app.current_conversation = Some(ConversationDetail {
            id: "2".into(),
            title: "Second".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });
        app.apply_rename("2", "Renamed");
        assert_eq!(app.conversations[1].title, "Renamed");
        assert_eq!(app.current_conversation.as_ref().unwrap().title, "Renamed");
    }

    #[test]
    fn switch_requested_default_false() {
        assert!(!App::new().switch_requested);
    }

    #[test]
    fn apply_model_override_updates_current_conversation_and_stages_pending() {
        let mut app = App::new();
        app.current_conversation = Some(ConversationDetail {
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
            .current_conversation
            .as_ref()
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
        // auto-following.
        let mut app = App::new();
        app.current_conversation = Some(detail("c1"));
        app.start_streaming("req1".into(), "c1".into());
        app.receive_chunk("req1", "data");
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn receive_chunk_while_scrolled_up_preserves_position() {
        // Acceptance (TUI-10): the user can read scrollback during a long
        // reply — chunks must not yank the view back to the bottom.
        let mut app = App::new();
        app.current_conversation = Some(detail("c1"));
        app.start_streaming("req1".into(), "c1".into());
        app.scroll_up(10);
        app.receive_chunk("req1", "data");
        assert_eq!(app.scroll_offset, 10);
        assert_eq!(app.streaming_buffer, "data", "chunk still buffered");
    }

    #[test]
    fn submit_prompt_resets_scroll() {
        let mut app = App::new();
        app.current_conversation = Some(ConversationDetail {
            id: "c1".into(),
            title: "Test".into(),
            messages: vec![],
            model_selection: None,
            conversation_personality: None,
        });
        app.scroll_up(10);
        app.textarea.insert_str("hello");
        app.submit_prompt();
        assert_eq!(app.scroll_offset, 0);
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
        app.current_conversation = Some(ConversationDetail {
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
        // Always reads every reply aloud whether or not You is Enabled.
        for you in [false, true] {
            let mut app = app_with_open_conversation("c1");
            app.voice_in.insert("c1".to_string(), you);
            app.set_adele_output("c1", AdeleOutput::Always);
            assert!(app.narrate_for("c1"), "Always must narrate (You={you})");
            assert!(
                app.say_this_spoken_for("c1"),
                "Always always speaks say_this (You={you})"
            );
        }
    }

    #[test]
    fn adele_on_demand_narrates_only_when_you_enabled() {
        // The gate's OnDemand arm: spoken iff You == Enabled.
        let mut app = app_with_open_conversation("c1");
        app.set_adele_output("c1", AdeleOutput::OnDemand);

        // You Disabled → reply text-only, but say_this aside still spoken.
        app.voice_in.insert("c1".to_string(), false);
        assert!(
            !app.narrate_for("c1"),
            "OnDemand + You=Disabled: no narration"
        );
        assert!(
            app.say_this_spoken_for("c1"),
            "OnDemand say_this aside spoken even when You=Disabled"
        );

        // You Enabled → reply narrated.
        app.voice_in.insert("c1".to_string(), true);
        assert!(app.narrate_for("c1"), "OnDemand + You=Enabled narrates");
        assert!(app.say_this_spoken_for("c1"));
    }

    #[test]
    fn adele_disabled_never_narrates_and_say_this_goes_inline() {
        let mut app = app_with_open_conversation("c1");
        app.set_adele_output("c1", AdeleOutput::Disabled);
        for you in [false, true] {
            app.voice_in.insert("c1".to_string(), you);
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
        app.voice_in.insert("c2".to_string(), true);
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
    fn narration_gates_on_the_originating_conversation_not_the_open_one() {
        // TUI-4: a backgrounded turn's reply is narrated per ITS
        // conversation's settings, not the open conversation's. c1 narrates
        // (Always), the open c2 is Disabled — the completion still reports c1
        // and the c1 gate holds.
        let mut app = app_with_open_conversation("c1");
        app.set_adele_output("c1", AdeleOutput::Always);
        app.apply_prompt_ack("task-1".into(), "c1".into());
        app.load_conversation(detail("c2"));

        let origin = app.complete_streaming("req-1", "spoken reply");
        assert_eq!(origin.as_deref(), Some("c1"));
        assert!(app.narrate_for("c1"), "origin's gate holds");
        assert!(!app.narrate_for("c2"), "open conversation stays silent");
    }

    #[test]
    fn push_speech_disabled_note_appends_inline_to_open_conversation() {
        let mut app = app_with_open_conversation("c1");
        assert!(app.push_speech_disabled_note("c1", "the kettle is on"));
        let msgs = &app.current_conversation.as_ref().unwrap().messages;
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "assistant");
        assert_eq!(msgs[0].content, "(speech mode disabled) the kettle is on");
    }

    #[test]
    fn push_speech_disabled_note_ignores_other_conversation() {
        // A say_this call referencing a conversation that isn't the open one
        // must NOT bleed text into the visible transcript.
        let mut app = app_with_open_conversation("c1");
        assert!(!app.push_speech_disabled_note("c2", "wrong conversation"));
        assert!(
            app.current_conversation
                .as_ref()
                .unwrap()
                .messages
                .is_empty()
        );
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
