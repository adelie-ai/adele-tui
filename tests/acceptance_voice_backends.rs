//! Which voice backends each target resolves.
//!
//! The property these guard is a dependency-graph one, and a build check cannot
//! see it: `ort` (ONNX Runtime) can enter the graph unasked-for and still
//! compile on a target that has a prebuilt for it. It has none for
//! `x86_64-apple-darwin`, so an `ort` that arrives there fails the whole crate -
//! `cargo test` and `cargo clippy` included, for work nowhere near voice.
//!
//! `cargo tree` resolves for a target without building for it, so both
//! assertions run from either platform. Whoever adds a voice backend finds out
//! on their own machine rather than on someone else's.

use std::process::Command;

/// Package names, one per line, that a build for `target` resolves.
fn resolved_packages(target: &str) -> String {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");

    let output = Command::new(cargo)
        .args([
            "tree",
            "--edges",
            "normal",
            "--prefix",
            "none",
            "--target",
            target,
            "--manifest-path",
            manifest,
        ])
        .output()
        .expect("cargo tree must run");

    assert!(
        output.status.success(),
        "cargo tree failed for {target}, so the assertion below would prove nothing: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("cargo tree output is utf-8")
}

fn resolves_onnx_runtime(target: &str) -> bool {
    resolved_packages(target)
        .lines()
        .any(|line| line.starts_with("ort v"))
}

#[test]
fn a_macos_build_resolves_no_onnx_runtime() {
    assert!(
        !resolves_onnx_runtime("x86_64-apple-darwin"),
        "macOS must select no ONNX-Runtime voice backend; `ort` has no prebuilt binary \
         for that target, so its arrival fails the whole crate there"
    );
}

/// The positive control. Without it, every way of failing to spot `ort` - a
/// renamed crate, a changed `cargo tree` format - would read as the property
/// above holding.
#[test]
fn a_linux_build_still_resolves_onnx_runtime() {
    assert!(
        resolves_onnx_runtime("x86_64-unknown-linux-gnu"),
        "Linux keeps the Silero VAD and Kokoro TTS backends, so `ort` belongs in its \
         graph; if it is gone, the check above can no longer detect anything"
    );
}
