// SPDX-License-Identifier: GPL-3.0-only
//! # The reference kernel
//!
//! Everything the BSP exposes, in one bootable image, driven from an
//! interactive shell over the UART.
//!
//! Run with `cargo run --release --example shell`, then press `h` for help.
//!
//! The other examples each isolate one subsystem; this one is what a kernel
//! built on [`cogware_bcm2837`](https://codeberg.org/Cogware/cogware-bcm2837)
//! looks like once they are combined:
//!
//! * a printed memory map, with the MMIO, VideoCore and mailbox windows;
//! * exception and FPU self-tests;
//! * four cores sharing a [`SpinLock`] and meeting at a [`Barrier`];
//! * an async main loop on every core, with a shell task, a heartbeat and
//!   per-core workers;
//! * heap statistics and a live allocator self-test;
//! * framebuffer mode setting, a QOI logo and text drawn over it;
//! * SD card identification, block dump, and a read-back-verified write.
//!
//! Every API it uses is public, and the smaller examples show each one on its
//! own.

#![no_std]
#![no_main]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::{boxed::Box, string::String, vec::Vec};
use core::{
    future::Future,
    hint::black_box,
    sync::atomic::{AtomicU64, Ordering},
};

use cogware_bcm2837::{
    BootInfo,
    arch::{exception, mmu},
    bsp::{
        self,
        memory::{self, layout},
    },
    console, cprintln, cpu, debug, drivers,
    drivers::{
        GPIO, MAILBOX,
        gpio::Function,
        mailbox::{Clock, Error as MailboxError},
    },
    entry, heap,
    math::Float,
    print, println, smp,
    sync::{Barrier, SpinLock},
    task, time,
};

entry!(primary = main, secondary = worker_core);

/// Shared state every core mutates, to demonstrate cross-core synchronisation.
struct Tally {
    /// Total number of visits by all cores.
    visits: u64,
    /// Per-core visit counts.
    per_core: [u64; memory::CORE_COUNT],
    /// Running floating-point accumulator: each core adds `1/(id + 1)`, so a
    /// full boot leaves the fourth harmonic number, 2.0833...
    ///
    /// The value only comes out exact if every core performed its reciprocal
    /// in double precision *and* the `f64` survived the lock round trip
    /// untouched.
    harmonic: f64,
}

/// The contended structure. Any core may take it; none may hold it long.
static TALLY: SpinLock<Tally> = SpinLock::new(Tally {
    visits: 0,
    per_core: [0; memory::CORE_COUNT],
    harmonic: 0.0,
});

/// Rendezvous point at the end of bring-up.
///
/// Without it, core 0 would report the tally while the secondaries were still
/// incrementing it.
static BOOT_BARRIER: Barrier = Barrier::new(memory::CORE_COUNT);

/// Core 0's entry point.
///
/// The BSP has already brought up the console, the MMU, the caches, the
/// exception vectors, the heap and JTAG by the time this runs; `boot` reports
/// what it found.
fn main(boot: BootInfo) -> ! {
    let core_id = boot.core_id;

    println!();
    println!("Cogware BCM2837 booting on {}", bsp::BOARD_NAME);
    println!(
        "  core {core_id}, EL{}, MPIDR_EL1 {:#018x}",
        boot.exception_level, boot.mpidr
    );
    println!("  FP/SIMD ungated: {}", boot.fpu_enabled);

    match boot.videocore_memory {
        Ok((base, size)) => println!(
            "  VideoCore reserves {} MiB at {base:#012x}",
            size / (1024 * 1024)
        ),
        Err(error) => println!("  VideoCore memory probe failed ({error}); assuming defaults"),
    }

    println!(
        "  translation tables built at {:#018x}",
        boot.translation_table
    );
    println!(
        "  MMU on, D-cache {}, I-cache {}",
        on_off(boot.data_cache_enabled),
        on_off(boot.instruction_cache_enabled)
    );
    println!("  VBAR_EL1 = {:#018x}", boot.vector_table);

    print_memory_map();

    debug::jtag::report();

    self_test_exceptions();
    self_test_fpu(core_id);

    println!();
    println!(
        "Releasing secondary cores (spin table {:#06x} + MMU gate)",
        memory::SPIN_TABLE_BASE
    );

    // SAFETY: core 0, once, with the console, MMU and shared statics all up.
    unsafe { smp::start_secondaries() };

    let online = smp::wait_for_cores(memory::CORE_COUNT, 1_000_000);
    if online == memory::CORE_COUNT {
        println!("All {online} cores online.");
    } else {
        println!("Only {online}/{} cores reported in.", memory::CORE_COUNT);
        for core in 1..memory::CORE_COUNT {
            println!("  spin slot {core}: {:#018x}", smp::spin_slot_value(core));
        }
    }

    visit_shared_state(core_id);

    // Let the secondaries finish before snapshotting the tally. Skipped if a
    // core never checked in, because then the barrier would never release.
    if online == memory::CORE_COUNT {
        BOOT_BARRIER.wait();
    }

    report_tally();

    println!();
    println!("Boot complete in {:?}.", time::uptime());
    print_help();

    spawn_or_report("shell", shell());
    spawn_or_report("heartbeat", heartbeat());

    // Core 0 becomes an executor. Everything from here on is a task.
    task::run()
}

/// Entry point for cores 1-3, once [`smp::start_secondaries`] releases them.
///
/// The BSP has already taken this core through the MMU gate, enabled its MMU
/// and caches, installed its vector table and marked it online.
fn worker_core(boot: BootInfo) -> ! {
    let core_id = boot.core_id;

    cprintln!(
        "online at EL{}, MPIDR_EL1 {:#018x}, MMU {}, FPU {}",
        boot.exception_level,
        boot.mpidr,
        on_off(boot.mmu_enabled),
        on_off(boot.fpu_enabled)
    );

    self_test_fpu(core_id);
    visit_shared_state(core_id);

    let (bottom, top) = layout::core_stack(core_id);
    cprintln!(
        "stack {bottom:#018x}..{top:#018x}, sp now {:#018x}",
        cpu::stack_pointer()
    );

    // Everything this core wanted to say has been said; let core 0 report.
    BOOT_BARRIER.wait();

    spawn_or_report("worker", worker(core_id));

    // Instead of `cpu::halt()`: the core sleeps in `wfe`, and a `sev` from
    // another core's `send` puts it back to work.
    task::run()
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

/// Formats a byte count as a value and a binary unit.
///
/// Returns the pieces rather than a `String`, so it stays usable on the boot
/// path before [`heap::init`] has run. The value is truncated to the largest
/// unit that yields a non-zero figure; the exact bounds are always visible in
/// the start and end columns.
fn human_size(bytes: usize) -> (usize, &'static str) {
    const KIB: usize = 1024;
    const MIB: usize = KIB * 1024;
    const GIB: usize = MIB * 1024;

    if bytes >= GIB {
        (bytes / GIB, "GiB")
    } else if bytes >= MIB {
        (bytes / MIB, "MiB")
    } else if bytes >= KIB {
        (bytes / KIB, "KiB")
    } else {
        (bytes, "B  ")
    }
}

/// Width of the trailing description column in both boot-time tables.
const NAME_WIDTH: usize = 30;

/// Horizontal rule for the physical memory map.
const MAP_RULE: &str = "  +--------------+--------------+---------+-----------+------+--------------------------------+";

/// Horizontal rule for the kernel image table.
const IMAGE_RULE: &str =
    "  +--------------+--------------+---------+--------------------------------+";

/// Row labels for the per-core stacks.
///
/// A fixed array rather than formatted strings, so the column stays aligned
/// without the allocator. The length is checked against [`memory::CORE_COUNT`]
/// by the compiler.
const CORE_STACK_NAMES: [&str; memory::CORE_COUNT] = [
    "core 0 stack",
    "core 1 stack",
    "core 2 stack",
    "core 3 stack",
];

fn print_memory_map() {
    // One lock for both tables, so another core cannot split a row.
    console::with_writer(|w| {
        use core::fmt::Write;

        let _ = writeln!(w);
        let _ = writeln!(w, "Physical memory map");
        let _ = writeln!(w, "{MAP_RULE}");
        let _ = writeln!(
            w,
            "  | {:<12} | {:<12} | {:>7} | {:<9} | {:<4} | {:<NAME_WIDTH$} |",
            "start", "end", "size", "type", "perm", "region"
        );
        let _ = writeln!(w, "{MAP_RULE}");

        for region in mmu::memory_map() {
            let (size, unit) = human_size(region.end.saturating_sub(region.start));
            let permissions = match (region.access, region.execute) {
                (mmu::Access::ReadWrite, mmu::Execute::Allowed) => "RW X",
                (mmu::Access::ReadWrite, mmu::Execute::Never) => "RW  ",
                (mmu::Access::ReadOnly, mmu::Execute::Allowed) => "RO X",
                (mmu::Access::ReadOnly, mmu::Execute::Never) => "RO  ",
            };

            let _ = writeln!(
                w,
                "  | {:#012x} | {:#012x} | {:>3} {} | {:<9} | {} | {:<NAME_WIDTH$} |",
                region.start,
                region.end,
                size,
                unit,
                region.kind.label(),
                permissions,
                region.name
            );
        }

        let _ = writeln!(w, "{MAP_RULE}");
        let _ = writeln!(
            w,
            "  Device = nGnRnE strongly ordered.  Normal-NC = shared with the VideoCore."
        );
        let _ = writeln!(
            w,
            "  VideoCore split {}: {} MiB reserved at {:#012x}.",
            if memory::vc_mem_probed() {
                "from firmware"
            } else {
                "assumed (probe failed)"
            },
            memory::vc_mem_size() / (1024 * 1024),
            memory::vc_mem_base()
        );

        let _ = writeln!(w);
        let _ = writeln!(w, "Kernel image");
        let _ = writeln!(w, "{IMAGE_RULE}");
        let _ = writeln!(
            w,
            "  | {:<12} | {:<12} | {:>7} | {:<NAME_WIDTH$} |",
            "start", "end", "size", "section"
        );
        let _ = writeln!(w, "{IMAGE_RULE}");

        // `__data_end` is the page-aligned end of `.data` *and* `.bss`, so the
        // two are listed by their own bounds rather than from that symbol.
        let mut rows: [(&str, usize, usize); 6 + memory::CORE_COUNT] = [
            (".text", layout::text_start(), layout::text_end()),
            (".rodata", layout::rodata_start(), layout::rodata_end()),
            (".data", layout::data_start(), layout::bss_start()),
            (".bss", layout::bss_start(), layout::bss_end()),
            (".stacks", layout::stacks_start(), layout::stacks_end()),
            ("whole image", layout::kernel_start(), layout::kernel_end()),
            ("", 0, 0),
            ("", 0, 0),
            ("", 0, 0),
            ("", 0, 0),
        ];

        for core in 0..memory::CORE_COUNT {
            let (bottom, top) = layout::core_stack(core);
            rows[6 + core] = (CORE_STACK_NAMES[core], bottom, top);
        }

        for (name, start, end) in rows {
            let (size, unit) = human_size(end.saturating_sub(start));
            let _ = writeln!(
                w,
                "  | {:#012x} | {:#012x} | {:>3} {} | {:<NAME_WIDTH$} |",
                start, end, size, unit, name
            );
        }

        let _ = writeln!(w, "{IMAGE_RULE}");
    });
}

/// Takes a synchronous exception on purpose and returns from it.
fn self_test_exceptions() {
    println!();
    println!("Exception self-test: executing BRK #0 ...");
    exception::trigger_breakpoint();
    println!("... returned from the handler, vector table works.");
}

/// Exercises the FPU with values the optimiser cannot fold away.
///
/// On the hard-float target this `f64` arithmetic compiles to `fmul`/`fadd`/
/// `fsqrt` on `d` registers, not to soft-float calls.
fn self_test_fpu(core_id: usize) {
    let base = black_box(core_id as f64 + 1.0);
    let scaled = black_box(base * core::f64::consts::PI);
    let root = black_box(scaled.sqrt());
    let round_trip = black_box(root * root);

    cprintln!("FPU: pi * {base} = {scaled}, sqrt = {root}, squared back = {round_trip}");

    assert!(
        (round_trip - scaled).abs() < 1e-9,
        "FPU round trip failed on core {core_id}"
    );
}

/// Mutates the shared tally and pokes the shared GPIO block, both under lock.
fn visit_shared_state(core_id: usize) {
    // Two different locks, taken one at a time and never nested.
    let uart_pin_function = GPIO.lock().function(14);

    TALLY.with(|tally| {
        tally.visits += 1;
        tally.per_core[core_id] += 1;
        tally.harmonic += (core_id as f64 + 1.0).recip();
    });

    cprintln!(
        "shared GPIO read under lock: GPIO14 FSEL=0b{uart_pin_function:03b} ({})",
        if uart_pin_function == Function::Alt0 as u32 {
            "ALT0/TXD0"
        } else {
            "unexpected"
        }
    );
}

fn report_tally() {
    let (visits, per_core, harmonic) =
        TALLY.with(|tally| (tally.visits, tally.per_core, tally.harmonic));

    println!();
    println!("Shared tally (SpinLock<Tally>)");
    println!("  visits    : {visits}");
    for (core, count) in per_core.iter().enumerate() {
        println!("  core {core}    : {count}");
    }
    println!("  harmonic  : {harmonic}");
}

/// Rule for the async task table.
const TASK_RULE: &str = "  +------+------+----------------------+------------+---------+";

/// A unit of work handed to the worker task on a secondary core.
#[derive(Clone, Copy, Debug)]
struct Job {
    /// Sequence number, and the term of the harmonic series to add.
    seq: u32,
}

/// Work queue drained by every secondary core's worker task.
///
/// One static, shared by four cores, with no allocator behind it: the ring
/// buffer lives in `.bss` and the wait queues are two `u32`s.
static JOBS: task::Channel<Job, 16> = task::Channel::new();

/// Results accumulated by the workers.
struct Results {
    /// Jobs finished since boot.
    completed: u32,
    /// Sum of `1/(seq + 1)` over every completed job.
    total: f64,
    /// Jobs finished per core.
    by_core: [u32; memory::CORE_COUNT],
}

/// Shared results, behind the *async* mutex.
///
/// [`sync::SpinLock`] would also work here, because no holder awaits. The
/// async [`task::Mutex`] is used deliberately: it stays correct when a future
/// does need to await while holding the lock.
static RESULTS: task::Mutex<Results> = task::Mutex::new(Results {
    completed: 0,
    total: 0.0,
    by_core: [0; memory::CORE_COUNT],
});

/// The task each secondary core runs forever: take a job, do it, report it.
async fn worker(core_id: usize) {
    loop {
        let job = JOBS.recv().await;

        // Hard-float on a core other than the one that computed the boot
        // harmonic: the FPU is ungated everywhere, not just on core 0.
        let term = 1.0 / f64::from(job.seq + 1);

        let completed = {
            let mut results = RESULTS.lock().await;
            results.completed += 1;
            results.total += term;
            results.by_core[core_id] += 1;
            results.completed
        };

        // Build the line on the heap before printing it: three secondary cores
        // allocating and freeing concurrently with core 0's shell is what
        // exercises the heap's `SpinLock` cross-core.
        let line = alloc::format!("job {:>2} -> {term:.8} (completed {completed})", job.seq);
        cprintln!("{line}");
    }
}
/// A silent periodic task, showing the timer wheel turns without an interrupt.
async fn heartbeat() {
    loop {
        task::sleep_millis(250).await;
        HEARTBEATS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Number of heartbeat ticks since boot.
static HEARTBEATS: AtomicU64 = AtomicU64::new(0);

/// Console command `w`: feed the workers and wait for them to finish.
async fn dispatch_jobs() {
    /// Jobs sent per invocation. More than the channel holds, so the sender
    /// has to park and wait for room at least once.
    const COUNT: u32 = 24;

    let target = RESULTS.lock().await.completed + COUNT;

    println!();
    println!(
        "Dispatching {COUNT} jobs to {} worker cores...",
        memory::CORE_COUNT - 1
    );

    for seq in 0..COUNT {
        JOBS.send(Job { seq }).await;
    }

    // Bounded, so a wedged worker costs a second rather than the console.
    let deadline = time::ticks().saturating_add(time::frequency_hz());
    loop {
        let completed = RESULTS.lock().await.completed;
        if completed >= target || time::ticks() > deadline {
            break;
        }
        task::next_tick().await;
    }

    let results = RESULTS.lock().await;
    println!();
    println!("  completed : {}/{target}", results.completed);
    println!("  harmonic  : {}", results.total);
    for core in 1..memory::CORE_COUNT {
        println!("  core {core}    : {} jobs", results.by_core[core]);
    }
}

/// Console command `k`: what every core's executor is doing.
fn report_tasks() {
    println!();
    println!("Async tasks");
    println!("{TASK_RULE}");
    println!(
        "  | {:^4} | {:^4} | {:<20} | {:>10} | {:^7} |",
        "core", "slot", "task", "polls", "state"
    );
    println!("{TASK_RULE}");

    for core in 0..memory::CORE_COUNT {
        let Some(executor) = task::executor_for(core) else {
            continue;
        };

        for slot in 0..task::TASKS_PER_CORE {
            let Some(info) = executor.slot_info(slot) else {
                continue;
            };

            let state = if info.ready {
                "ready"
            } else if info.polling {
                "polling"
            } else {
                "parked"
            };

            println!(
                "  | {core:^4} | {slot:^4} | {:<20} | {:>10} | {state:^7} |",
                info.name, info.polls
            );
        }
    }

    println!("{TASK_RULE}");

    for core in 0..memory::CORE_COUNT {
        let Some(executor) = task::executor_for(core) else {
            continue;
        };
        println!(
            "  core {core}: {} task(s), {} idle wakeups",
            executor.task_count(),
            executor.idle_count()
        );
    }

    println!(
        "  heartbeats: {} (250 ms each)",
        HEARTBEATS.load(Ordering::Relaxed)
    );
    println!("  queue     : {}/{} job(s)", JOBS.len(), JOBS.capacity());
}

/// Spawns a task and reports failure.
fn spawn_or_report(name: &'static str, future: impl Future<Output = ()> + Send + 'static) {
    match task::spawn(name, future) {
        Ok(id) => cprintln!("spawned '{name}' in slot {}", id.slot()),
        Err(error) => cprintln!("could not spawn '{name}': {error}"),
    }
}

fn print_help() {
    println!();
    println!("Console commands");
    println!("  h  this help          m  memory map");
    println!("  s  SMP status         j  JTAG status");
    println!("  t  uptime             c  CPU / cache state");
    println!("  b  board / clocks     k  async task table");
    println!("  w  dispatch jobs      a  heap stats + self-test");
    println!("  f  display mode       g  draw the test card");
    println!("  i  SD card info       r  hexdump block 0");
    println!("  x  SD write self-test e  raise BRK #0");
    println!("  d  raise a data abort p  panic");
    println!("  anything else echoes");
    println!();
}

/// Horizontal rule for the heap report.
const HEAP_RULE: &str =
    "  +------------------+--------------+---------+------------------------------+";

/// One row of the heap table.
///
/// A free function rather than a closure over the writer: a closure would hold
/// a unique borrow of `w` across the whole table, blocking the header and
/// footer.
fn heap_row(w: &mut impl core::fmt::Write, label: &str, value: usize, hex: bool, note: &str) {
    if hex {
        // An address has no meaningful size, so that column stays blank.
        let _ = writeln!(
            w,
            "  | {label:<16} | {value:#012x} | {:>7} | {note:<28} |",
            ""
        );
    } else {
        let (size, unit) = human_size(value);
        let _ = writeln!(
            w,
            "  | {label:<16} | {value:>12} | {size:>3} {unit} | {note:<28} |"
        );
    }
}

/// Prints heap occupancy, then exercises the allocator.
///
/// The self-test runs on every invocation rather than once at boot, so it can
/// be re-run after a workload has fragmented the free lists.
fn report_heap() {
    let stats = heap::stats();

    println!();
    if stats.capacity() == 0 {
        println!("Heap not initialised.");
        return;
    }

    let results = heap_self_test();
    let after = heap::stats();

    console::with_writer(|w| {
        use core::fmt::Write;

        let _ = writeln!(w, "Heap");
        let _ = writeln!(w, "{HEAP_RULE}");
        let _ = writeln!(
            w,
            "  | {:<16} | {:<12} | {:>7} | {:<28} |",
            "field", "value", "size", "note"
        );
        let _ = writeln!(w, "{HEAP_RULE}");

        heap_row(
            w,
            "arena start",
            after.start,
            true,
            "first byte above image",
        );
        heap_row(
            w,
            "arena end",
            after.end,
            true,
            "VideoCore reservation base",
        );
        heap_row(
            w,
            "capacity",
            after.capacity(),
            false,
            "mapped Normal RW at boot",
        );
        heap_row(
            w,
            "watermark",
            after.watermark,
            true,
            "highest byte ever touched",
        );
        heap_row(w, "live", after.live, false, "handed out right now");
        heap_row(w, "peak", after.peak, false, "high-water mark");
        heap_row(
            w,
            "cached",
            after.cached(),
            false,
            "on free lists, reusable",
        );
        heap_row(
            w,
            "untouched",
            after.untouched(),
            false,
            "never written DRAM",
        );

        let _ = writeln!(w, "{HEAP_RULE}");
        let _ = writeln!(
            w,
            "  allocs {} | frees {} | reused {} | resize-in-place {} | failed {}",
            after.allocs, after.deallocs, after.reused, after.grown_in_place, after.failed
        );

        // Only the populated classes: 27 mostly empty rows is noise.
        let mut printed = false;
        for (class, count) in after.free_counts.iter().enumerate() {
            if *count == 0 {
                continue;
            }
            if !printed {
                let _ = write!(w, "  free lists:");
                printed = true;
            }
            let (size, unit) = human_size(heap::Stats::class_size(class));
            let _ = write!(w, "  {size}{}x{count}", unit.trim());
        }
        if printed {
            let _ = writeln!(w);
        }

        let passed = results.iter().filter(|(_, ok)| *ok).count();
        let _ = writeln!(w, "  self-test: {passed}/{} passed", results.len());
        for (name, ok) in &results {
            let _ = writeln!(w, "    [{}] {name}", if *ok { "ok" } else { "FAIL" });
        }
    });

    // `stats` was taken before the self-test allocated anything; the delta is
    // what the test used.
    println!(
        "  self-test cost {} alloc(s), all returned",
        after.allocs - stats.allocs
    );
}

/// Exercises the allocator through the `alloc` collection types.
///
/// Returns one `(description, passed)` pair per check. The `Vec` holding them
/// is itself an allocation.
fn heap_self_test() -> Vec<(&'static str, bool)> {
    use core::fmt::Write;

    let mut results: Vec<(&'static str, bool)> = Vec::new();

    // Smallest class: a value that has to survive a round trip through DRAM.
    let boxed = black_box(Box::new(0x00C0_FFEE_u64));
    results.push(("Box<u64> round trip", *boxed == 0x00C0_FFEE));
    drop(boxed);

    // Vec growth walks the realloc path across several size classes.
    let mut numbers: Vec<u32> = Vec::new();
    for value in 0..1024 {
        numbers.push(value);
    }
    let sum: u64 = numbers.iter().map(|value| u64::from(*value)).sum();
    results.push((
        "Vec<u32> grown to 1024",
        numbers.len() == 1024 && sum == 523_776,
    ));
    drop(numbers);

    // Formatting into an owned String.
    let mut text = String::new();
    let _ = write!(text, "core {} up {:?}", cpu::core_id(), time::uptime());
    results.push(("String formatting", text.starts_with("core ")));
    drop(text);

    // Over-aligned types: the class is chosen as max(size, align), so the block
    // is aligned to its own size. Checked anyway, because it is the invariant a
    // header-free allocator relies on.
    #[repr(align(64))]
    struct Aligned(u64);
    let aligned = Box::new(Aligned(1));
    let address = (&raw const *aligned) as usize;
    results.push((
        "64-byte aligned Box",
        address.is_multiple_of(64) && aligned.0 == 1,
    ));
    drop(aligned);

    // Freeing and immediately reallocating the same class returns the same
    // block: the free lists are LIFO.
    let first = Box::new(1_u64);
    let first_address = (&raw const *first) as usize;
    drop(first);
    let second = Box::new(2_u64);
    let second_address = (&raw const *second) as usize;
    drop(second);
    results.push((
        "free list reuses the block",
        first_address == second_address,
    ));

    results
}

/// Horizontal rule for the display report.
const DISPLAY_RULE: &str =
    "  +--------------+----------------------+------------------------------+";

/// One row of the display table.
fn display_row(w: &mut impl core::fmt::Write, label: &str, value: &str, note: &str) {
    let _ = writeln!(w, "  | {label:<12} | {value:<20} | {note:<28} |");
}

/// Brings the display up if it is not already, then reports the mode.
///
/// The mode is not set at boot: allocating a framebuffer costs a mailbox round
/// trip and a slice of the VideoCore's DRAM, so the display is opt-in.
fn report_display() {
    /// Mode requested on first use. 720p is the largest mode that is certain to
    /// fit in the default 64 MiB `gpu_mem` split at 32 bpp with two pages.
    const MODE: (u32, u32, u32) = (1280, 720, 2);

    println!();

    let existing = drivers::FRAMEBUFFER.lock().info();
    let info = match existing {
        Some(info) => info,
        None => {
            println!("Display not initialised; asking the VideoCore for a mode...");
            let outcome = drivers::FRAMEBUFFER.lock().init(MODE.0, MODE.1, MODE.2);
            match outcome {
                Ok(()) => match drivers::FRAMEBUFFER.lock().info() {
                    Some(info) => info,
                    None => {
                        println!("  the framebuffer reported ready but has no mode");
                        return;
                    }
                },
                Err(error) => {
                    println!("  mode set failed: {error}");
                    println!("  the kernel carries on over the UART; no display is required.");
                    return;
                }
            }
        }
    };

    console::with_writer(|w| {
        use core::fmt::Write;

        let _ = writeln!(w, "Display");
        let _ = writeln!(w, "{DISPLAY_RULE}");
        let _ = writeln!(
            w,
            "  | {:<12} | {:<20} | {:<28} |",
            "field", "value", "note"
        );
        let _ = writeln!(w, "{DISPLAY_RULE}");

        let mut scratch = String::new();

        scratch.clear();
        let _ = write!(scratch, "{} x {}", info.width, info.height);
        display_row(w, "resolution", &scratch, "visible pixels");

        scratch.clear();
        let _ = write!(scratch, "{} bytes", info.pitch);
        let padding = info.pitch - info.width * 4;
        display_row(
            w,
            "pitch",
            &scratch,
            if padding == 0 {
                "no row padding"
            } else {
                "firmware padded each row"
            },
        );

        scratch.clear();
        let _ = write!(scratch, "{} bpp", info.depth);
        display_row(w, "depth", &scratch, "one pixel is one u32");

        scratch.clear();
        let _ = write!(scratch, "{:?}", info.order);
        display_row(
            w,
            "pixel order",
            &scratch,
            match info.order {
                drivers::framebuffer::PixelOrder::Bgr => "word is 0xAARRGGBB",
                drivers::framebuffer::PixelOrder::Rgb => "word is 0xAABBGGRR",
            },
        );

        scratch.clear();
        let _ = write!(
            scratch,
            "{} (draw {}, show {})",
            info.pages, info.draw_page, info.visible_page
        );
        display_row(w, "pages", &scratch, "flip re-points the scanout");

        scratch.clear();
        let _ = write!(scratch, "{:#010x}", info.bus_address);
        display_row(w, "bus address", &scratch, "what the VideoCore was given");

        scratch.clear();
        let _ = write!(scratch, "{:#012x}", info.base);
        display_row(w, "ARM address", &scratch, "mapped Normal non-cacheable");

        scratch.clear();
        let (size, unit) = human_size(info.size);
        let _ = write!(scratch, "{} ({size} {unit})", info.size);
        display_row(w, "allocation", &scratch, "inside the GPU reservation");

        let _ = writeln!(w, "{DISPLAY_RULE}");
        let _ = writeln!(
            w,
            "  'g' draws a test card. QEMU needs -Display to show it."
        );
    });
}

/// Draws a test card exercising every framebuffer primitive, then presents it.
///
/// The bars check colour encoding, the ramp checks
/// [`drivers::framebuffer::Color::mix`], the logo checks the QOI decoder and
/// the alpha-blended blit, and the geometry checks clipping at the edges.
fn draw_test_card() {
    use drivers::framebuffer::{BLACK, BLUE, Color, Framebuffer, GREEN, Image, RED, WHITE};

    /// The classic bar sequence, left to right.
    const BARS: [Color; 8] = [
        WHITE,
        Color::rgb(255, 255, 0),
        Color::rgb(0, 255, 255),
        GREEN,
        Color::rgb(255, 0, 255),
        RED,
        BLUE,
        Color::rgb(20, 20, 20),
    ];

    if !drivers::FRAMEBUFFER.lock().is_ready() {
        println!();
        println!("Display not initialised; run 'f' first.");
        return;
    }

    // Decoded before the lock is taken: holding the framebuffer across a
    // multi-millisecond decode would block every other core's drawing.
    let decode_started = time::ticks();
    let logo = drivers::qoi::decode(drivers::LOGO_QOI);
    let decode_ticks = time::ticks() - decode_started;

    let pattern = pattern_image(PATTERN_SIZE);

    let started = time::ticks();
    let mut fb = drivers::FRAMEBUFFER.lock();
    let (width, height) = (fb.width(), fb.height());

    // Background: a vertical gradient, which also checks that every row of the
    // surface is addressable.
    let top = Color::rgb(8, 12, 32);
    let bottom = Color::rgb(2, 2, 6);
    for y in 0..height {
        let amount = (y * 255 / height.max(1)) as u8;
        fb.fill_rect(0, y, width, 1, top.mix(bottom, amount));
    }

    // Colour bars.
    let bar_height = height / 8;
    let bar_width = width / BARS.len() as u32;
    for (index, colour) in BARS.iter().enumerate() {
        let x = index as u32 * bar_width;
        // The last bar absorbs the rounding remainder so the row is full.
        let this_width = if index == BARS.len() - 1 {
            width - x
        } else {
            bar_width
        };
        fb.fill_rect(x, 0, this_width, bar_height, *colour);
    }

    // Greyscale ramp, one column per step.
    let ramp_top = bar_height;
    let ramp_height = height / 24;
    for x in 0..width {
        let amount = (x * 255 / width.max(1)) as u8;
        fb.fill_rect(x, ramp_top, 1, ramp_height, BLACK.mix(WHITE, amount));
    }

    let content_top = ramp_top + ramp_height + 16;

    // The logo, composited over the gradient so its transparency shows, with
    // text drawn on top of it afterwards. The source is a 480x480 canvas that
    // is mostly empty margin, so it is trimmed to its artwork first.
    let mut logo_bottom = content_top;
    match &logo {
        Ok((header, pixels)) => {
            let whole = Image::new(pixels, header.width, header.height);
            if let Some(art) = whole.and_then(|image| image.trimmed()) {
                let x = (width.saturating_sub(art.width)) / 2;
                fb.blit_blend(x, content_top, &art);

                // Text over the image: a `None` background leaves the logo
                // pixels between the glyph strokes untouched.
                let caption = "COGWARE BCM2837";
                let caption_width = Framebuffer::text_width(caption, 3);
                fb.draw_text_scaled(
                    x + (art.width.saturating_sub(caption_width)) / 2,
                    content_top + art.height / 2 - 12,
                    caption,
                    WHITE,
                    None,
                    3,
                );
                logo_bottom = content_top + art.height;

                // The fine print goes under the artwork, where it is legible.
                let subtitle = "480x480 QOI, alpha-composited, text drawn on top";
                fb.draw_text(
                    (width.saturating_sub(Framebuffer::text_width(subtitle, 1))) / 2,
                    logo_bottom + 10,
                    subtitle,
                    Color::rgb(255, 190, 60),
                    None,
                );
                logo_bottom += 24;
            }
        }
        Err(error) => {
            fb.draw_fmt(
                32,
                content_top,
                format_args!("logo decode failed: {error}"),
                RED,
                None,
            );
            logo_bottom = content_top + 16;
        }
    }

    // Geometry and the procedural image, spread across the space below.
    let row_top = logo_bottom + 40;
    let gap = 48;
    if let Some(image) = Image::new(&pattern, PATTERN_SIZE, PATTERN_SIZE) {
        let x = 32;
        fb.blit(x, row_top, &image);
        fb.draw_rect(
            x.saturating_sub(1),
            row_top.saturating_sub(1),
            PATTERN_SIZE + 2,
            PATTERN_SIZE + 2,
            WHITE,
        );
        // Text over an opaque image, so the glyphs overwrite rather than blend.
        fb.draw_text(x + 6, row_top + 6, "on image", WHITE, None);
        fb.draw_text(
            x,
            row_top + PATTERN_SIZE + 6,
            "blit + heap + FPU",
            WHITE,
            None,
        );
    }

    // Deliberately drawn past the right edge so the clipping path is
    // exercised on every run.
    let box_x = 32 + PATTERN_SIZE + gap;
    let box_size = PATTERN_SIZE;
    for step in 0..8 {
        let inset = step * 8;
        if inset * 2 >= box_size {
            break;
        }
        let shade = 60 + step as u8 * 24;
        fb.draw_rect(
            box_x + inset,
            row_top + inset,
            box_size - inset * 2,
            box_size - inset * 2,
            Color::rgb(shade, shade / 2, 255 - shade),
        );
    }
    fb.draw_text(box_x, row_top + box_size + 6, "rects", WHITE, None);

    let line_x = box_x + box_size + gap;
    for step in 0..9 {
        let offset = step * (box_size / 8);
        fb.line(
            line_x as i32,
            (row_top + offset) as i32,
            (line_x + box_size) as i32,
            (row_top + box_size - offset) as i32,
            Color::rgb(255, 200, 80),
        );
    }
    fb.draw_text(line_x, row_top + box_size + 6, "lines", WHITE, None);

    // Mode readout, pinned to the bottom left.
    let text_top = height.saturating_sub(36);
    fb.draw_fmt(
        32,
        text_top,
        format_args!("{width}x{height} @ 32bpp via the VideoCore property mailbox"),
        Color::rgb(180, 180, 180),
        None,
    );
    fb.draw_text(
        32,
        text_top + 12,
        "no V3D in QEMU - shaders need real silicon",
        Color::rgb(180, 180, 180),
        None,
    );

    // A frame round the whole surface checks the far edges are addressable.
    fb.draw_rect(0, 0, width, height, Color::rgb(90, 90, 90));

    let result = fb.flip();
    let elapsed = time::ticks() - started;
    let info = fb.info();
    drop(fb);

    let frequency = time::frequency_hz().max(1);
    println!();
    match result {
        Ok(()) => println!("Test card drawn and presented."),
        Err(error) => println!("Test card drawn but the flip failed: {error}"),
    }
    match &logo {
        Ok((header, _)) => println!(
            "  logo   : {}x{} QOI, {} KiB compressed, decoded in {} us",
            header.width,
            header.height,
            drivers::LOGO_QOI.len() / 1024,
            decode_ticks * 1_000_000 / frequency
        ),
        Err(error) => println!("  logo   : decode failed: {error}"),
    }
    println!(
        "  drawing: {} us for {} pixels",
        elapsed * 1_000_000 / frequency,
        info.map_or(0, |i| i.width * i.height)
    );
    if let Some(info) = info {
        println!(
            "  showing page {} of {}, drawing into {}",
            info.visible_page, info.pages, info.draw_page
        );
    }
}

/// Side of the procedural image the test card blits.
const PATTERN_SIZE: u32 = 128;

/// Horizontal rule for the SD card report.
const CARD_RULE: &str = "  +---------------+--------------------------------------------+";

/// One row of the SD card table.
fn card_row(w: &mut impl core::fmt::Write, label: &str, value: &str) {
    let _ = writeln!(w, "  | {label:<13} | {value:<42} |");
}

/// Brings the SD card up if it is not already, then reports what it is.
///
/// Card discovery is not done at boot: the ACMD41 busy-loop can run for the
/// best part of a second on a slow card.
fn report_sd_card() {
    use drivers::emmc::BLOCK_SIZE;

    println!();

    let existing = drivers::EMMC.lock().info();
    let info = match existing {
        Some(info) => info,
        None => {
            println!("SD card not initialised; probing the EMMC controller...");
            let started = time::ticks();
            let outcome = drivers::EMMC.lock().init();
            let elapsed = time::ticks() - started;
            match outcome {
                Ok(()) => {
                    println!(
                        "  card ready in {} ms",
                        elapsed * 1000 / time::frequency_hz().max(1)
                    );
                    match drivers::EMMC.lock().info() {
                        Some(info) => info,
                        None => {
                            println!("  controller reported ready but has no card info");
                            return;
                        }
                    }
                }
                Err(error) => {
                    println!("  init failed: {error}");
                    println!("  QEMU needs -SdImage; hardware needs a card in the slot.");
                    return;
                }
            }
        }
    };

    let blocks = drivers::EMMC.lock().block_count();

    console::with_writer(|w| {
        use core::fmt::Write;

        let _ = writeln!(w, "SD card");
        let _ = writeln!(w, "{CARD_RULE}");
        let _ = writeln!(w, "  | {:<13} | {:<42} |", "field", "value");
        let _ = writeln!(w, "{CARD_RULE}");

        let mut scratch = String::new();

        card_row(w, "type", info.card_type.name());

        scratch.clear();
        let (size, unit) = human_size(info.capacity_bytes as usize);
        let _ = write!(scratch, "{size} {unit} ({} bytes)", info.capacity_bytes);
        card_row(w, "capacity", &scratch);

        scratch.clear();
        let _ = write!(scratch, "{blocks} x {BLOCK_SIZE} bytes");
        card_row(w, "blocks", &scratch);

        scratch.clear();
        // The product name is 5 ASCII bytes, space padded, straight from CID.
        for byte in info.product_name {
            scratch.push(if byte.is_ascii_graphic() || byte == b' ' {
                byte as char
            } else {
                '.'
            });
        }
        let _ = write!(
            scratch,
            "  rev {}.{}",
            info.product_revision_major, info.product_revision_minor
        );
        card_row(w, "product", &scratch);

        scratch.clear();
        let _ = write!(
            scratch,
            "{:#04x} (OEM {}{})",
            info.manufacturer_id, info.oem_id[0] as char, info.oem_id[1] as char
        );
        card_row(w, "manufacturer", &scratch);

        scratch.clear();
        let _ = write!(scratch, "{:#010x}", info.serial);
        card_row(w, "serial", &scratch);

        scratch.clear();
        let _ = write!(
            scratch,
            "{:04}-{:02}",
            info.manufacture_year, info.manufacture_month
        );
        card_row(w, "manufactured", &scratch);

        scratch.clear();
        let _ = write!(scratch, "{}-bit", info.bus_width);
        card_row(w, "bus width", &scratch);

        scratch.clear();
        let _ = write!(scratch, "{:#010x}", info.rca);
        card_row(w, "RCA", &scratch);

        scratch.clear();
        let _ = write!(scratch, "{:#010x}  CSD v{}", info.ocr, info.csd_version);
        card_row(w, "OCR", &scratch);

        let _ = writeln!(w, "{CARD_RULE}");
        let _ = writeln!(w, "  'r' hexdumps block 0.");
    });
}

/// Reads block 0 and hexdumps it.
///
/// Block 0 is the MBR on a partitioned card, so a real card shows a partition
/// table and the `55 AA` signature at offset 0x1FE. A blank QEMU image reads
/// back all zeroes, which still shows the transfer worked.
fn dump_sd_block() {
    use drivers::emmc::BLOCK_SIZE;

    println!();
    if !drivers::EMMC.lock().is_initialised() {
        println!("SD card not initialised; run 'i' first.");
        return;
    }

    let mut block = alloc::vec![0u8; BLOCK_SIZE];
    let started = time::ticks();
    let outcome = drivers::EMMC.lock().read_blocks(0, &mut block);
    let elapsed = time::ticks() - started;

    if let Err(error) = outcome {
        println!("Read of block 0 failed: {error}");
        return;
    }

    println!(
        "Block 0, {BLOCK_SIZE} bytes in {} us",
        elapsed * 1_000_000 / time::frequency_hz().max(1)
    );

    console::with_writer(|w| {
        use core::fmt::Write;

        // 512 bytes at 16 per line is 32 lines of console. Runs of identical
        // lines collapse, so a blank card prints three lines instead of
        // thirty-two.
        let mut skipping = false;
        for (index, chunk) in block.chunks(16).enumerate() {
            let all_zero = chunk.iter().all(|byte| *byte == 0);
            if all_zero && index != 0 && index != (BLOCK_SIZE / 16) - 1 {
                if !skipping {
                    let _ = writeln!(w, "  *");
                    skipping = true;
                }
                continue;
            }
            skipping = false;

            let _ = write!(w, "  {:04x}  ", index * 16);
            for byte in chunk {
                let _ = write!(w, "{byte:02x} ");
            }
            let _ = write!(w, " |");
            for byte in chunk {
                let _ = write!(
                    w,
                    "{}",
                    if byte.is_ascii_graphic() || *byte == b' ' {
                        *byte as char
                    } else {
                        '.'
                    }
                );
            }
            let _ = writeln!(w, "|");
        }

        let signature = u16::from_le_bytes([block[510], block[511]]);
        if signature == 0xAA55 {
            let _ = writeln!(
                w,
                "  MBR signature 0x55AA present: this card is partitioned."
            );
        } else {
            let _ = writeln!(
                w,
                "  no MBR signature (got {signature:#06x}): card is unformatted."
            );
        }
    });
}

/// Writes a signature to a scratch block and reads it back.
///
/// Block 1 is used deliberately: block 0 holds the MBR on a formatted card and
/// clobbering it would make the card unbootable, whereas block 1 is inside the
/// reserved gap before the first partition on every layout this kernel will
/// meet.
fn sd_write_test() {
    use drivers::emmc::BLOCK_SIZE;

    /// Scratch block. Never block 0.
    const SCRATCH: u32 = 1;

    println!();
    if !drivers::EMMC.lock().is_initialised() {
        println!("SD card not initialised; run 'i' first.");
        return;
    }

    // A recognisable, position-dependent pattern: a stuck bus returning the
    // same byte for everything, or a driver that transposes words, both fail.
    let mut written = alloc::vec![0u8; BLOCK_SIZE];
    written[..8].copy_from_slice(b"COGWARE!");
    for (index, byte) in written.iter_mut().enumerate().skip(8) {
        *byte = (index as u8).wrapping_mul(31).wrapping_add(7);
    }

    let started = time::ticks();
    let write = drivers::EMMC.lock().write_blocks(SCRATCH, &written);
    let write_ticks = time::ticks() - started;
    if let Err(error) = write {
        println!("Write to block {SCRATCH} failed: {error}");
        return;
    }

    let mut read_back = alloc::vec![0u8; BLOCK_SIZE];
    let started = time::ticks();
    let read = drivers::EMMC.lock().read_blocks(SCRATCH, &mut read_back);
    let read_ticks = time::ticks() - started;
    if let Err(error) = read {
        println!("Read back of block {SCRATCH} failed: {error}");
        return;
    }

    let hz = time::frequency_hz().max(1);
    println!("SD write/read-back on block {SCRATCH}");
    println!(
        "  wrote     : {BLOCK_SIZE} bytes in {} us",
        write_ticks * 1_000_000 / hz
    );
    println!(
        "  read back : {BLOCK_SIZE} bytes in {} us",
        read_ticks * 1_000_000 / hz
    );

    match written
        .iter()
        .zip(read_back.iter())
        .position(|(sent, got)| sent != got)
    {
        None => println!("  verify    : ok, all {BLOCK_SIZE} bytes match"),
        Some(offset) => println!(
            "  verify    : MISMATCH at byte {offset}: wrote {:#04x}, read {:#04x}",
            written[offset], read_back[offset]
        ),
    }
}

/// Builds a procedural image, to exercise the blit path with real data.
///
/// Computed rather than embedded, to keep it out of `.rodata`. No `sqrt` and
/// no transcendentals, because `core` provides neither — the falloff works in
/// squared distance instead.
fn pattern_image(size: u32) -> Vec<u32> {
    use drivers::framebuffer::Color;

    let mut pixels = Vec::with_capacity((size as usize) * (size as usize));
    let centre = (size as f32 - 1.0) * 0.5;
    let falloff = centre * centre;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - centre;
            let dy = y as f32 - centre;
            let distance = (dx * dx + dy * dy) / falloff;
            let glow = if distance >= 1.0 { 0.0 } else { 1.0 - distance };
            let texture = ((x ^ y) & 0x1F) as f32 / 31.0;

            let red = (glow * 255.0) as u8;
            let green = (texture * glow * 255.0) as u8;
            let blue = ((1.0 - glow) * 210.0) as u8;
            pixels.push(Color::rgb(red, green, blue).to_argb());
        }
    }
    pixels
}

/// Queries the VideoCore for board identity, the DRAM split and every clock.
///
/// This is the read-only half of the mailbox interface. The write half —
/// `MAILBOX.lock().set_clock_rate(Clock::Arm, rate, false)` — raises the CPU
/// frequency on real hardware; it is not called automatically, because doing so
/// without the right `config.txt` voltage settings makes boards unstable.
fn report_board() {
    /// Clocks shown, in reading order.
    const CLOCKS: [(Clock, &str); 5] = [
        (Clock::Arm, "ARM"),
        (Clock::Core, "core"),
        (Clock::V3d, "V3D (GPU)"),
        (Clock::Sdram, "SDRAM"),
        (Clock::Uart, "UART"),
    ];

    let mut mailbox = MAILBOX.lock();

    println!();
    println!("Board (via VideoCore property mailbox)");

    match mailbox.board_revision() {
        Ok(revision) => println!("  revision   : {revision:#010x}"),
        Err(error) => println!("  revision   : {error}"),
    }
    match mailbox.board_serial() {
        Ok(serial) => println!("  serial     : {serial:#018x}"),
        Err(error) => println!("  serial     : {error}"),
    }
    match mailbox.arm_memory() {
        Ok((base, size)) => println!(
            "  ARM DRAM   : {:#012x} + {} MiB",
            base,
            size / (1024 * 1024)
        ),
        Err(error) => println!("  ARM DRAM   : {error}"),
    }
    match mailbox.vc_memory() {
        Ok((base, size)) => println!(
            "  GPU DRAM   : {:#012x} + {} MiB",
            base,
            size / (1024 * 1024)
        ),
        Err(error) => println!("  GPU DRAM   : {error}"),
    }
    match mailbox.temperature() {
        Ok(milli_c) => println!(
            "  temperature: {}.{} C",
            milli_c / 1000,
            (milli_c % 1000) / 100
        ),
        Err(error) => println!("  temperature: {error}"),
    }

    println!();
    println!("  clock       current MHz    max MHz");
    println!("  ---------------------------------");
    for (clock, name) in CLOCKS {
        let current = mailbox.clock_rate(clock);
        let maximum = mailbox.max_clock_rate(clock);
        match (current, maximum) {
            (Ok(current), Ok(maximum)) => println!(
                "  {name:<10} {:>10}  {:>9}",
                current / 1_000_000,
                maximum / 1_000_000
            ),
            _ => println!("  {name:<10} {:>10}  {:>9}", "-", "-"),
        }
    }

    println!();
    println!("  To clock the ARM up on hardware:");
    println!("    let max = MAILBOX.lock().max_clock_rate(Clock::Arm)?;");
    println!("    MAILBOX.lock().set_clock_rate(Clock::Arm, max, false)?;");

    // Keep the error type named even when every call above succeeds, so the
    // import cannot rot.
    let _: Option<MailboxError> = None;
}

/// Interactive loop on core 0, running as an async task.
///
/// Between keystrokes the `.await` on [`console::next_byte`] lets core 0 sleep
/// in `wfe`, and any other task spawned on core 0 gets to run.
async fn shell() {
    print!("> ");

    loop {
        let byte = console::next_byte().await;

        match byte {
            b'h' | b'?' => print_help(),
            b'm' => print_memory_map(),
            b's' => report_smp(),
            b'j' => debug::jtag::report(),
            b'b' => report_board(),
            b't' => println!(
                "uptime {:?} ({} ticks @ {} Hz)",
                time::uptime(),
                time::ticks(),
                time::frequency_hz()
            ),
            b'c' => report_cpu(),
            b'k' => report_tasks(),
            b'w' => dispatch_jobs().await,
            b'a' => report_heap(),
            b'f' => report_display(),
            b'g' => draw_test_card(),
            b'i' => report_sd_card(),
            b'r' => dump_sd_block(),
            b'x' => sd_write_test(),
            b'e' => self_test_exceptions(),
            b'd' => raise_data_abort(),
            b'p' => panic!("operator requested a panic from the console"),
            b'\r' | b'\n' => println!(),
            other => println!("{}", other as char),
        }

        print!("> ");
    }
}

fn report_smp() {
    println!();
    println!("SMP");
    println!(
        "  cores online : {}/{}",
        smp::online_count(),
        memory::CORE_COUNT
    );
    println!(
        "  MMU gate     : {}",
        if smp::mmu_gate_open() {
            "open"
        } else {
            "closed"
        }
    );
    println!("  spin table   : {:#06x}", memory::SPIN_TABLE_BASE);
    for core in 0..memory::CORE_COUNT {
        println!("    slot {core}: {:#018x}", smp::spin_slot_value(core));
    }
    report_tally();
}

fn report_cpu() {
    let (written, read) = console::statistics();

    println!();
    println!("Core {} state", cpu::core_id());
    println!("  EL          : {}", cpu::current_el());
    println!("  MPIDR_EL1   : {:#018x}", cpu::mpidr());
    println!("  SCTLR_EL1   : {:#018x}", cpu::sctlr_el1());
    println!("  MMU         : {}", on_off(mmu::is_enabled()));
    println!("  D-cache     : {}", on_off(mmu::data_cache_enabled()));
    println!(
        "  I-cache     : {}",
        on_off(mmu::instruction_cache_enabled())
    );
    println!("  FP/SIMD     : {}", on_off(cpu::fpu_enabled()));
    println!(
        "  event strm  : {} ({} ticks)",
        on_off(cpu::event_stream_enabled()),
        cpu::EVENT_STREAM_PERIOD_TICKS
    );
    println!("  SP          : {:#018x}", cpu::stack_pointer());
    println!("  console I/O : {written} written, {read} read");
}

/// Reads from an address that is deliberately left unmapped, to demonstrate
/// the fault decoder.
fn raise_data_abort() {
    // 0xC000_0000 is above the last L1 entry the kernel populates.
    const UNMAPPED: usize = 0xC000_0000;
    println!("Reading unmapped {UNMAPPED:#018x} ...");

    // SAFETY: intentionally faulting. The handler prints and halts, so this
    // never returns.
    let value = unsafe { core::ptr::read_volatile(UNMAPPED as *const u32) };
    println!("unexpectedly read {value:#010x}");
}
