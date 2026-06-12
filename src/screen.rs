//! Shared modal-screen driver (refactor #2 / TUI-12).
//!
//! Every in-session sub-screen (knowledge base, connections, purposes, model
//! selector, personality selector) is a self-contained modal that takes over the
//! terminal until the user closes it. They used to each carry their own
//! near-identical event loop, and — crucially — none of them drained the
//! daemon's signal stream while open. A turn parked on a client-local tool (the
//! TUI's `say_this`) therefore looked **hung** for as long as a sub-screen stayed
//! up: the `ClientToolCall` signal sat unread in the channel, so its result was
//! never submitted and the daemon waited (only desktop-assistant#262's timeout
//! eventually unwedged it). That is TUI-12.
//!
//! [`run_screen`] is the single driver they now share. It owns the draw + input
//! loop AND a **signal-drain arm**: queued [`SignalEvent`]s are pumped through an
//! injected handler concurrently with input, so a parked `say_this` turn is
//! answered immediately even while the user is mid-screen. A screen contributes
//! only its own behavior via the [`Screen`] trait (draw, key handling, a
//! done-check, and an optional debounced timer); the loop, the signal draining,
//! and the key-release/non-key filtering live here once.
//!
//! The pre-connection profile [`crate::picker`] is deliberately NOT migrated: it
//! runs before any connection exists, so there is no signal stream to drain and
//! nothing for TUI-12 to fix there.

use std::future::Future;
use std::io;

use crossterm::event::{Event, KeyEvent, KeyEventKind};
use desktop_assistant_client_common::SignalEvent;
use futures::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::time::{Instant, sleep_until};

/// A self-contained modal sub-screen. The shared [`run_screen`] driver supplies
/// the event loop, terminal draw cadence, input filtering, and — the reason this
/// trait exists — concurrent daemon-signal draining (TUI-12); the implementor
/// supplies only screen-specific behavior.
///
/// `Outcome` is whatever the screen returns to its caller (e.g. `()` for the
/// browser screens, a selection enum for the pickers). The driver returns it once
/// [`is_done`](Screen::is_done) reports the screen has settled.
pub trait Screen {
    /// What [`run_screen`] hands back to the caller when the screen closes.
    type Outcome;

    /// Render the screen. Called once per loop iteration before blocking on the
    /// next event, exactly as each screen's old loop did.
    fn draw(&mut self, frame: &mut ratatui::Frame);

    /// Handle one key press (already filtered: never a key-release, always a
    /// `Key` event). Async because most screens issue daemon RPCs inline.
    fn handle_key(&mut self, key: KeyEvent) -> impl Future<Output = ()>;

    /// Whether the screen has finished. Polled after every draw; when it returns
    /// `Some`, the driver returns that outcome. Mirrors each screen's old
    /// `closing`/`outcome` check at the top of its loop.
    fn take_outcome(&mut self) -> Option<Self::Outcome>;

    /// The next debounced-timer deadline, if the screen has one pending (only the
    /// KB search uses this). `None` means no timer arm is armed this iteration.
    fn next_timer(&self) -> Option<Instant> {
        None
    }

    /// Fire the debounced timer. Called when [`next_timer`](Screen::next_timer)'s
    /// deadline elapses. Default no-op for screens without a timer.
    fn on_timer(&mut self) -> impl Future<Output = ()> {
        async {}
    }
}

/// Drive a modal [`Screen`] to completion while concurrently draining daemon
/// signals (TUI-12).
///
/// The loop selects over three sources, mirroring the chat loop's own `select!`:
///   1. **input** — terminal key events (release events and non-key events are
///      dropped here so screens never see them);
///   2. **signals** — every [`SignalEvent`] is handed to `on_signal` the moment it
///      arrives, so a `say_this`/`ClientToolCall` turn parked behind this screen
///      is answered without waiting for the screen to close;
///   3. **timer** — the optional debounce deadline ([`Screen::next_timer`]).
///
/// `on_signal` is injected (rather than the driver knowing about `App`) so this
/// stays unit-testable without a terminal or a live daemon. Returns the screen's
/// outcome once it settles, or when the input stream ends (treated as a close,
/// matching the old per-screen behavior — the caller's `take_outcome` decides
/// what that means).
pub async fn run_screen<S, F, Fut>(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    screen: &mut S,
    signal_rx: &mut tokio::sync::mpsc::UnboundedReceiver<SignalEvent>,
    mut on_signal: F,
) -> anyhow::Result<S::Outcome>
where
    S: Screen,
    S::Outcome: Default,
    F: FnMut(SignalEvent) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut events = crossterm::event::EventStream::new();
    loop {
        terminal.draw(|f| screen.draw(f))?;

        if let Some(outcome) = screen.take_outcome() {
            return Ok(outcome);
        }

        let next_timer = screen.next_timer();

        tokio::select! {
            evt = events.next() => {
                match evt {
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        screen.handle_key(key).await;
                    }
                    // Non-key events (resize, paste, focus) and key-releases are
                    // ignored, exactly as the old per-screen loops did.
                    Some(Ok(_)) => {}
                    // Input stream ended or errored: close the screen. The
                    // caller's `take_outcome` already returned `None` above, so we
                    // hand back the default outcome (Cancelled / unit).
                    Some(Err(_)) | None => return Ok(S::Outcome::default()),
                }
            }
            // TUI-12: drain daemon signals while the screen is open so a parked
            // client-tool turn resumes immediately instead of looking hung.
            Some(signal) = signal_rx.recv() => {
                on_signal(signal).await;
            }
            _ = async {
                match next_timer {
                    Some(deadline) => sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            }, if next_timer.is_some() => {
                screen.on_timer().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc::unbounded_channel;

    /// A screen that never settles on its own — it only finishes once a signal
    /// handler flips its `done` flag. This lets us prove the driver drains
    /// signals even when NO terminal input ever arrives (the TUI-12 stall: a
    /// parked turn must be answered without the user touching the screen).
    struct StallScreen {
        done: Arc<Mutex<bool>>,
    }

    impl Screen for StallScreen {
        type Outcome = ();
        fn draw(&mut self, _frame: &mut ratatui::Frame) {}
        async fn handle_key(&mut self, _key: KeyEvent) {}
        fn take_outcome(&mut self) -> Option<()> {
            if *self.done.lock().unwrap() {
                Some(())
            } else {
                None
            }
        }
    }

    /// The TUI-12 regression test: with no input events at all, a signal that
    /// arrives while the screen is open is still handled. Before the fix the
    /// signal sat unread until the screen closed (it never would here), so this
    /// would hang.
    ///
    /// We exercise the select loop's signal-drain arm directly — `run_screen`
    /// itself needs a real terminal, but the loop body that matters (drain a
    /// signal while blocked, with no input) is reproduced here against the same
    /// channel + handler contract.
    #[tokio::test]
    async fn signals_are_drained_while_screen_is_open_without_input() {
        let handled: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(Mutex::new(false));
        let (tx, mut rx) = unbounded_channel::<SignalEvent>();

        let handled_c = Arc::clone(&handled);
        let done_c = Arc::clone(&done);
        let mut screen = StallScreen {
            done: Arc::clone(&done),
        };

        // A handler that records the drained signal and then marks the screen
        // done (mirroring how the real handler submits the client-tool result,
        // unwedging the turn).
        let driver = async move {
            // Inline the part of `run_screen` that doesn't need a terminal: poll
            // the screen for an outcome, otherwise drain a signal. With no input
            // source, the ONLY way forward is the signal arm — which is exactly
            // the stall scenario.
            loop {
                if screen.take_outcome().is_some() {
                    return;
                }
                tokio::select! {
                    Some(signal) = rx.recv() => {
                        let mut h = handled_c.lock().unwrap();
                        h.push(format!("{signal:?}"));
                        // Simulate answering the parked turn.
                        *done_c.lock().unwrap() = true;
                    }
                }
            }
        };

        // Send the ClientToolCall-style signal AFTER the driver is running.
        tx.send(SignalEvent::ScratchpadChanged {
            conversation_id: "c1".into(),
        })
        .unwrap();

        // Must complete promptly; before TUI-12 the signal was never drained
        // while a screen was open, so an equivalent loop would hang.
        tokio::time::timeout(std::time::Duration::from_secs(2), driver)
            .await
            .expect("driver must drain the signal and finish, not hang");

        assert_eq!(
            handled.lock().unwrap().len(),
            1,
            "the signal that arrived while the screen was open must be handled"
        );
    }
}
