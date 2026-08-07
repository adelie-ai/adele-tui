//! Probe for the `adele-tui#152` acceptance tests, and for a manual check
//! against a real collector.
//!
//! Installs telemetry exactly the way `adele`'s `main` does, opens the same
//! span shape `run_headless` does, records both of `adele-tui`'s own metrics,
//! logs at every level, then optionally writes a fixed marker to stdout
//! standing in for the model reply `run_headless` writes there. The tests in
//! `tests/acceptance_telemetry.rs` run this as a real process and inspect its
//! real file descriptors — the only honest way to prove what does and does
//! not reach stdout versus stderr.
//!
//! Usage: `logging_probe [-v]... [--with-reply]`
//!
//! Against a collector (see the `adelie-telemetry` README for how to run
//! one): `OTEL_EXPORTER_OTLP_ENDPOINT=... OTEL_EXPORTER_OTLP_PROTOCOL=... \
//! cargo run --features otel --example logging_probe -- -v`

use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let verbose = args.iter().filter(|a| a.as_str() == "-v").count() as u8;
    let with_reply = args.iter().any(|a| a == "--with-reply");

    let _guard = adele::telemetry::init_logging(verbose);

    tracing::trace!("trace level");
    tracing::debug!("debug level");
    tracing::info!("info level");
    tracing::warn!("warn level");
    tracing::error!("error level");

    // The same span shape `run_headless` opens, and both of `adele-tui`'s own
    // metrics (adele-tui#152 task 6), so a manual check against a collector
    // sees exactly what a real turn would export.
    let turn = adele::telemetry::turn_span();
    let _turn_guard = turn.enter();
    adele::telemetry::record_conversation_id(&turn, "probe-conversation");
    adele::telemetry::connect_span().in_scope(|| tracing::info!("connected"));
    adele::telemetry::registration_span().in_scope(|| tracing::info!("registered"));
    let stream = adele::telemetry::reply_streaming_span("probe-request");
    let _stream_guard = stream.enter();
    adele::telemetry::trace_chunk_received(4);
    adele::telemetry::record_reply_bytes(&stream, 4);
    drop(_stream_guard);
    drop(_turn_guard);

    adele::telemetry::record_reconnect();
    adele::telemetry::record_turn_duration(Duration::from_millis(1));

    // Stands in for the model reply `run_headless` writes to stdout.
    if with_reply {
        print!("REPLY-MARKER");
    }
}
