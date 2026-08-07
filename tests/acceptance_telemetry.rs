//! Acceptance criteria for `adele-tui#152` (adopt `adelie-telemetry`).
//!
//! The stdout/stderr checks run a separate process on purpose (mirroring
//! `adelie-telemetry`'s own acceptance tests): a subscriber built against a
//! capture writer proves what it was told to do, not what actually reached
//! file descriptor 1. The headless `--prompt`/`exec` path writes the model
//! reply to stdout, so the difference is the whole point (D1).

use std::path::PathBuf;
use std::process::Command;

/// Where `cargo test` leaves the example binary. `cargo test` builds
/// examples (to verify they compile) even when it does not run them, so the
/// probe is always there by the time these tests run.
fn probe_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("a test binary knows its own path");
    path.pop(); // this test binary's file name
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("examples");
    path.push("logging_probe");
    path
}

fn run_probe(args: &[&str], rust_log: Option<&str>) -> std::process::Output {
    let probe = probe_binary();
    assert!(
        probe.is_file(),
        "the logging_probe example must be built before this test can prove anything; \
         expected it at {}",
        probe.display()
    );

    let mut command = Command::new(&probe);
    command.args(args);
    // A clean environment: the test must control RUST_LOG/verbosity itself,
    // not inherit whatever the outer shell happens to have set.
    command.env_remove("RUST_LOG");
    if let Some(value) = rust_log {
        command.env("RUST_LOG", value);
    }
    command.output().expect("the probe must run")
}

/// With `verbose == 0` and `RUST_LOG` unset, no subscriber is installed and
/// nothing is written to either stream.
#[test]
fn no_subscriber_when_quiet() {
    let output = run_probe(&[], None);

    assert!(
        output.status.success(),
        "the probe must exit cleanly: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "quiet by default: stdout must be empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.stderr.is_empty(),
        "quiet by default: no subscriber means no log line on stderr either, got {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `-v` produces output on stderr and nothing on stdout.
#[test]
fn verbose_flag_installs_a_stderr_subscriber() {
    let output = run_probe(&["-v"], None);

    assert!(
        output.status.success(),
        "the probe must exit cleanly: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "no --with-reply was given, so stdout must stay empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ERROR"),
        "-v must install a subscriber that writes to stderr; got {stderr:?}"
    );
}

/// With `RUST_LOG=trace` and the headless `--prompt`/`exec` path, stdout
/// holds only the model reply and no log line.
#[test]
fn prompt_mode_stdout_carries_only_the_reply() {
    let output = run_probe(&["--with-reply"], Some("trace"));

    assert!(
        output.status.success(),
        "the probe must exit cleanly: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout, "REPLY-MARKER",
        "stdout must hold only the reply marker, nothing else: {stdout:?}"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    for level in ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"] {
        assert!(
            stderr.contains(level),
            "RUST_LOG=trace must reach every level on stderr; missing {level} in {stderr:?}"
        );
    }
    for level in ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"] {
        assert!(
            !stdout.contains(level),
            "no log line may reach stdout; found {level} in {stdout:?}"
        );
    }
}

/// A default-feature build of `adele` resolves no `opentelemetry` crate.
#[test]
fn default_build_pulls_no_opentelemetry() {
    if cfg!(feature = "otel") {
        // This test binary was compiled with the feature on, so it cannot
        // say anything about a default build. `just check` runs the plain
        // configuration too, and that run makes this assertion for real.
        return;
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");

    let output = Command::new(cargo)
        .args([
            "tree",
            "--edges",
            "normal",
            "--prefix",
            "none",
            "--manifest-path",
            manifest,
        ])
        .output()
        .expect("cargo tree must run");

    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let tree = String::from_utf8_lossy(&output.stdout);
    let leaked: Vec<&str> = tree
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("opentelemetry"))
        .collect();

    assert!(
        leaked.is_empty(),
        "a default build must resolve no opentelemetry crate, found: {leaked:?}"
    );
}
