// SPDX-License-Identifier: MIT OR Apache-2.0
//! AArch64 architecture support: reset path, cache maintenance, CPU state,
//! the stage-1 MMU and the exception vector table.

use core::arch::global_asm;

pub mod cpu;
pub mod exception;
pub mod mmu;

// The reset path. `CONST_*` operands are substituted as decimal literals so
// that the assembly and `bsp::memory` cannot drift apart.
global_asm!(
    include_str!("boot.s"),
    CONST_CORE_ID_MASK = const 0b11_usize,
    CONST_KERNEL_STACK_SIZE = const crate::bsp::memory::KERNEL_STACK_SIZE,
    CONST_BOOT_TRACE = const cfg!(feature = "boot-trace") as u32,
    CONST_UART0_BASE = const crate::bsp::memory::PERIPHERAL_BASE + 0x20_1000,
);

global_asm!(include_str!("cache.s"));
global_asm!(include_str!("exception.s"));

/// Per-core architectural bring-up shared by the primary and secondary paths.
///
/// Assumes the boot assembly has already dropped the core to EL1, ungated
/// FP/SIMD, set `CPUECTLR_EL1.SMPEN` and invalidated the caches.
///
/// # Safety
///
/// [`mmu::init`] must have completed on core 0 before this is called on any
/// core, and it must be called exactly once per core.
pub unsafe fn init_core() {
    unsafe {
        exception::install_vector_table();
        mmu::enable();
    }

    // Bounds how long `wfe` can block, which is what lets the async executor
    // idle instead of spin. Done after the MMU so this core's first wake-up
    // already runs with caches on.
    cpu::enable_event_stream();
}
