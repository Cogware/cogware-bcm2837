// SPDX-License-Identifier: MIT OR Apache-2.0
//! `Box`, `Vec` and `String` on bare metal, and what the allocator behind them
//! is doing.
//!
//! Run with `cargo run --release --example heap`.
//!
//! The BSP installs a `#[global_allocator]` and initialises the arena during
//! boot, so `alloc` works without any `init` call here; this example only
//! exercises the allocator and reads the counters back.
//!
//! The design is a **segregated free list over a bump pointer**:
//!
//! * `init` is two stores: no arena to zero and no metadata to build.
//! * `alloc` rounds the request up to a power-of-two size class, then either
//!   pops that class's free list or bumps the pointer. Both O(1), no search.
//! * `dealloc` pushes the block back onto its class's list. Also O(1).
//!
//! Blocks are never split or coalesced, so a 33-byte request occupies 64 bytes
//! and memory freed from one size class is never returned to another. The
//! `live` figure below is therefore in whole blocks, not requested bytes.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::{boxed::Box, collections::BTreeMap, string::String, vec::Vec};

use cogware_bcm2837::{BootInfo, cpu, entry, heap, println, time};

entry!(main);

fn main(_boot: BootInfo) -> ! {
    println!();
    println!("Heap demo.");

    let start = heap::stats();
    println!(
        "  arena       : {:#012x}..{:#012x} ({} MiB)",
        start.start,
        start.end,
        start.capacity() / (1024 * 1024)
    );
    println!(
        "  size classes: {} B .. {} MiB ({} classes, powers of two)",
        heap::MIN_SIZE,
        heap::MAX_SIZE / (1024 * 1024),
        heap::CLASS_COUNT
    );
    println!("  ready       : {}", heap::is_ready());
    println!("  after boot  : {} bytes live", start.live);

    collections();
    reuse();
    fragmentation_cost();

    report("final");

    println!();
    println!("Done in {:?}.", time::uptime());
    cpu::halt()
}

/// Exercises the `alloc` collections.
fn collections() {
    println!();
    println!("-- collections --");

    let boxed = Box::new(0xcafe_2837_u64);
    println!("  Box<u64>     : {:#x} at {:p}", *boxed, &raw const *boxed);

    let mut squares: Vec<u64> = Vec::new();
    for n in 1..=16u64 {
        squares.push(n * n);
    }
    println!(
        "  Vec<u64>     : len {}, cap {}, sum {}",
        squares.len(),
        squares.capacity(),
        squares.iter().sum::<u64>()
    );

    let mut text = String::from("cogware");
    text.push_str("-bcm2837");
    text.make_ascii_uppercase();
    println!("  String       : {text:?} ({} bytes)", text.len());

    // A node-based container: every insert is its own allocation.
    let mut map = BTreeMap::new();
    for (index, name) in ["gpio", "uart", "mailbox", "emmc", "framebuffer"]
        .iter()
        .enumerate()
    {
        map.insert(*name, index);
    }
    println!("  BTreeMap     : {} entries, first {:?}", map.len(), {
        map.first_key_value()
    });

    // f64 in a heap collection: checks hard-float values survive a round trip
    // through `alloc`.
    let harmonic: Vec<f64> = (1..=4).map(|n| 1.0 / f64::from(n)).collect();
    println!("  Vec<f64>     : H(4) = {}", harmonic.iter().sum::<f64>());

    report("after collections");
}

/// Frees feed the free lists; the next allocation of that class pops one
/// instead of touching new memory.
fn reuse() {
    println!();
    println!("-- reuse --");

    let before = heap::stats();

    // Allocate and drop a hundred blocks of the same class.
    for _ in 0..100 {
        let block: Vec<u8> = Vec::with_capacity(48);
        core::hint::black_box(&block);
    }

    let after = heap::stats();
    println!(
        "  100 x 48 B alloc/free: {} allocation(s), {} served from a free list",
        after.allocs - before.allocs,
        after.reused - before.reused
    );
    println!(
        "  new memory touched   : {} bytes",
        after.watermark - before.watermark
    );
    println!("  cached on free lists : {} bytes", after.cached());
    println!("  (the same 64 B block is handed straight back out, 100 times over)");
}

/// The rounding waste of power-of-two size classes.
fn fragmentation_cost() {
    println!();
    println!("-- rounding --");
    println!("  request   class   waste");

    for request in [1usize, 16, 17, 48, 65, 512, 1000] {
        let class = request.max(heap::MIN_SIZE).next_power_of_two();
        println!(
            "  {request:>7}   {class:>5}   {:>5}",
            class.saturating_sub(request)
        );
    }
}

fn report(label: &str) {
    let stats = heap::stats();
    println!();
    println!("  [{label}]");
    println!(
        "    live {} B, peak {} B, cached {} B, untouched {} MiB",
        stats.live,
        stats.peak,
        stats.cached(),
        stats.untouched() / (1024 * 1024)
    );
    println!(
        "    allocs {}, deallocs {}, reused {}, grown in place {}, failed {}",
        stats.allocs, stats.deallocs, stats.reused, stats.grown_in_place, stats.failed
    );
}
