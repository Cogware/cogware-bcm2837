// SPDX-License-Identifier: GPL-3.0-only
//! An async mutex.
//!
//! [`SpinLock`](crate::sync::SpinLock) suits a critical section of a handful
//! of instructions. It cannot be held across an `.await`: a task holding a
//! spin lock cannot yield.
//!
//! This [`Mutex`] parks the waiting task instead. Because the task table is
//! fixed, the wait queue is a [`TaskSet`] — a single `u32` — so there is no
//! allocation and no intrusive list.
//!
//! # Which lock to use
//!
//! | Situation | Use |
//! | --- | --- |
//! | A few instructions, no `.await` | [`SpinLock`](crate::sync::SpinLock) |
//! | Held across an `.await` | [`Mutex`] |
//! | Called from an exception handler | [`SpinLock`] with `try_lock` |
//!
//! The console keeps its `SpinLock`: it is taken and released inside a single
//! `write_fmt`, and a fault handler must be able to reach it.

use core::{
    cell::UnsafeCell,
    future::Future,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll},
};

use super::{TaskSet, current};

/// A mutual-exclusion lock that yields rather than spins.
pub struct Mutex<T: ?Sized> {
    /// `true` while a guard is outstanding.
    locked: AtomicBool,
    /// Tasks parked waiting for the lock.
    waiters: TaskSet,
    /// The protected value.
    data: UnsafeCell<T>,
}

// SAFETY: the lock serialises every access to `data`, so a shared reference to
// the `Mutex` may cross cores provided the value itself may.
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}
// SAFETY: as above.
unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}

impl<T> Mutex<T> {
    /// Creates a new unlocked mutex.
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            waiters: TaskSet::new(),
            data: UnsafeCell::new(value),
        }
    }

    /// Consumes the mutex, returning the wrapped value.
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Takes the lock if it is free, without waiting.
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| MutexGuard {
                mutex: self,
                _borrow: PhantomData,
            })
    }

    /// Whether the lock is currently held.
    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    /// Waits for the lock.
    ///
    /// The returned future parks the task in the mutex's wait set; the task is
    /// woken when the guard is dropped.
    pub fn lock(&self) -> Lock<'_, T> {
        Lock { mutex: self }
    }
}

/// Future returned by [`Mutex::lock`].
#[must_use = "futures do nothing unless awaited"]
pub struct Lock<'a, T: ?Sized> {
    /// The mutex being waited on.
    mutex: &'a Mutex<T>,
}

impl<'a, T: ?Sized> Future for Lock<'a, T> {
    type Output = MutexGuard<'a, T>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(guard) = self.mutex.try_lock() {
            return Poll::Ready(guard);
        }

        // Register *before* the final check, or the holder could release
        // between a failed `try_lock` and the insertion, find the wait set
        // empty, and leave this task parked forever.
        self.mutex.waiters.insert_current();

        if let Some(guard) = self.mutex.try_lock() {
            if let Some(task) = current() {
                self.mutex.waiters.remove(task);
            }
            return Poll::Ready(guard);
        }

        Poll::Pending
    }
}

/// Proof that the lock is held, and the only way to reach the value.
pub struct MutexGuard<'a, T: ?Sized> {
    /// The mutex this guard unlocks on drop.
    mutex: &'a Mutex<T>,
    // Sharing the guard shares &T, not just the mutex: it requires T: Sync.
    _borrow: PhantomData<&'a mut T>,
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the guard's existence proves exclusive access.
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above, and `&mut self` rules out aliasing guards.
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        // Release first, so a woken task finds the lock actually free.
        self.mutex.locked.store(false, Ordering::Release);
        self.mutex.waiters.wake_all();
    }
}
