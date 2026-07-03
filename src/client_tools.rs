//! Client-local tool handling for the TUI (adele-tui#73, reworked for the
//! You/Adele model in adele-tui#77).
//!
//! The daemon can suspend a turn on a *client* tool (`SignalEvent::ClientToolCall`)
//! and park until the client posts a result back. Before #261 the per-user tool
//! registry meant another client's tools (e.g. a voice session's `say_this`)
//! could fire on a TUI turn that the TUI then ignored, wedging the turn forever.
//!
//! This module turns an incoming call into a pure [`ToolOutcome`]: the result
//! string to submit back to the daemon (so the turn ALWAYS resumes) plus an
//! optional side effect (speak the text, show it inline, or set the `Adele`
//! output level). The async event loop performs the side effect and submits the
//! result; the decision itself is pure so it is unit-testable without a
//! transport or an audio device.
//!
//! The TUI understands three tools:
//!
//! * `say_this` — "speak this text aloud". Whether it actually speaks is gated
//!   by whether the call's conversation's `Adele` level is `OnDemand` — its sole
//!   spoken channel (voice#126); the caller passes that boolean as
//!   `say_this_spoken`. Spoken ⇒ speak (daemon-first, chunked) AND show the line
//!   tagged `Spoken`; not spoken ⇒ show it tagged `SpeechDisabled` instead.
//! * `request_voice` (adele-tui#77) — the model switching this conversation into
//!   spoken voice mode ("ok, let's talk by voice"). Sets `Adele = OnDemand`.
//! * `stop_voice` (adele-tui#77) — the model leaving voice mode; sets
//!   `Adele = Disabled`.
//!
//! Either way the turn always completes, and any other tool name resolves to an
//! error result rather than a wedge. The decision is pure; the async handler
//! performs the side effect (speak / show inline / set `App::adele_output`).

use crate::app::AdeleOutput;

/// The TUI's `say_this` client tool: speak a short piece of text aloud. The
/// daemon forwards this name verbatim to the LLM's tool list.
pub const SAY_THIS: &str = "say_this";

/// The model-driven "enter voice mode" tool (adele-tui#77). The model calls it
/// when the user asks to talk by voice; it sets the conversation's `Adele`
/// output level to `OnDemand` — the written reply stays text and `say_this` is
/// the model's spoken channel (voice#126).
pub const REQUEST_VOICE: &str = "request_voice";

/// The model-driven "leave voice mode" tool (adele-tui#77). Sets the
/// conversation's `Adele` level to `Disabled`, back to text-only.
pub const STOP_VOICE: &str = "stop_voice";

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
    /// Speak `text` aloud (daemon-first, chunked; embedded fallback).
    Speak(String),
    /// The call's conversation has `Adele == Disabled`: show `text` inline in
    /// the transcript prefixed with `(speech mode disabled)` instead of speaking.
    ShowDisabled(String),
    /// Set the call's conversation's `Adele` output level (adele-tui#77) —
    /// `OnDemand` for `request_voice`, `Disabled` for `stop_voice`. The async
    /// handler applies it to `App::adele_output` so this decision stays pure.
    SetAdeleOutput(AdeleOutput),
    /// Nothing to do beyond submitting the result (unknown tool / bad args).
    None,
}

/// Decide how to handle a `say_this` client tool call.
///
/// `arguments` is the raw JSON the daemon forwarded; `say_this_spoken` is whether
/// the call's conversation's `Adele` level is `OnDemand` or `Always` (i.e. not
/// `Disabled`) — the aside gate (adele-tui#77). Never panics: a non-object
/// payload or a missing/non-string `text` field becomes an error result (the
/// turn still resumes) rather than an unwrap.
pub fn handle_say_this(arguments: &serde_json::Value, say_this_spoken: bool) -> ToolOutcome {
    let Some(text) = arguments.get("text").and_then(|t| t.as_str()) else {
        return ToolOutcome {
            effect: ToolEffect::None,
            result: Err("say_this requires a string `text` argument".to_string()),
        };
    };
    if say_this_spoken {
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

/// Decide how to handle a `request_voice` call (adele-tui#77): the model is
/// entering voice mode for the conversation. Pure — emits a
/// `SetAdeleOutput(OnDemand)` effect the handler applies to `App::adele_output`,
/// and always a result.
pub fn handle_request_voice() -> ToolOutcome {
    ToolOutcome {
        effect: ToolEffect::SetAdeleOutput(AdeleOutput::OnDemand),
        result: Ok(
            "voice mode on (on-demand): your written reply is shown as text and not read aloud; \
             speak to the user by calling say_this, kept brief and conversational"
                .to_string(),
        ),
    }
}

/// Decide how to handle a `stop_voice` call (adele-tui#77): the model is leaving
/// voice mode. Pure — emits a `SetAdeleOutput(Disabled)` effect and always a
/// result.
pub fn handle_stop_voice() -> ToolOutcome {
    ToolOutcome {
        effect: ToolEffect::SetAdeleOutput(AdeleOutput::Disabled),
        result: Ok("voice mode off: this conversation is back to text-only".to_string()),
    }
}

/// Dispatch an arbitrary client tool call by name. `say_this`, `request_voice`,
/// and `stop_voice` are handled; anything else resolves to an error result so an
/// unexpected tool (e.g. one leaked from another session pre-#261, or a future
/// tool the TUI doesn't yet implement) still resumes the turn instead of wedging
/// it. `say_this_spoken` is whether the call's conversation's `Adele` level
/// speaks asides (OnDemand/Always); it only affects `say_this`.
pub fn dispatch(
    tool_name: &str,
    arguments: &serde_json::Value,
    say_this_spoken: bool,
) -> ToolOutcome {
    match tool_name {
        SAY_THIS => handle_say_this(arguments, say_this_spoken),
        REQUEST_VOICE => handle_request_voice(),
        STOP_VOICE => handle_stop_voice(),
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

/// The `request_voice` tool registration (adele-tui#77). Advertised on every
/// (re)connect alongside `say_this`. Calling it switches the conversation into
/// spoken voice mode (`Adele = OnDemand`).
pub fn request_voice_registration() -> desktop_assistant_api_model::ClientToolRegistration {
    desktop_assistant_api_model::ClientToolRegistration {
        name: REQUEST_VOICE.to_string(),
        description: "Switch this conversation into spoken (on-demand) voice mode (the user asked \
            to talk by voice). Your written reply stays text; to speak, call say_this with a \
            brief spoken version. Call when the user says something like \"let's talk by voice\" \
            or \"read your replies to me\". No arguments."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {}
        }),
    }
}

/// The `stop_voice` tool registration (adele-tui#77). Leaves voice mode
/// (`Adele = Disabled`), back to text-only.
pub fn stop_voice_registration() -> desktop_assistant_api_model::ClientToolRegistration {
    desktop_assistant_api_model::ClientToolRegistration {
        name: STOP_VOICE.to_string(),
        description: "Leave voice mode; this conversation goes back to text-only (replies are no \
            longer read aloud). Call when the user says something like \"stop talking\" or \"let's \
            go back to text\". No arguments."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {}
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

    // --- Voice mode (adele-tui#77) ---

    #[test]
    fn request_voice_sets_adele_on_demand_with_a_result() {
        let outcome = handle_request_voice();
        assert_eq!(
            outcome.effect,
            ToolEffect::SetAdeleOutput(AdeleOutput::OnDemand)
        );
        let msg = outcome
            .result
            .expect("request_voice always returns a result");
        assert!(msg.contains("voice mode on"));
    }

    #[test]
    fn stop_voice_sets_adele_disabled_with_a_result() {
        let outcome = handle_stop_voice();
        assert_eq!(
            outcome.effect,
            ToolEffect::SetAdeleOutput(AdeleOutput::Disabled)
        );
        let msg = outcome.result.expect("stop_voice always returns a result");
        assert!(msg.contains("voice mode off"));
    }

    #[test]
    fn dispatch_routes_request_voice() {
        // request_voice ignores its arguments and the say_this gate; it just sets
        // Adele = OnDemand. Pass an empty object and gate OFF to prove that.
        let outcome = dispatch(REQUEST_VOICE, &serde_json::json!({}), false);
        assert_eq!(
            outcome.effect,
            ToolEffect::SetAdeleOutput(AdeleOutput::OnDemand)
        );
        assert!(outcome.result.is_ok());
    }

    #[test]
    fn dispatch_routes_stop_voice() {
        let outcome = dispatch(STOP_VOICE, &serde_json::json!({}), true);
        assert_eq!(
            outcome.effect,
            ToolEffect::SetAdeleOutput(AdeleOutput::Disabled)
        );
        assert!(outcome.result.is_ok());
    }

    #[test]
    fn request_voice_with_malformed_args_still_returns_a_result_no_panic() {
        // The model shouldn't send args, but a non-object payload must not
        // panic — request_voice ignores arguments entirely.
        let outcome = dispatch(REQUEST_VOICE, &serde_json::Value::Null, false);
        assert_eq!(
            outcome.effect,
            ToolEffect::SetAdeleOutput(AdeleOutput::OnDemand)
        );
        assert!(outcome.result.is_ok());
    }

    #[test]
    fn say_this_speaks_when_aside_is_spoken() {
        // The say_this gate is "Adele speaks asides" (OnDemand/Always). The
        // caller passes that boolean; here it is on.
        let outcome = handle_say_this(&args("aside"), true);
        assert_eq!(outcome.effect, ToolEffect::Speak("aside".to_string()));
    }

    #[test]
    fn say_this_shows_inline_when_adele_disabled() {
        let outcome = handle_say_this(&args("aside"), false);
        assert_eq!(
            outcome.effect,
            ToolEffect::ShowDisabled("aside".to_string())
        );
        assert!(outcome.result.is_ok());
    }

    #[test]
    fn voice_registrations_have_names_and_descriptions() {
        let req = request_voice_registration();
        assert_eq!(req.name, REQUEST_VOICE);
        assert!(!req.description.is_empty());
        let stop = stop_voice_registration();
        assert_eq!(stop.name, STOP_VOICE);
        assert!(!stop.description.is_empty());
    }
}
