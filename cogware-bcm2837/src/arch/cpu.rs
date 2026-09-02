// SPDX-License-Identifier: MIT OR Apache-2.0
//! Core-local AArch64 state: identity, exception level, barriers and the
//! handful of system registers the rest of the kernel needs.

use core::arch::asm;

/// Reads a 64-bit system register.
macro_rules! read_sysreg {
    ($reg:literal) => {{
        let value: u64;
        // SAFETY: `mrs` into a scratch register has no side effects for any of
        // the registers this macro is used with. `unused_unsafe` is allowed
        // because the macro also expands inside `unsafe` blocks that exist for
        // the neighbouring `write_sysreg!`.
        #[allow(unused_unsafe)]
        unsafe {
            ::core::arch::asm!(
                concat!("mrs {value}, ", $reg),
                value = out(reg) value,
                options(nostack, preserves_flags),
            );
        }
        value
    }};
}

/// Writes a 64-bit system register followed by an `isb`.
macro_rules! write_sysreg {
    ($reg:literal, $value:expr) => {{
        let value: u64 = $value;
        ::core::arch::asm!(
            concat!("msr ", $reg, ", {value}"),
            "isb",
            value = in(reg) value,
            options(nostack, preserves_flags),
        );
    }};
}

pub(crate) use {read_sysreg, write_sysreg};

/// Bits of `MPIDR_EL1.Aff0` that identify a core on the BCM2837.
///
/// `Aff1` (cluster) is always zero on this SoC; the firmware stub and QEMU's
/// SMP trampoline both mask with `0b11`.
const CORE_ID_MASK: u64 = 0b11;

/// Index of the core executing this code, 0..=3.
#[inline(always)]
pub fn core_id() -> usize {
    (read_sysreg!("MPIDR_EL1") & CORE_ID_MASK) as usize
}

/// Raw `MPIDR_EL1`, useful for diagnostics.
#[inline(always)]
pub fn mpidr() -> u64 {
    read_sysreg!("MPIDR_EL1")
}

/// The exception level the calling code runs at (1, 2 or 3).
#[inline(always)]
pub fn current_el() -> u8 {
    ((read_sysreg!("CurrentEL") >> 2) & 0b11) as u8
}

/// Raw `SCTLR_EL1`.
#[inline(always)]
pub fn sctlr_el1() -> u64 {
    read_sysreg!("SCTLR_EL1")
}

/// Current stack pointer.
#[inline(always)]
pub fn stack_pointer() -> usize {
    let value: usize;
    // SAFETY: reading SP into a scratch register is side-effect free.
    unsafe {
        asm!("mov {value}, sp", value = out(reg) value, options(nomem, nostack, preserves_flags))
    };
    value
}

/// Instruction synchronisation barrier.
#[inline(always)]
pub fn isb() {
    // SAFETY: barrier instructions take no operands.
    unsafe { asm!("isb", options(nostack, preserves_flags)) };
}

/// Full-system data synchronisation barrier.
#[inline(always)]
pub fn dsb_sy() {
    // SAFETY: as above.
    unsafe { asm!("dsb sy", options(nostack, preserves_flags)) };
}

/// Full-system data memory barrier.
#[inline(always)]
pub fn dmb_sy() {
    // SAFETY: as above.
    unsafe { asm!("dmb sy", options(nostack, preserves_flags)) };
}

/// Signals an event to every core, waking those parked in `wfe`.
#[inline(always)]
pub fn sev() {
    // SAFETY: as above.
    unsafe { asm!("sev", options(nostack, preserves_flags)) };
}

/// Waits for an event. Returns spuriously; always re-check the condition.
#[inline(always)]
pub fn wfe() {
    // SAFETY: as above.
    unsafe { asm!("wfe", options(nostack, preserves_flags)) };
}

/// Waits for an interrupt.
#[inline(always)]
pub fn wfi() {
    // SAFETY: as above.
    unsafe { asm!("wfi", options(nostack, preserves_flags)) };
}

/// Bit of the virtual counter that drives the event stream.
///
/// The stream fires once per 0→1 transition of the selected bit, i.e. once
/// every `2^(bit + 1)` ticks. Bit 15 gives a period of 65 536 ticks: about
/// 1.05 ms under QEMU's 62.5 MHz counter and 3.41 ms on real hardware's
/// 19.2 MHz one.
const EVENT_STREAM_BIT: u64 = 15;

/// How many counter ticks pass between two event-stream events.
pub const EVENT_STREAM_PERIOD_TICKS: u64 = 1 << (EVENT_STREAM_BIT + 1);

/// `CNTKCTL_EL1.EVNTEN`, the event-stream enable.
const CNTKCTL_EVNTEN: u64 = 1 << 2;

/// Turns on the architectural timer's event stream for this core.
///
/// Without it `wfe` has no bounded wake-up: a core would sleep until some other
/// core issued `sev`. With it every core wakes at a fixed low rate, which lets
/// the async executor's [`poll_again`](crate::task::poll_again) path sleep
/// instead of spinning, with no interrupt controller.
///
/// Idempotent, and must be called on every core that runs an executor.
pub fn enable_event_stream() {
    // EVNTEN (bit 2) turns the stream on; EVNTDIR (bit 3) clear selects 0→1
    // transitions; EVNTI (bits 7:4) picks the counter bit. Everything else in
    // CNTKCTL_EL1 — the EL0 access controls — is preserved.
    let value = read_sysreg!("CNTKCTL_EL1");
    let value = (value & !0xFF) | (EVENT_STREAM_BIT << 4) | CNTKCTL_EVNTEN;

    // SAFETY: CNTKCTL_EL1 is writable at EL1 and this only enables a wake-up
    // source; no other state depends on the bits being cleared.
    unsafe { write_sysreg!("CNTKCTL_EL1", value) };
}

/// Whether this core's timer event stream is running.
///
/// Read back from `CNTKCTL_EL1` rather than cached, so an implementation that
/// ignores the write — leaving `wfe` with no bounded wake-up — is reported
/// accurately. The executor uses this to decide whether it may sleep.
pub fn event_stream_enabled() -> bool {
    read_sysreg!("CNTKCTL_EL1") & CNTKCTL_EVNTEN != 0
}

/// Burns roughly `count` core clock cycles.
///
/// Used for the GPIO pull-up/pull-down settling delays, which are specified in
/// core cycles rather than wall-clock time.
#[inline(always)]
pub fn spin_cycles(count: usize) {
    for _ in 0..count {
        // SAFETY: `nop` cannot fault and the asm block stops LLVM from
        // collapsing the loop.
        unsafe { asm!("nop", options(nomem, nostack, preserves_flags)) };
    }
}

/// Parks the calling core forever.
pub fn halt() -> ! {
    loop {
        wfe();
    }
}

/// Masks IRQ, FIQ, SError and debug exceptions on the calling core.
#[inline(always)]
pub fn mask_interrupts() {
    // SAFETY: `msr DAIFSet` only affects the current core's mask bits.
    unsafe { asm!("msr DAIFSet, #0b1111", options(nostack, preserves_flags)) };
}

/// Unmasks IRQ, FIQ, SError and debug exceptions on the calling core.
///
/// # Safety
///
/// The vector table must already be installed via
/// [`crate::arch::exception::install_vector_table`].
#[inline(always)]
pub unsafe fn unmask_interrupts() {
    unsafe { asm!("msr DAIFClr, #0b1111", options(nostack, preserves_flags)) };
}

/// Saved interrupt mask, restored by [`restore_interrupts`].
#[derive(Clone, Copy)]
pub struct InterruptState(u64);

/// Masks interrupts and returns the previous mask so it can be restored.
#[inline(always)]
pub fn save_and_mask_interrupts() -> InterruptState {
    let daif = read_sysreg!("DAIF");
    mask_interrupts();
    InterruptState(daif)
}

/// Restores a mask captured by [`save_and_mask_interrupts`].
///
/// # Safety
///
/// `state` must come from a matching [`save_and_mask_interrupts`] on this core.
#[inline(always)]
pub unsafe fn restore_interrupts(state: InterruptState) {
    unsafe { write_sysreg!("DAIF", state.0) };
}

/// Enables the Cortex-A53 SMP coherency bit (`CPUECTLR_EL1.SMPEN`).
///
/// Without it the core neither joins the coherency domain nor receives
/// broadcast cache/TLB maintenance, so shared data silently goes stale. Must be
/// set before the data cache is enabled.
///
/// The boot assembly already does this; this function re-asserts it from Rust.
///
/// # Safety
///
/// Must run before [`crate::arch::mmu::enable`] on the same core.
pub unsafe fn enable_smp_coherency() {
    const SMPEN: u64 = 1 << 6;
    let value = read_sysreg!("S3_1_C15_C2_1");
    unsafe { write_sysreg!("S3_1_C15_C2_1", value | SMPEN) };
}

/// Whether FP/SIMD access is currently ungated at EL1 (`CPACR_EL1.FPEN`).
pub fn fpu_enabled() -> bool {
    const FPEN: u64 = 0b11 << 20;
    read_sysreg!("CPACR_EL1") & FPEN == FPEN
}
