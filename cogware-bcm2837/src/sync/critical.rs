// SPDX-License-Identifier: GPL-3.0-only
//! The `critical-section` implementation, for crates that need one.
//!
//! `critical-section` is the ecosystem's answer to "how does a `no_std`
//! library protect a global without knowing what machine it is on". The crate
//! ships no implementation deliberately; the binary supplies one. A kernel
//! that links such a library would otherwise have to write this itself, and
//! writing it correctly needs the two facts below, which belong here rather
//! than in every downstream crate.
//!
//! # Both halves are required
//!
//! Masking exceptions stops *this* core re-entering the section. It says
//! nothing whatever to another core. A kernel that has released the
//! secondaries and protects a global with the mask alone has no protection at
//! all, and the failure is a torn read rather than a hang — so it looks like
//! bad data, not like a lock bug. The lock below is what makes the section
//! mean something across cores; the mask is what makes it mean something
//! within one.
//!
//! # Nesting is ordinary
//!
//! A caller reading two protected globals in one expression nests two
//! sections, so the lock counts depth per core rather than deadlocking
//! against itself. Only the outermost release hands it back.
//!
//! # Before the MMU
//!
//! `LDXR`/`STXR` are architecturally UNPREDICTABLE on Device memory, which is
//! what all memory is until `arch::mmu::enable` has run — so the
//! compare-exchange here is illegal that early. It is also unnecessary: core 0
//! is alone until `smp::start_secondaries`. The mask alone is used while the
//! MMU is off, exactly as `console` does.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::arch::{cpu, mmu};
use crate::bsp::memory::CORE_COUNT;

/// No core is inside the section.
const NO_OWNER: usize = usize::MAX;

/// Which core holds the section, or [`NO_OWNER`].
static OWNER: AtomicUsize = AtomicUsize::new(NO_OWNER);

/// How deep the owning core has nested. Only the owner touches its own slot,
/// and only while it owns the lock, so `Relaxed` is the whole requirement.
static DEPTH: [AtomicU32; CORE_COUNT] = [const { AtomicU32::new(0) }; CORE_COUNT];

/// The implementation, named so that the error from a second one in the same
/// binary points at something.
struct MaskAndLock;

critical_section::set_impl!(MaskAndLock);

// SAFETY: `acquire` masks every exception that could re-enter this core and,
// once the MMU makes exclusives legal, excludes every other core as well.
// `release` undoes exactly what the matching `acquire` did, in the reverse
// order: the lock is handed back first, then the mask lifted. Lifting it
// first would let an exception on this core nest onto a section this core
// has already left, and the depth it counted would never come back down.
unsafe impl critical_section::Impl for MaskAndLock {
    unsafe fn acquire() -> critical_section::RawRestoreState {
        let saved = cpu::save_and_mask_interrupts().to_raw();
        if !mmu::is_enabled() {
            return saved;
        }

        let me = cpu::core_id();
        if OWNER.load(Ordering::Relaxed) == me {
            DEPTH[me].fetch_add(1, Ordering::Relaxed);
            return saved;
        }

        while OWNER
            .compare_exchange_weak(NO_OWNER, me, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // The holder has exceptions masked and cannot be preempted, so it
            // is running and about to release. `wfe` would need a matching
            // `sev` on the release path, which would cost every uncontended
            // section an event.
            cpu::spin_cycles(16);
        }
        DEPTH[me].store(1, Ordering::Relaxed);
        saved
    }

    unsafe fn release(saved: critical_section::RawRestoreState) {
        if mmu::is_enabled() {
            let me = cpu::core_id();
            if DEPTH[me].fetch_sub(1, Ordering::Relaxed) == 1 {
                OWNER.store(NO_OWNER, Ordering::Release);
            }
        }
        // SAFETY: `saved` comes from the matching `acquire` on this core,
        // which is the only way a caller can have obtained it.
        unsafe { cpu::restore_interrupts(cpu::InterruptState::from_raw(saved)) };
    }
}
