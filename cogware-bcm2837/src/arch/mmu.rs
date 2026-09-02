// SPDX-License-Identifier: MIT OR Apache-2.0
//! Stage-1 EL1 MMU: identity mapping with per-section permissions.
//!
//! # Why the MMU is mandatory here
//!
//! With the MMU off every access is Device-nGnRnE, where `LDXR`/`STXR` — and so
//! every Rust read-modify-write atomic and every [`crate::sync::SpinLock`] —
//! are architecturally UNPREDICTABLE. An SMP kernel sharing peripherals between
//! four cores therefore switches the MMU on before releasing the secondaries.
//!
//! # Layout
//!
//! `TTBR0_EL1` only, `T0SZ = 32` (a 4 GiB virtual address space), 4 KiB
//! granule, so the walk starts at level 1:
//!
//! ```text
//! L1[0] -> L2 table            0x0000_0000 .. 0x4000_0000
//!          L2[0..8]   -> L3 tables, 4 KiB pages   0x0000_0000 .. 0x0100_0000
//!          L2[8..504] -> 2 MiB blocks, Normal     0x0100_0000 .. 0x3F00_0000
//!          L2[504..]  -> 2 MiB blocks, Device     0x3F00_0000 .. 0x4000_0000
//! L1[1] -> 1 GiB block, Device 0x4000_0000 .. 0x8000_0000  (ARM local peripherals)
//! L1[2], L1[3]                 unmapped
//! ```
//!
//! The low 16 MiB gets 4 KiB granularity so `.text` can be mapped read-only and
//! executable while everything else is writable and no-execute.

use core::cell::UnsafeCell;

use crate::{
    arch::cpu::{self, read_sysreg, write_sysreg},
    bsp::memory::{self, layout},
};

/// Size of a translation granule.
pub const PAGE_SIZE: usize = 4096;

/// Descriptors per translation table.
const ENTRIES_PER_TABLE: usize = 512;

/// Bytes covered by one level-2 block descriptor.
const L2_BLOCK_SIZE: usize = 2 * 1024 * 1024;

/// Number of level-3 tables, i.e. how much low memory is mapped with 4 KiB
/// granularity. Must cover the whole kernel image; `link.ld` asserts this.
const L3_TABLE_COUNT: usize = 8;

/// One past the last address mapped with 4 KiB pages.
pub const FINE_WINDOW_END: usize = L3_TABLE_COUNT * L2_BLOCK_SIZE;

/// `T0SZ`: the TTBR0 region spans `2^(64 - T0SZ)` bytes.
const T0SZ: u64 = 32;

/// `MAIR_EL1` attribute index for Device-nGnRnE memory.
const ATTR_DEVICE: u64 = 0;

/// `MAIR_EL1` attribute index for Normal, write-back, write-allocate memory.
const ATTR_NORMAL: u64 = 1;

/// `MAIR_EL1` attribute index for Normal, inner and outer non-cacheable.
///
/// Required for anything shared with the VideoCore: the GPU is outside the ARM
/// coherency domain, so a buffer it writes and the ARM reads must not be held
/// in a data cache. Normal-NC rather than Device keeps unaligned access and
/// ordinary `memcpy` legal.
const ATTR_NORMAL_NC: u64 = 2;

/// `MAIR_EL1`: attr0 = Device-nGnRnE (0x00), attr1 = Normal WB/WA (0xFF),
/// attr2 = Normal non-cacheable (0x44).
const MAIR_EL1_VALUE: u64 =
    (0x00 << (8 * ATTR_DEVICE)) | (0xFF << (8 * ATTR_NORMAL)) | (0x44 << (8 * ATTR_NORMAL_NC));

/// Translation table descriptor bits (ARM DDI 0487, D8.3).
mod desc {
    /// Descriptor is valid.
    pub const VALID: u64 = 1 << 0;
    /// At levels 0-2 selects a table descriptor; at level 3 it is RES1 and
    /// selects a page descriptor. Clearing it at levels 1-2 selects a block.
    pub const TABLE_OR_PAGE: u64 = 1 << 1;
    /// Access flag. Left unset, the first access takes an Access Flag fault.
    pub const AF: u64 = 1 << 10;
    /// Privileged execute-never.
    pub const PXN: u64 = 1 << 53;
    /// Unprivileged execute-never.
    pub const UXN: u64 = 1 << 54;
    /// Output address field of a descriptor, bits [47:12].
    pub const OUTPUT_MASK: u64 = 0x0000_FFFF_FFFF_F000;

    /// Read/write at EL1, no EL0 access.
    pub const AP_RW_EL1: u64 = 0b00 << 6;
    /// Read-only at EL1, no EL0 access.
    pub const AP_RO_EL1: u64 = 0b10 << 6;

    /// Non-shareable.
    pub const SH_NONE: u64 = 0b00 << 8;
    /// Outer shareable.
    pub const SH_OUTER: u64 = 0b10 << 8;
    /// Inner shareable: required for `LDXR`/`STXR` to work across cores.
    pub const SH_INNER: u64 = 0b11 << 8;

    /// Places `index` in the `AttrIndx` field, bits [4:2].
    pub const fn attr_index(index: u64) -> u64 {
        index << 2
    }
}

/// Cacheability and ordering class of a region.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemoryKind {
    /// Device-nGnRnE: no gathering, reordering or early write acknowledgement.
    Device,
    /// Normal write-back, write-allocate, inner shareable.
    Normal,
    /// Normal but non-cacheable, outer shareable. For memory shared with an
    /// agent outside the ARM coherency domain: the VideoCore, or a DMA engine.
    NormalNonCacheable,
}

impl MemoryKind {
    /// Short label used by the memory-map table.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Device => "Device",
            Self::Normal => "Normal",
            Self::NormalNonCacheable => "Normal-NC",
        }
    }
}

/// EL1 data access permission.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    /// Readable and writable.
    ReadWrite,
    /// Readable only. Writing takes a permission fault.
    ReadOnly,
}

/// Whether instructions may be fetched from a region.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Execute {
    /// Instruction fetch is allowed at EL1.
    Allowed,
    /// Instruction fetch faults.
    Never,
}

/// A contiguous physical range and the attributes it should be mapped with.
#[derive(Clone, Copy, Debug)]
pub struct Region {
    /// First address in the region.
    pub start: usize,
    /// One past the last address in the region.
    pub end: usize,
    /// Cacheability class.
    pub kind: MemoryKind,
    /// Data permissions.
    pub access: Access,
    /// Instruction fetch permission.
    pub execute: Execute,
    /// Name used by the boot-time memory map dump.
    pub name: &'static str,
}

impl Region {
    /// The attribute bits (everything but the output address) for a leaf
    /// descriptor covering this region.
    const fn leaf_attributes(&self) -> u64 {
        let attr = match self.kind {
            MemoryKind::Device => desc::attr_index(ATTR_DEVICE) | desc::SH_OUTER,
            MemoryKind::Normal => desc::attr_index(ATTR_NORMAL) | desc::SH_INNER,
            // Shareability is architecturally ignored for non-cacheable Normal
            // memory; outer shareable is the conventional encoding.
            MemoryKind::NormalNonCacheable => desc::attr_index(ATTR_NORMAL_NC) | desc::SH_OUTER,
        };

        let ap = match self.access {
            Access::ReadWrite => desc::AP_RW_EL1,
            Access::ReadOnly => desc::AP_RO_EL1,
        };

        // UXN is always set: nothing in this kernel runs at EL0.
        let xn = match self.execute {
            Execute::Allowed => desc::UXN,
            Execute::Never => desc::UXN | desc::PXN,
        };

        attr | ap | xn | desc::AF | desc::VALID
    }

    /// Whether `address` falls inside this region.
    const fn contains(&self, address: usize) -> bool {
        address >= self.start && address < self.end
    }
}

/// Silences the unused-constant warning for the shareability variant the
/// current map does not use.
const _: u64 = desc::SH_NONE;

/// The kernel's physical memory map, most specific entry first.
///
/// Built at runtime because the section bounds come from the linker.
pub fn memory_map() -> [Region; 8] {
    // Clamped: the firmware reports the VideoCore reservation as running to the
    // top of SDRAM, but in the ARM's view the top 16 MiB of that range is
    // shadowed by the peripheral window.
    let vc_start = memory::vc_mem_base();
    let vc_end = vc_start
        .saturating_add(memory::vc_mem_size())
        .min(memory::PERIPHERAL_BASE);

    [
        Region {
            start: layout::text_start(),
            end: layout::text_end(),
            kind: MemoryKind::Normal,
            access: Access::ReadOnly,
            execute: Execute::Allowed,
            name: "kernel .text",
        },
        Region {
            start: layout::rodata_start(),
            end: layout::rodata_end(),
            kind: MemoryKind::Normal,
            access: Access::ReadOnly,
            execute: Execute::Never,
            name: "kernel .rodata",
        },
        Region {
            start: layout::data_start(),
            end: layout::kernel_end(),
            kind: MemoryKind::Normal,
            access: Access::ReadWrite,
            execute: Execute::Never,
            name: "kernel .data/.bss + stacks",
        },
        Region {
            start: memory::DRAM_START,
            end: memory::FIRMWARE_RESERVED_END,
            kind: MemoryKind::Normal,
            access: Access::ReadWrite,
            execute: Execute::Never,
            name: "firmware stub + SMP spin table",
        },
        Region {
            start: layout::kernel_end(),
            end: vc_start,
            kind: MemoryKind::Normal,
            access: Access::ReadWrite,
            execute: Execute::Never,
            name: "free DRAM (ARM)",
        },
        Region {
            start: vc_start,
            end: vc_end,
            // Non-cacheable: the VideoCore writes here and is outside the ARM
            // coherency domain. Holds the firmware's framebuffer and QPU
            // buffers.
            kind: MemoryKind::NormalNonCacheable,
            access: Access::ReadWrite,
            execute: Execute::Never,
            name: "VideoCore DRAM (GPU)",
        },
        Region {
            start: memory::PERIPHERAL_BASE,
            end: memory::PERIPHERAL_END,
            kind: MemoryKind::Device,
            access: Access::ReadWrite,
            execute: Execute::Never,
            name: "BCM2837 peripherals (MMIO)",
        },
        Region {
            start: memory::ARM_LOCAL_BASE,
            end: memory::ARM_LOCAL_END,
            kind: MemoryKind::Device,
            access: Access::ReadWrite,
            execute: Execute::Never,
            name: "ARM local peripherals (MMIO)",
        },
    ]
}

fn region_for(map: &[Region], address: usize) -> Option<&Region> {
    map.iter().find(|region| region.contains(address))
}

#[repr(C, align(4096))]
struct TranslationTables {
    l1: [u64; ENTRIES_PER_TABLE],
    l2: [u64; ENTRIES_PER_TABLE],
    l3: [[u64; ENTRIES_PER_TABLE]; L3_TABLE_COUNT],
}

/// Wrapper that lets the all-zero tables live in `.bss` while still being
/// mutable from the single core that builds them.
struct Tables(UnsafeCell<TranslationTables>);

// SAFETY: only core 0 writes the tables, and only before the secondary cores
// have been released. Afterwards they are read-only to software and are walked
// by hardware.
unsafe impl Sync for Tables {}

static TABLES: Tables = Tables(UnsafeCell::new(TranslationTables {
    l1: [0; ENTRIES_PER_TABLE],
    l2: [0; ENTRIES_PER_TABLE],
    l3: [[0; ENTRIES_PER_TABLE]; L3_TABLE_COUNT],
}));

/// Builds the translation tables.
///
/// Runs on core 0 only, with the MMU still off, so the stores land straight in
/// DRAM and are visible to the secondary cores' table walks.
///
/// # Safety
///
/// Must be called exactly once, from core 0, before [`enable`].
pub unsafe fn init() {
    // SAFETY: sole writer, before any other core is running.
    let tables = unsafe { &mut *TABLES.0.get() };
    let map = memory_map();

    assert!(
        layout::kernel_end() <= FINE_WINDOW_END,
        "kernel image outgrew the 4 KiB-granular MMU window"
    );

    // Level 3: 4 KiB pages over the low window, one page at a time so that
    // section permissions are exact.
    for (table_index, table) in tables.l3.iter_mut().enumerate() {
        for (page_index, entry) in table.iter_mut().enumerate() {
            let va = table_index * L2_BLOCK_SIZE + page_index * PAGE_SIZE;
            *entry = match region_for(&map, va) {
                Some(region) => {
                    (va as u64 & desc::OUTPUT_MASK) | region.leaf_attributes() | desc::TABLE_OR_PAGE
                }
                None => 0,
            };
        }
    }

    // Level 2: table descriptors for the fine window, 2 MiB blocks after it.
    for index in 0..ENTRIES_PER_TABLE {
        tables.l2[index] = if index < L3_TABLE_COUNT {
            let table_addr = tables.l3[index].as_ptr() as u64;
            (table_addr & desc::OUTPUT_MASK) | desc::TABLE_OR_PAGE | desc::VALID
        } else {
            let va = index * L2_BLOCK_SIZE;
            match region_for(&map, va) {
                // Block descriptor: TABLE_OR_PAGE stays clear.
                Some(region) => (va as u64 & desc::OUTPUT_MASK) | region.leaf_attributes(),
                None => 0,
            }
        };
    }

    // Level 1: the first GiB via the L2 table, the second as one Device block
    // so the ARM local peripherals at 0x4000_0000 are reachable.
    let l2_addr = tables.l2.as_ptr() as u64;
    tables.l1[0] = (l2_addr & desc::OUTPUT_MASK) | desc::TABLE_OR_PAGE | desc::VALID;

    let arm_local = Region {
        start: memory::ARM_LOCAL_BASE,
        end: memory::ARM_LOCAL_BASE + (1 << 30),
        kind: MemoryKind::Device,
        access: Access::ReadWrite,
        execute: Execute::Never,
        name: "ARM local peripherals",
    };
    tables.l1[1] =
        (memory::ARM_LOCAL_BASE as u64 & desc::OUTPUT_MASK) | arm_local.leaf_attributes();

    tables.l1[2] = 0;
    tables.l1[3] = 0;

    // The tables were written with the caches off, so they are already in DRAM;
    // this only orders them against the TTBR0_EL1 write that follows.
    cpu::dsb_sy();
}

/// Physical address of the level 1 table, i.e. the value for `TTBR0_EL1`.
pub fn root_table_address() -> usize {
    TABLES.0.get() as usize
}

fn tcr_el1_value() -> u64 {
    const IRGN_WBWA: u64 = 0b01;
    const ORGN_WBWA: u64 = 0b01;
    const SH_INNER: u64 = 0b11;
    const TG0_4K: u64 = 0b00;
    const TG1_4K: u64 = 0b10;
    /// 40-bit intermediate physical address size, the Cortex-A53 maximum.
    const IPS_40_BIT: u64 = 0b010;
    /// Disable TTBR1 walks: the kernel maps everything through TTBR0.
    const EPD1: u64 = 1 << 23;

    T0SZ | (IRGN_WBWA << 8)
        | (ORGN_WBWA << 10)
        | (SH_INNER << 12)
        | (TG0_4K << 14)
        | (T0SZ << 16)
        | EPD1
        | (IRGN_WBWA << 24)
        | (ORGN_WBWA << 26)
        | (SH_INNER << 28)
        | (TG1_4K << 30)
        | (IPS_40_BIT << 32)
}

/// Turns on the MMU, the data cache and the instruction cache for the calling
/// core.
///
/// Every core must call this for itself: `SCTLR_EL1`, `TCR_EL1`, `MAIR_EL1` and
/// `TTBR0_EL1` are banked per core. They all point at the same tables.
///
/// # Safety
///
/// [`init`] must have completed on core 0 first. The mapping is an identity
/// map, so the program counter and stack pointer stay valid across the switch.
pub unsafe fn enable() {
    /// MMU enable.
    const SCTLR_M: u64 = 1 << 0;
    /// Alignment check enable. Left clear: `+strict-align` already stops LLVM
    /// from generating unaligned accesses.
    const SCTLR_A: u64 = 1 << 1;
    /// Data cache enable.
    const SCTLR_C: u64 = 1 << 2;
    /// Stack alignment check at EL1.
    const SCTLR_SA: u64 = 1 << 3;
    /// Instruction cache enable.
    const SCTLR_I: u64 = 1 << 12;

    unsafe {
        write_sysreg!("MAIR_EL1", MAIR_EL1_VALUE);
        write_sysreg!("TCR_EL1", tcr_el1_value());
        write_sysreg!("TTBR0_EL1", root_table_address() as u64);
        write_sysreg!("TTBR1_EL1", 0);

        // Publish the tables, then drop every stale translation.
        core::arch::asm!(
            "dsb ishst",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags),
        );

        let sctlr = read_sysreg!("SCTLR_EL1");
        let sctlr = (sctlr | SCTLR_M | SCTLR_C | SCTLR_I | SCTLR_SA) & !SCTLR_A;
        write_sysreg!("SCTLR_EL1", sctlr);

        // Instruction cache contents predating the MMU switch are stale.
        core::arch::asm!(
            "ic iallu",
            "dsb nsh",
            "isb",
            options(nostack, preserves_flags)
        );
    }
}

/// Whether the MMU is on for the calling core.
pub fn is_enabled() -> bool {
    cpu::sctlr_el1() & 1 == 1
}

/// Whether the data cache is on for the calling core.
pub fn data_cache_enabled() -> bool {
    cpu::sctlr_el1() & (1 << 2) != 0
}

/// Whether the instruction cache is on for the calling core.
pub fn instruction_cache_enabled() -> bool {
    cpu::sctlr_el1() & (1 << 12) != 0
}

/// Cleans and invalidates the cache line containing `address` all the way to
/// the point of coherency.
///
/// Needed when a core with caches enabled hands data to a core still running
/// with them off, as when releasing the secondary cores through the firmware
/// spin table.
///
/// # Safety
///
/// `address` must be mapped.
#[inline]
pub unsafe fn clean_invalidate_line(address: usize) {
    unsafe {
        core::arch::asm!(
            "dc civac, {addr}",
            "dsb sy",
            addr = in(reg) address,
            options(nostack, preserves_flags),
        );
    }
}

/// Cleans and invalidates every data cache level to the point of coherency.
///
/// Turning the data cache off does not empty it: lines stay resident and dirty,
/// and the eventual eviction lands on whatever occupies that address by then.
/// A core about to disable its cache and leave memory to somebody else -- a
/// chain-loader handing over to its payload -- must write everything back
/// first.
///
/// # Safety
///
/// This walks the shared L2, so every other core must already be parked with
/// its caches off. Use [`clean_invalidate_l1`] when that is not true.
#[inline]
pub unsafe fn clean_invalidate_all() {
    unsafe {
        core::arch::asm!("bl __dcache_clean_invalidate_all", clobber_abi("C"));
    }
}

/// Cleans and invalidates the calling core's private L1 data cache.
///
/// The counterpart of [`clean_invalidate_all`] for a core standing down while
/// the others keep running: pushes this core's dirty lines out without touching
/// the shared L2.
///
/// # Safety
///
/// Leaves this core's L1 empty, so the caller must not depend on anything it
/// had cached.
#[inline]
pub unsafe fn clean_invalidate_l1() {
    unsafe {
        core::arch::asm!("bl __dcache_clean_invalidate_l1", clobber_abi("C"));
    }
}
