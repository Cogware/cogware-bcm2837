// SPDX-License-Identifier: GPL-3.0-only
//! Interrupt delivery: per-core timer ticks, inter-processor interrupts, a
//! system-timer match and a DMA completion that wakes an awaiting task.
//!
//! Run with `cargo run --release --example interrupts`.
//!
//! Every core ticks at 100 Hz. After a fifth of a second core 0 checks each
//! core's tick count, interrupts each secondary, and awaits a DMA transfer
//! whose completion interrupt is what wakes it.

#![no_std]
#![no_main]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::{boxed::Box, vec};
use core::sync::atomic::{AtomicU32, Ordering};

use cogware_bcm2837::{
    BootInfo,
    arch::mmu,
    bsp::memory::{self, bus},
    cprintln,
    drivers::{
        dma::{self, ControlBlock, Kind, ti},
        interrupt, systimer,
    },
    entry, println, smp, task,
};

entry!(primary = main, secondary = worker);

const TICK_HZ: u32 = 100;

static COMPARE_HITS: AtomicU32 = AtomicU32::new(0);
static DMA_IRQS: AtomicU32 = AtomicU32::new(0);
static FAILURES: AtomicU32 = AtomicU32::new(0);

fn main(_boot: BootInfo) -> ! {
    println!();
    println!("Interrupt checks");
    println!();

    interrupt::init();
    let compare = systimer::Channel::Three;
    let ok = interrupt::register(compare.irq(), on_compare)
        .and_then(|()| interrupt::enable(compare.irq()));
    report(
        "register",
        ok.is_ok(),
        &format_args!("system timer IRQ {}: {ok:?}", compare.irq()),
    );
    systimer::set_compare_in(compare, 10_000);

    bring_up_core();

    // SAFETY: core 0, called once, after the console and MMU are up.
    unsafe { smp::start_secondaries() };
    let online = smp::wait_for_cores(memory::CORE_COUNT, 1_000_000);
    println!("  {online}/{} cores online", memory::CORE_COUNT);

    if let Err(error) = task::spawn("checks", checks()) {
        println!("could not spawn checks: {error}");
    }
    task::run()
}

fn worker(_boot: BootInfo) -> ! {
    bring_up_core();
    task::run()
}

/// What every core does for itself: tick, accept IPIs, unmask.
fn bring_up_core() {
    interrupt::start_tick(TICK_HZ);
    interrupt::enable_ipi();
    // SAFETY: `interrupt::init` ran on core 0 before any core gets here, and
    // the only handlers enabled take no lock.
    unsafe { interrupt::unmask() };
}

/// Re-arms itself every 10 ms, so the count tracks elapsed time.
fn on_compare() {
    systimer::set_compare_in(systimer::Channel::Three, 10_000);
    COMPARE_HITS.fetch_add(1, Ordering::Relaxed);
}

fn on_dma() {
    DMA_IRQS.fetch_add(1, Ordering::Relaxed);
    dma::handle_interrupt();
}

fn report(name: &str, ok: bool, detail: &dyn core::fmt::Display) {
    cprintln!("  {name:<9}: {} {detail}", if ok { "ok  " } else { "FAIL" });
    if !ok {
        FAILURES.fetch_add(1, Ordering::Relaxed);
    }
}

async fn checks() {
    let before: [u64; memory::CORE_COUNT] = core::array::from_fn(interrupt::tick_count);
    task::sleep_millis(200).await;

    // 200 ms at 100 Hz is twenty ticks; the window allows for start-up skew.
    for (core, before) in before.iter().enumerate() {
        let ticks = interrupt::tick_count(core) - before;
        report(
            "tick",
            (18..=22).contains(&ticks),
            &format_args!("core {core}: {ticks} ticks in 200 ms"),
        );
    }

    let hits = COMPARE_HITS.load(Ordering::Relaxed);
    report(
        "compare",
        hits >= 15,
        &format_args!("{hits} system timer matches"),
    );

    for core in 1..memory::CORE_COUNT {
        let before = interrupt::ipi_count(core);
        interrupt::send_ipi(core);
        task::sleep_millis(5).await;
        let after = interrupt::ipi_count(core);
        report(
            "ipi",
            after == before + 1,
            &format_args!("core {core}: {before} -> {after}"),
        );
    }

    dma_check().await;

    let spurious = interrupt::spurious_count();
    report(
        "spurious",
        spurious == 0,
        &format_args!("{spurious} unhandled"),
    );

    println!();
    match FAILURES.load(Ordering::Relaxed) {
        0 => println!("All interrupt checks passed."),
        n => println!("{n} interrupt check(s) FAILED."),
    }
}

/// A 16 KiB copy whose last block raises an interrupt.
async fn dma_check() {
    let Some(mut channel) = dma::claim(Kind::Full) else {
        report("dma", false, &"no full channel free");
        return;
    };
    let irq = channel.irq();
    let registered = interrupt::register(irq, on_dma).and_then(|()| interrupt::enable(irq));

    // On the heap: a task's future is limited to 1 KiB.
    let src: alloc::vec::Vec<u32> = (0..4096u32).map(|i| !i).collect();
    let mut dst = vec![0u32; 4096];
    let bytes = core::mem::size_of_val(src.as_slice());
    let block = Box::new(ControlBlock::new(
        ti::INTEN | ti::SRC_INC | ti::DEST_INC | ti::WAIT_RESP,
        bus::to_bus(src.as_ptr() as usize),
        bus::to_bus(dst.as_mut_ptr() as usize),
        bytes as u32,
    ));
    // SAFETY: both buffers are live for the whole function.
    unsafe {
        mmu::clean_invalidate_range(src.as_ptr() as usize, bytes);
        mmu::clean_invalidate_range(dst.as_ptr() as usize, bytes);
    }

    let irqs = DMA_IRQS.load(Ordering::Relaxed);
    // SAFETY: the block and buffers outlive the transfer, which is awaited
    // below, and nothing touches them until then; their caches were maintained
    // above.
    let result = match unsafe { channel.start(&block) } {
        Ok(()) => channel.done().await,
        Err(error) => Err(error),
    };
    // SAFETY: as above; drops lines prefetched during the transfer.
    unsafe { mmu::clean_invalidate_range(dst.as_ptr() as usize, bytes) };

    let woke = DMA_IRQS.load(Ordering::Relaxed) - irqs;
    let _ = interrupt::disable(irq);
    report(
        "dma",
        registered.is_ok() && result.is_ok() && src == dst && woke >= 1,
        &format_args!(
            "channel {}, IRQ {irq}: {result:?}, {woke} completion interrupt(s)",
            channel.index()
        ),
    );
}
