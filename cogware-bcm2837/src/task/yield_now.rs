// SPDX-License-Identifier: MIT OR Apache-2.0
//! Yielding, and the hardware-polling escape hatch.

use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use super::executor;

/// Requests one more poll of the current task after the next idle tick.
///
/// The primitive for a future waiting on hardware with no interrupt to
/// announce it — a UART receive FIFO, a mailbox status bit, a deadline on the
/// generic timer. It is what lets the core sleep in `wfe` between polls
/// instead of spinning.
///
/// Call from inside a [`Future::poll`] implementation, immediately before
/// returning [`Poll::Pending`]. From an `async fn`, `await` [`next_tick`]
/// instead.
///
/// Does nothing when called from outside a poll, where there is no task to
/// reschedule.
pub fn poll_again() {
    let executor = executor::executor();

    if let Some(task) = executor.current() {
        executor.mark_polling(task.slot());
    }
}

/// Future returned by [`yield_now`].
#[derive(Debug)]
#[must_use = "futures do nothing unless awaited"]
pub struct YieldNow {
    /// Whether the single pending round has already happened.
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            return Poll::Ready(());
        }

        self.yielded = true;

        // Ready again immediately: this is a fairness point, not a sleep.
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// Gives the other ready tasks on this core a turn.
///
/// The task is immediately runnable again, so this does not let the core
/// sleep. Use it to break up a long computation; use [`next_tick`] to wait.
pub fn yield_now() -> YieldNow {
    YieldNow { yielded: false }
}

/// Future returned by [`next_tick`].
#[derive(Debug)]
#[must_use = "futures do nothing unless awaited"]
pub struct NextTick {
    /// Whether the tick has already been waited for.
    waited: bool,
}

impl Future for NextTick {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.waited {
            return Poll::Ready(());
        }

        self.waited = true;
        poll_again();
        Poll::Pending
    }
}

/// Sleeps until the executor's next idle tick.
///
/// The core parks in `wfe` in the meantime. The tick rate is set by the
/// generic timer's event stream — see
/// [`crate::arch::cpu::enable_event_stream`] — making this the cheapest way
/// for an `async fn` to poll hardware in a loop:
///
/// ```ignore
/// loop {
///     if let Some(byte) = console::read_byte() {
///         return byte;
///     }
///     task::next_tick().await;
/// }
/// ```
pub fn next_tick() -> NextTick {
    NextTick { waited: false }
}
