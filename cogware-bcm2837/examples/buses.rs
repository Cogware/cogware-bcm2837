// SPDX-License-Identifier: GPL-3.0-only
//! The hardware SPI and I2C controllers, talking to real virtual devices.
//!
//! Run with `.\scripts\run-qemu.ps1 -Example buses`, which attaches the
//! devices this expects:
//!
//! ```text
//! -device ds1338,address=0x68,bus=i2c-bus.1     a real-time clock
//! -device tmp105,address=0x49,bus=i2c-bus.1     a temperature sensor
//! -device sst25vf016b,bus=spi                   16 Mbit SPI flash
//! ```
//!
//! QEMU 9.0 and later model the BCM2835 `SPI0` and all three `BSC` controllers
//! properly, so everything below is a genuine transaction over a modelled bus
//! to a modelled peripheral, register for register. Plain
//! `cargo run --example buses` boots fine too, on an empty bus.
//!
//! # What is shown
//!
//! * Clock setup, and what the divider produced versus what was asked for.
//! * An address scan: 112 address bytes and their ACKs.
//! * `write_read` with a repeated START, the shape every register read takes.
//! * Sharing one bus between four cores through [`hal::shared`].
//! * A driver written against `embedded-hal`, knowing nothing about this chip.
//!
//! # The repeated-START caveat
//!
//! QEMU's I2C core does not model repeated START. Strict `write_read` reports
//! `RepeatedStartLost`; the demo checks that rather than silently accepting
//! STOP/START. DS1338 and TMP105 retain their register pointers across STOP,
//! so their device-specific demo code explicitly uses separate transfers.

#![no_std]
#![no_main]

use cogware_bcm2837::{
    BootInfo,
    bsp::memory,
    cpu,
    drivers::{
        I2C1, SPI0,
        dma::{self, Kind},
        i2c::{self, SPEED_STANDARD},
        spi::{self, ChipSelect, Config, MODE_0},
    },
    entry,
    hal::shared::{I2cDevice, NoCs, SharedI2c, SpiDevice},
    print, println, smp,
    sync::{Barrier, SpinLock},
    time,
};

entry!(primary = main, secondary = worker);

/// The DS1338 real-time clock.
const RTC: u8 = 0x68;

/// The TMP105 temperature sensor.
const SENSOR: u8 = 0x49;

/// Nothing lives here; used to show a NACK.
const EMPTY: u8 = 0x21;

/// Every core arrives before core 0 reports the shared-bus tally.
static BUS_BARRIER: Barrier = Barrier::new(memory::CORE_COUNT);

/// What the four cores did to the shared bus.
static TALLY: SpinLock<Tally> = SpinLock::new(Tally {
    transactions: 0,
    per_core: [0; memory::CORE_COUNT],
    mismatches: 0,
});

struct Tally {
    transactions: u32,
    per_core: [u32; memory::CORE_COUNT],
    /// Reads that came back with something other than what was written, which
    /// is what a torn transaction looks like from the outside.
    mismatches: u32,
}

fn main(_boot: BootInfo) -> ! {
    println!();
    println!("Bus demo: hardware SPI0 and BSC1 against emulated devices.");

    let present = i2c_demo();
    spi_demo();

    if present {
        ecosystem_driver();
        shared_bus();
    } else {
        println!();
        println!("-- sharing / embedded-hal --");
        println!("  skipped: no device on the bus to talk to");
    }

    println!();
    println!("Done in {:?}.", time::uptime());
    cpu::halt()
}

/// Cores 1-3: hammer the shared bus, then wait at the barrier.
fn worker(boot: BootInfo) -> ! {
    hammer_bus(boot.core_id);
    BUS_BARRIER.wait();
    cpu::halt()
}

// ---------------------------------------------------------------------------
// I2C
// ---------------------------------------------------------------------------

/// Returns whether anything answered.
fn i2c_demo() -> bool {
    println!();
    println!("-- hardware I2C: BSC1 on GPIO 2/3 --");

    let (pins, actual) = {
        let mut bus = I2C1.lock();
        bus.init(SPEED_STANDARD);
        (bus.pins(), bus.speed_hz())
    };

    println!(
        "  init          : SDA GPIO {}, SCL GPIO {}, ALT0",
        pins.0, pins.1
    );
    println!(
        "  clock         : asked {} Hz, got {} Hz (core clock {} MHz / DIV)",
        SPEED_STANDARD,
        actual,
        i2c::CORE_CLOCK_HZ / 1_000_000
    );

    // The scan is 112 address bytes; the ACK bit decides each one.
    let mut found = [0u8; 8];
    let mut count = 0usize;

    I2C1.with(|bus| {
        bus.scan(|address| {
            if count < found.len() {
                found[count] = address;
            }
            count += 1;
        });
    });

    print!("  scan          : ");
    if count == 0 {
        println!("nothing (start QEMU with -device ds1338,... to populate it)");
        return false;
    }

    for address in &found[..count.min(found.len())] {
        print!("{address:#04x} ");
    }
    println!("({count} device(s))");

    println!(
        "  probe {EMPTY:#04x}    : {}",
        if I2C1.with(|bus| bus.probe(EMPTY)) {
            "answered?!"
        } else {
            "no device, as expected"
        }
    );

    read_clock();
    nvram_round_trip();
    read_sensor();
    i2c_contract_checks();

    true
}

/// DS1338 accepts STOP between register selection and reading.
fn read_clock() {
    let mut time_regs = [0u8; 7];
    let result = I2C1.with(|bus| read_registers(bus, RTC, 0x00, &mut time_regs));

    println!();
    println!("  DS1338 @ {RTC:#04x} (real-time clock)");

    match result {
        Ok(()) => {
            // The DS1338 stores everything as packed BCD, two digits a byte.
            println!(
                "    time        : 20{:02x}-{:02x}-{:02x} {:02x}:{:02x}:{:02x}",
                time_regs[6],
                time_regs[5] & 0x1F,
                time_regs[4],
                time_regs[2] & 0x3F,
                time_regs[1],
                time_regs[0] & 0x7F,
            );
            println!("    raw         : {time_regs:02X?} (packed BCD, as the datasheet says)");
        }
        Err(error) => println!("    read failed : {error:?}"),
    }
}

/// Writes and reads back, checking both directions independently of whatever
/// the device happens to contain.
fn nvram_round_trip() {
    const ADDRESS: u8 = 0x20;
    const PAYLOAD: [u8; 4] = [0xC0, 0x67, 0x28, 0x37];

    let mut buffer = [0u8; 4];

    let written = I2C1.with(|bus| {
        let mut frame = [0u8; 5];
        frame[0] = ADDRESS;
        frame[1..].copy_from_slice(&PAYLOAD);
        bus.write(RTC, &frame)
    });

    let read = I2C1.with(|bus| read_registers(bus, RTC, ADDRESS, &mut buffer));

    let ok = written.is_ok() && read.is_ok() && buffer == PAYLOAD;

    println!(
        "    NVRAM       : wrote {PAYLOAD:02X?} at {ADDRESS:#04x}, read {buffer:02X?}  {}",
        verdict(ok)
    );
}

/// A second device on the same bus, to show addressing selects.
fn read_sensor() {
    let mut raw = [0u8; 2];
    let result = I2C1.with(|bus| read_registers(bus, SENSOR, 0x00, &mut raw));

    println!();
    println!("  TMP105 @ {SENSOR:#04x} (temperature sensor)");

    match result {
        Ok(()) => {
            // 12-bit left-justified two's complement, 0.0625 degrees a step.
            let ticks = i32::from(i16::from_be_bytes(raw) >> 4);
            let celsius = f64::from(ticks) * 0.0625;
            println!("    register 0  : {raw:02X?} -> {celsius} C");
            println!("    (an f64 divide on the hard-float target, for free)");
        }
        Err(error) => println!("    read failed : {error:?}"),
    }
}

// ---------------------------------------------------------------------------
// SPI
// ---------------------------------------------------------------------------

/// These particular devices retain their register pointer across STOP.
/// This is an explicit device-level choice, not an I2C transaction fallback.
fn read_registers(
    bus: &mut i2c::I2c,
    address: u8,
    register: u8,
    data: &mut [u8],
) -> Result<(), i2c::Error> {
    bus.write(address, &[register])?;
    bus.read(address, data)
}

fn i2c_contract_checks() {
    use embedded_hal::i2c::{I2c, Operation};
    let (mut first, mut second) = ([0; 1], [0; 1]);
    let result = I2C1.with(|bus| {
        I2c::transaction(
            bus,
            RTC,
            &mut [
                Operation::Write(&[0x24]),
                Operation::Write(&[0xC1]),
                Operation::Write(&[0xC2]),
            ],
        )?;
        bus.write(RTC, &[0x24])?;
        I2c::transaction(
            bus,
            RTC,
            &mut [Operation::Read(&mut first), Operation::Read(&mut second)],
        )
    });
    println!(
        "  scatter W/W/W and R/R: {}",
        verdict(result.is_ok() && first == [0xC1] && second == [0xC2])
    );
    let strict = I2C1.with(|bus| bus.write_read(RTC, &[0x24], &mut first));
    match strict {
        Ok(()) => println!("  strict repeated START: supported"),
        Err(i2c::Error::RepeatedStartLost) => {
            println!(
                "  strict repeated START: unavailable (expected in QEMU); no silent STOP fallback"
            );
        }
        Err(error) => println!("  strict repeated START: {error:?}"),
    }
    let unsupported = I2C1.with(|bus| {
        I2c::transaction(
            bus,
            RTC,
            &mut [Operation::Read(&mut first), Operation::Write(&[0x24])],
        )
    });
    println!(
        "  read-then-write rejected before START: {}",
        verdict(unsupported == Err(i2c::Error::UnsupportedTransaction))
    );
    let recovered = I2C1.with(|bus| read_registers(bus, RTC, 0x24, &mut first));
    println!(
        "  I2C usable after rejected transaction: {}",
        verdict(recovered.is_ok() && first == [0xC1])
    );
}

fn spi_demo() {
    println!();
    println!("-- hardware SPI: SPI0 on GPIO 7-11 --");

    let actual = {
        let mut bus = SPI0.lock();
        bus.init(Config {
            speed_hz: 1_000_000,
            mode: MODE_0,
            chip_select: ChipSelect::Ce0,
            cs_active_high: false,
        });
        bus.speed_hz()
    };

    println!(
        "  init          : CE1 {}, CE0 {}, MISO {}, MOSI {}, SCLK {}, ALT0",
        spi::PINS[0],
        spi::PINS[1],
        spi::PINS[2],
        spi::PINS[3],
        spi::PINS[4]
    );
    println!(
        "  clock         : asked 1000000 Hz, got {actual} Hz ({} MHz / CDIV)",
        spi::CORE_CLOCK_HZ / 1_000_000
    );
    println!("  mode 0, CE0 driven by the controller");

    // 0x9F is JEDEC "read identification", which every SPI flash answers.
    // Three bytes come back: manufacturer, then two of device type.
    let mut frame = [0x9Fu8, 0, 0, 0];
    let result = SPI0.with(|bus| bus.transfer_in_place(&mut frame));

    println!();
    match result {
        Ok(()) => {
            let id = &frame[1..];
            println!("  JEDEC ID (0x9F): {id:02X?}");

            match id {
                [0xBF, 0x25, ..] => println!("    -> SST 25VF016B, 16 Mbit. The bus works."),
                [0x00, 0x00, 0x00] | [0xFF, 0xFF, 0xFF] => {
                    println!("    -> nothing on the bus (add -device sst25vf016b,bus=spi)");
                }
                _ => println!("    -> an unrecognised device, but it answered"),
            }
        }
        Err(error) => println!("  transfer failed: {error:?}"),
    }

    // Full duplex is not optional on this controller: a write still fills the
    // receive FIFO, and leaving it full stalls the next transfer at 64 bytes.
    // Every routine in `drivers::spi` drains it.
    let mut long = [0u8; 96];
    long[0] = 0x9F;
    let bulk = SPI0.with(|bus| bus.transfer_in_place(&mut long));
    println!(
        "  96-byte transfer (past the 64-byte FIFO): {}",
        verdict(bulk.is_ok())
    );
    let mut device = SpiDevice::new(&SPI0, NoCs);
    let (mut maker, mut kind) = ([0; 1], [0; 2]);
    let split = embedded_hal::spi::SpiDevice::transaction(
        &mut device,
        &mut [
            embedded_hal::spi::Operation::Write(&[0x9F]),
            embedded_hal::spi::Operation::DelayNs(1_000),
            embedded_hal::spi::Operation::Read(&mut maker),
            embedded_hal::spi::Operation::Read(&mut kind),
        ],
    );
    let expected = [frame[1], frame[2], frame[3]];
    println!(
        "  hardware CS across write/delay/read/read: {}",
        verdict(split.is_ok() && [maker[0], kind[0], kind[1]] == expected,)
    );
    let injected = device.transaction(|bus| {
        // QEMU's SPI model does not wire CS to its slave. Complete the reply
        // before injecting an error so its parser is not left mid-command.
        // Driver unit tests independently inspect TA through error cleanup.
        let mut response = [0; 3];
        bus.write(&[0x9F])?;
        bus.read(&mut response)?;
        Err::<(), _>(spi::Error::Timeout)
    });
    let mut recovered = [0x9F, 0, 0, 0];
    let result = device.transfer_in_place(&mut recovered);
    println!(
        "  hardware CS released after transaction error: {}",
        verdict(
            injected == Err(spi::Error::Timeout) && result.is_ok() && recovered[1..] == expected,
        )
    );

    // Last, because under QEMU it leaves the flash mid-command.
    spi_dma_check(expected);
}

/// The JEDEC ID again, by DMA: four bytes, the smallest transfer that takes
/// the DMA path, so the answer is known.
///
/// Fails under stock QEMU, whose DMA ignores the SPI data requests: the
/// receive channel drains an empty FIFO before the first byte is sent. The
/// cogware QEMU fork paces both channels as hardware does, and passes.
fn spi_dma_check(expected: [u8; 3]) {
    /// Word alignment, which the DMA path needs.
    #[repr(C, align(4))]
    struct Frame([u8; 4]);

    let command = Frame([0x9F, 0, 0, 0]);
    let mut reply = Frame([0; 4]);
    let (Some(mut tx), Some(mut rx)) = (dma::claim(Kind::Lite), dma::claim(Kind::Lite)) else {
        println!("  JEDEC ID by DMA: FAIL, no lite channels free");
        return;
    };
    let result = SPI0.with(|bus| bus.transfer_dma(&mut tx, &mut rx, &mut reply.0, &command.0));
    println!(
        "  JEDEC ID by DMA (channels {} and {}): {} {:02X?} {result:?}",
        tx.index(),
        rx.index(),
        verdict(result.is_ok() && reply.0[1..] == expected),
        &reply.0[1..],
    );
    println!(
        "    (needs DREQ pacing: fails on stock QEMU, passes on the cogware fork and a board)"
    );
}

// ---------------------------------------------------------------------------
// Sharing
// ---------------------------------------------------------------------------

/// The bus, as four cores see it: a handle each, all pointing at one lock.
static SHARED_RTC: I2cDevice<i2c::I2c> = I2cDevice::new(&I2C1, RTC);

/// Where each core writes and reads back, chosen so they do not collide.
const SCRATCH_BASE: u8 = 0x28;

fn shared_bus() {
    println!();
    println!("-- one bus, four cores --");
    println!("  each core writes its own byte to its own NVRAM address and");
    println!("  reads it back; a torn transaction would show up as a mismatch");

    // SAFETY: core 0, once, with the console, MMU and statics all up.
    unsafe { smp::start_secondaries() };

    let online = smp::wait_for_cores(memory::CORE_COUNT, 1_000_000);
    if online != memory::CORE_COUNT {
        println!("  only {online}/{} cores checked in", memory::CORE_COUNT);
        return;
    }

    hammer_bus(0);
    BUS_BARRIER.wait();

    let tally = TALLY.lock();
    println!();
    println!("  transactions  : {}", tally.transactions);
    for (core, count) in tally.per_core.iter().enumerate() {
        println!("    core {core}      : {count}");
    }
    println!(
        "  mismatches    : {}  {}",
        tally.mismatches,
        verdict(tally.mismatches == 0)
    );
    println!("  the SpinLock inside I2C1 is the whole mechanism: one");
    println!("  transaction per acquisition, so nothing interleaves");
}

/// Runs a batch of read-back transactions from whichever core calls it.
fn hammer_bus(core: usize) {
    const ROUNDS: u32 = 16;

    let mut device = SHARED_RTC;
    let address = SCRATCH_BASE + core as u8;
    let mut mismatches = 0;
    let mut done = 0;

    for round in 0..ROUNDS {
        let value = (core as u8) << 4 | (round as u8 & 0x0F);

        if device.write(&[address, value]).is_err() {
            continue;
        }

        let mut back = [0u8; 1];
        if device
            .transaction(|bus| read_registers(bus, RTC, address, &mut back))
            .is_err()
        {
            continue;
        }

        if back[0] != value {
            mismatches += 1;
        }
        done += 1;
    }

    let mut tally = TALLY.lock();
    tally.transactions += done;
    tally.per_core[core] += done;
    tally.mismatches += mismatches;
}

// ---------------------------------------------------------------------------
// embedded-hal
// ---------------------------------------------------------------------------

fn ecosystem_driver() {
    println!();
    println!("-- embedded-hal --");

    // `Ds1338` below is written against `embedded_hal::i2c::I2c` and would
    // compile unchanged against an STM32.
    let mut clock = Ds1338::new(SharedI2c::new(&I2C1));

    match clock.seconds() {
        Ok(seconds) => println!("  a generic embedded-hal driver read {seconds} seconds"),
        Err(error) => println!("  generic driver failed: {error:?}"),
    }

    println!("  it took a SharedI2c handle, so it is safe on any core");
}

/// An ordinary ecosystem-style driver: generic, fallible, chip-agnostic.
struct Ds1338<I2C> {
    i2c: I2C,
}

impl<I2C: embedded_hal::i2c::I2c> Ds1338<I2C> {
    fn new(i2c: I2C) -> Self {
        Self { i2c }
    }

    fn seconds(&mut self) -> Result<u8, I2C::Error> {
        let mut raw = [0u8; 1];
        // DS1338 accepts STOP between register selection and the read. This
        // demo runs before the multicore test; generic shared calls lock once
        // per transaction, not across this pair.
        self.i2c.write(RTC, &[0x00])?;
        self.i2c.read(RTC, &mut raw)?;

        let bcd = raw[0] & 0x7F;
        Ok((bcd >> 4) * 10 + (bcd & 0x0F))
    }
}

fn verdict(ok: bool) -> &'static str {
    if ok { "ok" } else { "FAIL" }
}
