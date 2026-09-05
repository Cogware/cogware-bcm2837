// SPDX-License-Identifier: MIT OR Apache-2.0
//! Physical memory map of the BCM2837 as wired on the Raspberry Pi 3.
//!
//! Every address here is a *physical* address. The kernel identity-maps the
//! low 2 GiB, so physical and virtual addresses are interchangeable.
//!
//! The map is complete rather than minimal: peripherals the kernel does not
//! drive are still listed.
#![allow(dead_code)]

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Number of Cortex-A53 cores in the BCM2837.
///
/// Must stay in sync with `CORE_COUNT` in `kernel/link.ld`.
pub const CORE_COUNT: usize = 4;

/// Stack reserved for each core, in bytes.
///
/// Must stay in sync with `KERNEL_STACK_SIZE` in `kernel/link.ld`. Exception
/// entry pushes an 832-byte trap frame, including the full NEON register file.
pub const KERNEL_STACK_SIZE: usize = 128 * 1024;

/// Address at which the firmware (and QEMU) loads `kernel8.img`.
pub const KERNEL_LOAD_ADDR: usize = 0x0008_0000;

/// Start of usable DRAM.
pub const DRAM_START: usize = 0x0000_0000;

/// First address of the memory-mapped peripheral window. Also the end of the
/// DRAM the ARM cores can see once the VideoCore split is applied.
pub const PERIPHERAL_BASE: usize = 0x3F00_0000;

/// One past the last peripheral address.
pub const PERIPHERAL_END: usize = 0x4000_0000;

/// Per-core interrupt controller, mailboxes and timers (BCM2836/7 "ARM local").
pub const ARM_LOCAL_BASE: usize = 0x4000_0000;

/// One past the last ARM-local address.
pub const ARM_LOCAL_END: usize = 0x4002_0000;

/// Firmware SMP spin table. Core `n` (1..=3) polls `SPIN_TABLE_BASE + n * 8`
/// and branches to the 64-bit address written there.
///
/// Identical on real hardware (`armstub8.S`) and QEMU (`hw/arm/raspi.c`).
pub const SPIN_TABLE_BASE: usize = 0x0000_00D8;

/// Low memory owned by the firmware stub: vectors, the spin table and the
/// QEMU SMP trampoline at 0x300. Mapped, but never allocated from.
pub const FIRMWARE_RESERVED_END: usize = KERNEL_LOAD_ADDR;

/// Version-1 chain-load handoff marker in entry register x4 (`CGCHAIN1`).
///
/// x5 supplies the parked-code page base and x6 its length. x0-x3 retain the
/// firmware ABI. The primary validates the descriptor before initialising
/// the heap; the page remains excluded even if secondaries are never started.
pub const CHAINLOAD_MAGIC: usize = 0x4347_4348_4149_4E31;

/// Exact reserved length accepted by the version-1 chain-load protocol.
pub const CHAINLOAD_PAGE_BYTES: usize = 4096;

#[derive(Clone, Copy)]
pub(crate) struct ReservedPage {
    pub start: usize,
    pub end: usize,
}

pub(crate) const fn validate_chainload(
    magic: usize,
    base: usize,
    length: usize,
    arm_base: usize,
    arm_size: usize,
    vc_base: usize,
) -> Result<Option<ReservedPage>, &'static str> {
    if magic != CHAINLOAD_MAGIC {
        return Ok(None);
    }
    let Some(end) = base.checked_add(length) else {
        return Err("reserved page address overflow");
    };
    let Some(arm_end) = arm_base.checked_add(arm_size) else {
        return Err("firmware ARM memory overflow");
    };
    if length != CHAINLOAD_PAGE_BYTES
        || !base.is_multiple_of(CHAINLOAD_PAGE_BYTES)
        || base < crate::arch::mmu::FINE_WINDOW_END
        || base < arm_base
        || end > arm_end
        || end > vc_base
        || end > PERIPHERAL_BASE
    {
        return Err("reserved page is outside confirmed free ARM RAM");
    }
    Ok(Some(ReservedPage { start: base, end }))
}

const _: () = {
    let base = crate::arch::mmu::FINE_WINDOW_END;
    let limit = 64 * 1024 * 1024;
    let page = CHAINLOAD_PAGE_BYTES;
    assert!(matches!(
        validate_chainload(CHAINLOAD_MAGIC, base, page, 0, limit, limit),
        Ok(Some(ReservedPage { start, end })) if start == base && end == base + page
    ));
    assert!(matches!(
        validate_chainload(CHAINLOAD_MAGIC, limit - page, page, base, limit - base, limit),
        Ok(Some(ReservedPage { start, end })) if start == limit - page && end == limit
    ));
    assert!(matches!(
        validate_chainload(0, usize::MAX, usize::MAX, usize::MAX, usize::MAX, 0),
        Ok(None)
    ));
    assert!(validate_chainload(CHAINLOAD_MAGIC, base - page, page, 0, limit, limit).is_err());
    assert!(validate_chainload(CHAINLOAD_MAGIC, base + 1, page, 0, limit, limit).is_err());
    assert!(validate_chainload(CHAINLOAD_MAGIC, base, page * 2, 0, limit, limit).is_err());
    assert!(validate_chainload(CHAINLOAD_MAGIC, base, 0, 0, limit, limit).is_err());
    assert!(validate_chainload(CHAINLOAD_MAGIC, base, page, 0, base, limit).is_err());
    assert!(validate_chainload(CHAINLOAD_MAGIC, base, page, 0, limit, base).is_err());
    assert!(validate_chainload(CHAINLOAD_MAGIC, base, page, base + page, limit, limit).is_err());
    assert!(validate_chainload(CHAINLOAD_MAGIC, limit - page, page, 0, limit - 1, limit).is_err());
    assert!(
        validate_chainload(
            CHAINLOAD_MAGIC,
            PERIPHERAL_BASE,
            page,
            0,
            PERIPHERAL_END,
            PERIPHERAL_END,
        )
        .is_err()
    );
    assert!(validate_chainload(CHAINLOAD_MAGIC, base, page, 1, usize::MAX, limit).is_err());
    assert!(
        validate_chainload(
            CHAINLOAD_MAGIC,
            usize::MAX - page + 1,
            page,
            0,
            limit,
            limit
        )
        .is_err()
    );
};

/// VideoCore memory reservation assumed before the mailbox has been asked.
///
/// The firmware default (`gpu_mem=64` in `config.txt`), used as a fallback
/// until `probe_vc_memory` replaces it with the real split.
pub const VC_MEM_DEFAULT_BYTES: usize = 64 * 1024 * 1024;

static VC_MEM_BASE: AtomicUsize = AtomicUsize::new(PERIPHERAL_BASE - VC_MEM_DEFAULT_BYTES);
static VC_MEM_SIZE: AtomicUsize = AtomicUsize::new(VC_MEM_DEFAULT_BYTES);
static VC_MEM_PROBED: AtomicBool = AtomicBool::new(false);

/// First address of the VideoCore's private DRAM reservation.
///
/// DRAM below this belongs to the ARM cores; DRAM above it belongs to the GPU
/// and is where the firmware allocates framebuffers and QPU code.
pub fn vc_mem_base() -> usize {
    VC_MEM_BASE.load(Ordering::Relaxed)
}

/// Size in bytes of the VideoCore's DRAM reservation.
pub fn vc_mem_size() -> usize {
    VC_MEM_SIZE.load(Ordering::Relaxed)
}

/// One past the last DRAM address the ARM cores may allocate from.
pub fn arm_mem_end() -> usize {
    vc_mem_base()
}

/// Whether the split above came from the firmware rather than the default.
pub fn vc_mem_probed() -> bool {
    VC_MEM_PROBED.load(Ordering::Relaxed)
}

/// Records the VideoCore split reported by the property mailbox.
///
/// The base is rounded down to a 2 MiB boundary because the level-2 block
/// mapping cannot express a finer split above the low 16 MiB. Rounding down
/// errs towards mapping ARM memory non-cacheable rather than mapping GPU
/// memory cacheable.
///
/// # Safety
///
/// Must be called by core 0 only, before [`crate::arch::mmu::init`] reads the
/// map and before any secondary core is released.
pub unsafe fn set_vc_memory(base: usize, size: usize) {
    const BLOCK: usize = 2 * 1024 * 1024;

    if size == 0 || base == 0 || base >= PERIPHERAL_END {
        return;
    }

    let aligned = base & !(BLOCK - 1);
    VC_MEM_BASE.store(aligned, Ordering::Relaxed);
    VC_MEM_SIZE.store(size + (base - aligned), Ordering::Relaxed);
    VC_MEM_PROBED.store(true, Ordering::Relaxed);
}

/// Peripheral MMIO base addresses.
///
/// Everything here sits inside [`PERIPHERAL_BASE`]..[`PERIPHERAL_END`], which
/// the MMU maps as Device-nGnRnE, so a new driver need only wrap the address
/// in a [`crate::drivers::mmio::RegisterBlock`].
pub mod map {
    use super::PERIPHERAL_BASE;

    /// System timer (free-running 64-bit counter plus four compare channels).
    pub const SYSTEM_TIMER: usize = PERIPHERAL_BASE + 0x0000_3000;

    /// DMA controller, channels 0-14.
    pub const DMA: usize = PERIPHERAL_BASE + 0x0000_7000;

    /// ARM interrupt controller (IRQ basic/pending/enable/disable).
    pub const IRQ_CONTROLLER: usize = PERIPHERAL_BASE + 0x0000_B200;

    /// ARM side timer (SP804-like).
    pub const ARM_TIMER: usize = PERIPHERAL_BASE + 0x0000_B400;

    /// VideoCore mailbox block. See [`mailbox`] for the register layout.
    pub const MAILBOX: usize = PERIPHERAL_BASE + 0x0000_B880;

    /// Power management and watchdog. Needed to reset the board.
    pub const PM: usize = PERIPHERAL_BASE + 0x0010_0000;

    /// Clock manager: peripheral clock sources and dividers.
    pub const CM: usize = PERIPHERAL_BASE + 0x0010_1000;

    /// Hardware random number generator.
    pub const RNG: usize = PERIPHERAL_BASE + 0x0010_4000;

    /// GPIO function-select / set / clear / pull-up-down block.
    pub const GPIO: usize = PERIPHERAL_BASE + 0x0020_0000;

    /// PL011 (UART0). The kernel console.
    pub const PL011_UART: usize = PERIPHERAL_BASE + 0x0020_1000;

    /// SPI0.
    pub const SPI0: usize = PERIPHERAL_BASE + 0x0020_4000;

    /// BSC0 (I2C0).
    pub const I2C0: usize = PERIPHERAL_BASE + 0x0020_5000;

    /// PWM.
    pub const PWM: usize = PERIPHERAL_BASE + 0x0020_C000;

    /// AUX block containing the mini UART (UART1), SPI1 and SPI2.
    pub const AUX: usize = PERIPHERAL_BASE + 0x0021_5000;

    /// External mass media controller (SD host).
    pub const EMMC: usize = PERIPHERAL_BASE + 0x0030_0000;

    /// BSC1 (I2C1), the one exposed on the 40-pin header.
    pub const I2C1: usize = PERIPHERAL_BASE + 0x0080_4000;

    /// Synopsys DesignWare USB 2.0 OTG controller.
    pub const USB: usize = PERIPHERAL_BASE + 0x0098_0000;

    /// VideoCore IV 3D block (V3D / QPU).
    ///
    /// These registers read back as zero until the QPU has been powered on
    /// through the property mailbox with `SET_POWER_STATE` on device 10.
    pub const V3D: usize = PERIPHERAL_BASE + 0x00C0_0000;

    /// SDRAM controller.
    pub const SDRAM: usize = PERIPHERAL_BASE + 0x00E0_0000;

    /// DMA channel 15, which lives apart from channels 0-14.
    pub const DMA15: usize = PERIPHERAL_BASE + 0x00E0_5000;

    /// VideoCore mailbox register layout.
    ///
    /// There are two mailboxes. Mailbox 0 is read-only to the ARM and carries
    /// VC-to-ARM traffic; mailbox 1 is write-only to the ARM and carries
    /// ARM-to-VC traffic. Reading mailbox 1, or writing mailbox 0, hangs.
    ///
    /// The register offsets repeat within each mailbox.
    pub mod mailbox {
        use super::MAILBOX;

        /// Mailbox 0 base: VideoCore to ARM.
        pub const MB0: usize = MAILBOX;

        /// Mailbox 1 base: ARM to VideoCore.
        pub const MB1: usize = MAILBOX + 0x20;

        /// Data register. Read from MB0, write to MB1.
        pub const DATA: usize = 0x00;

        /// Read the next message without dequeuing it.
        pub const PEEK: usize = 0x10;

        /// Sender ID.
        pub const SENDER: usize = 0x14;

        /// Status register; see [`status`].
        pub const STATUS: usize = 0x18;

        /// Configuration register.
        pub const CONFIG: usize = 0x1C;

        /// Status register bits.
        pub mod status {
            /// No room to write another message.
            pub const FULL: u32 = 1 << 31;
            /// No message waiting to be read.
            pub const EMPTY: u32 = 1 << 30;
        }

        /// Mailbox channel numbers. The kernel only uses `PROPERTY_ARM_TO_VC`.
        pub mod channel {
            /// Power management.
            pub const POWER: u32 = 0;
            /// Framebuffer (the old, superseded interface).
            pub const FRAMEBUFFER: u32 = 1;
            /// Virtual UART.
            pub const VIRTUAL_UART: u32 = 2;
            /// The property interface, ARM to VC. This is the one to use.
            pub const PROPERTY_ARM_TO_VC: u32 = 8;
            /// The property interface, VC to ARM.
            pub const PROPERTY_VC_TO_ARM: u32 = 9;
        }
    }
}

/// Per-core interrupt and mailbox registers of the BCM2836/2837 "ARM local"
/// block at [`ARM_LOCAL_BASE`].
///
/// Configures inter-processor interrupts and the per-core generic timer
/// interrupts.
pub mod local {
    use super::ARM_LOCAL_BASE;

    /// Control register: selects the core timer clock source.
    // The `+ 0x0000` keeps every entry in this table in `base + offset` form;
    // the datasheet lists this register at offset 0.
    #[allow(clippy::identity_op)]
    pub const CONTROL: usize = ARM_LOCAL_BASE + 0x0000;

    /// Core timer prescaler.
    pub const TIMER_PRESCALER: usize = ARM_LOCAL_BASE + 0x0008;

    /// Routing of the GPU interrupt to a specific core.
    pub const GPU_IRQ_ROUTING: usize = ARM_LOCAL_BASE + 0x000C;

    /// Timer interrupt control for core `n`.
    pub const fn timer_irq_control(core: usize) -> usize {
        ARM_LOCAL_BASE + 0x0040 + core * 4
    }

    /// Mailbox interrupt control for core `n`.
    pub const fn mailbox_irq_control(core: usize) -> usize {
        ARM_LOCAL_BASE + 0x0050 + core * 4
    }

    /// Pending IRQ source register for core `n`.
    pub const fn irq_source(core: usize) -> usize {
        ARM_LOCAL_BASE + 0x0060 + core * 4
    }

    /// Pending FIQ source register for core `n`.
    pub const fn fiq_source(core: usize) -> usize {
        ARM_LOCAL_BASE + 0x0070 + core * 4
    }

    /// Write-set register for mailbox `mbox` of core `core`. Writing a bit here
    /// raises an IPI on that core.
    pub const fn mailbox_write_set(core: usize, mbox: usize) -> usize {
        ARM_LOCAL_BASE + 0x0080 + core * 16 + mbox * 4
    }

    /// Read / write-high-to-clear register for mailbox `mbox` of core `core`.
    pub const fn mailbox_read_clear(core: usize, mbox: usize) -> usize {
        ARM_LOCAL_BASE + 0x00C0 + core * 16 + mbox * 4
    }
}

/// Translation between ARM physical addresses and VideoCore bus addresses.
///
/// The GPU does not see the ARM's physical address space. It sees SDRAM through
/// one of four aliases selected by the top two address bits, and it sees the
/// peripherals at `0x7E00_0000` rather than [`PERIPHERAL_BASE`].
///
/// Any address handed to the VideoCore — a property-mailbox message, a DMA
/// control block, a framebuffer pointer — must be a bus address, and for
/// anything the ARM also touches it must be the *uncached* alias, because the
/// GPU is not in the ARM coherency domain.
pub mod bus {
    /// Where the VideoCore sees the peripheral window.
    pub const PERIPHERAL_BASE: u32 = 0x7E00_0000;

    /// L1 and L2 cached. Unsafe to share with the ARM.
    pub const ALIAS_L1_L2: u32 = 0x0000_0000;

    /// L2 coherent, L1 bypassed.
    pub const ALIAS_L2_COHERENT: u32 = 0x4000_0000;

    /// L2 cached, non-allocating.
    pub const ALIAS_L2_ONLY: u32 = 0x8000_0000;

    /// Fully uncached. The correct alias for anything shared with the ARM.
    pub const ALIAS_UNCACHED: u32 = 0xC000_0000;

    /// Mask covering the alias selector bits.
    pub const ALIAS_MASK: u32 = 0xC000_0000;

    /// Converts an ARM physical DRAM address to an uncached VideoCore bus
    /// address.
    pub const fn to_bus(physical: usize) -> u32 {
        (physical as u32) | ALIAS_UNCACHED
    }

    /// Converts a VideoCore bus address back to an ARM physical address.
    pub const fn to_physical(bus: u32) -> usize {
        (bus & !ALIAS_MASK) as usize
    }
}

/// Reference clock the firmware feeds to the PL011.
///
/// The RPi firmware default is 48 MHz on the Pi 3; `config.txt` can override it
/// with `init_uart_clock`. QEMU ignores the baud divisors entirely.
pub const PL011_CLOCK_HZ: u32 = 48_000_000;

/// Console baud rate.
pub const CONSOLE_BAUD: u32 = 115_200;

unsafe extern "C" {
    static __kernel_start: u8;
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __data_end: u8;
    static __bss_start: u8;
    static __bss_end: u8;
    static __stacks_start: u8;
    static __stacks_end: u8;
    static __kernel_end: u8;
}

/// Addresses of the linker-defined section boundaries.
///
/// Reading a `static` declared in `extern "C"` only to take its address is
/// sound; the values themselves are never loaded.
pub mod layout {
    macro_rules! linker_symbol {
        ($(#[$doc:meta] $name:ident => $sym:ident;)*) => {$(
            #[$doc]
            #[inline(always)]
            pub fn $name() -> usize {
                core::ptr::addr_of!(super::$sym) as usize
            }
        )*};
    }

    linker_symbol! {
        /// First byte of the loaded image (== [`super::KERNEL_LOAD_ADDR`]).
        kernel_start => __kernel_start;
        /// First byte of `.text`.
        text_start => __text_start;
        /// One past the last byte of `.text`.
        text_end => __text_end;
        /// First byte of `.rodata`.
        rodata_start => __rodata_start;
        /// One past the last byte of `.rodata`.
        rodata_end => __rodata_end;
        /// First byte of `.data`.
        data_start => __data_start;
        /// One past the last byte of `.bss`, page aligned.
        data_end => __data_end;
        /// First byte of `.bss`.
        bss_start => __bss_start;
        /// One past the last byte of `.bss`.
        bss_end => __bss_end;
        /// Lowest address of the per-core stack region.
        stacks_start => __stacks_start;
        /// One past the highest address of the per-core stack region.
        stacks_end => __stacks_end;
        /// One past the last byte the kernel image occupies.
        kernel_end => __kernel_end;
    }

    /// Inclusive-exclusive bounds of core `core_id`'s stack.
    ///
    /// Stacks grow downwards from the returned end address.
    pub fn core_stack(core_id: usize) -> (usize, usize) {
        let top = stacks_end() - core_id * super::KERNEL_STACK_SIZE;
        (top - super::KERNEL_STACK_SIZE, top)
    }
}
