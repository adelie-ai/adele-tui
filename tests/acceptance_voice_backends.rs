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
            // Every edge kind that reaches a compiler invocation. `normal`
            // alone would miss an `ort` arriving as a dev- or build-dependency,
            // which breaks `cargo test` and `cargo clippy` just as thoroughly.
            "--edges",
            "normal,build,dev",
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
fn an_intel_macos_build_resolves_no_onnx_runtime() {
    assert!(
        !resolves_onnx_runtime("x86_64-apple-darwin"),
        "Intel macOS must select no ONNX-Runtime voice backend; `ort` has no prebuilt \
         binary for x86_64-apple-darwin, so its arrival fails the whole crate there"
    );
}

/// Apple Silicon keeps dictation. `ort` does publish an `aarch64-apple-darwin`
/// build, so the carve-out above is by architecture; widening it to all of
/// macOS would take working dictation away from an M-series machine.
#[test]
fn an_apple_silicon_build_still_resolves_onnx_runtime() {
    assert!(
        resolves_onnx_runtime("aarch64-apple-darwin"),
        "aarch64-apple-darwin has a prebuilt `ort`, so it keeps the Silero VAD and \
         Kokoro TTS backends; losing them here means the carve-out went too wide"
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

/// The `has_dictation` cfg must agree with what the manifest actually resolved.
///
/// `build.rs` decides whether this build has a dictation pipeline, and the
/// target-scoped `adele-voice-module` dependency decides whether the VAD adapter
/// is really there. Cargo cannot make one derive from the other: a build script
/// runs after resolution, and a manifest cannot run code. So the two are written
/// by hand, and this is what holds them together.
///
/// One direction already fails at compile time - claiming dictation the manifest
/// did not supply leaves `build_dictation` undefined. The other direction is the
/// one this catches: a manifest that grants a VAD while `build.rs` says
/// otherwise compiles perfectly and silently ships with dictation switched off.
/// That is the state adelie-ai/voice#133 will walk into when it adds the Apple
/// adapters to the macOS feature set.
#[test]
fn has_dictation_agrees_with_what_the_manifest_resolves() {
    let target = env!("ADELE_TARGET");
    let resolves_vad = resolved_packages(target)
        .lines()
        .any(|line| line.starts_with("adele-voice-vad-silero v"));

    assert_eq!(
        cfg!(has_dictation),
        resolves_vad,
        "build.rs says has_dictation={} for {target}, but the manifest resolves \
         adele-voice-vad-silero={resolves_vad}. Those two are written by hand and \
         have drifted; change both.",
        cfg!(has_dictation),
    );
}
