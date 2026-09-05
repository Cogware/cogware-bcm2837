//! QEMU payload exercising large BSS and heap use before secondary startup.
//! Build instructions are in docs/22-chainloading.md.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

use core::alloc::Layout;

use cogware_bcm2837::{BootInfo, bsp::memory, cpu, entry, heap, println, smp};

entry!(primary = main, secondary = secondary);

core::arch::global_asm!(
    r#"
.section .bss.chainload_regression, "aw", %nobits
.balign 4096
.global __chainload_large_bss
.type __chainload_large_bss, %object
__chainload_large_bss:
    .skip 14 * 1024 * 1024
.size __chainload_large_bss, . - __chainload_large_bss
"#
);

unsafe extern "C" {
    static __chainload_large_bss: [u8; 14 * 1024 * 1024];
}

fn main(_boot: BootInfo) -> ! {
    let start = (&raw const __chainload_large_bss).cast::<u8>();
    // SAFETY: the linker reserves this BSS object and boot.s zeroes it.
    let zeroed = unsafe {
        start.read_volatile() == 0 && start.add(14 * 1024 * 1024 - 1).read_volatile() == 0
    };
    let end = memory::layout::kernel_end();
    if !zeroed || !(8 * 1024 * 1024..=16 * 1024 * 1024).contains(&end) {
        println!("Large-BSS regression failed: kernel_end={end:#x}, zeroed={zeroed}");
        cpu::halt();
    }
    let stats = heap::stats();
    let Some((reserved_start, reserved_end)) = stats.reserved_range else {
        println!("Large-BSS regression failed: missing persistent reservation.");
        cpu::halt();
    };
    if stats.reserved_bytes() != memory::CHAINLOAD_PAGE_BYTES || smp::online_count() != 1 {
        println!("Large-BSS regression failed: invalid single-core starting state.");
        cpu::halt();
    }
    // SAFETY: the boot path validated this page against firmware RAM bounds;
    // the first words are installed instructions, unchanged while cores park.
    let fingerprint = unsafe { (reserved_start as *const u64).read_volatile() };

    // The first large carve crosses the old park address. A second allocation
    // catches accidentally donating that skipped page to a large free list.
    let sizes = [
        16 * 1024 * 1024,
        16 * 1024 * 1024,
        4096,
        8192,
        16384,
        32768,
        65536,
        16,
        64,
        256,
    ];
    for size in sizes {
        let Ok(layout) = Layout::from_size_align(size, size) else {
            println!("Large-BSS regression failed: invalid allocation layout.");
            cpu::halt();
        };
        let block = heap::alloc(layout);
        let address = block as usize;
        if block.is_null() || (address < reserved_end && reserved_start < address + size) {
            println!("Large-BSS regression failed: allocation overlaps parked code.");
            cpu::halt();
        }
        // SAFETY: this owned block is disjoint from the reserved page. Writing
        // both ends exercises returned free-list blocks as well as bump blocks.
        unsafe {
            block.write_bytes(0xa5, size.min(4096));
            block.add(size - 1).write_volatile(0x5a);
        }
    }
    let stats = heap::stats();
    // SAFETY: same validated instruction word as above.
    let unchanged = unsafe { (reserved_start as *const u64).read_volatile() } == fingerprint;
    if !unchanged
        || stats.watermark <= reserved_end
        || stats.capacity() != stats.live + stats.cached() + stats.untouched()
    {
        println!("Large-BSS regression failed: park page or heap accounting changed.");
        cpu::halt();
    }
    println!(
        "Single-core heap before release: PASS; reserved={reserved_start:#x}..{reserved_end:#x}."
    );
    if cfg!(chainload_single_core) {
        cpu::halt();
    }

    // SAFETY: primary, MMU on, called exactly once after shared state is ready.
    unsafe { smp::start_secondaries() };
    if smp::wait_for_cores(memory::CORE_COUNT, 1_000_000) != memory::CORE_COUNT {
        println!("Large-BSS regression failed: secondaries did not survive heap use.");
        cpu::halt();
    }
    println!("Large-BSS: all 4 cores online after heap use; kernel_end={end:#x}.");
    cpu::halt()
}

fn secondary(_boot: BootInfo) -> ! {
    cpu::halt()
}
