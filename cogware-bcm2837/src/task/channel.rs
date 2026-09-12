// SPDX-License-Identifier: GPL-3.0-only
//! A bounded multi-producer, multi-consumer channel.
//!
//! Tasks on different cores hand work to each other through a
//! `static Channel<T, N>` in `.bss`, shared by reference, with the ring buffer
//! guarded by a [`SpinLock`] held only long enough to move one element.
//!
//! Blocking is done by the async runtime, not the lock. A sender that finds
//! the ring full parks in the `senders` [`TaskSet`]; a receiver that finds it
//! empty parks in `receivers`. Both sets are single `u32`s, so a channel of
//! any capacity costs 8 bytes of queue state.
//!
//! # Example
//!
//! ```ignore
//! static WORK: Channel<u32, 16> = Channel::new();
//!
//! // On core 1
//! WORK.send(42).await;
//!
//! // On core 2
//! let job = WORK.recv().await;
//! ```

use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use super::{TaskSet, current};
use crate::sync::SpinLock;

/// The ring buffer behind a [`Channel`].
struct Ring<T, const N: usize> {
    /// Slots, `None` where empty.
    slots: [Option<T>; N],
    /// Index of the oldest element.
    head: usize,
    /// Number of elements currently queued.
    len: usize,
}

impl<T, const N: usize> Ring<T, N> {
    /// Creates an empty ring.
    const fn new() -> Self {
        Self {
            slots: [const { None }; N],
            head: 0,
            len: 0,
        }
    }

    /// Appends `value`, returning it back if there is no room.
    fn push(&mut self, value: T) -> Result<(), T> {
        if self.len == N {
            return Err(value);
        }

        let index = (self.head + self.len) % N;
        self.slots[index] = Some(value);
        self.len += 1;
        Ok(())
    }

    /// Removes the oldest element.
    fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }

        let value = self.slots[self.head].take();
        self.head = (self.head + 1) % N;
        self.len -= 1;
        value
    }
}

/// A bounded queue of `T` with room for `N` elements.
pub struct Channel<T, const N: usize> {
    /// The queue itself.
    ring: SpinLock<Ring<T, N>>,
    /// Tasks waiting for room to send.
    senders: TaskSet,
    /// Tasks waiting for something to receive.
    receivers: TaskSet,
}

impl<T, const N: usize> Channel<T, N> {
    /// Creates an empty channel.
    ///
    /// # Panics
    ///
    /// Panics at compile time if `N` is zero.
    pub const fn new() -> Self {
        assert!(N > 0, "a channel needs at least one slot");

        Self {
            ring: SpinLock::new(Ring::new()),
            senders: TaskSet::new(),
            receivers: TaskSet::new(),
        }
    }

    /// The channel's capacity.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// The number of queued elements right now.
    pub fn len(&self) -> usize {
        self.ring.with(|ring| ring.len)
    }

    /// Whether the channel is empty right now.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the channel is full right now.
    pub fn is_full(&self) -> bool {
        self.len() == N
    }

    /// Enqueues `value` if there is room, without waiting.
    ///
    /// Returns the value back on failure. Safe to call from anywhere,
    /// including outside a task and from an exception handler.
    pub fn try_send(&self, value: T) -> Result<(), T> {
        let result = self.ring.with(|ring| ring.push(value));

        if result.is_ok() {
            self.receivers.wake_all();
        }

        result
    }

    /// Dequeues an element if one is waiting, without blocking.
    ///
    /// Safe to call from anywhere, including outside a task.
    pub fn try_recv(&self) -> Option<T> {
        let value = self.ring.with(Ring::pop);

        if value.is_some() {
            self.senders.wake_all();
        }

        value
    }

    /// Enqueues `value`, waiting for room if the channel is full.
    pub fn send(&self, value: T) -> Sending<'_, T, N> {
        Sending {
            channel: self,
            value: Some(value),
        }
    }

    /// Dequeues an element, waiting for one if the channel is empty.
    pub fn recv(&self) -> Receiving<'_, T, N> {
        Receiving { channel: self }
    }
}

impl<T, const N: usize> Default for Channel<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Future returned by [`Channel::send`].
#[must_use = "futures do nothing unless awaited"]
pub struct Sending<'a, T, const N: usize> {
    /// The channel being sent to.
    channel: &'a Channel<T, N>,
    /// The value, taken once it is handed over.
    value: Option<T>,
}

impl<T, const N: usize> Future for Sending<'_, T, N> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        // SAFETY: nothing in `Sending` is address-sensitive, so taking a `&mut`
        // out of the `Pin` cannot invalidate a self-reference.
        let this = unsafe { self.get_unchecked_mut() };

        let Some(value) = this.value.take() else {
            // Polled after completion; harmless.
            return Poll::Ready(());
        };

        match this.channel.try_send(value) {
            Ok(()) => return Poll::Ready(()),
            Err(value) => this.value = Some(value),
        }

        // Register before re-checking, or a receiver draining the ring in the
        // gap would leave this task parked with room available.
        this.channel.senders.insert_current();

        if let Some(value) = this.value.take() {
            match this.channel.try_send(value) {
                Ok(()) => {
                    if let Some(task) = current() {
                        this.channel.senders.remove(task);
                    }
                    return Poll::Ready(());
                }
                Err(value) => this.value = Some(value),
            }
        }

        Poll::Pending
    }
}

/// Future returned by [`Channel::recv`].
#[must_use = "futures do nothing unless awaited"]
pub struct Receiving<'a, T, const N: usize> {
    /// The channel being received from.
    channel: &'a Channel<T, N>,
}

impl<T, const N: usize> Future for Receiving<'_, T, N> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<T> {
        if let Some(value) = self.channel.try_recv() {
            return Poll::Ready(value);
        }

        // Register before re-checking — see `Sending::poll`.
        self.channel.receivers.insert_current();

        if let Some(value) = self.channel.try_recv() {
            if let Some(task) = current() {
                self.channel.receivers.remove(task);
            }
            return Poll::Ready(value);
        }

        Poll::Pending
    }
}
