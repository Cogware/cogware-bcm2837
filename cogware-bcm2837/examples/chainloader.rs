// SPDX-License-Identifier: GPL-3.0-only
//! A UART chain-loader: receives a kernel over the serial line and boots it,
//! so the SD card only ever has to be written once.
//!
//! Put this kernel on the SD card once; every rebuild after that is a serial
//! push and a power cycle rather than a card swap.
//!
//! # Protocol
//!
//! Identical to `minipush.rb` from the `rust-raspberrypi-OS-tutorials`
//! project, so that tool works against this loader unmodified:
//!
//! | Direction | Bytes | Meaning |
//! | --- | --- | --- |
//! | target → host | any | banner text, printed by the host |
//! | target → host | `03 03 03` | three `ETX`: "send me a kernel" |
//! | host → target | `LL LL LL LL` | payload length, u32 little-endian |
//! | target → host | `OK` | length accepted |
//! | host → target | *length* bytes | the raw image |
//!
//! The host then becomes a plain terminal, so the payload's own output appears
//! on the same connection. Two host implementations ship with the crate:
//! [`scripts/minipush.py`](../../scripts/minipush.py) for Linux and macOS, and
//! [`scripts/minipush.ps1`](../../scripts/minipush.ps1) for Windows, which also
//! speaks TCP so the exchange can be driven under QEMU.
//!
//! # Why a relocating stub
//!
//! The firmware loads us at `0x80000`, which is also where the payload must
//! go, so the final copy would overwrite the code performing it. Instead of
//! linking the loader high, this one keeps the ordinary layout and relocates a
//! position-independent stub into a reserved heap allocation above the
//! complete 16 MiB kernel window. Only the stub has to survive the copy.
//!
//! ```text
//!   0x0008_0000  payload lands here          <- overwritten
//!   >= 16 MiB   relocated stub and per-core park acknowledgements
//!   base + 4K   staged payload (up to 8 MiB)
//! ```
//!
//! # Cores
//!
//! Cores 1-3 have to leave the image before it is overwritten, but parking them
//! for good would cost the payload its SMP. The parking loop inside the stub is
//! therefore a faithful copy of the firmware's spin table: it clears its slot at
//! `0xd8 + core_id * 8`, then waits for a non-zero entry point exactly as
//! `armstub8.S` and QEMU's `write_smpboot64` do.
//!
//! The payload's own `smp::start_secondaries()` then releases them into the
//! payload, so a chain-loaded kernel comes up on four cores just like one
//! booted from the card. This also matters under emulation: QEMU has no spin
//! table of its own and releases all four cores at the reset vector, so without
//! this they would execute the payload's bytes at whatever address they had
//! been spinning on.
//!
//! # Persistent handoff
//!
//! Entry registers x4-x6 describe the parked-code page. This BSP preserves
//! them through reset, validates them against firmware RAM bounds and excludes
//! that page from its fresh heap before allocations are possible. Single-core
//! payloads can therefore allocate without releasing the parked secondaries.
//! Foreign or older payloads ignoring this protocol must still release and
//! await all secondaries before heap reuse; see docs/22-chainloading.md.

#![no_std]
#![no_main]

use core::{
    alloc::Layout,
    sync::atomic::{AtomicUsize, Ordering},
};

use cogware_bcm2837::{
    BootInfo, arch::mmu, bsp::memory, console, cpu, drivers::MAILBOX, entry, heap, println, smp,
    time,
};

entry!(primary = main, secondary = secondary);

/// Alignment keeps the handover outside any kernel accepted by `link.ld`,
/// including its NOLOAD BSS and stacks, not just the received file.
const HANDOVER_ALIGN: usize = mmu::FINE_WINDOW_END;
const HANDOVER_BYTES: usize = HANDOVER_ALIGN;
const STAGING_OFFSET: usize = mmu::PAGE_SIZE;
const CACHE_LINE: usize = 64;
const ACK_OFFSET: usize = STAGING_OFFSET - memory::CORE_COUNT * CACHE_LINE;

/// Largest image accepted.
///
const MAX_PAYLOAD: usize = 8 * 1024 * 1024;

/// Sent three times to ask the host for a payload.
const REQUEST_TOKEN: u8 = 0x03;

/// Published only after the reservation's instructions and flags reach DRAM.
static HANDOVER_BASE: AtomicUsize = AtomicUsize::new(0);

const _: () = {
    assert!(memory::KERNEL_LOAD_ADDR + MAX_PAYLOAD <= mmu::FINE_WINDOW_END);
    assert!(STAGING_OFFSET + MAX_PAYLOAD <= HANDOVER_BYTES);
    assert!(ACK_OFFSET.is_multiple_of(CACHE_LINE));
    assert!(memory::KERNEL_LOAD_ADDR == 0x80000);
    assert!(STAGING_OFFSET == memory::CHAINLOAD_PAGE_BYTES);
};

// The stub. Position-independent by construction: no literal pools, no absolute
// symbol references, only registers and relative branches. It is copied
// verbatim to the reservation, so anything PC-relative outside this block would
// break.
core::arch::global_asm!(
    r#"
.section .text.chainload, "ax"
.balign 64

// x0 = target, x1-x3 = target arguments, x4 = clean all levels (else L1).
// Everything after the final clean is register-only, including the MMU switch.
// cache.s clobbers x0-x11/x16/x17, so keep the handover in callee-saved registers.
.global __chainload_leave
.type __chainload_leave, %function
__chainload_leave:
    msr     daifset, #15
    mov     x19, x0
    mov     x20, x1
    mov     x21, x2
    mov     x22, x3
    cbz     x4, .L_chainload_l1
    bl      __dcache_clean_invalidate_all
    b       .L_chainload_disable
.L_chainload_l1:
    bl      __dcache_clean_invalidate_l1
.L_chainload_disable:
    mrs     x9, sctlr_el1
    bic     x9, x9, #(1 << 0)
    bic     x9, x9, #(1 << 2)
    bic     x9, x9, #(1 << 12)
    msr     sctlr_el1, x9
    isb
    tlbi    vmalle1
    dsb     sy
    isb
    mov     x0, x20
    mov     x1, x21
    mov     x2, x22
    br      x19
.size __chainload_leave, . - __chainload_leave

.balign 64
// x0 = source, x1 = destination, x2 = byte count, rounded up to 8.
.global __chainload_stub_start
.type __chainload_stub_start, %function
__chainload_stub_start:
    // The caller already dropped the MMU -- it had to, because this address is
    // mapped execute-never. Repeat it anyway: it is idempotent, it costs four
    // instructions, and it means the stub is correct entered from anywhere.
    mrs     x3, sctlr_el1
    mov     x4, #1                  // M:  MMU
    orr     x4, x4, #(1 << 2)       // C:  data cache
    orr     x4, x4, #(1 << 12)      // I:  instruction cache
    bic     x3, x3, x4
    msr     sctlr_el1, x3
    isb
    tlbi    vmalle1
    dsb     sy
    isb

0:  ldr     x3, [x0], #8
    str     x3, [x1], #8
    subs    x2, x2, #8
    b.hi    0b

    // The payload is now in DRAM, but the I-cache may still hold whatever used
    // to live at the load address.
    dsb     sy
    ic      iallu
    dsb     sy
    isb

    // Enter exactly as the firmware does: x0 holds a device-tree pointer, which
    // we do not have, and x1-x3 are reserved and zero.
    mov     x0, xzr
    mov     x1, xzr
    mov     x2, xzr
    mov     x3, xzr
    // The payload's allocator starts fresh. Preserve only this code/ack page,
    // not staging, through the versioned register handoff; x0-x3 stay unchanged.
    movz    x4, #({CHAINLOAD_MAGIC} & 0xffff)
    movk    x4, #(({CHAINLOAD_MAGIC} >> 16) & 0xffff), lsl #16
    movk    x4, #(({CHAINLOAD_MAGIC} >> 32) & 0xffff), lsl #32
    movk    x4, #(({CHAINLOAD_MAGIC} >> 48) & 0xffff), lsl #48
    adr     x5, __chainload_stub_start
    mov     x6, #{CHAINLOAD_PAGE_BYTES}
    mov     x7, #0x80000
    br      x7
.size __chainload_stub_start, . - __chainload_stub_start

// Secondaries wait here while the image beneath them is replaced.
//
// This is not a dead-end park: it is a faithful copy of the firmware's spin
// table, so the *payload's* `start_secondaries()` releases these cores into the
// payload exactly as if the loader had never run. That is what keeps SMP alive
// across a chain-load.
.global __chainload_park
.type __chainload_park, %function
__chainload_park:
    mov     x5, #0xd8               // SPIN_TABLE_BASE
    mrs     x6, mpidr_el1
    and     x6, x6, #3

    // The slot still holds the loader's _start, which this core has already
    // taken. Clear it, or the load below succeeds immediately and the core
    // branches into an image that is about to be overwritten.
    str     xzr, [x5, x6, lsl #3]
    dsb     sy

    // x0 is this core's private acknowledgement cache line. Report only now:
    // executing relocated code, caches/MMU off, old spin slot cleared.
    stlr    x6, [x0]
    dsb     sy
    sev

    // `wfe` before the load, matching armstub8 and QEMU's write_smpboot64:
    // the release must therefore be followed by `sev`, which is what
    // `smp::start_secondaries` does.
1:  wfe
    ldr     x4, [x5, x6, lsl #3]
    cbz     x4, 1b
    br      x4
.size __chainload_park, . - __chainload_park

.global __chainload_stub_end
__chainload_stub_end:
"#,
    CHAINLOAD_MAGIC = const memory::CHAINLOAD_MAGIC,
    CHAINLOAD_PAGE_BYTES = const memory::CHAINLOAD_PAGE_BYTES,
);

unsafe extern "C" {
    fn __chainload_leave(target: usize, x0: usize, x1: usize, x2: usize, all_caches: usize) -> !;
    static __chainload_stub_start: u8;
    static __chainload_park: u8;
    static __chainload_stub_end: u8;
}

// The park loop hard-codes the spin-table base, because `global_asm!` cannot
// see a Rust constant. Fail the build rather than the board if it ever moves.
const _: () = assert!(
    memory::SPIN_TABLE_BASE == 0xd8,
    "the parking loop in the relocation stub hard-codes 0xd8"
);

fn main(boot: BootInfo) -> ! {
    println!();
    println!("CogWare BCM2837 chain-loader");
    println!("  core            : {}", boot.core_id);
    println!("  exception level : EL{}", boot.exception_level);
    println!("  load address    : {:#010x}", memory::KERNEL_LOAD_ADDR);
    println!("  maximum image   : {} KiB", MAX_PAYLOAD / 1024);
    println!("  handover ABI    : CGCHAIN1, one reserved park page");

    let handover = match prepare_handover(&boot) {
        Ok(handover) => handover,
        Err(error) => refuse(error),
    };
    println!("  staging buffer  : {:#010x}", handover.staging());
    println!("  relocation stub : {:#010x}", handover.base);
    println!("  stub installed  : {} bytes", handover.stub_len);
    println!("  secondaries     : {} parked", memory::CORE_COUNT - 1);
    println!();
    println!("Waiting for a kernel. Push one with:");
    println!("  python3 scripts/minipush.py --payload kernel8.img");
    println!("  .\\scripts\\minipush.ps1 -Serial COM3 -Payload kernel8.img");
    println!();

    let payload = receive(&handover);

    println!();
    println!("  received        : {} bytes", payload.len());
    println!("  crc32           : {:#010x}", crc32(payload));
    println!("  entry           : {:#010x}", memory::KERNEL_LOAD_ADDR);
    println!();
    println!("Booting payload.");
    println!();
    console::flush();

    // SAFETY: `payload` is the staging buffer, which does not overlap the
    // destination; the stub is installed and cache-coherent; the secondaries
    // are parked inside it; and nothing after this point touches this image.
    unsafe { jump_to_payload(&handover, payload) }
}

fn secondary(boot: BootInfo) -> ! {
    let deadline = time::uptime() + core::time::Duration::from_secs(2);
    let base = loop {
        let base = HANDOVER_BASE.load(Ordering::Acquire);
        if base != 0 {
            break base;
        }
        if time::uptime() >= deadline {
            cpu::halt();
        }
        core::hint::spin_loop();
    };
    let park = base + offset_of_park();
    let acknowledgement = base + ACK_OFFSET + boot.core_id * CACHE_LINE;

    // SAFETY: the published parking loop is installed and cleaned to DRAM.
    // The helper cleans this core's private L1 before disabling its cache;
    // only relocated code acknowledges parking, without touching this image.
    unsafe { leave_mmu_and_branch(park, acknowledgement, 0, 0, false) }
}

/// Byte offset of the parking loop from the start of the stub.
fn offset_of_park() -> usize {
    (&raw const __chainload_park) as usize - (&raw const __chainload_stub_start) as usize
}

struct Handover {
    base: usize,
    stub_len: usize,
}

impl Handover {
    fn staging(&self) -> usize {
        self.base + STAGING_OFFSET
    }

    fn all_parked(&self) -> bool {
        (1..memory::CORE_COUNT).all(|core| {
            let address = self.base + ACK_OFFSET + core * CACHE_LINE;
            // SAFETY: an initialised AtomicUsize on its own reserved line.
            // The writer has its caches off; evict our clean copy before each
            // load, without sharing a line with any other core's flag.
            unsafe {
                mmu::clean_invalidate_line(address);
                (*(address as *const AtomicUsize)).load(Ordering::Acquire) == core
            }
        })
    }
}

const fn valid_reservation(
    base: usize,
    kernel_end: usize,
    arm_end: usize,
    stub_len: usize,
) -> bool {
    let Some(end) = base.checked_add(HANDOVER_BYTES) else {
        return false;
    };
    kernel_end <= mmu::FINE_WINDOW_END
        && base >= mmu::FINE_WINDOW_END
        && base >= kernel_end
        && base.is_multiple_of(HANDOVER_ALIGN)
        && end <= arm_end
        && end <= memory::PERIPHERAL_BASE
        && stub_len > 0
        && stub_len <= ACK_OFFSET
}

// Include the old overlapping address, the largest permitted BSS/stacks,
// insufficient ARM RAM, an oversized stub and arithmetic overflow.
const _: () = {
    let low_end = mmu::FINE_WINDOW_END;
    assert!(valid_reservation(low_end, low_end, low_end * 2, 256));
    assert!(!valid_reservation(
        8 * 1024 * 1024,
        low_end,
        low_end * 2,
        256
    ));
    assert!(!valid_reservation(low_end, low_end, low_end * 2 - 1, 256));
    assert!(!valid_reservation(low_end, low_end + 1, low_end * 2, 256));
    assert!(!valid_reservation(
        low_end,
        low_end,
        low_end * 2,
        ACK_OFFSET + 1
    ));
    assert!(!valid_reservation(usize::MAX, low_end, usize::MAX, 256));
};

fn prepare_handover(boot: &BootInfo) -> Result<Handover, &'static str> {
    let (vc_base, vc_size) = boot
        .videocore_memory
        .map_err(|_| "firmware RAM bounds unavailable; refusing to reserve handover memory")?;
    let vc_end = vc_base
        .checked_add(vc_size)
        .ok_or("invalid firmware RAM bounds")?;
    let (arm_base, arm_size) = MAILBOX
        .with(|mailbox| mailbox.arm_memory())
        .map_err(|_| "firmware ARM RAM bounds unavailable")?;
    let arm_end = arm_base
        .checked_add(arm_size)
        .ok_or("invalid firmware ARM RAM bounds")?;
    let kernel_end = memory::layout::kernel_end();
    let arena = heap::stats();
    if !memory::vc_mem_probed()
        || vc_size == 0
        || vc_end > memory::PERIPHERAL_END
        || arm_size == 0
        || arena.start != kernel_end
        || arena.start < arm_base
        || arena.end > arm_end
        || arena.end > vc_base
    {
        return Err("firmware RAM bounds do not match the mapped heap");
    }
    let arm_end = arena.end.min(memory::PERIPHERAL_BASE);
    let start = (&raw const __chainload_stub_start) as usize;
    let end = (&raw const __chainload_stub_end) as usize;
    let len = end - start;
    if !valid_reservation(HANDOVER_ALIGN, kernel_end, arm_end, len) {
        return Err("not enough mapped ARM RAM for an 8 MiB payload and safe handover");
    }

    // First bring every core into this image. This also protects older loaders
    // which did not supply a descriptor for the page our heap might reuse.
    // SAFETY: called once on core 0, after the MMU and console are ready.
    unsafe { smp::start_secondaries() };
    if smp::wait_for_cores(memory::CORE_COUNT, 1_000_000) != memory::CORE_COUNT {
        return Err("secondary startup timed out; payload will not be copied");
    }

    let layout = Layout::from_size_align(HANDOVER_BYTES, HANDOVER_ALIGN)
        .map_err(|_| "invalid handover allocation layout")?;
    let base = heap::alloc(layout) as usize;
    if base == 0 {
        return Err("could not reserve handover memory from the heap");
    }
    if !valid_reservation(base, kernel_end, arm_end, len) {
        return Err("handover allocation is outside the safe ARM RAM range");
    }
    let handover = Handover {
        base,
        stub_len: len,
    };
    // This allocation protects the current loader; the register handoff keeps
    // just its first page excluded from the next payload's fresh heap.
    install_stub(&handover);
    HANDOVER_BASE.store(base, Ordering::Release);
    cpu::sev();

    let deadline = time::uptime() + core::time::Duration::from_secs(1);
    while !handover.all_parked() {
        if time::uptime() >= deadline {
            return Err("secondary parking timed out; payload will not be copied");
        }
        core::hint::spin_loop();
    }
    Ok(handover)
}

/// Publish the relocated instructions and per-core acknowledgement slots.
fn install_stub(handover: &Handover) {
    let start = (&raw const __chainload_stub_start) as usize;
    // SAFETY: the stub is `stub_len` bytes of read-only text; the destination is
    // an exclusively owned allocation checked by `valid_reservation`.
    unsafe {
        core::ptr::copy_nonoverlapping(
            start as *const u8,
            handover.base as *mut u8,
            handover.stub_len,
        );
        for core in 0..memory::CORE_COUNT {
            let flag = handover.base + ACK_OFFSET + core * CACHE_LINE;
            (flag as *mut AtomicUsize).write(AtomicUsize::new(0));
        }
    }
    // SAFETY: freshly written instructions are dirty in the D-cache and absent
    // from the I-cache. Clean each line to the point of coherency, then drop
    // the whole I-cache before anything branches there.
    unsafe {
        for line in (handover.base..handover.staging()).step_by(CACHE_LINE) {
            mmu::clean_invalidate_line(line);
        }
        core::arch::asm!("dsb sy", "ic iallu", "dsb sy", "isb");
    }
}

/// Ask for a kernel and read it into the staging buffer.
fn receive(handover: &Handover) -> &[u8] {
    for _ in 0..3 {
        console::with_writer(|uart| uart.write_byte(REQUEST_TOKEN));
    }
    console::flush();

    let mut length = [0u8; 4];
    for byte in &mut length {
        *byte = read_blocking();
    }
    let length = u32::from_le_bytes(length) as usize;

    if length == 0 || length > MAX_PAYLOAD {
        println!();
        println!("Refusing a {length} byte payload (limit {MAX_PAYLOAD}). Reset the board.");
        cpu::halt()
    }

    console::with_writer(|uart| {
        uart.write_byte(b'O');
        uart.write_byte(b'K');
    });
    console::flush();

    let buffer = handover.staging() as *mut u8;
    for index in 0..length {
        // SAFETY: `index < length <= MAX_PAYLOAD`, so this stays inside the
        // staging region, which is inside our checked heap reservation.
        unsafe { buffer.add(index).write_volatile(read_blocking()) };
    }
    for index in length..length.next_multiple_of(8) {
        // SAFETY: MAX_PAYLOAD is 8-byte aligned, so padding remains reserved.
        unsafe { buffer.add(index).write_volatile(0) };
    }

    // SAFETY: `length` bytes starting at the staging address were just written, and
    // the buffer outlives everything that reads it.
    let payload = unsafe { core::slice::from_raw_parts(buffer as *const u8, length) };

    // An ELF instead of a flat image hangs with no output: the stub branches to
    // 0x80000 and executes the `7F E L F` header. Say so instead.
    if payload.starts_with(b"\x7fELF") {
        println!();
        println!("That is an ELF, not a flat image. Rebuild it with:");
        println!("  cargo image --example <name> -- -O binary kernel8.img");
        cpu::halt()
    }

    payload
}

/// Spin until the UART yields a byte.
///
/// No timeout: a half-sent image should stall visibly rather than boot
/// something truncated.
fn read_blocking() -> u8 {
    loop {
        if let Some(byte) = console::read_byte() {
            return byte;
        }
        core::hint::spin_loop();
    }
}

/// Hand control to the stub, which moves the payload into place and enters it.
///
/// # Safety
///
/// Never returns, and destroys this kernel's image on the way. Every core that
/// is not parked outside the load region must already be stopped.
unsafe fn jump_to_payload(handover: &Handover, payload: &[u8]) -> ! {
    let source = payload.as_ptr() as usize;
    let length = payload.len().next_multiple_of(8);

    // The payload was written through the D-cache; the stub copies it with the
    // cache off, so it has to be in DRAM first.
    // SAFETY: the range was written by `receive` and is mapped Normal memory.
    unsafe {
        for line in (source..source + length).step_by(64) {
            mmu::clean_invalidate_line(line);
        }
        core::arch::asm!("dsb sy", "isb");
    }

    // The staging buffer is not the only thing that has to reach DRAM: this
    // core is about to disable its data cache, and every line it leaves dirty
    // -- the loader's own stack and statics, and anything still cached for the
    // load address -- gets written back eventually, over the payload.
    //
    // SAFETY: every secondary acknowledged from relocated code with caches
    // off. The assembly helper can therefore clean shared L2, disable the MMU
    // and branch to the installed stub without dirtying another stack line.
    unsafe {
        leave_mmu_and_branch(
            handover.base,
            source,
            memory::KERNEL_LOAD_ADDR,
            length,
            true,
        )
    }
}

/// Turn the MMU and both caches off, then branch, without touching memory in
/// between.
///
/// The stub cannot do this itself: the MMU maps DRAM outside `.text` as
/// execute-never, so branching to the reservation with translation still enabled
/// takes a permission fault before the first instruction runs.
///
/// It must be one `noreturn` asm block rather than a function that returns:
/// once the D-cache is off the stack is read straight from DRAM while the live
/// frame may still be dirty in a cache line, so there must be no stack access
/// between the write to `SCTLR_EL1` and the branch.
///
/// # Safety
///
/// Never returns. Everything the target will read must already have been
/// cleaned to the point of coherency, and `target` must be a valid physical
/// address holding position-independent code.
/// If `all_caches` is true, every secondary must have acknowledged parking
/// with its caches off before this call touches the shared L2.
unsafe fn leave_mmu_and_branch(
    target: usize,
    x0: usize,
    x1: usize,
    x2: usize,
    all_caches: bool,
) -> ! {
    // SAFETY: the assembly entry performs the final cache maintenance after
    // all compiler-generated stack accesses; the caller guarantees its range
    // and cross-core preconditions.
    unsafe { __chainload_leave(target, x0, x1, x2, usize::from(all_caches)) }
}

fn refuse(error: &str) -> ! {
    println!();
    println!("Chain-load refused: {error}. Reset the board.");
    console::flush();
    cpu::halt()
}

/// CRC-32/ISO-HDLC, so a truncated or corrupted push is visible. Nibble-table
/// form: 16 entries instead of 256.
fn crc32(data: &[u8]) -> u32 {
    const TABLE: [u32; 16] = [
        0x0000_0000,
        0x1db7_1064,
        0x3b6e_20c8,
        0x26d9_30ac,
        0x76dc_4190,
        0x6b6b_51f4,
        0x4db2_6158,
        0x5005_713c,
        0xedb8_8320,
        0xf00f_9344,
        0xd6d6_a3e8,
        0xcb61_b38c,
        0x9b64_c2b0,
        0x86d3_d2d4,
        0xa00a_e278,
        0xbdbd_f21c,
    ];

    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc = TABLE[((crc ^ u32::from(byte)) & 0x0f) as usize] ^ (crc >> 4);
        crc = TABLE[((crc ^ (u32::from(byte) >> 4)) & 0x0f) as usize] ^ (crc >> 4);
    }
    !crc
}
