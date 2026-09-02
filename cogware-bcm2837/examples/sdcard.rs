// SPDX-License-Identifier: MIT OR Apache-2.0
//! A small block-device exerciser for the Raspberry Pi SD slot.
//!
//! Run with `cargo run --release --example sdcard`.
//!
//! QEMU needs a raw image attached with `-drive file=sd.img,if=sd,format=raw`.
//! Without one the example reports that no card was found and halts cleanly.
//!
//! The write test uses block 1: block 0 holds the MBR on partitioned cards and
//! must never be overwritten.
//!
//! Cores 1-3 are left parked at the MMU gate; this is a single-core hardware
//! smoke test for the EMMC driver.

#![no_std]
#![no_main]

use core::fmt::Write;

use cogware_bcm2837::{
    BootInfo, console, cpu,
    drivers::{self, emmc::BLOCK_SIZE},
    entry, println, time,
};

entry!(main);

const SCRATCH_BLOCK: u32 = 1;
const HEXDUMP_BYTES: usize = 64;

fn main(boot: BootInfo) -> ! {
    println!();
    println!("BCM2837 EMMC / SD card example.");
    println!("  core            : {}", boot.core_id);
    println!("  exception level : EL{}", boot.exception_level);
    println!("  boot health     : {}", pass_fail(boot.healthy()));
    println!();

    let Some(card_ok) = initialise_and_report() else {
        println!();
        println!("Summary: FAIL (no usable SD card).");
        cpu::halt();
    };

    let read_ok = read_mbr();
    let write_ok = write_read_verify();

    println!();
    if card_ok && read_ok && write_ok {
        println!("Summary: PASS.");
    } else {
        println!("Summary: FAIL.");
    }

    cpu::halt()
}

fn initialise_and_report() -> Option<bool> {
    println!("Initialising SD card...");
    let started = time::ticks();
    let init = drivers::EMMC.lock().init();
    let init_ticks = time::ticks() - started;

    if let Err(error) = init {
        println!("  init failed: {error}");
        println!("  attach an SD image with -drive file=...,if=sd,format=raw.");
        return None;
    }

    let (info, blocks) = {
        let emmc = drivers::EMMC.lock();
        (emmc.info(), emmc.block_count())
    };

    let Some(info) = info else {
        println!("  controller initialised but returned no card info");
        return Some(false);
    };

    let hz = time::frequency_hz().max(1);
    println!("  ready in        : {} us", init_ticks * 1_000_000 / hz);
    println!("  type            : {}", info.card_type.name());
    println!(
        "  capacity        : {} MiB ({} bytes)",
        info.capacity_bytes / (1024 * 1024),
        info.capacity_bytes
    );
    println!("  blocks          : {blocks} x {BLOCK_SIZE} bytes");
    println!("  CSD             : v{}", info.csd_version);
    println!("  OCR             : {:#010x}", info.ocr);
    println!("  RCA             : {:#010x}", info.rca);
    println!("  bus width       : {}-bit", info.bus_width);
    println!("  transfer clock  : driver normal rate");
    print_product(&info);

    Some(true)
}

fn print_product(info: &drivers::emmc::CardInfo) {
    console::with_writer(|w| {
        let _ = write!(w, "  product         : ");
        for byte in info.product_name {
            let ch = if byte.is_ascii_graphic() || byte == b' ' {
                byte as char
            } else {
                '.'
            };
            let _ = write!(w, "{ch}");
        }
        let _ = writeln!(
            w,
            " rev {}.{}, serial {:#010x}",
            info.product_revision_major, info.product_revision_minor, info.serial
        );
        let _ = writeln!(
            w,
            "  manufacturer    : {:#04x} (OEM {}{})",
            info.manufacturer_id, info.oem_id[0] as char, info.oem_id[1] as char
        );
        let _ = writeln!(
            w,
            "  manufactured    : {:04}-{:02}",
            info.manufacture_year, info.manufacture_month
        );
    });
}

fn read_mbr() -> bool {
    let mut block = [0u8; BLOCK_SIZE];
    let started = time::ticks();
    let read = drivers::EMMC.lock().read_blocks(0, &mut block);
    let read_ticks = time::ticks() - started;

    if let Err(error) = read {
        println!();
        println!("Read of block 0 failed: {error}");
        return false;
    }

    let hz = time::frequency_hz().max(1);
    println!();
    println!(
        "Block 0 read      : {BLOCK_SIZE} bytes in {} us",
        read_ticks * 1_000_000 / hz
    );
    println!("First {HEXDUMP_BYTES} bytes:");
    hexdump(&block[..HEXDUMP_BYTES]);

    let signature = u16::from_le_bytes([block[510], block[511]]);
    if signature == 0xAA55 {
        println!("MBR signature     : 0x55AA present.");
    } else {
        println!("MBR signature     : absent (got {signature:#06x}).");
    }

    true
}

fn write_read_verify() -> bool {
    let mut written = [0u8; BLOCK_SIZE];
    written[..8].copy_from_slice(b"COGWARE!");
    for (index, byte) in written.iter_mut().enumerate().skip(8) {
        *byte = (index as u8).wrapping_mul(31).wrapping_add(7);
    }

    let started = time::ticks();
    let write = drivers::EMMC.lock().write_blocks(SCRATCH_BLOCK, &written);
    let write_ticks = time::ticks() - started;
    if let Err(error) = write {
        println!();
        println!("Write to block {SCRATCH_BLOCK} failed: {error}");
        return false;
    }

    let mut read_back = [0u8; BLOCK_SIZE];
    let started = time::ticks();
    let read = drivers::EMMC
        .lock()
        .read_blocks(SCRATCH_BLOCK, &mut read_back);
    let read_ticks = time::ticks() - started;
    if let Err(error) = read {
        println!();
        println!("Read back of block {SCRATCH_BLOCK} failed: {error}");
        return false;
    }

    let hz = time::frequency_hz().max(1);
    println!();
    println!("Scratch write     : block {SCRATCH_BLOCK}, never block 0");
    println!(
        "  wrote           : {BLOCK_SIZE} bytes in {} us",
        write_ticks * 1_000_000 / hz
    );
    println!(
        "  read back       : {BLOCK_SIZE} bytes in {} us",
        read_ticks * 1_000_000 / hz
    );

    match written
        .iter()
        .zip(read_back.iter())
        .position(|(sent, got)| sent != got)
    {
        None => {
            println!("  verify          : ok, all {BLOCK_SIZE} bytes match");
            true
        }
        Some(offset) => {
            println!(
                "  verify          : MISMATCH at byte {offset}: wrote {:#04x}, read {:#04x}",
                written[offset], read_back[offset]
            );
            false
        }
    }
}

fn hexdump(bytes: &[u8]) {
    console::with_writer(|w| {
        for (index, chunk) in bytes.chunks(16).enumerate() {
            let _ = write!(w, "  {:04x}  ", index * 16);
            for byte in chunk {
                let _ = write!(w, "{byte:02x} ");
            }
            for _ in chunk.len()..16 {
                let _ = write!(w, "   ");
            }
            let _ = write!(w, " |");
            for byte in chunk {
                let ch = if byte.is_ascii_graphic() || *byte == b' ' {
                    *byte as char
                } else {
                    '.'
                };
                let _ = write!(w, "{ch}");
            }
            let _ = writeln!(w, "|");
        }
    });
}

fn pass_fail(value: bool) -> &'static str {
    if value { "pass" } else { "FAIL" }
}
