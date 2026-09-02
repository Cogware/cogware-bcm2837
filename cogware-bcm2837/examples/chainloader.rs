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
//! 20-instruction, position-independent stub to [`STUB_ADDR`]. Only the stub
//! has to survive the copy, and it touches nothing but registers.
//!
//! ```text
//!   0x0008_0000  payload lands here          <- overwritten
//!   0x0080_0000  the stub runs here          STUB_ADDR
//!   0x0100_0000  the payload waits here      STAGING_ADDR
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

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicUsize, Ordering};

use cogware_bcm2837::{BootInfo, arch::mmu, bsp::memory, console, cpu, entry, println, smp, time};

entry!(primary = main, secondary = secondary);

/// Where the relocating stub is copied to before it takes over.
///
/// 8 MiB: far above the kernel image and its stacks, far below [`STAGING_ADDR`],
/// and comfortably inside the DRAM the MMU maps as Normal memory.
const STUB_ADDR: usize = 0x0080_0000;

/// Where a received payload is buffered until it is moved into place.
const STAGING_ADDR: usize = 0x0100_0000;

/// Largest image accepted.
///
/// The real ceiling is the gap between the load address and [`STAGING_ADDR`];
/// this is well inside it and stops a corrupt length from being believed.
const MAX_PAYLOAD: usize = 8 * 1024 * 1024;

/// Sent three times to ask the host for a payload.
const REQUEST_TOKEN: u8 = 0x03;

/// How many secondaries have parked themselves inside the stub.
static PARKED: AtomicUsize = AtomicUsize::new(0);

// The stub. Position-independent by construction: no literal pools, no absolute
// symbol references, only registers and relative branches. It is copied
// verbatim to `STUB_ADDR`, so anything PC-relative outside this block would
// break.
core::arch::global_asm!(
    r#"
.section .text.chainload, "ax"
.balign 64

// x0 = source, x1 = destination, x2 = byte count, rounded up to 8.
.global __chainload_stub_start
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
    mov     x4, #0x80000
    br      x4

// Secondaries wait here while the image beneath them is replaced.
//
// This is not a dead-end park: it is a faithful copy of the firmware's spin
// table, so the *payload's* `start_secondaries()` releases these cores into the
// payload exactly as if the loader had never run. That is what keeps SMP alive
// across a chain-load.
.global __chainload_park
__chainload_park:
    mov     x5, #0xd8               // SPIN_TABLE_BASE
    mrs     x6, mpidr_el1
    and     x6, x6, #3

    // The slot still holds the loader's _start, which this core has already
    // taken. Clear it, or the load below succeeds immediately and the core
    // branches into an image that is about to be overwritten.
    str     xzr, [x5, x6, lsl #3]
    dsb     sy

    // `wfe` before the load, matching armstub8 and QEMU's write_smpboot64:
    // the release must therefore be followed by `sev`, which is what
    // `smp::start_secondaries` does.
1:  wfe
    ldr     x4, [x5, x6, lsl #3]
    cbz     x4, 1b
    br      x4

.global __chainload_stub_end
__chainload_stub_end:
"#
);

unsafe extern "C" {
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
    println!("  staging buffer  : {STAGING_ADDR:#010x}");
    println!("  relocation stub : {STUB_ADDR:#010x}");
    println!("  maximum image   : {} KiB", MAX_PAYLOAD / 1024);

    let stub_len = install_stub();
    println!("  stub installed  : {stub_len} bytes");
    park_secondaries();
    println!();
    println!("Waiting for a kernel. Push one with:");
    println!("  python3 scripts/minipush.py --payload kernel8.img");
    println!("  .\\scripts\\minipush.ps1 -Serial COM3 -Payload kernel8.img");
    println!();

    let payload = receive();

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
    unsafe { jump_to_payload(payload) }
}

fn secondary(_boot: BootInfo) -> ! {
    let park = STUB_ADDR + offset_of_park();
    PARKED.fetch_add(1, Ordering::Release);

    // This core is about to run with its data cache off. Write back whatever
    // it dirtied while the cache was on -- its stack, mostly -- or those lines
    // would be evicted later, on top of the payload.
    //
    // SAFETY: L1 is private to this core, so this leaves core 0 alone while it
    // carries on running with its caches enabled.
    unsafe { mmu::clean_invalidate_l1() };

    // SAFETY: the parking loop was installed and cleaned to DRAM before this
    // core was released, it is reached with no further memory access, and the
    // payload never overwrites it.
    unsafe { leave_mmu_and_branch(park, 0, 0, 0) }
}

/// Byte offset of the parking loop from the start of the stub.
fn offset_of_park() -> usize {
    (&raw const __chainload_park) as usize - (&raw const __chainload_stub_start) as usize
}

/// Copy the stub to [`STUB_ADDR`] and make it visible to the instruction fetch.
fn install_stub() -> usize {
    let start = (&raw const __chainload_stub_start) as usize;
    let end = (&raw const __chainload_stub_end) as usize;
    let len = end - start;

    // SAFETY: the stub is `len` bytes of read-only text; the destination is
    // mapped Normal memory that nothing else in this kernel uses. The regions
    // are megabytes apart, so they cannot overlap.
    unsafe {
        core::ptr::copy_nonoverlapping(start as *const u8, STUB_ADDR as *mut u8, len);
    }
    // SAFETY: freshly written instructions are dirty in the D-cache and absent
    // from the I-cache. Clean each line to the point of coherency, then drop
    // the whole I-cache before anything branches there.
    unsafe {
        for line in (STUB_ADDR..STUB_ADDR + len).step_by(64) {
            mmu::clean_invalidate_line(line);
        }
        core::arch::asm!("dsb sy", "ic iallu", "dsb sy", "isb");
    }
    len
}

/// Release cores 1-3 so they can park themselves out of the payload's way.
fn park_secondaries() {
    // SAFETY: called once, from the primary, after the MMU is enabled.
    unsafe { smp::start_secondaries() };
    smp::wait_for_cores(memory::CORE_COUNT, 1_000_000);

    // The gate opening is not the same event as a core reaching the stub, so
    // wait for the cores themselves to report. On real hardware none arrives,
    // because the firmware still owns them; the timeout is the normal path
    // there, not a failure.
    let deadline = time::uptime() + core::time::Duration::from_millis(50);
    while PARKED.load(Ordering::Acquire) < memory::CORE_COUNT - 1 && time::uptime() < deadline {
        core::hint::spin_loop();
    }
    let parked = PARKED.load(Ordering::Acquire);
    println!(
        "  secondaries     : {parked} parked{}",
        if parked == 0 {
            " (firmware still holds them - expected on hardware)"
        } else {
            ""
        }
    );
}

/// Ask for a kernel and read it into the staging buffer.
fn receive() -> &'static [u8] {
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

    let buffer = STAGING_ADDR as *mut u8;
    for index in 0..length {
        // SAFETY: `index < length <= MAX_PAYLOAD`, so this stays inside the
        // staging region, which is mapped Normal memory owned by nothing else.
        unsafe { buffer.add(index).write_volatile(read_blocking()) };
    }

    // SAFETY: `length` bytes starting at `STAGING_ADDR` were just written, and
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
unsafe fn jump_to_payload(payload: &[u8]) -> ! {
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
    // SAFETY: the secondaries are parked inside the stub with their caches
    // off, so walking the shared L2 is safe at this point.
    unsafe { mmu::clean_invalidate_all() };

    // SAFETY: the stub is installed at `STUB_ADDR`, is position-independent,
    // and takes exactly these three arguments. It does not return.
    unsafe { leave_mmu_and_branch(STUB_ADDR, source, memory::KERNEL_LOAD_ADDR, length) }
}

/// Turn the MMU and both caches off, then branch, without touching memory in
/// between.
///
/// The stub cannot do this itself: the MMU maps DRAM outside `.text` as
/// execute-never, so branching to [`STUB_ADDR`] with translation still enabled
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
unsafe fn leave_mmu_and_branch(target: usize, x0: usize, x1: usize, x2: usize) -> ! {
    // SAFETY: guaranteed by the caller.
    //
    // `target` goes in an explicit register rather than `in(reg)`: the compiler
    // could otherwise pick x9, which the `mrs` below overwrites before the
    // branch reads it. x9 needs no clobber declaration because nothing follows
    // a `noreturn` block.
    unsafe {
        core::arch::asm!(
            "dsb sy",
            "isb",
            "mrs x9, sctlr_el1",
            "bic x9, x9, #(1 << 0)",    // M: address translation
            "bic x9, x9, #(1 << 2)",    // C: data cache
            "bic x9, x9, #(1 << 12)",   // I: instruction cache
            "msr sctlr_el1, x9",
            "isb",
            "tlbi vmalle1",
            "dsb sy",
            "isb",
            "br x4",
            in("x0") x0,
            in("x1") x1,
            in("x2") x2,
            in("x4") target,
            options(noreturn)
        )
    }
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
