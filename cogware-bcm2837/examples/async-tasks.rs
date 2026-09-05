// SPDX-License-Identifier: MIT OR Apache-2.0
//! The cooperative async executor: tasks, channels, timers and cross-core
//! spawning.
//!
//! Run with `cargo run --release --example async-tasks`.
//!
//! Each core runs its own [`Executor`]. `run()` never returns: the core polls
//! whatever is ready, then sleeps in `wfe` until another core's `sev` wakes it.
//! There is no scheduler thread and no timer interrupt.
//!
//! Tasks are stored *inline*, in a fixed table, so nothing here allocates.
//! A future larger than [`task::TASK_STORAGE_BYTES`] is rejected at spawn time,
//! which is why [`task::spawn`] returns a `Result`.
//!
//! Four things are shown:
//!
//! * **`sleep`** - a timer future backed by the architectural counter.
//! * **[`Channel`]** - a bounded queue two tasks pass values through, on
//!   different cores.
//! * **[`Mutex`]** - the async lock, which yields instead of spinning.
//! * **[`task::spawn_on`]** - putting work on another core's executor.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use cogware_bcm2837::{
    BootInfo,
    bsp::memory,
    cprintln, cpu, entry, println, smp,
    task::{self, Channel, Mutex},
    time,
};

entry!(primary = main, secondary = worker);

/// Values pass through here from a producer on core 1 to the consumer on
/// core 0. Capacity 4, so the producer will block once it runs ahead.
static PIPE: Channel<u32, 4> = Channel::new();

/// An async lock. A task that cannot take it yields to the executor instead of
/// spinning.
static LEDGER: Mutex<u32> = Mutex::new(0);

/// How many worker tasks have finished, so core 0 knows when to stop.
static FINISHED: AtomicU32 = AtomicU32::new(0);

/// Number of tasks that report into [`FINISHED`]: one producer plus one
/// ledger task per secondary core.
const EXPECTED_FINISHERS: u32 = 1 + (memory::CORE_COUNT as u32 - 1);

/// Set once the consumer has drained the pipe, to stop the heartbeat.
static DONE: AtomicBool = AtomicBool::new(false);

/// Timer ticks the heartbeat task has counted.
static BEATS: AtomicU32 = AtomicU32::new(0);

fn main(_boot: BootInfo) -> ! {
    println!();
    println!("Async executor demo.");
    println!(
        "  {} slots per core, {} bytes of inline storage each",
        task::TASKS_PER_CORE,
        task::TASK_STORAGE_BYTES
    );
    println!();

    // SAFETY: core 0, called once, after the console and MMU are up.
    unsafe { smp::start_secondaries() };
    let online = smp::wait_for_cores(memory::CORE_COUNT, 1_000_000);
    println!("{online}/{} cores online.", memory::CORE_COUNT);
    println!();

    spawn("consumer", consumer());
    spawn("heartbeat", heartbeat());
    spawn("reporter", reporter());

    // Core 0 becomes an executor. Everything from here on is a task.
    task::run()
}

fn worker(boot: BootInfo) -> ! {
    cprintln!("executor starting");

    // Core 1 feeds the pipe; the rest contend on the async mutex.
    if boot.core_id == 1 {
        spawn("producer", producer());
    }
    spawn("ledger", ledger(boot.core_id));

    task::run()
}

/// Spawns the future and reports failure.
///
/// `spawn` fails on a full table or a future too big for a slot.
fn spawn(name: &'static str, future: impl Future<Output = ()> + Send + 'static) {
    if let Err(error) = task::spawn(name, future) {
        cprintln!("could not spawn {name}: {error}");
    }
}

/// Produces sixteen values on core 1, pausing so the pipe fills.
async fn producer() {
    for value in 1..=16u32 {
        PIPE.send(value).await;
        if value % 2 == 0 {
            task::sleep_millis(3).await;
        }
    }
    cprintln!("producer finished, sent 16 values");
    FINISHED.fetch_add(1, Ordering::Release);
}

/// Drains the pipe on core 0 and sums what arrives.
async fn consumer() {
    let mut total = 0u32;
    let mut count = 0u32;

    while count < 16 {
        let value = PIPE.recv().await;
        total += value;
        count += 1;
    }

    // 1 + 2 + ... + 16
    let expected = 16 * 17 / 2;
    println!(
        "consumer received {count} values, sum {total} (expected {expected}) - {}",
        if total == expected { "ok" } else { "MISMATCH" }
    );
    DONE.store(true, Ordering::Release);
}

/// Contends on the async mutex from every secondary core.
///
/// A `SpinLock` guard may not cross an await; a [`Mutex`] guard may, because
/// waiting on it parks the task rather than the core.
async fn ledger(core_id: usize) {
    for _ in 0..8 {
        let mut entry = LEDGER.lock().await;
        *entry += core_id as u32 + 1;
        task::yield_now().await;
        drop(entry);
    }
    FINISHED.fetch_add(1, Ordering::Release);
}

/// Shows the timer future working while other tasks are runnable.
///
/// This task and the consumer both make progress on core 0, on one thread of
/// execution, with no interrupt anywhere.
async fn heartbeat() {
    while !DONE.load(Ordering::Acquire) {
        task::sleep_millis(5).await;
        let beat = BEATS.fetch_add(1, Ordering::Relaxed) + 1;
        println!("  ..heartbeat {beat} at {:?}", time::uptime());
    }
}

/// Waits for everyone, prints the summary, and stops the machine.
async fn reporter() {
    // This is a polled condition, so let the core idle between checks rather
    // than keeping the reporter immediately runnable with `yield_now`.
    while FINISHED.load(Ordering::Acquire) < EXPECTED_FINISHERS || !DONE.load(Ordering::Acquire) {
        task::next_tick().await;
    }

    // Every core added `(id + 1)` eight times: 8 * (2 + 3 + 4) = 72.
    let ledger = *LEDGER.lock().await;
    let expected: u32 = (1..memory::CORE_COUNT as u32).map(|id| 8 * (id + 1)).sum();
    println!(
        "ledger total {ledger} (expected {expected}) - {}",
        if ledger == expected { "ok" } else { "MISMATCH" }
    );
    println!(
        "heartbeat ticked {} times on the 5 ms timer",
        BEATS.load(Ordering::Relaxed)
    );

    println!();
    println!("Per-core executors:");
    for core in 0..memory::CORE_COUNT {
        let Some(executor) = task::executor_for(core) else {
            continue;
        };
        // `idle_count` is how many times the core reached `wfe` with nothing
        // ready.
        println!(
            "  core {core}: {} live task(s), slept {} times",
            executor.task_count(),
            executor.idle_count()
        );
        for slot in 0..task::TASKS_PER_CORE {
            let Some(info) = executor.slot_info(slot) else {
                continue;
            };
            println!(
                "    slot {}: {:<10} {} polls{}",
                info.slot,
                info.name,
                info.polls,
                if info.ready { ", ready" } else { "" }
            );
        }
    }

    println!();
    println!("Done in {:?}.", time::uptime());
    cpu::halt()
}
