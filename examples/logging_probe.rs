//! Probe for the `adele-tui#152` acceptance tests.
//!
//! Installs telemetry exactly the way `adele`'s `main` does, logs at every
//! level, then optionally writes a fixed marker to stdout standing in for
//! the model reply `run_headless` writes there. The tests in
//! `tests/acceptance_telemetry.rs` run this as a real process and inspect
//! its real file descriptors — the only honest way to prove what does and
//! does not reach stdout versus stderr.
//!
//! Usage: `logging_probe [-v]... [--with-reply]`

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

    // Stands in for the model reply `run_headless` writes to stdout.
    if with_reply {
        print!("REPLY-MARKER");
    }
}
