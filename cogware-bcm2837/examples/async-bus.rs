//! Async buses and async pins: `embedded-hal-async` on the BSP's executor.
//!
//! Four cores each run their own executor and contend for one I2C bus without
//! blocking a core while waiting for it.
//!
//! What it shows:
//!
//! * **`embedded_hal_async::i2c::I2c`** on a [`SharedI2c`] handle, driven from
//!   a task rather than from `main`.
//! * **A driver crate written against the async trait.**
//! * **`embedded_hal_async::delay::DelayNs`**, which yields to the executor
//!   instead of spinning on the counter.
//! * **Four cores contending for one bus** from async tasks, with the whole
//!   transaction as the unit of atomicity.
//! * **`embedded_hal_async::digital::Wait`**, which QEMU cannot fire the edge
//!   for.
//!
//! Run it with the emulated devices attached:
//!
//! ```powershell
//! .\scripts\qemu-console-test.ps1 -Example async-bus
//! ```

#![no_std]
#![no_main]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embedded_hal_async::{delay::DelayNs, i2c::I2c as AsyncI2c};

use cogware_bcm2837::{
    BootInfo,
    bsp::memory,
    cpu,
    drivers::{
        I2C1,
        i2c::{self, SPEED_STANDARD},
    },
    entry,
    hal::{
        asynch::{PROBE_PIN, edge_detection_works},
        delay::Delay,
        gpio::Pins,
        shared::SharedI2c,
    },
    println, smp, task, time,
};

entry!(primary = main, secondary = worker);

/// The DS1338 real-time clock QEMU attaches to `i2c-bus.1`.
const RTC: u8 = 0x68;

/// Scratch NVRAM, one byte per core, chosen so the cores cannot collide.
const SCRATCH_BASE: u8 = 0x30;

/// How many round trips each core performs.
const ROUNDS: u32 = 12;

/// Transactions that came back with something other than what was written,
/// which is what a torn transaction looks like from the outside.
static MISMATCHES: AtomicU32 = AtomicU32::new(0);

/// Transactions completed, across all cores.
static COMPLETED: AtomicU32 = AtomicU32::new(0);

/// Set once the async-delay task has finished.
///
/// The bus tasks finish well before it under emulation, so the report must
/// wait for this or it would halt the machine mid-sleep.
static DELAY_DONE: AtomicBool = AtomicBool::new(false);

/// The bus as every core sees it: a `Copy` handle onto one lock.
static BUS: SharedI2c<i2c::I2c> = SharedI2c::new(&I2C1);

fn main(_boot: BootInfo) -> ! {
    println!();
    println!("Async bus demo: embedded-hal-async over four executors.");

    {
        let mut bus = I2C1.lock();
        bus.init(SPEED_STANDARD);
    }

    let present = I2C1.lock().probe(RTC);
    println!();
    println!("-- setup --");
    println!("  BSC1          : {} Hz", I2C1.lock().speed_hz());
    println!(
        "  DS1338 @ {RTC:#04x} : {}",
        if present { "present" } else { "ABSENT" }
    );

    if !present {
        println!();
        println!("  no device on the bus; run with -Example async-bus so the");
        println!("  script attaches the emulated chips, or pass -BusDevices");
        println!();
        println!("Done in {:?}.", time::uptime());
        cpu::halt();
    }

    edge_report();

    // SAFETY: core 0, once, with the console, MMU and statics all up.
    unsafe { smp::start_secondaries() };

    spawn("rtc", async_driver());
    spawn("delay", async_delay());
    spawn("bus0", bus_task(0));
    spawn("report", report());

    // Core 0 becomes an executor too. Everything past here is a task.
    task::run()
}

/// Cores 1-3: one bus task each, then the executor.
fn worker(boot: BootInfo) -> ! {
    spawn("bus", bus_task(boot.core_id));
    task::run()
}

/// Spawns the future and reports failure.
fn spawn(name: &'static str, future: impl Future<Output = ()> + Send + 'static) {
    if let Err(error) = task::spawn(name, future) {
        println!("  could not spawn {name}: {error}");
    }
}

// ---------------------------------------------------------------------------
// An ecosystem-shaped async driver
// ---------------------------------------------------------------------------

/// A driver written the way a crates.io driver is written: generic over the
/// async trait, fallible, and chip-agnostic.
struct Ds1338<I2C> {
    i2c: I2C,
}

impl<I2C: AsyncI2c> Ds1338<I2C> {
    const fn new(i2c: I2C) -> Self {
        Self { i2c }
    }

    /// Reads the seven timekeeping registers in one transaction.
    async fn time(&mut self) -> Result<[u8; 7], I2C::Error> {
        let mut raw = [0u8; 7];
        self.i2c.write_read(RTC, &[0x00], &mut raw).await?;
        Ok(raw)
    }
}

/// Decodes one packed-BCD byte.
fn bcd(byte: u8, mask: u8) -> u8 {
    let value = byte & mask;
    (value >> 4) * 10 + (value & 0x0F)
}

async fn async_driver() {
    let mut clock = Ds1338::new(BUS);

    println!();
    println!("-- an async ecosystem driver --");

    match clock.time().await {
        Ok(raw) => {
            println!(
                "  read over .await : 20{:02}-{:02}-{:02} {:02}:{:02}:{:02}",
                bcd(raw[6], 0xFF),
                bcd(raw[5], 0x1F),
                bcd(raw[4], 0x3F),
                bcd(raw[2], 0x3F),
                bcd(raw[1], 0x7F),
                bcd(raw[0], 0x7F),
            );
            println!("  the driver is generic over embedded_hal_async::i2c::I2c");
            println!("  and never names a BCM2837 type");
        }
        Err(error) => println!("  driver failed: {error:?}"),
    }
}

// ---------------------------------------------------------------------------
// Async delay
// ---------------------------------------------------------------------------

async fn async_delay() {
    let mut delay = Delay::new();

    let start = time::uptime();
    for _ in 0..4 {
        // Yields to the executor, so the core runs other tasks meanwhile.
        delay.delay_ms(2).await;
    }
    let elapsed = time::uptime() - start;

    println!();
    println!("-- async delay --");
    println!("  4 x 2 ms via embedded_hal_async : {elapsed:?}");
    println!("  the core polled other tasks throughout");

    DELAY_DONE.store(true, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Four cores, one bus, all async
// ---------------------------------------------------------------------------

/// Writes a per-core byte to a per-core NVRAM address and reads it back.
///
/// Every `write_read` is one acquisition of the bus lock, so a value that
/// comes back wrong means two cores' transactions interleaved.
async fn bus_task(core: usize) {
    let mut bus = BUS;
    let address = SCRATCH_BASE + core as u8;
    let mut delay = Delay::new();

    for round in 0..ROUNDS {
        let written = (core as u8) << 4 | (round as u8 & 0x0F);

        if bus.write(RTC, &[address, written]).await.is_err() {
            MISMATCHES.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        let mut read = [0u8; 1];
        if bus.write_read(RTC, &[address], &mut read).await.is_err() {
            MISMATCHES.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        if read[0] != written {
            MISMATCHES.fetch_add(1, Ordering::Relaxed);
        }
        COMPLETED.fetch_add(1, Ordering::Relaxed);

        // Give the other cores a chance to interleave; otherwise they tend to
        // run to completion one after another.
        delay.delay_us(50).await;
    }
}

async fn report() {
    // No `Barrier` here: `Barrier::wait` spins, and a blocking spin inside an
    // async task starves every other task on that core, including the timers
    // they are waiting on.
    let expected = ROUNDS * memory::CORE_COUNT as u32;
    let mut delay = Delay::new();

    while COMPLETED.load(Ordering::Relaxed) + MISMATCHES.load(Ordering::Relaxed) < expected
        || !DELAY_DONE.load(Ordering::Acquire)
    {
        delay.delay_us(200).await;
    }

    let completed = COMPLETED.load(Ordering::Relaxed);
    let mismatches = MISMATCHES.load(Ordering::Relaxed);

    println!();
    println!("-- four cores, one bus, all from async tasks --");
    println!("  transactions  : {completed} of {expected}");
    println!(
        "  mismatches    : {mismatches}  {}",
        if mismatches == 0 { "ok" } else { "FAIL" }
    );
    println!("  each await point is a whole transaction, never half of one");

    println!();
    println!("Per-core executors:");
    for core in 0..memory::CORE_COUNT {
        let Some(executor) = task::executor_for(core) else {
            continue;
        };
        println!(
            "  core {core}: {} live task(s), slept {} times",
            executor.task_count(),
            executor.idle_count()
        );
    }

    println!();
    println!("Done in {:?}.", time::uptime());
    cpu::halt()
}

// ---------------------------------------------------------------------------
// Async pins
// ---------------------------------------------------------------------------

/// Reports whether `Wait` can possibly work here, without hanging to find out.
///
/// `embedded_hal_async::digital::Wait` rests on `GPEDS` and the edge-enable
/// registers, which QEMU does not implement, so awaiting an edge under
/// emulation blocks forever. The probe arms both edges on one pin, toggles it,
/// and reports whether the latch caught it.
fn edge_report() {
    println!();
    println!("-- async pins --");

    let Some(pins) = Pins::take() else {
        println!("  pins already taken; skipping the probe");
        return;
    };

    let mut probe = pins.p4.into_output_low();
    let works = edge_detection_works(&mut probe);

    println!("  GPEDS probe on GPIO {PROBE_PIN} : {}", verdict(works));
    if works {
        println!("  pin.wait_for(Edge::Rising).await would resolve here");
    } else {
        println!("  QEMU models neither GPEDS nor GPREN/GPFEN, so an awaited");
        println!("  edge never fires. The impl is correct; there is nothing");
        println!("  under emulation to fire it. Probe before you await.");
    }
}

fn verdict(ok: bool) -> &'static str {
    if ok { "latched" } else { "not modelled" }
}
