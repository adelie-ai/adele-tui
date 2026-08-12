//! Decides, once, whether this build has an embedded dictation pipeline.
//!
//! The question the code actually asks is "did the manifest give this build a
//! voice-activity detector", not "which platform is this". The platform is only
//! how that is currently decided: `ort` (ONNX Runtime) publishes no prebuilt
//! binary for `x86_64-apple-darwin`, so the target-scoped `adele-voice-module`
//! dependency in `Cargo.toml` selects no ONNX-Runtime backend there, and
//! `build_dictation` does not exist in such a build.
//!
//! Emitting one named cfg keeps that reasoning in one place instead of at every
//! site that has to branch on it. It is derived from the target rather than
//! taken from a cargo feature on purpose: a feature is only correct when
//! whoever invokes cargo passes the right flags, whereas a plain `cargo build`
//! has to be correct on every platform with no flags.
//!
//! **This condition is stated twice**, here and in the target-scoped dependency
//! in `Cargo.toml`, and nothing checks that the two agree. A build script runs
//! after resolution, so it cannot influence which crates arrive; the manifest
//! cannot run code. Two adjacent statements of one rule is the floor Cargo
//! allows. Claiming dictation the manifest did not supply fails to build, on the
//! missing `build_dictation`; the reverse is silent, and closing it needs the
//! module to publish the fact - adelie-ai/voice#171.

fn main() {
    // Declare the cfg so a typo in the name is a warning rather than a
    // condition that is silently false for ever.
    println!("cargo::rustc-check-cfg=cfg(has_dictation)");
    println!("cargo::rerun-if-changed=build.rs");

    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    // Keep in step with the `adele-voice-module` entries in Cargo.toml.
    let intel_macos = os == "macos" && arch == "x86_64";
    if !intel_macos {
        println!("cargo::rustc-cfg=has_dictation");
    }
}
