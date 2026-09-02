// SPDX-License-Identifier: MIT OR Apache-2.0
//! A ticket-free test-and-test-and-set spin lock.
//!
//! # Why this needs the MMU
//!
//! `lock()` compiles to `LDAXR`/`STLXR`. On a Cortex-A53 those are only
//! architecturally guaranteed to work on *Normal, Inner-Shareable, cacheable*
//! memory. With the MMU off every access is Device-nGnRnE and the exclusive
//! monitor's behaviour is UNPREDICTABLE, so **no `SpinLock` may be taken
//! before [`crate::arch::mmu::enable`] has run on the calling core**.
//!
//! Plain atomic loads and stores (`load`, `store`) are *not* affected: they
//! lower to `LDAR`/`STLR`, ordinary single-copy-atomic accesses that are fine
//! with the MMU off.

use core::{
    cell::UnsafeCell,
    hint,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU32, Ordering},
};

/// Sentinel stored in `holder` when the lock is free.
const NO_HOLDER: u32 = u32::MAX;

/// Mutual exclusion primitive for data shared between cores.
pub struct SpinLock<T: ?Sized> {
    /// 0 = free, 1 = held.
    state: AtomicU32,
    /// Core id of the current holder, or [`NO_HOLDER`]. Diagnostics only.
    holder: AtomicU32,
    data: UnsafeCell<T>,
}

// SAFETY: the lock serialises all access to `data`, so `&SpinLock<T>` can be
// shared across cores as long as `T` itself can move between them.
unsafe impl<T: ?Sized + Send> Sync for SpinLock<T> {}
unsafe impl<T: ?Sized + Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Creates a new unlocked `SpinLock`.
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicU32::new(0),
            holder: AtomicU32::new(NO_HOLDER),
            data: UnsafeCell::new(value),
        }
    }

    /// Consumes the lock, returning the wrapped value.
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> SpinLock<T> {
    /// Acquires the lock, spinning until it is available.
    ///
    /// # Panics
    ///
    /// If the calling core already holds the lock: this lock is not reentrant.
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let me = crate::cpu::core_id() as u32;

        assert!(
            self.holder.load(Ordering::Relaxed) != me,
            "SpinLock is not reentrant: core {me} tried to re-acquire a lock it already holds"
        );

        loop {
            // Test-and-test-and-set: the plain load below keeps the cache line
            // shared while spinning instead of ping-ponging exclusive
            // ownership between waiters.
            if self
                .state
                .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                self.holder.store(me, Ordering::Relaxed);
                return SpinLockGuard { lock: self };
            }

            while self.state.load(Ordering::Relaxed) != 0 {
                hint::spin_loop();
            }
        }
    }

    /// Acquires the lock only if it is currently free.
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        if self
            .state
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            self.holder
                .store(crate::cpu::core_id() as u32, Ordering::Relaxed);
            Some(SpinLockGuard { lock: self })
        } else {
            None
        }
    }

    /// Runs `f` with the lock held.
    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut *self.lock())
    }

    /// Whether the lock is currently held by some core.
    pub fn is_locked(&self) -> bool {
        self.state.load(Ordering::Relaxed) != 0
    }

    /// Borrows the protected data without taking the lock.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that no other core can be touching the data.
    /// Two legitimate uses:
    ///
    /// * before the secondary cores have been released, when the MMU may still
    ///   be off and `lock()` would be UNPREDICTABLE;
    /// * from the panic handler, after [`Self::force_unlock`].
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn bypass(&self) -> &mut T {
        unsafe { &mut *self.data.get() }
    }

    /// Releases the lock regardless of who holds it.
    ///
    /// # Safety
    ///
    /// Only for the panic path, to recover a lock held by a core that will
    /// never release it.
    pub unsafe fn force_unlock(&self) {
        self.holder.store(NO_HOLDER, Ordering::Relaxed);
        self.state.store(0, Ordering::Release);
    }
}

/// RAII guard returned by [`SpinLock::lock`].
pub struct SpinLockGuard<'a, T: ?Sized> {
    lock: &'a SpinLock<T>,
}

impl<T: ?Sized> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: holding the guard means holding the lock.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: holding the guard means holding the lock exclusively.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.holder.store(NO_HOLDER, Ordering::Relaxed);
        self.lock.state.store(0, Ordering::Release);
    }
}
