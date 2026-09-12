// SPDX-License-Identifier: GPL-3.0-only
//! Cooperative async runtime.
//!
//! Each core runs its own [`Executor`] over a fixed table of task slots. There
//! is no allocator, no work stealing and no preemption: a task runs until it
//! returns [`Poll::Pending`](core::task::Poll::Pending), and the core moves on
//! to the next ready task or goes to sleep.
//!
//! # Fixed task table
//!
//! With no heap a future cannot be boxed, so every slot owns
//! [`TASK_STORAGE_BYTES`] of inline, 16-byte-aligned storage plus a vtable of
//! `poll`/`drop` function pointers. [`spawn`] moves the future into that
//! storage and refuses, at run time, anything that does not fit.
//!
//! [`TASKS_PER_CORE`] slots on each of the four cores gives at most
//! [`MAX_TASKS`] tasks system-wide, so a set of tasks is a single `u32`
//! bitmask. [`TaskSet`] is that bitmask, and it is what makes the wait queues
//! in [`Mutex`] and [`Channel`] possible without allocating.
//!
//! # Sleeping
//!
//! There is no interrupt controller, so a future waiting on hardware cannot be
//! woken by an IRQ, and re-waking yourself on every poll would turn the idle
//! loop into a busy spin.
//!
//! The executor therefore keeps *two* wake masks:
//!
//! * `ready`, whose tasks are polled immediately, and
//! * `polling`, whose tasks are polled after an idle wake or a timer period
//!   spent processing ready tasks.
//!
//! [`poll_again`] puts the current task in the second. When `ready` empties,
//! the executor promotes `polling` into `ready` and issues `wfe`. The
//! architectural timer's **event stream** (`CNTKCTL_EL1.EVNTEN`, configured by
//! [`crate::arch::cpu::enable_event_stream`]) guarantees that `wfe` returns
//! within one event period, bounding the hardware polling rate while the core
//! sleeps the rest of the time.
//!
//! A real [`Waker::wake`](core::task::Waker::wake) from another core is served
//! immediately: it sets a `ready` bit and issues `sev`.
//!
//! # Ordering rules
//!
//! Everything here uses `LDXR`/`STXR` by way of [`SpinLock`](crate::sync::SpinLock)
//! and the read-modify-write atomics, so **nothing in this module may be
//! touched before the calling core has enabled its MMU**. The executor is only
//! started at the end of a core's bring-up.

// A runtime exposes a complete API — `yield_now`, `timeout`,
// `Mutex::into_inner` and the rest — of which the kernel uses only a part.
#![allow(dead_code)]

mod channel;
mod executor;
mod mutex;
mod timer;
mod waker;
mod yield_now;

// Re-exports form the runtime's public face. `unused_imports` fires on the
// ones the kernel has not called yet, since a binary crate has no downstream
// consumer to make them "used".
#[allow(unused_imports)]
pub use channel::{Channel, Receiving, Sending};
#[allow(unused_imports)]
pub use executor::{Executor, SlotInfo, SpawnError, executor, executor_for, run, spawn, spawn_on};
#[allow(unused_imports)]
pub use mutex::{Lock, Mutex, MutexGuard};
#[allow(unused_imports)]
pub use timer::{Elapsed, Sleep, Timeout, sleep, sleep_micros, sleep_millis, timeout};
#[allow(unused_imports)]
pub use yield_now::{NextTick, YieldNow, next_tick, poll_again, yield_now};

use core::sync::atomic::{AtomicU32, Ordering};

use crate::{arch::cpu, bsp::memory};

/// Task slots available on each core.
///
/// Chosen so that `CORE_COUNT * TASKS_PER_CORE` is exactly 32, which lets
/// [`TaskSet`] be a single `u32`.
pub const TASKS_PER_CORE: usize = 8;

/// Total number of task slots in the system.
pub const MAX_TASKS: usize = memory::CORE_COUNT * TASKS_PER_CORE;

/// Inline storage reserved for each task's future, in bytes.
///
/// An `async fn` compiles to a state machine holding every local live across
/// an `.await`, so nested combinators grow quickly. [`spawn`] reports
/// [`SpawnError::TooLarge`] rather than corrupting anything if a future does
/// not fit.
pub const TASK_STORAGE_BYTES: usize = 1024;

// `TaskSet` is a `u32`, so the whole system must fit in 32 slots.
const _: () = assert!(
    MAX_TASKS <= 32,
    "TaskSet is a u32; reduce TASKS_PER_CORE or widen it"
);

/// Identifies one task slot, system-wide.
///
/// A single small integer, packed into a
/// [`RawWaker`](core::task::RawWaker)'s data pointer, so a waker needs no
/// allocation, reference counting or lifetime.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TaskId(u8);

impl TaskId {
    /// Builds an identifier from a core index and a slot index.
    ///
    /// # Panics
    ///
    /// If either index is out of range.
    pub const fn new(core: usize, slot: usize) -> Self {
        assert!(core < memory::CORE_COUNT, "core index out of range");
        assert!(slot < TASKS_PER_CORE, "slot index out of range");
        Self((core * TASKS_PER_CORE + slot) as u8)
    }

    /// The core that owns this task.
    pub const fn core(self) -> usize {
        self.0 as usize / TASKS_PER_CORE
    }

    /// The slot this task occupies on its core.
    pub const fn slot(self) -> usize {
        self.0 as usize % TASKS_PER_CORE
    }

    /// The system-wide index, in `0..MAX_TASKS`.
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// Rebuilds an identifier from [`index`](Self::index).
    ///
    /// Returns `None` if the value is out of range.
    pub const fn from_index(index: usize) -> Option<Self> {
        if index < MAX_TASKS {
            Some(Self(index as u8))
        } else {
            None
        }
    }
}

/// A set of tasks, one bit per [`TaskId`].
///
/// The kernel's entire wait-queue mechanism. Because the task table is fixed
/// and small, "the set of tasks blocked on this mutex" is a `u32` updated
/// atomically from any core, with no allocation and no intrusive list.
#[derive(Debug)]
pub struct TaskSet(AtomicU32);

impl Default for TaskSet {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskSet {
    /// Creates an empty set.
    pub const fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    /// Adds a task to the set.
    ///
    /// `Release` so that whatever the task did before parking itself — such as
    /// observing a lock as held — is visible to the core that later drains the
    /// set and wakes it.
    pub fn insert(&self, task: TaskId) {
        self.0.fetch_or(1 << task.index(), Ordering::Release);
    }

    /// Adds the currently running task, if there is one.
    ///
    /// The usual way to park: a future calls it just before returning
    /// [`Poll::Pending`](core::task::Poll::Pending).
    pub fn insert_current(&self) {
        if let Some(task) = current() {
            self.insert(task);
        }
    }

    /// Removes a task from the set.
    pub fn remove(&self, task: TaskId) {
        self.0.fetch_and(!(1 << task.index()), Ordering::Relaxed);
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.load(Ordering::Relaxed) == 0
    }

    /// Empties the set and wakes every task that was in it.
    ///
    /// Waking a task that has already made progress is harmless: a spurious
    /// poll is always permitted, and every future in this kernel re-checks its
    /// condition on entry.
    pub fn wake_all(&self) {
        let mut bits = self.0.swap(0, Ordering::Acquire);

        while bits != 0 {
            let index = bits.trailing_zeros() as usize;
            bits &= bits - 1;

            if let Some(task) = TaskId::from_index(index) {
                executor::wake(task);
            }
        }
    }
}

/// The task currently being polled on this core, if any.
///
/// Returns `None` when called from outside a poll — from the boot path, an
/// exception handler, or a core whose executor has not started.
pub fn current() -> Option<TaskId> {
    executor::executor().current()
}

/// Wakes every core parked in `wfe`.
///
/// Needed by code that publishes state a task might be waiting on, such as a
/// future interrupt handler.
pub fn notify_all() {
    cpu::sev();
}
