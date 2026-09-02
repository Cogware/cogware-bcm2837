// SPDX-License-Identifier: MIT OR Apache-2.0
//! Releasing cores 1-3 and sharing state between them.
//!
//! Run with `cargo run --release --example multicore`.
//!
//! Cores do not start on their own: the BSP parks them at the MMU gate until
//! [`smp::start_secondaries`] opens it, which is what keeps single-core
//! examples safe on a machine where all four cores leave reset together.
//!
//! Once running, they demonstrate:
//!
//! * a [`SpinLock`] shared by all four cores, as every driver in this BSP is;
//! * a [`Barrier`], without which core 0 would report the tally while the
//!   others are still incrementing it.
//!
//! The accumulator is an `f64` so that the fourth harmonic number only comes
//! out right if each core did its reciprocal in double precision and the value
//! survived the lock round trip.
//!
//! Expect the last digit to move between runs: `f64` addition is not
//! associative and the cores accumulate in whatever order they win the lock, so
//! order `0,1,2,3` yields 2.083333333333333 while `0,1,3,2` yields
//! 2.0833333333333335. QEMU releases the cores deterministically and tends to
//! print the same value every time; real silicon does not. Hence the 1e-12
//! tolerance in the check below.

#![no_std]
#![no_main]

use cogware_bcm2837::{
    BootInfo, bsp::memory, cprintln, cpu, entry, println, smp, sync::Barrier, sync::SpinLock, time,
};

entry!(primary = main, secondary = worker);

/// State every core mutates.
struct Tally {
    visits: u64,
    per_core: [u64; memory::CORE_COUNT],
    /// Each core adds `1/(id + 1)`, so a full pass leaves the fourth harmonic
    /// number, 2.0833...
    harmonic: f64,
}

static TALLY: SpinLock<Tally> = SpinLock::new(Tally {
    visits: 0,
    per_core: [0; memory::CORE_COUNT],
    harmonic: 0.0,
});

/// Rendezvous point: every core arrives before core 0 reports.
static BOOT_BARRIER: Barrier = Barrier::new(memory::CORE_COUNT);

fn main(boot: BootInfo) -> ! {
    println!();
    println!("Multicore demo - core {} is the primary.", boot.core_id);
    println!("Releasing cores 1-{}.", memory::CORE_COUNT - 1);
    println!();

    // SAFETY: core 0, called once, with the console, MMU and the shared
    // statics above all initialised.
    unsafe { smp::start_secondaries() };

    let online = smp::wait_for_cores(memory::CORE_COUNT, 1_000_000);
    if online != memory::CORE_COUNT {
        println!();
        println!("Only {online}/{} cores checked in.", memory::CORE_COUNT);
        for core in 1..memory::CORE_COUNT {
            println!("  spin slot {core}: {:#018x}", smp::spin_slot_value(core));
        }
        cpu::halt();
    }

    visit(boot.core_id);

    // Everyone has had their turn before the snapshot is taken.
    BOOT_BARRIER.wait();

    let tally = TALLY.lock();
    println!();
    println!("All {online} cores online after {:?}.", time::uptime());
    println!("  visits : {}", tally.visits);
    for (core, count) in tally.per_core.iter().enumerate() {
        println!("  core {core}: {count}");
    }
    println!("  H({}) : {}", memory::CORE_COUNT, tally.harmonic);
    println!(
        "  FPU across cores: {}",
        if (tally.harmonic - 2.083_333_333_333_333_3).abs() < 1e-12 {
            "correct"
        } else {
            "WRONG"
        }
    );
    drop(tally);

    println!();
    println!("Done.");
    cpu::halt()
}

fn worker(boot: BootInfo) -> ! {
    cprintln!(
        "online at EL{}, MMU {}, FPU {}",
        boot.exception_level,
        on_off(boot.mmu_enabled),
        on_off(boot.fpu_enabled)
    );

    visit(boot.core_id);

    BOOT_BARRIER.wait();
    cpu::halt()
}

/// Takes the shared lock and mutates. Called by all four cores.
fn visit(core_id: usize) {
    let mut tally = TALLY.lock();
    tally.visits += 1;
    tally.per_core[core_id] += 1;
    tally.harmonic += 1.0 / (core_id as f64 + 1.0);
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}
