//! Base values for the upstream wire types this crate's tests construct.
//!
//! `desktop-assistant-api-model` and `client-ui-common` live in other repos and
//! gain fields without notice. A test that names every field of one of those
//! types stops compiling the moment a field arrives, and the repair is then one
//! edit per test - this crate has paid that bill three times, for three
//! different types.
//!
//! None of those types derives `Default`, so each one gets a base value here
//! instead. A test starts from the base and names only the fields it cares
//! about:
//!
//! ```ignore
//! let detail = ConversationDetail {
//!     title: "Second".into(),
//!     ..conversation_detail("c1")
//! };
//! ```
//!
//! A field added upstream is then one edit in this file. Every function below
//! is the only place in this crate that names every field of its type, so keep
//! it that way: reach for functional update syntax rather than a second full
//! literal.

use desktop_assistant_client_common::{
    ChatMessage, ConversationDetail, ConversationSummary, MessageKind,
};

/// A conversation with no messages and no per-conversation overrides.
pub fn conversation_detail(id: &str) -> ConversationDetail {
    ConversationDetail {
        id: id.into(),
        title: format!("Conv {id}"),
        messages: vec![],
        model_selection: None,
        conversation_personality: None,
        tool_gate_disabled: false,
    }
}

/// A sidebar row for a live (not archived) conversation with no messages.
pub fn conversation_summary(id: &str) -> ConversationSummary {
    ConversationSummary {
        id: id.into(),
        title: format!("Conv {id}"),
        message_count: 0,
        archived: false,
    }
}

/// An empty daemon-sourced chat message: no id, ordinary presentation, and
/// none of the client-side send bookkeeping. A test names the `role` and
/// `content` it needs on top of this.
pub fn chat_message() -> ChatMessage {
    ChatMessage {
        id: String::new(),
        role: String::new(),
        content: String::new(),
        kind: MessageKind::Normal,
        idempotency_key: None,
        created_at_ms: None,
    }
}
