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

use tracing::field::Empty;

/// Verbose-count -> `EnvFilter` directive. `RUST_LOG`, when set, wins over
/// this; see [`init_logging`]. Higher counts widen the level for our own
/// crates while keeping third-party noise at `warn`.
fn log_filter(verbose: u8) -> String {
    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    format!(
        "warn,adele={level},desktop_assistant_client_common={level},\
         desktop_assistant_mcp_client={level},client_ui_common={level}"
    )
}

/// Install telemetry for this process (epic `mcp-core#38`, `adele-tui#152`).
///
/// Silent by default: with `verbose == 0` and `RUST_LOG` unset, this installs
/// nothing and returns `None`, matching the CLI's documented behavior (a
/// headless `exec` run must stay quiet unless asked). Hold the returned guard
/// for the life of `main` — dropping it early stops the process from
/// flushing on exit.
///
/// The periodic metrics summary stays off here (`Duration::ZERO`): a TUI has
/// a screen to protect, and the daemon is where the interesting numbers
/// live. `init` itself never fails the process — a bad `OTEL_*` value costs
/// the run its exporter, not its console logging — but its signature still
/// returns a `Result`, so a failure that cannot happen today is still
/// reported rather than unwrapped.
pub fn init_logging(verbose: u8) -> Option<adelie_telemetry::Guard> {
    let has_rust_log = std::env::var_os("RUST_LOG").is_some();
    if verbose == 0 && !has_rust_log {
        return None;
    }
    let config = adelie_telemetry::Config::new("adele-tui")
        .with_default_filter(log_filter(verbose))
        .with_metrics_dump_interval(Duration::ZERO);
    match adelie_telemetry::init(config) {
        Ok(guard) => Some(guard),
        Err(error) => {
            eprintln!("adele: telemetry did not start: {error}");
            None
        }
    }
}

/// The root span for one turn (D12/D13 shape). Open it the moment the prompt
/// is committed — read from the CLI in the headless path, submitted from the
/// composer in the interactive one — and hold it until the reply finishes
/// streaming. Carries the conversation id once known, via
/// [`record_conversation_id`]. Never carries the prompt or reply text (D10).
pub fn turn_span() -> tracing::Span {
    tracing::info_span!("turn", conversation_id = Empty)
}

/// Record the conversation a turn span belongs to, once it is known.
pub fn record_conversation_id(span: &tracing::Span, conversation_id: &str) {
    span.record("conversation_id", conversation_id);
}

/// A span around establishing the daemon connection.
pub fn connect_span() -> tracing::Span {
    tracing::info_span!("connect")
}

/// A span around advertising this client's tools to the daemon.
pub fn registration_span() -> tracing::Span {
    tracing::info_span!("registration")
}

/// A span around streaming one turn's reply. Carries the request id and,
/// once streaming ends, the byte count — never the reply text (D10).
pub fn reply_streaming_span(request_id: &str) -> tracing::Span {
    tracing::info_span!("reply_streaming", request_id = %request_id, reply_bytes = Empty)
}

/// Record how many bytes a completed reply carried, without recording the
/// text itself (D10).
pub fn record_reply_bytes(span: &tracing::Span, bytes: usize) {
    span.record("reply_bytes", bytes as u64);
}

/// Log that a reply chunk arrived, at TRACE, without its content (D10).
/// Content-level detail belongs to the daemon's own DEBUG logging of the
/// same turn (D10), not the client's.
pub fn trace_chunk_received(chunk_len: usize) {
    tracing::trace!(chunk_len, "reply chunk received");
}

/// Count a successful reconnect to the daemon after a connection drop.
pub fn record_reconnect() {
    adelie_telemetry::metrics::increment("adele_tui.reconnects", &[]);
}

/// Record how long one turn took end to end, from submit to reply-complete.
pub fn record_turn_duration(elapsed: Duration) {
    adelie_telemetry::metrics::record_duration("adele_tui.turn_duration", elapsed, &[]);
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

    #[test]
    fn log_filter_scales_level_with_verbosity_and_quiets_third_party() {
        assert!(
            log_filter(0).contains("adele=warn"),
            "no -v => our crates stay at warn"
        );
        assert!(log_filter(1).contains("adele=info"), "one -v => info");
        assert!(log_filter(2).contains("adele=debug"), "two -v => debug");
        assert!(log_filter(3).contains("adele=trace"), "three -v => trace");
        assert!(log_filter(9).contains("adele=trace"), "saturates at trace");
        assert!(log_filter(2).contains("desktop_assistant_client_common=debug"));
        assert!(
            log_filter(3).starts_with("warn,"),
            "base directive keeps third-party at warn"
        );
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
