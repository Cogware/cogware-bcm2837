// SPDX-License-Identifier: GPL-3.0-only
//! A sense-reversing barrier for rendezvousing all cores.
//!
//! Like [`crate::sync::SpinLock`], this relies on read-modify-write atomics,
//! so every participating core must already have its MMU enabled.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::cpu;

/// Blocks each arriving core until `total` cores have arrived, then releases
/// them all and resets for the next round.
pub struct Barrier {
    /// Cores that have arrived in the current round.
    arrived: AtomicUsize,
    /// Bumped once per completed round. The "sense" that waiters watch.
    generation: AtomicUsize,
    /// How many cores make a round.
    total: usize,
}

impl Barrier {
    /// Creates a barrier that releases once `total` cores have arrived.
    ///
    /// # Panics
    ///
    /// If `total` is zero, which would never release.
    pub const fn new(total: usize) -> Self {
        assert!(total > 0, "a barrier needs at least one participant");
        Self {
            arrived: AtomicUsize::new(0),
            generation: AtomicUsize::new(0),
            total,
        }
    }

    /// Waits for every other participant to arrive.
    ///
    /// The generation is read *before* incrementing, which makes the barrier
    /// reusable: the last arrival resets the counter and bumps the generation,
    /// and waiters spin only until the generation changes, so a core that
    /// re-enters immediately cannot be counted into the round it just left.
    pub fn wait(&self) {
        let generation = self.generation.load(Ordering::Acquire);

        if self.arrived.fetch_add(1, Ordering::AcqRel) + 1 == self.total {
            self.arrived.store(0, Ordering::Release);
            self.generation.fetch_add(1, Ordering::AcqRel);
            cpu::sev();
            return;
        }

        while self.generation.load(Ordering::Acquire) == generation {
            core::hint::spin_loop();
        }
    }

    /// Number of cores currently waiting.
    pub fn waiting(&self) -> usize {
        self.arrived.load(Ordering::Acquire)
    }

    /// How many rounds have completed.
    pub fn rounds(&self) -> usize {
        self.generation.load(Ordering::Acquire)
    }
}
