// SPDX-License-Identifier: MIT OR Apache-2.0
//! Timer futures built on the architectural counter.
//!
//! There is no timer interrupt, so these are poll-driven: a sleeping task is
//! re-polled once per executor idle tick and compares `CNTPCT_EL0` against its
//! deadline. The core sleeps in `wfe` in between, bounded by the generic
//! timer's event stream.
//!
//! A sleep therefore overruns by up to one event period — around a millisecond
//! with the current [`enable_event_stream`](crate::arch::cpu::enable_event_stream)
//! setting. These are timers for pacing work, not for meeting deadlines.

use core::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use super::yield_now::poll_again;
use crate::time;

/// Converts a duration into an absolute deadline on the physical counter.
fn deadline_after(duration: Duration) -> u64 {
    let frequency = u128::from(time::frequency_hz());

    let ticks = duration
        .as_nanos()
        .saturating_mul(frequency)
        .saturating_div(1_000_000_000);

    time::ticks().saturating_add(u64::try_from(ticks).unwrap_or(u64::MAX))
}

/// Whether the counter has reached `deadline`.
///
/// `CNTPCT_EL0` is 64 bits and counts at tens of megahertz, so it does not
/// wrap in any timescale this kernel will see; a plain comparison is safe.
fn expired(deadline: u64) -> bool {
    time::ticks() >= deadline
}

/// Future returned by [`sleep`].
#[derive(Debug)]
#[must_use = "futures do nothing unless awaited"]
pub struct Sleep {
    /// Absolute `CNTPCT_EL0` value to wake at.
    deadline: u64,
}

impl Sleep {
    /// The counter value this sleep is waiting for.
    pub fn deadline(&self) -> u64 {
        self.deadline
    }

    /// Whether the deadline has passed.
    pub fn is_elapsed(&self) -> bool {
        expired(self.deadline)
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if expired(self.deadline) {
            return Poll::Ready(());
        }

        poll_again();
        Poll::Pending
    }
}

/// Sleeps for at least `duration`, letting the core idle in the meantime.
pub fn sleep(duration: Duration) -> Sleep {
    Sleep {
        deadline: deadline_after(duration),
    }
}

/// Sleeps for at least `millis` milliseconds.
pub fn sleep_millis(millis: u64) -> Sleep {
    sleep(Duration::from_millis(millis))
}

/// Sleeps for at least `micros` microseconds.
///
/// The executor's tick granularity is coarser than a microsecond, so this
/// rounds up to the next tick. For short, precise delays use
/// [`time::spin_for_micros`], which busy-waits.
pub fn sleep_micros(micros: u64) -> Sleep {
    sleep(Duration::from_micros(micros))
}

/// Error returned by [`timeout`] when the inner future did not finish in time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Elapsed;

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "timed out")
    }
}

/// Future returned by [`timeout`].
#[derive(Debug)]
#[must_use = "futures do nothing unless awaited"]
pub struct Timeout<F> {
    /// The future being raced against the clock.
    future: F,
    /// Absolute `CNTPCT_EL0` value at which to give up.
    deadline: u64,
}

impl<F: Future> Future for Timeout<F> {
    type Output = Result<F::Output, Elapsed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: structural pinning. `future` is never moved out of `self`,
        // never replaced, and `Timeout` is not `Unpin` unless `F` is.
        let this = unsafe { self.get_unchecked_mut() };

        // SAFETY: as above; the inner future stays put for its whole life.
        let future = unsafe { Pin::new_unchecked(&mut this.future) };

        if let Poll::Ready(value) = future.poll(cx) {
            return Poll::Ready(Ok(value));
        }

        if expired(this.deadline) {
            return Poll::Ready(Err(Elapsed));
        }

        // Ask for a tick even though the inner future may have registered a
        // real waker: without this the timeout could never fire.
        poll_again();
        Poll::Pending
    }
}

/// Runs `future`, giving up after `duration`.
///
/// The inner future is dropped on timeout; that is how cancellation works
/// here.
pub fn timeout<F: Future>(duration: Duration, future: F) -> Timeout<F> {
    Timeout {
        future,
        deadline: deadline_after(duration),
    }
}
