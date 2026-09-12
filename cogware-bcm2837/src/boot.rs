// SPDX-License-Identifier: GPL-3.0-only
//! Boot machinery and the kernel entry point.
//!
//! Owns the two symbols the boot assembly branches to. Performs the fixed part
//! of bring-up — console, memory probe, MMU, caches, exception vectors, heap,
//! JTAG — then calls into the application with a [`BootInfo`] describing what
//! it found.
//!
//! The application supplies its entry points through the [`entry!`](crate::entry) macro, a
//! plain `macro_rules!` (an attribute macro would require a proc-macro crate,
//! and this BSP has no dependencies) that defines two `#[unsafe(no_mangle)]`
//! shims which this module declares as `extern` and calls.
//!
//! # Order of operations
//!
//! On core 0:
//!
//! 1. **Console** — so that everything after it can report its own failure.
//! 2. **VideoCore memory probe** — must run with the MMU *off*, so the mailbox
//!    buffer is already uncached as the GPU requires. Its answer decides where
//!    the non-cacheable window goes. A recognised chain-load descriptor also
//!    requires a confirmed ARM memory range before its park page is reserved.
//! 3. **MMU** — needs the probe's answer.
//! 4. **`init_core`** — enables the MMU and caches and installs the vector
//!    table. Locks are illegal before this point: the exclusive monitor needs
//!    Normal memory.
//! 5. **`mark_online`** — the first thing that takes a lock.
//! 6. **Heap** — publish arena bounds and the optional validated park page.
//! 7. **JTAG** — claims GPIO 22-27, so it needs the GPIO lock.
//!
//! Secondary cores skip 1, 2, 3, 6 and 7's pin routing: core 0 did those once,
//! for everyone.

use crate::{
    arch::{self, cpu, exception, mmu},
    bsp::memory,
    console,
    drivers::mailbox,
    smp,
};

/// What the BSP observed while bringing the machine up.
///
/// Handed to the application's entry point. Every field is read back from the
/// hardware *after* the corresponding step ran, not a value the BSP intended
/// to write.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct BootInfo {
    /// Which core is running, 0..=3, from `MPIDR_EL1`.
    pub core_id: usize,
    /// Exception level, read from `CurrentEL`. Expected to be 1: the boot
    /// assembly drops from EL2 before any Rust runs.
    pub exception_level: u8,
    /// Raw `MPIDR_EL1`, for logging the full affinity.
    pub mpidr: u64,
    /// Whether FP/SIMD is ungated, from `CPACR_EL1`.
    ///
    /// False would mean any `f64` in the application traps.
    pub fpu_enabled: bool,
    /// Physical address of the stage-1 root translation table.
    pub translation_table: usize,
    /// Whether the MMU is on, from `SCTLR_EL1.M`.
    pub mmu_enabled: bool,
    /// Whether the data cache is on, from `SCTLR_EL1.C`.
    pub data_cache_enabled: bool,
    /// Whether the instruction cache is on, from `SCTLR_EL1.I`.
    pub instruction_cache_enabled: bool,
    /// `VBAR_EL1`, the base of the installed exception vector table.
    pub vector_table: usize,
    /// The VideoCore's DRAM reservation as `(base, size)` in bytes, or the
    /// error that stopped the probe.
    ///
    /// Only meaningful on core 0; secondaries get [`Err`]`(`[`Skipped`]`)`,
    /// since the probe runs once, before they exist.
    ///
    /// [`Skipped`]: ProbeError::Skipped
    pub videocore_memory: Result<(usize, usize), ProbeError>,
    /// What the ARM cores are clocked at, and what they could be.
    ///
    /// Read, never set. See [`ClockRate`] for why the BSP reports this rather
    /// than acting on it.
    ///
    /// Only meaningful on core 0; secondaries get [`Err`]`(`[`Skipped`]`)`.
    ///
    /// [`Skipped`]: ProbeError::Skipped
    pub arm_clock: Result<ClockRate, ProbeError>,
}

/// The ARM core clock, as the firmware reports it.
///
/// # Why this is only reported
///
/// The firmware boots the cores at their *minimum* rate — 600 MHz of a possible
/// 1200 on a Pi 3 — and leaves them there. On Linux the cpufreq governor asks
/// for more within a second of boot and nobody notices; a bare-metal kernel
/// that never asks runs at half speed forever, and nothing in the machine state
/// says so.
///
/// Raising it is [`Mailbox::set_clock_rate`], and it stays the application's
/// call: a rate change costs boot time, has thermal and power consequences the
/// BSP cannot judge, and is exactly the kind of policy that does not belong in
/// a boot path the application cannot skip. What the BSP owes is the fact,
/// which is both numbers side by side.
///
/// [`Mailbox::set_clock_rate`]: crate::drivers::mailbox::Mailbox::set_clock_rate
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockRate {
    /// The rate the cores are running at now, in hertz.
    pub hz: u32,
    /// The highest rate the firmware will grant, in hertz. Already accounts for
    /// `config.txt`, the board revision and the firmware's own thermal and
    /// voltage headroom.
    pub max_hz: u32,
}

impl ClockRate {
    /// True when the cores are already at the firmware's ceiling.
    pub fn at_maximum(&self) -> bool {
        self.hz >= self.max_hz
    }
}

/// Why the VideoCore memory probe did not produce an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeError {
    /// The mailbox transaction failed. The BSP fell back to the compiled-in
    /// default split, so the map is still usable.
    Mailbox(mailbox::Error),
    /// The probe was not attempted on this core. Core 0 runs it once, with the
    /// MMU off, before the secondaries are released.
    Skipped,
}

impl core::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Mailbox(error) => write!(f, "{error}"),
            Self::Skipped => write!(f, "not probed on this core"),
        }
    }
}

impl BootInfo {
    /// Reads back the parts of the machine state that every core shares.
    fn observe(
        core_id: usize,
        videocore_memory: Result<(usize, usize), ProbeError>,
        arm_clock: Result<ClockRate, ProbeError>,
    ) -> Self {
        Self {
            core_id,
            exception_level: cpu::current_el(),
            mpidr: cpu::mpidr(),
            fpu_enabled: cpu::fpu_enabled(),
            translation_table: mmu::root_table_address(),
            mmu_enabled: mmu::is_enabled(),
            data_cache_enabled: mmu::data_cache_enabled(),
            instruction_cache_enabled: mmu::instruction_cache_enabled(),
            vector_table: exception::vector_table_address(),
            videocore_memory,
            arm_clock,
        }
    }

    /// True when every machine feature the BSP is responsible for came up.
    ///
    /// Excludes the VideoCore probe, which fails on hardware without a GPU
    /// reservation while the kernel remains healthy.
    pub fn healthy(&self) -> bool {
        self.exception_level == 1
            && self.fpu_enabled
            && self.mmu_enabled
            && self.data_cache_enabled
            && self.instruction_cache_enabled
            && self.vector_table != 0
    }
}

// The application's entry points, defined by `entry!`.
//
// `extern "Rust"` rather than `extern "C"`: both sides are compiled together
// from the same crate graph, and the Rust ABI passes `BootInfo` without the
// `improper_ctypes_definitions` warning a `#[repr(Rust)]` struct would attract
// over a C boundary.
unsafe extern "Rust" {
    #[link_name = "__cogware_main_primary"]
    fn main_primary(info: BootInfo) -> !;

    #[link_name = "__cogware_main_secondary"]
    fn main_secondary(info: BootInfo) -> !;
}

/// Entry point for core 0, tail-called from `_start`.
///
/// # Safety
///
/// Called exactly once, by the boot assembly, at EL1 with a valid stack, the
/// FPU ungated and `.bss` zeroed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kernel_boot_primary(
    core_id: usize,
    handover_magic: usize,
    handover_base: usize,
    handover_length: usize,
) -> ! {
    // Console first: everything after this can report its own failures.
    //
    // SAFETY: core 0, MMU off, no other core running.
    unsafe { console::init() };
    boot_trace(b'9');

    // Locate the GPU's DRAM reservation before the map is built, so the
    // VideoCore range can be mapped non-cacheable. Must run with the MMU off:
    // the mailbox buffer is then already uncached, as the VideoCore requires,
    // and no cache maintenance is needed.
    //
    // SAFETY: core 0, MMU off, no secondary core running.
    let videocore_memory = unsafe { mailbox::probe_memory() }.map_err(ProbeError::Mailbox);
    let reservation = if handover_magic == memory::CHAINLOAD_MAGIC {
        // SAFETY: core 0, MMU off, and no secondary has entered the payload.
        // The descriptor is never dereferenced: only firmware-confirmed bounds
        // determine whether the page may be withheld from this kernel's heap.
        let arm_memory = unsafe { crate::drivers::MAILBOX.bypass().arm_memory() };
        let validated = match (arm_memory, videocore_memory) {
            (Ok((base, size)), Ok(_)) if memory::vc_mem_probed() => memory::validate_chainload(
                handover_magic,
                handover_base,
                handover_length,
                base,
                size,
                memory::vc_mem_base(),
            ),
            _ => Err("firmware could not confirm chain-load reservation bounds"),
        };
        match validated {
            Ok(reservation) => reservation,
            Err(error) => {
                crate::println!("Invalid chain-load handover: {error}; heap remains disabled.");
                console::flush();
                cpu::halt();
            }
        }
    } else {
        None
    };
    // SAFETY: the same window as the probe above — core 0, MMU still off, no
    // secondary running — which is what makes the unlocked mailbox sound.
    let arm_clock = unsafe { mailbox::probe_clock() }
        .map(|(hz, max_hz)| ClockRate { hz, max_hz })
        .map_err(ProbeError::Mailbox);
    boot_trace(b'a');

    // SAFETY: core 0, before any secondary core exists.
    unsafe { mmu::init() };
    boot_trace(b'b');

    // SAFETY: tables are built; the mapping is an identity map so PC and SP
    // stay valid across the switch.
    unsafe { arch::init_core() };
    boot_trace(b'c');

    // Locks are legal from here on.
    smp::mark_online();

    // Only metadata changes: the arena is already mapped DRAM, with nothing
    // to zero. Publish any park-page exclusion before enabling allocations.
    //
    // SAFETY: core 0, MMU on, mailbox already probed, no secondary running.
    unsafe { crate::heap::init_with_reservation(reservation) };
    boot_trace(b'd');

    #[cfg(feature = "jtag")]
    // SAFETY: EL1, MMU on, so the GPIO lock is usable.
    unsafe {
        crate::debug::jtag::init(true)
    };

    // SAFETY: `entry!` defines this symbol, and the linker would have refused
    // the image without it. Everything the application may touch is up.
    boot_trace(b'e');
    unsafe { main_primary(BootInfo::observe(core_id, videocore_memory, arm_clock)) }
}

/// Entry point for cores 1-3, tail-called from `_start` after the spin table
/// releases them.
///
/// # Safety
///
/// Called once per secondary core by the boot assembly, at EL1 with a valid
/// stack and the FPU ungated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kernel_boot_secondary(core_id: usize) -> ! {
    // Wait until core 0 says the translation tables exist. On real hardware
    // the spin table has already held this core back; under QEMU the core got
    // here on its own and this gate is all that stands between it and an abort
    // loop.
    smp::wait_for_mmu_gate();

    // The MMU must come up before anything else: `println!` and every lock
    // need the exclusive monitor, which needs Normal memory.
    //
    // SAFETY: the gate guarantees core 0 finished `mmu::init` before this.
    unsafe { arch::init_core() };

    smp::mark_online();

    #[cfg(feature = "jtag")]
    // SAFETY: EL1, MMU on. Pins are already routed by core 0.
    unsafe {
        crate::debug::jtag::init(false)
    };

    // SAFETY: as above; `entry!` guarantees the symbol exists.
    unsafe {
        main_secondary(BootInfo::observe(
            core_id,
            Err(ProbeError::Skipped),
            Err(ProbeError::Skipped),
        ))
    }
}

/// Names the functions the BSP should call once the machine is up.
///
/// Invoked exactly once, at the crate root. It defines the two symbols the BSP
/// links against; without it the image does not link.
///
/// # Forms
///
/// ```ignore
/// // Core 0 only. Cores 1-3 stay parked unless released, and halt if they are.
/// cogware_bcm2837::entry!(main);
///
/// // Core 0 runs `main`; cores 1-3 run `worker` once released.
/// cogware_bcm2837::entry!(main, worker);
///
/// // The same, spelled out.
/// cogware_bcm2837::entry!(primary = main, secondary = worker);
/// ```
///
/// Both functions take a [`BootInfo`] and never return:
///
/// ```ignore
/// fn main(boot: BootInfo) -> ! { .. }
/// ```
///
/// Cores 1-3 do not start on their own. Call
/// [`smp::start_secondaries`](crate::smp::start_secondaries) from the primary
/// entry point to release them; until then they wait at the MMU gate, which
/// costs nothing and makes the single-argument form safe.
#[macro_export]
macro_rules! entry {
    (primary = $primary:path, secondary = $secondary:path $(,)?) => {
        const _: () = {
            #[unsafe(no_mangle)]
            unsafe extern "Rust" fn __cogware_main_primary(info: $crate::BootInfo) -> ! {
                // Named binding first, so a signature mismatch is reported at
                // the user's function rather than inside the macro.
                let entry: fn($crate::BootInfo) -> ! = $primary;
                entry(info)
            }

            #[unsafe(no_mangle)]
            unsafe extern "Rust" fn __cogware_main_secondary(info: $crate::BootInfo) -> ! {
                let entry: fn($crate::BootInfo) -> ! = $secondary;
                entry(info)
            }
        };
    };

    (primary = $primary:path $(,)?) => {
        $crate::entry!(primary = $primary, secondary = $crate::boot::park);
    };

    ($primary:path, $secondary:path $(,)?) => {
        $crate::entry!(primary = $primary, secondary = $secondary);
    };

    ($primary:path $(,)?) => {
        $crate::entry!(primary = $primary, secondary = $crate::boot::park);
    };
}

/// The default secondary entry point: announce and stop.
///
/// Used when [`entry!`](crate::entry) is given only a primary function. A core reaching here
/// was released deliberately: [`smp::start_secondaries`] is the only way out
/// of the MMU gate.
pub fn park(info: BootInfo) -> ! {
    crate::println!(
        "[core {}] online, no secondary entry point; halting.",
        info.core_id
    );
    cpu::halt()
}

/// Emits one byte straight to the PL011, bypassing the console driver.
///
/// The Rust counterpart to the `TRACE` macro in `boot.s`, covering the part of
/// the boot path that is in Rust but still ahead of anything that can report a
/// failure for itself. Compiles to nothing without `boot-trace`.
#[inline(always)]
fn boot_trace(_ch: u8) {
    #[cfg(feature = "boot-trace")]
    {
        let dr = (crate::bsp::memory::PERIPHERAL_BASE + 0x20_1000) as *mut u32;
        let fr = (crate::bsp::memory::PERIPHERAL_BASE + 0x20_1018) as *const u32;
        // SAFETY: Device-mapped PL011 registers. The firmware has already
        // enabled UART0, and the peripheral window is mapped in every
        // configuration of the kernel's translation tables.
        unsafe {
            while core::ptr::read_volatile(fr) & (1 << 5) != 0 {}
            core::ptr::write_volatile(dr, _ch as u32);
        }
    }
}
