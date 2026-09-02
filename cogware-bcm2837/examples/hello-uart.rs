// SPDX-License-Identifier: MIT OR Apache-2.0
//! The smallest useful kernel: bring the machine up and say so.
//!
//! Run with `cargo run --release --example hello-uart`.
//!
//! Everything printed here is read back from the hardware *after* the BSP set
//! it, so a clean run self-checks the boot path: EL1, hard-float FPU, MMU,
//! both caches and the exception vector table.
//!
//! Cores 1-3 are left parked at the MMU gate - see the `multicore` example for
//! how to release them.

#![no_std]
#![no_main]

use cogware_bcm2837::{BootInfo, cpu, drivers, entry, println, time};

entry!(main);

fn main(boot: BootInfo) -> ! {
    println!();
    println!("Hello from the BCM2837.");
    println!();
    println!("  core            : {}", boot.core_id);
    println!("  exception level : EL{}", boot.exception_level);
    println!("  MPIDR_EL1       : {:#018x}", boot.mpidr);
    println!("  FP/SIMD         : {}", yes_no(boot.fpu_enabled));
    println!("  MMU             : {}", yes_no(boot.mmu_enabled));
    println!("  D-cache         : {}", yes_no(boot.data_cache_enabled));
    println!(
        "  I-cache         : {}",
        yes_no(boot.instruction_cache_enabled)
    );
    println!("  VBAR_EL1        : {:#018x}", boot.vector_table);

    match boot.videocore_memory {
        Ok((base, size)) => println!(
            "  VideoCore       : {} MiB at {base:#012x}",
            size / (1024 * 1024)
        ),
        Err(error) => println!("  VideoCore       : unavailable ({error})"),
    }

    // Reported, not raised. The firmware boots the cores at their minimum and
    // leaves them there, so this line is often the only thing that says a
    // kernel is running at half the speed of the part it is on. Acting on it is
    // `Mailbox::set_clock_rate`, and it is this program's decision, not the
    // BSP's.
    match boot.arm_clock {
        Ok(clock) if clock.at_maximum() => {
            println!(
                "  ARM clock       : {} MHz (at maximum)",
                clock.hz / 1_000_000
            )
        }
        Ok(clock) => println!(
            "  ARM clock       : {} MHz of a possible {} - see set_clock_rate",
            clock.hz / 1_000_000,
            clock.max_hz / 1_000_000
        ),
        Err(error) => println!("  ARM clock       : unavailable ({error})"),
    }

    throttled();

    println!();

    // A hard-float target means this is a real `fadd`, not a soft-float call.
    let harmonic: f64 = (1..=4).map(|n| 1.0 / f64::from(n)).sum();
    println!("  FPU check       : H(4) = {harmonic}");

    println!();
    if boot.healthy() {
        println!("Boot complete in {:?}. All checks passed.", time::uptime());
    } else {
        println!("Boot complete in {:?}, but a check FAILED.", time::uptime());
    }

    cpu::halt()
}

/// Asks the firmware whether it has throttled, using a tag the BSP does not
/// name.
///
/// `GET_THROTTLED` is not in [`Tag`]: the property interface is large and only
/// partly documented, and the BSP enumerates the parts it uses itself. Rather
/// than fork the crate to add one discriminant, a downstream kernel sends the
/// identifier directly.
///
/// [`Tag`]: cogware_bcm2837::drivers::mailbox::Tag
fn throttled() {
    /// `GET_THROTTLED`, which QEMU does not implement.
    const GET_THROTTLED: u32 = 0x0003_0046;
    /// Bits the firmware sets for a condition that is happening now.
    const ACTIVE: u32 = 0xF;

    let mut reply = [0u32; 1];
    match drivers::MAILBOX
        .lock()
        .call_raw(GET_THROTTLED, &[0], &mut reply)
    {
        Ok(words) if words >= 1 && reply[0] & ACTIVE == 0 => {
            println!("  throttling      : none (raw tag {GET_THROTTLED:#010x})")
        }
        Ok(words) if words >= 1 => println!("  throttling      : flags {:#06x}", reply[0]),
        Ok(_) | Err(_) => println!("  throttling      : not reported by this firmware"),
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}
