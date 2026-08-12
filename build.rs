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
//! in `Cargo.toml`. A build script runs after resolution, so it cannot influence
//! which crates arrive; the manifest cannot run code. Two adjacent statements of
//! one rule is the floor Cargo allows.
//!
//! They are checked against each other, in both directions, by
//! `tests/acceptance_voice_backends.rs`, which is why `ADELE_TARGET` is exported
//! below: the test needs the triple this build was compiled for so it can ask
//! cargo what the manifest resolves for that same triple. Claiming dictation the
//! manifest did not supply already fails to compile, on the missing
//! `build_dictation`; the reverse used to be silent, and is now a test failure.

fn main() {
    // Declare the cfg so a typo in the name is a warning rather than a
    // condition that is silently false for ever.
    println!("cargo::rustc-check-cfg=cfg(has_dictation)");
    println!("cargo::rerun-if-changed=build.rs");

    // The triple this build targets, for the test that checks this decision
    // against what the manifest actually resolves.
    println!(
        "cargo::rustc-env=ADELE_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );

    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    // Keep in step with the `adele-voice-module` entries in Cargo.toml.
    //
    // An unset variable yields "", so this is false - which is how Cargo's own
    // `cfg(all(target_os = "macos", target_arch = "x86_64"))` evaluates an unset
    // cfg too. The two statements therefore degrade in the same direction, and
    // that direction fails loudly: a build claiming dictation it was not given
    // stops on the missing `build_dictation` rather than quietly losing it.
    let intel_macos = os == "macos" && arch == "x86_64";
    if !intel_macos {
        println!("cargo::rustc-cfg=has_dictation");
    }
}
