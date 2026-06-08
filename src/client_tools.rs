//! Client-local tool handling for the TUI (adele-tui#73).
//!
//! The daemon can suspend a turn on a *client* tool (`SignalEvent::ClientToolCall`)
//! and park until the client posts a result back. Before #261 the per-user tool
//! registry meant another client's tools (e.g. a voice session's `say_this`)
//! could fire on a TUI turn that the TUI then ignored, wedging the turn forever.
//!
//! This module turns an incoming call into a pure [`ToolOutcome`]: the result
//! string to submit back to the daemon (so the turn ALWAYS resumes) plus an
//! optional side effect (speak the text, or show it inline). The async event
//! loop performs the side effect and submits the result; the decision itself is
//! pure so it is unit-testable without a transport or an audio device.
//!
//! The one tool the TUI understands is `say_this` — "speak this text aloud".
//! Whether it actually speaks is gated by the per-conversation speech toggle
//! (the hard cut-off): ON ⇒ speak; OFF ⇒ render `(speech mode disabled) <text>`
//! inline instead. Either way the turn completes. Any other tool name resolves
//! to an error result rather than a wedge.

/// The TUI's one client tool: speak a short piece of text aloud. The daemon
/// forwards this name verbatim to the LLM's tool list.
pub const SAY_THIS: &str = "say_this";

/// A `SignalEvent::ClientToolCall` flattened into one value so the async
/// handler takes a single argument instead of five positional ones. Built in
/// the event-loop match arm from the event's destructured fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientToolCall {
    pub task_id: String,
    pub conversation_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: serde_json::Value,
}

/// What the event loop should do with a client tool call, plus the result
/// string to submit back to the daemon. `result` is `Ok` on success and `Err`
/// with a human-readable message on failure (malformed args, unknown tool);
/// either way it is submitted so the suspended turn resumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    /// Side effect the loop performs after deciding (speak / show / nothing).
    pub effect: ToolEffect,
    /// The result to hand to `submit_client_tool_result`.
    pub result: Result<String, String>,
}

/// The side effect a tool call requires, decoupled from the async machinery so
/// the decision stays pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolEffect {
    /// Speak `text` aloud via the embedded `Speaker`.
    Speak(String),
    /// Speech is off for this conversation: show `text` inline in the
    /// transcript prefixed with `(speech mode disabled)` instead of speaking.
    ShowDisabled(String),
    /// Nothing to do beyond submitting the result (unknown tool / bad args).
    None,
}

/// Decide how to handle a `say_this` client tool call.
///
/// `arguments` is the raw JSON the daemon forwarded; `speech_enabled` is the
/// per-conversation hard toggle for the call's conversation. Never panics:
/// a non-object payload or a missing/non-string `text` field becomes an error
/// result (the turn still resumes) rather than an unwrap.
pub fn handle_say_this(arguments: &serde_json::Value, speech_enabled: bool) -> ToolOutcome {
    let Some(text) = arguments.get("text").and_then(|t| t.as_str()) else {
        return ToolOutcome {
            effect: ToolEffect::None,
            result: Err("say_this requires a string `text` argument".to_string()),
        };
    };
    if speech_enabled {
        ToolOutcome {
            effect: ToolEffect::Speak(text.to_string()),
            result: Ok("spoken".to_string()),
        }
    } else {
        ToolOutcome {
            effect: ToolEffect::ShowDisabled(text.to_string()),
            result: Ok(
                "speech mode is disabled in this conversation; the text was shown to the user, \
                 not spoken"
                    .to_string(),
            ),
        }
    }
}

/// Dispatch an arbitrary client tool call by name. `say_this` is handled;
/// anything else resolves to an error result so an unexpected tool (e.g. one
/// leaked from another session pre-#261, or a future tool the TUI doesn't yet
/// implement) still resumes the turn instead of wedging it.
pub fn dispatch(
    tool_name: &str,
    arguments: &serde_json::Value,
    speech_enabled: bool,
) -> ToolOutcome {
    match tool_name {
        SAY_THIS => handle_say_this(arguments, speech_enabled),
        other => ToolOutcome {
            effect: ToolEffect::None,
            result: Err(format!("unknown client tool `{other}`")),
        },
    }
}

/// The `say_this` tool registration to advertise to the daemon. Re-sent on
/// every connect because the daemon replaces the whole set each time (#231).
pub fn say_this_registration() -> desktop_assistant_api_model::ClientToolRegistration {
    desktop_assistant_api_model::ClientToolRegistration {
        name: SAY_THIS.to_string(),
        description: "Speak a short piece of text aloud to the user through their speakers. \
            Use for brief spoken asides; the user's reply still arrives as text."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The text to speak aloud."
                }
            },
            "required": ["text"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> serde_json::Value {
        serde_json::json!({ "text": text })
    }

    #[test]
    fn say_this_while_enabled_speaks_and_reports_spoken() {
        let outcome = handle_say_this(&args("hello there"), true);
        assert_eq!(outcome.effect, ToolEffect::Speak("hello there".to_string()));
        assert_eq!(outcome.result, Ok("spoken".to_string()));
    }

    #[test]
    fn say_this_while_disabled_shows_inline_and_does_not_speak() {
        let outcome = handle_say_this(&args("hello there"), false);
        assert_eq!(
            outcome.effect,
            ToolEffect::ShowDisabled("hello there".to_string())
        );
        // Result is Ok so the turn completes, but it tells the model the text
        // was shown, not spoken.
        let msg = outcome.result.expect("disabled say_this still succeeds");
        assert!(msg.contains("disabled"));
        assert!(msg.contains("not spoken"));
        // Crucially, NOT a Speak effect.
        assert!(!matches!(outcome.effect, ToolEffect::Speak(_)));
    }

    #[test]
    fn say_this_missing_text_is_an_error_not_a_panic() {
        let outcome = handle_say_this(&serde_json::json!({ "other": 1 }), true);
        assert_eq!(outcome.effect, ToolEffect::None);
        assert!(outcome.result.is_err());
    }

    #[test]
    fn say_this_non_string_text_is_an_error() {
        let outcome = handle_say_this(&serde_json::json!({ "text": 42 }), true);
        assert_eq!(outcome.effect, ToolEffect::None);
        assert!(outcome.result.is_err());
    }

    #[test]
    fn say_this_non_object_payload_is_an_error_not_a_panic() {
        // A malformed (non-object) arguments value must not panic.
        let outcome = handle_say_this(&serde_json::json!("just a string"), true);
        assert_eq!(outcome.effect, ToolEffect::None);
        assert!(outcome.result.is_err());
        let outcome = handle_say_this(&serde_json::Value::Null, false);
        assert!(outcome.result.is_err());
    }

    #[test]
    fn dispatch_routes_say_this() {
        let outcome = dispatch(SAY_THIS, &args("hi"), true);
        assert_eq!(outcome.effect, ToolEffect::Speak("hi".to_string()));
    }

    #[test]
    fn dispatch_unknown_tool_returns_error_result_so_turn_resumes() {
        // The wedge-killer: any unexpected tool name still yields a result to
        // submit (an error), never a silent drop.
        let outcome = dispatch("delete_everything", &args("ignored"), true);
        assert_eq!(outcome.effect, ToolEffect::None);
        assert!(outcome.result.is_err());
        assert!(outcome.result.unwrap_err().contains("delete_everything"));
    }

    #[test]
    fn registration_has_say_this_name_and_required_text() {
        let reg = say_this_registration();
        assert_eq!(reg.name, SAY_THIS);
        assert!(!reg.description.is_empty());
        let required = reg.input_schema.get("required").unwrap();
        assert_eq!(required, &serde_json::json!(["text"]));
    }
}
