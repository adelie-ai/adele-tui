//! Off-loop RPC driver (TUI-5 / #83).
//!
//! Daemon / D-Bus round-trips used to be `await`ed *inline* in the TUI event
//! loop, so any slow RPC — a wedged voice daemon, a stalled WebSocket — froze
//! the whole UI: no redraw, no input, no cancel until the call returned. This
//! driver moves those RPCs **off** the render/event loop.
//!
//! Each RPC is pushed as a future into a [`FuturesUnordered`] that the event
//! loop polls as one more `select!` branch (alongside terminal input and the
//! daemon signal stream). While an RPC future sits in flight the loop keeps
//! drawing and handling keys; when the future resolves, [`InFlight::next`]
//! yields its outcome and the loop applies it to the [`App`](crate::app::App).
//!
//! The futures borrow the connection for their lifetime (the loop owns the
//! `Connector`), which is why this is a *driver* rather than `tokio::spawn` —
//! the transport client is not `'static`/`Clone`, so it can't be moved into a
//! detached task. Polling concurrently in the same `select!` gives the same
//! non-blocking behaviour without needing an owned client.
//!
//! The type is generic over the outcome `T` (the already-mapped result the loop
//! applies to `App`) so it carries no transport types and is unit-testable with
//! plain futures.
//!
//! The futures are `LocalBoxFuture` (not `Send`): they hold an `Rc<Connector>`
//! clone (cheap; keeps the connection alive independent of the loop's
//! `connector` variable being reassigned on reconnect) and are polled in place
//! inside the event loop's `select!` — never `tokio::spawn`ed — so they never
//! cross threads.

use std::future::Future;

use futures::future::LocalBoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};

/// A set of in-flight RPC futures, each resolving to an outcome of type `T`.
///
/// Lifetime `'a` ties the contained futures to the borrows they capture (e.g. a
/// `&'a TransportClient`); the event loop that owns the connection also owns the
/// `InFlight`, so the borrows stay valid for as long as any RPC is pending.
pub struct InFlight<'a, T> {
    futures: FuturesUnordered<LocalBoxFuture<'a, T>>,
}

impl<'a, T> InFlight<'a, T> {
    /// Create an empty driver — no RPCs in flight.
    pub fn new() -> Self {
        Self {
            futures: FuturesUnordered::new(),
        }
    }

    /// Add an RPC future to the in-flight set. It begins making progress the
    /// next time the loop polls [`next`](Self::next); it never blocks `push`
    /// itself.
    pub fn push<F>(&mut self, future: F)
    where
        F: Future<Output = T> + 'a,
    {
        self.futures.push(Box::pin(future));
    }

    /// `true` when no RPCs are in flight.
    pub fn is_empty(&self) -> bool {
        self.futures.is_empty()
    }

    /// Number of RPCs currently in flight.
    pub fn len(&self) -> usize {
        self.futures.len()
    }

    /// Drive the in-flight futures and yield the next completed outcome.
    ///
    /// Returns `Some(outcome)` when an RPC resolves. When the set is **empty**
    /// this future stays *pending forever* (it does NOT resolve to `None`),
    /// exactly like the `Connected`-state reconnect timer: an empty branch in
    /// `select!` must simply never fire, so the loop blocks on its other
    /// branches (input / signals) instead of busy-spinning. (`FuturesUnordered`
    /// would otherwise return `None` immediately when empty, which in a
    /// `select!` arm spins the loop at 100% CPU.)
    ///
    /// In tests where you want the empty case observable, wrap the call in a
    /// `timeout` and treat the elapsed error as "nothing in flight".
    pub async fn next(&mut self) -> Option<T> {
        if self.futures.is_empty() {
            std::future::pending::<()>().await;
        }
        self.futures.next().await
    }
}

impl<'a, T> Default for InFlight<'a, T> {
    fn default() -> Self {
        Self::new()
    }
}
