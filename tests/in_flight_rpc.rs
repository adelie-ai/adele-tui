//! TUI-5 (#83): RPCs must run off the render/event loop so a slow or wedged
//! daemon round-trip can't freeze the UI. These tests exercise the
//! transport-agnostic [`adele::in_flight::InFlight`] driver that the event loop
//! polls in its `select!` alongside input and signals.
//!
//! The driver holds in-flight RPC futures concurrently and yields each outcome
//! when (and only when) that future completes — so a still-pending slow RPC
//! never blocks either another RPC or the loop's other branches (input/redraw).

use std::time::Duration;

use adele::in_flight::InFlight;
use futures::StreamExt;
use tokio::time::{sleep, timeout};

/// A completed RPC's outcome is delivered exactly once, and the driver reports
/// empty afterwards.
#[tokio::test]
async fn completed_rpc_yields_its_outcome_once() {
    let mut inflight: InFlight<u32> = InFlight::new();
    assert!(inflight.is_empty());

    inflight.push(async { 7 });
    assert!(!inflight.is_empty());
    assert_eq!(inflight.len(), 1);

    assert_eq!(inflight.next().await, Some(7));
    assert!(inflight.is_empty());
    // Nothing left to drive: an empty driver never resolves (it must be a
    // pending branch in `select!`, not a busy-spinning one that returns None).
    assert!(timeout(Duration::from_millis(50), inflight.next())
        .await
        .is_err());
}

/// Unhappy path — SLOW RPC MUST NOT BLOCK INPUT/OTHER WORK. With a never-
/// completing ("wedged daemon") RPC in flight, a second, fast RPC still
/// resolves, AND a concurrent non-RPC branch (the stand-in for input handling /
/// redraw) keeps making progress. This is the core anti-freeze guarantee.
#[tokio::test]
async fn slow_rpc_does_not_block_fast_rpc_or_the_loop() {
    let mut inflight: InFlight<&'static str> = InFlight::new();

    // A wedged round-trip that never returns.
    inflight.push(async {
        std::future::pending::<()>().await;
        "never"
    });
    // A healthy round-trip that completes promptly.
    inflight.push(async {
        sleep(Duration::from_millis(5)).await;
        "fast"
    });

    // Model the event loop: poll the in-flight driver concurrently with an
    // "input tick". Both must make progress despite the wedged RPC.
    let mut input_ticks = 0u32;
    let fast_outcome = loop {
        tokio::select! {
            outcome = inflight.next() => {
                // Only the fast RPC can resolve; the wedged one never does.
                break outcome;
            }
            _ = sleep(Duration::from_millis(1)) => {
                input_ticks += 1;
                assert!(input_ticks < 10_000, "loop livelocked behind a wedged RPC");
            }
        }
    };

    assert_eq!(fast_outcome, Some("fast"));
    // The input branch kept firing while the slow RPC sat in flight.
    assert!(input_ticks >= 1, "input handling never progressed while an RPC was in flight");
    // The wedged RPC is still pending — it didn't block anything.
    assert_eq!(inflight.len(), 1);
}

/// Concurrent RPCs all complete and every outcome is delivered, regardless of
/// the order they were pushed (completion order, not submission order).
#[tokio::test]
async fn concurrent_rpcs_each_deliver_their_outcome() {
    let mut inflight: InFlight<u64> = InFlight::new();

    // Push in one order, complete in the reverse order via staggered delays.
    inflight.push(async {
        sleep(Duration::from_millis(30)).await;
        1
    });
    inflight.push(async {
        sleep(Duration::from_millis(20)).await;
        2
    });
    inflight.push(async {
        sleep(Duration::from_millis(10)).await;
        3
    });
    assert_eq!(inflight.len(), 3);

    let mut got = Vec::new();
    while let Some(v) = timeout(Duration::from_secs(1), inflight.next())
        .await
        .expect("all RPCs must complete")
    {
        got.push(v);
    }

    got.sort_unstable();
    assert_eq!(got, vec![1, 2, 3]);
    assert!(inflight.is_empty());
}

/// An RPC whose work surfaces an error still delivers an outcome (the error,
/// carried in the outcome type) — it never silently drops, and never hangs.
/// The event loop applies it to the status line just like a success.
#[tokio::test]
async fn errored_rpc_surfaces_without_freezing() {
    let mut inflight: InFlight<Result<(), String>> = InFlight::new();
    inflight.push(async { Err("boom".to_string()) });

    let outcome = timeout(Duration::from_millis(100), inflight.next())
        .await
        .expect("an errored RPC must still resolve promptly");
    assert_eq!(outcome, Some(Err("boom".to_string())));
}
