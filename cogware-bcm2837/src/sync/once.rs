// SPDX-License-Identifier: GPL-3.0-only
//! One-shot initialisation guard, safe to race on from several cores.

use core::{
    cell::UnsafeCell,
    hint,
    mem::MaybeUninit,
    sync::atomic::{AtomicU32, Ordering},
};

const UNINITIALISED: u32 = 0;
const INITIALISING: u32 = 1;
const READY: u32 = 2;

/// A value initialised exactly once, by whichever core gets there first.
///
/// Like [`crate::sync::SpinLock`], this relies on a compare-exchange and must
/// not be used before the MMU is enabled on the calling core.
pub struct Once<T> {
    state: AtomicU32,
    value: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: `get` only hands out `&T` once `state == READY`, which is published
// with a release store and observed with an acquire load.
unsafe impl<T: Send + Sync> Sync for Once<T> {}
unsafe impl<T: Send> Send for Once<T> {}

impl<T> Once<T> {
    /// Creates an empty `Once`.
    pub const fn new() -> Self {
        Self {
            state: AtomicU32::new(UNINITIALISED),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Returns the stored value, running `init` first if nobody has yet.
    ///
    /// `init` runs at most once across all cores; losers of the race spin until
    /// the winner publishes the value.
    pub fn get_or_init(&self, init: impl FnOnce() -> T) -> &T {
        if self.state.load(Ordering::Acquire) == READY {
            // SAFETY: READY is only published after the value was written.
            return unsafe { (*self.value.get()).assume_init_ref() };
        }

        match self.state.compare_exchange(
            UNINITIALISED,
            INITIALISING,
            Ordering::Acquire,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // SAFETY: we won the race, so we have exclusive access.
                unsafe { (*self.value.get()).write(init()) };
                self.state.store(READY, Ordering::Release);
            }
            Err(_) => {
                while self.state.load(Ordering::Acquire) != READY {
                    hint::spin_loop();
                }
            }
        }

        // SAFETY: the value is initialised and published by this point.
        unsafe { (*self.value.get()).assume_init_ref() }
    }

    /// Returns the stored value, or `None` if it has not been initialised.
    pub fn get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) == READY {
            // SAFETY: as above.
            Some(unsafe { (*self.value.get()).assume_init_ref() })
        } else {
            None
        }
    }
}

impl<T> Default for Once<T> {
    fn default() -> Self {
        Self::new()
    }
}
