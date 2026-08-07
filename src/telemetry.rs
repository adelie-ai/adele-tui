//! Telemetry wiring for the `adele` binary (epic `mcp-core#38`, `adele-tui#152`).
//!
//! `init_logging` is the only place in this process that installs a tracing
//! subscriber (epic D5); this binary hosts mcp-core server libraries
//! in-process, and none of them installs one of their own.
//!
//! The span and metric helpers below are the seam the D10 content contract is
//! enforced at: none of them accept the prompt or reply text, only ids and
//! counts, so a call site cannot pass content through them even by accident.
//!
//! The root `turn` span follows the shape `desktop-assistant#1152` needs
//! (D12/D13): open it the moment a prompt is committed, close it when the
//! reply finishes streaming, and carry the conversation id as an attribute.
//! This crate does not yet carry that span to the daemon as a `traceparent`
//! — that propagation is `desktop-assistant#1152`'s job. Placing the span
//! correctly now means that ticket does not have to move it.

use std::time::Duration;

/// Install telemetry for this process (epic `mcp-core#38`, `adele-tui#152`).
///
/// Silent by default: with `verbose == 0` and `RUST_LOG` unset, this installs
/// nothing and returns `None`, matching the CLI's documented behavior (a
/// headless `exec` run must stay quiet unless asked). Hold the returned guard
/// for the life of `main` — dropping it early stops the process from
/// flushing on exit.
pub fn init_logging(_verbose: u8) -> Option<adelie_telemetry::Guard> {
    unimplemented!("adele-tui#152: init_logging must route through adelie_telemetry::init")
}

/// The root span for one turn (D12/D13 shape). Open it the moment the prompt
/// is committed — read from the CLI in the headless path, submitted from the
/// composer in the interactive one — and hold it until the reply finishes
/// streaming. Carries the conversation id once known, via
/// [`record_conversation_id`]. Never carries the prompt or reply text (D10).
pub fn turn_span() -> tracing::Span {
    unimplemented!("adele-tui#152: turn_span")
}

/// Record the conversation a turn span belongs to, once it is known.
pub fn record_conversation_id(_span: &tracing::Span, _conversation_id: &str) {
    unimplemented!("adele-tui#152: record_conversation_id")
}

/// A span around establishing the daemon connection.
pub fn connect_span() -> tracing::Span {
    unimplemented!("adele-tui#152: connect_span")
}

/// A span around advertising this client's tools to the daemon.
pub fn registration_span() -> tracing::Span {
    unimplemented!("adele-tui#152: registration_span")
}

/// A span around streaming one turn's reply. Carries the request id and,
/// once streaming ends, the byte count — never the reply text (D10).
pub fn reply_streaming_span(_request_id: &str) -> tracing::Span {
    unimplemented!("adele-tui#152: reply_streaming_span")
}

/// Record how many bytes a completed reply carried, without recording the
/// text itself (D10).
pub fn record_reply_bytes(_span: &tracing::Span, _bytes: usize) {
    unimplemented!("adele-tui#152: record_reply_bytes")
}

/// Log that a reply chunk arrived, at TRACE, without its content (D10).
pub fn trace_chunk_received(_chunk_len: usize) {
    unimplemented!("adele-tui#152: trace_chunk_received")
}

/// Count a successful reconnect to the daemon after a connection drop.
pub fn record_reconnect() {
    unimplemented!("adele-tui#152: record_reconnect")
}

/// Record how long one turn took end to end, from submit to reply-complete.
pub fn record_turn_duration(_elapsed: Duration) {
    unimplemented!("adele-tui#152: record_turn_duration")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
        type Writer = SharedBuf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// D10: none of the span/metric helpers may let prompt or reply content
    /// reach a log line or a span field. This exercises every helper with a
    /// secret standing in for real conversation content and asserts the
    /// secret never appears in the captured console output, while the
    /// structural markers (span names, ids) do.
    #[test]
    fn spans_do_not_record_prompt_or_reply_text() {
        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();

        let secret = "the model must never see this logged: SECRET-PROMPT-CONTENT";

        tracing::subscriber::with_default(subscriber, || {
            let turn = turn_span();
            let _turn_guard = turn.enter();
            record_conversation_id(&turn, "conv-abc123");

            let connect = connect_span();
            connect.in_scope(|| tracing::info!("connected"));

            let register = registration_span();
            register.in_scope(|| tracing::info!("registered"));

            let stream = reply_streaming_span("req-xyz789");
            let _stream_guard = stream.enter();
            trace_chunk_received(secret.len());
            record_reply_bytes(&stream, secret.len());
            tracing::trace!("stream closed");
        });

        let output = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            !output.contains("SECRET-PROMPT-CONTENT") && !output.contains(secret),
            "a span or event recorded reply content: {output}"
        );
        assert!(
            output.contains("turn"),
            "the root turn span must appear: {output}"
        );
        assert!(
            output.contains("conv-abc123"),
            "the conversation id must be recorded on the root span: {output}"
        );
        assert!(
            output.contains("connect"),
            "the connect span must appear: {output}"
        );
        assert!(
            output.contains("registration"),
            "the registration span must appear: {output}"
        );
        assert!(
            output.contains("reply_streaming"),
            "the reply-streaming span must appear: {output}"
        );
        assert!(
            output.contains("req-xyz789"),
            "the request id must be recorded on the streaming span: {output}"
        );
    }
}
