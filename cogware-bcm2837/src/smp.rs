// SPDX-License-Identifier: MIT OR Apache-2.0
//! Symmetric multiprocessing: releasing cores 1-3 and tracking who is up.
//!
//! # Starting the secondary cores
//!
//! The Raspberry Pi firmware and QEMU both park cores 1-3 in a *spin table*: a
//! loop (`armstub8.S` on hardware, QEMU's `write_smpboot64` in emulation) that
//! reads a 64-bit entry point from `SPIN_TABLE_BASE + core_id * 8` and branches
//! to it once the value becomes non-zero.
//!
//! QEMU's version of that loop:
//!
//! ```text
//!     mov  x5, #0xd8
//!     mrs  x6, mpidr_el1
//!     and  x6, x6, #0x3
//! spin:
//!     wfe
//!     ldr  x4, [x5, x6, lsl #3]
//!     cbz  x4, spin
//!     br   x4
//! ```
//!
//! Two consequences:
//!
//! * `wfe` precedes the load, so the release must be followed by `sev` or the
//!   secondary never re-checks the slot;
//! * the secondary reads the slot with its MMU and caches off, so the write
//!   must be pushed past the point of coherency with `dc civac` or, on real
//!   hardware, it stays in core 0's L1 and the secondary reads a stale zero.
//!
//! The address written is `_start`, so the released core re-runs the same
//! boot path, sees a non-zero core id, and takes the secondary branch.
//!
//! # MMU_GATE
//!
//! The spin table says when a core may start running, not when the kernel is
//! ready for it. QEMU's `raspi3b` does not park the secondaries at all: all
//! four cores enter `_start` at reset, so cores 1-3 reach
//! `kernel_boot_secondary` and try to enable their MMU while core 0 is still
//! bringing up the console and the translation tables are still zeroed —
//! every instruction fetch then takes a translation fault.
//!
//! `MMU_GATE` blocks a secondary until core 0 has published the translation
//! tables, regardless of how the core was started. It is built from plain
//! atomic loads and stores, which lower to `LDAR`/`STLR` and are defined on
//! Device memory — unlike the `LDXR`/`STXR` pair behind
//! [`crate::sync::SpinLock`], which a core with its MMU off may not use.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::{
    arch::{cpu, mmu},
    bsp::memory,
    time,
};

unsafe extern "C" {
    /// Kernel reset entry point, defined in `arch/boot.s`.
    fn _start();
}

/// Number of cores that have finished their own bring-up.
///
/// Core 0 counts itself, so this reaches [`memory::CORE_COUNT`] when the whole
/// SoC is up.
static ONLINE: AtomicUsize = AtomicUsize::new(0);

/// Value [`open_mmu_gate`] writes to `MMU_GATE`.
///
/// A recognisable non-zero pattern rather than 1, so a secondary spinning on
/// uninitialised or corrupted memory cannot be let through by accident.
const GATE_OPEN: u64 = 0x5747_4154_454F_4B21; // "WGATEOK!"

/// Set by core 0 once the translation tables are valid and the console is
/// under lock discipline. Secondary cores must not enable their MMU until they
/// have seen it.
static MMU_GATE: AtomicU64 = AtomicU64::new(0);

/// Blocks the calling secondary core until core 0 has published the
/// translation tables.
///
/// Safe to call with the MMU off: it only uses `LDAR`.
pub fn wait_for_mmu_gate() {
    while MMU_GATE.load(Ordering::Acquire) != GATE_OPEN {
        core::hint::spin_loop();
    }
}

/// Whether the gate has been opened.
pub fn mmu_gate_open() -> bool {
    MMU_GATE.load(Ordering::Acquire) == GATE_OPEN
}

/// Publishes the translation tables to the secondary cores.
///
/// Core 0's data cache is on by this point, so the store must be pushed to the
/// point of coherency: the waiting cores read with their caches off.
///
/// # Safety
///
/// Core 0 only, after [`crate::arch::mmu::init`] and after core 0 has switched
/// the console over to lock-based access.
unsafe fn open_mmu_gate() {
    MMU_GATE.store(GATE_OPEN, Ordering::Release);

    // SAFETY: `MMU_GATE` lives in `.bss`, which is identity mapped.
    unsafe { mmu::clean_invalidate_line(core::ptr::addr_of!(MMU_GATE) as usize) };

    cpu::dsb_sy();
    cpu::sev();
}

/// Announces that the calling core has completed bring-up.
///
/// # Safety
///
/// The caller's MMU must already be enabled: this uses a read-modify-write
/// atomic, which needs the exclusive monitor.
pub fn mark_online() {
    debug_assert!(
        mmu::is_enabled(),
        "mark_online needs the MMU for LDXR/STXR to be defined"
    );
    ONLINE.fetch_add(1, Ordering::AcqRel);
}

/// How many cores have reported in.
pub fn online_count() -> usize {
    ONLINE.load(Ordering::Acquire)
}

/// Address of a core's spin-table slot.
const fn spin_slot(core_id: usize) -> *mut u64 {
    (memory::SPIN_TABLE_BASE + core_id * 8) as *mut u64
}

/// Brings cores 1..[`memory::CORE_COUNT`] into the kernel.
///
/// Two independent releases, since either can be holding a secondary back:
///
/// 1. the firmware spin table, on real hardware, where the core has not
///    executed a single kernel instruction yet;
/// 2. `MMU_GATE`, where the core is already inside `kernel_boot_secondary`
///    waiting for the translation tables (the QEMU case).
///
/// # Safety
///
/// Core 0 only, exactly once, after the console and the MMU are up. The
/// secondary cores begin executing kernel code immediately, so every structure
/// they touch must already be initialised.
pub unsafe fn start_secondaries() {
    let entry = _start as *const () as u64;

    for core_id in 1..memory::CORE_COUNT {
        let slot = spin_slot(core_id);

        // SAFETY: 0xd8..0xf8 is inside the identity-mapped low window and is
        // owned by the firmware stub, which is finished with it by now.
        unsafe {
            slot.write_volatile(entry);
            // The target core still runs with its caches off, so push the
            // release past the point of coherency.
            mmu::clean_invalidate_line(slot as usize);
        }
    }

    cpu::dsb_sy();
    // The stub's `wfe` precedes its load, so without this it never looks again.
    cpu::sev();

    // SAFETY: the tables are built and core 0 is using the console under lock.
    unsafe { open_mmu_gate() };
}

/// Waits until `expected` cores are online, or `timeout_micros` elapses.
///
/// Returns the number of cores online when the wait ended.
///
/// "Online" means the BSP has finished bringing the core up and is about to
/// call the secondary entry point — **not** that user code has run. To observe
/// something a secondary produces, synchronise on it directly with a
/// [`Barrier`](crate::sync::Barrier) or an atomic of your own.
pub fn wait_for_cores(expected: usize, timeout_micros: u64) -> usize {
    let frequency = time::frequency_hz();
    let deadline_ticks = timeout_micros.saturating_mul(frequency) / 1_000_000;
    let start = time::ticks();

    loop {
        let online = online_count();
        if online >= expected {
            return online;
        }

        if time::ticks().wrapping_sub(start) >= deadline_ticks {
            return online;
        }

        core::hint::spin_loop();
    }
}

/// Current contents of a core's spin-table slot. Diagnostics only.
pub fn spin_slot_value(core_id: usize) -> u64 {
    // SAFETY: the slot is mapped and 8-byte aligned.
    unsafe { spin_slot(core_id).read_volatile() }
}
