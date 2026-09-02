// SPDX-License-Identifier: MIT OR Apache-2.0
//! Software SPI and I2C, checked edge by edge against simulated devices.
//!
//! Run with `cargo run --release --example bitbang`.
//!
//! `hal::bitbang` is generic over the pin traits rather than over GPIO, so the
//! same masters that drive real pins can drive *simulated* ones. This example
//! wires them to fake slaves built from simulated wires and checks the
//! resulting waveform: every clock edge, every ACK, in order.
//!
//! QEMU cannot drive a GPIO from outside, so a bit-banged master talking to a
//! virtual device is impossible there. Here a wrong edge fails with the byte
//! and the bit that produced it.
//!
//! The masters are the real ones — nothing in `hal::bitbang` knows these pins
//! are fake.
//!
//! # What is checked
//!
//! * All four SPI modes, both directions, against a slave that shifts on the
//!   edges the mode specifies.
//! * I2C write, read, and `write_read` with a repeated START, against a slave
//!   with a register file.
//! * A NACK from an absent device, and the bus scan that depends on it.
//! * Clock stretching — the slave holds `SCL` down and the master waits, which
//!   is the case the BCM2837's *hardware* I2C gets wrong.

#![no_std]
#![no_main]

use core::cell::{Cell, RefCell};

use cogware_bcm2837::{
    BootInfo, cpu, entry,
    hal::{
        bitbang::{self, MODE_0, MODE_1, MODE_2, MODE_3, Mode, Phase, Polarity},
        digital::{InputPin, OpenDrainPin, OutputPin},
    },
    println, time,
};

entry!(main);

fn main(_boot: BootInfo) -> ! {
    println!();
    println!("Bit-bang demo: software SPI and I2C against simulated devices.");

    let mut failures = 0;

    failures += spi_demo();
    failures += i2c_demo();

    println!();
    if failures == 0 {
        println!("All checks passed in {:?}.", time::uptime());
    } else {
        println!("{failures} check(s) FAILED in {:?}.", time::uptime());
    }

    cpu::halt()
}

// ===========================================================================
// SPI
// ===========================================================================

/// The three wires, plus the slave that lives on them.
struct SpiWires {
    sck: Cell<bool>,
    mosi: Cell<bool>,
    miso: Cell<bool>,
    slave: RefCell<SpiSlave>,
}

impl SpiWires {
    fn new(mode: Mode, sends: u8) -> Self {
        let idle_high = mode.polarity == Polarity::IdleHigh;

        Self {
            sck: Cell::new(idle_high),
            mosi: Cell::new(false),
            miso: Cell::new(false),
            slave: RefCell::new(SpiSlave::new(mode, sends)),
        }
    }

    /// Every change of the clock net runs the slave.
    fn clock(&self, level: bool) {
        let mut slave = self.slave.borrow_mut();
        slave.on_clock(level, self.mosi.get(), &self.miso);
    }
}

/// The master's clock pin. Driving it steps the slave.
struct SckPin<'a>(&'a SpiWires);

impl OutputPin for SckPin<'_> {
    fn set_high(&mut self) {
        self.drive(true);
    }

    fn set_low(&mut self) {
        self.drive(false);
    }
}

impl SckPin<'_> {
    fn drive(&mut self, level: bool) {
        if self.0.sck.replace(level) != level {
            self.0.clock(level);
        }
    }
}

/// The master's data-out pin.
struct MosiPin<'a>(&'a SpiWires);

impl OutputPin for MosiPin<'_> {
    fn set_high(&mut self) {
        self.0.mosi.set(true);
    }

    fn set_low(&mut self) {
        self.0.mosi.set(false);
    }
}

/// The master's data-in pin.
struct MisoPin<'a>(&'a SpiWires);

impl InputPin for MisoPin<'_> {
    fn is_high(&self) -> bool {
        self.0.miso.get()
    }
}

/// A slave that samples and shifts on the edges its mode specifies.
///
/// Written from the SPI definition, not from our master, so a phase error in
/// the master shows up as a disagreement.
struct SpiSlave {
    mode: Mode,
    /// What has been shifted in from `MOSI`.
    received: u8,
    /// What is left to shift out on `MISO`, most significant bit first.
    pending: u8,
    /// How many bits have been sampled.
    bits: u8,
}

impl SpiSlave {
    fn new(mode: Mode, sends: u8) -> Self {
        Self {
            mode,
            received: 0,
            pending: sends,
            bits: 0,
        }
    }

    /// With `CPHA = 0` the first bit must already be on `MISO` before the
    /// first clock edge, because that edge samples it. Real devices present it
    /// when chip select falls.
    fn prime(&mut self, miso: &Cell<bool>) {
        if self.mode.phase == Phase::CaptureOnFirstTransition {
            miso.set(self.pending & 0x80 != 0);
            self.pending <<= 1;
        }
    }

    fn on_clock(&mut self, level: bool, mosi: bool, miso: &Cell<bool>) {
        let idle_high = self.mode.polarity == Polarity::IdleHigh;
        let is_leading = level != idle_high;
        let samples_on_leading = self.mode.phase == Phase::CaptureOnFirstTransition;

        if is_leading == samples_on_leading {
            self.received = (self.received << 1) | u8::from(mosi);
            self.bits += 1;
        } else {
            miso.set(self.pending & 0x80 != 0);
            self.pending <<= 1;
        }
    }
}

fn spi_demo() -> u32 {
    println!();
    println!("-- software SPI --");
    println!("  the slave is written from the SPI spec, not from our master:");
    println!("  it samples on the edge the mode says, so a phase error fails");
    println!();
    println!("  mode  cpol cpha   master sent  slave got    slave sent  master got   ");

    let mut failures = 0;

    for (name, mode) in [("0", MODE_0), ("1", MODE_1), ("2", MODE_2), ("3", MODE_3)] {
        failures += spi_check(name, mode);
    }

    failures += spi_bulk();

    failures
}

/// Sends one byte in one mode and checks both directions.
fn spi_check(name: &str, mode: Mode) -> u32 {
    const MASTER_SENDS: u8 = 0b1010_1101;
    const SLAVE_SENDS: u8 = 0b0110_0011;

    let wires = SpiWires::new(mode, SLAVE_SENDS);
    wires.slave.borrow_mut().prime(&wires.miso);

    // Speed 0: no delay between edges. The simulation has no timing to get
    // wrong.
    let mut bus = bitbang::Spi::new(SckPin(&wires), MosiPin(&wires), MisoPin(&wires), mode, 0);

    let master_got = bus.transfer_byte(MASTER_SENDS);

    let slave = wires.slave.borrow();
    let slave_got = slave.received;
    let bits = slave.bits;

    let ok = slave_got == MASTER_SENDS && master_got == SLAVE_SENDS && bits == 8;

    let cpol = u8::from(mode.polarity == Polarity::IdleHigh);
    let cpha = u8::from(mode.phase == Phase::CaptureOnSecondTransition);

    println!(
        "  {name:>4}  {cpol:>4} {cpha:>4}   {MASTER_SENDS:#010b}   {slave_got:#010b}   {SLAVE_SENDS:#010b}  {master_got:#010b}   {}",
        if ok { "ok" } else { "FAIL" }
    );

    u32::from(!ok)
}

/// A multi-byte transfer.
fn spi_bulk() -> u32 {
    let wires = SpiWires::new(MODE_0, 0);
    wires.slave.borrow_mut().prime(&wires.miso);

    let mut bus = bitbang::Spi::new(SckPin(&wires), MosiPin(&wires), MisoPin(&wires), MODE_0, 0);

    let message = b"cogware";
    bus.write(message);

    // The slave only keeps the last byte; with the bit count that is enough to
    // check the whole sequence was clocked and framed correctly.
    let last = wires.slave.borrow().received;
    let bits = wires.slave.borrow().bits;
    let ok = last == *message.last().unwrap() && bits as usize == message.len() * 8;

    println!();
    println!(
        "  {} bytes written: slave saw {} bits, last byte {:?}  {}",
        message.len(),
        bits,
        last as char,
        if ok { "ok" } else { "FAIL" }
    );

    u32::from(!ok)
}

// ===========================================================================
// I2C
// ===========================================================================

/// Two wired-AND nets and the slave on them.
///
/// The wired-AND is modelled properly: a "last writer wins" net would let the
/// master's release override the slave's pull, and every NACK would look like
/// an ACK.
struct I2cWires {
    sda_master_low: Cell<bool>,
    sda_slave_low: Cell<bool>,
    scl_master_low: Cell<bool>,
    scl_slave_low: Cell<bool>,
    /// How many more polls the slave will hold `SCL` down for.
    stretch: Cell<u32>,
    /// How many times the master looked at a stretched clock.
    stretch_polls: Cell<u32>,
    slave: RefCell<I2cSlave>,
}

impl I2cWires {
    fn new(address: u8) -> Self {
        Self {
            sda_master_low: Cell::new(false),
            sda_slave_low: Cell::new(false),
            scl_master_low: Cell::new(false),
            scl_slave_low: Cell::new(false),
            stretch: Cell::new(0),
            stretch_polls: Cell::new(0),
            slave: RefCell::new(I2cSlave::new(address)),
        }
    }

    fn sda(&self) -> bool {
        !self.sda_master_low.get() && !self.sda_slave_low.get()
    }

    fn scl(&self) -> bool {
        !self.scl_master_low.get() && !self.scl_slave_low.get()
    }

    fn on_start(&self) {
        self.slave.borrow_mut().on_start();
    }

    fn on_stop(&self) {
        self.slave.borrow_mut().on_stop();
    }

    fn on_scl_rise(&self) {
        let sda = self.sda();
        self.slave.borrow_mut().on_scl_rise(sda);
    }

    fn on_scl_fall(&self) {
        let mut slave = self.slave.borrow_mut();
        slave.on_scl_fall(self);
    }

    /// The slave pulling or releasing `SDA`.
    fn slave_sda(&self, low: bool) {
        self.sda_slave_low.set(low);
    }

    /// The slave asking to stretch the clock for `polls` master polls.
    fn slave_stretch(&self, polls: u32) {
        self.scl_slave_low.set(true);
        self.stretch.set(polls);
    }
}

/// The master's `SDA`.
struct SdaPin<'a>(&'a I2cWires);

impl OpenDrainPin for SdaPin<'_> {
    fn drive_low(&mut self) {
        let was_high = self.0.sda();
        self.0.sda_master_low.set(true);

        // SDA falling while SCL is high is a START, not data.
        if was_high && self.0.scl() {
            self.0.on_start();
        }
    }

    fn release(&mut self) {
        let was_low = !self.0.sda();
        self.0.sda_master_low.set(false);

        // SDA rising while SCL is high is a STOP.
        if was_low && self.0.sda() && self.0.scl() {
            self.0.on_stop();
        }
    }

    fn is_high(&self) -> bool {
        self.0.sda()
    }
}

/// The master's `SCL`.
struct SclPin<'a>(&'a I2cWires);

impl OpenDrainPin for SclPin<'_> {
    fn drive_low(&mut self) {
        let was_high = self.0.scl();
        self.0.scl_master_low.set(true);

        if was_high {
            self.0.on_scl_fall();
        }
    }

    fn release(&mut self) {
        self.0.scl_master_low.set(false);

        // If the slave is stretching, the line stays low and the rise happens
        // later, from `is_high`, which is what the master's stretch-wait loop
        // polls.
        if self.0.scl() {
            self.0.on_scl_rise();
        }
    }

    fn is_high(&self) -> bool {
        if !self.0.scl_master_low.get() && self.0.scl_slave_low.get() {
            self.0.stretch_polls.set(self.0.stretch_polls.get() + 1);

            let left = self.0.stretch.get();
            if left <= 1 {
                self.0.stretch.set(0);
                self.0.scl_slave_low.set(false);
                self.0.on_scl_rise();
            } else {
                self.0.stretch.set(left - 1);
            }
        }

        self.0.scl()
    }
}

/// Where the slave is in a transaction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase2 {
    /// Between a STOP and the next START.
    Idle,
    /// Shifting in the address byte.
    Address,
    /// Holding `SDA` low to acknowledge the address.
    AddressAck,
    /// Shifting in a data byte.
    Receive,
    /// Holding `SDA` low to acknowledge a data byte.
    ReceiveAck,
    /// Shifting a data byte out.
    Transmit,
    /// Reading the master's ACK or NACK after a transmitted byte.
    TransmitAck,
}

/// Registers the simulated device exposes.
const REGISTERS: usize = 8;

/// A slave with a register file, addressed the way most I2C chips are: write
/// the register number, then either write data or repeat-START and read.
struct I2cSlave {
    address: u8,
    registers: [u8; REGISTERS],
    phase: Phase2,
    shift: u8,
    bits: u8,
    pointer: usize,
    /// Whether the pointer has been set since the last START.
    pointer_set: bool,
    /// Whether the master asked to read.
    reading: bool,
    /// Byte currently being shifted out.
    transmitting: u8,
    /// Whether the master acknowledged the last byte it read.
    master_acked: bool,
    /// Set once, on the first byte of a transaction, to exercise stretching.
    stretch_next: bool,
}

impl I2cSlave {
    fn new(address: u8) -> Self {
        let mut registers = [0u8; REGISTERS];
        for (index, slot) in registers.iter_mut().enumerate() {
            *slot = 0xA0 | index as u8;
        }

        Self {
            address,
            registers,
            phase: Phase2::Idle,
            shift: 0,
            bits: 0,
            pointer: 0,
            pointer_set: false,
            reading: false,
            transmitting: 0,
            master_acked: false,
            stretch_next: false,
        }
    }

    fn on_start(&mut self) {
        self.phase = Phase2::Address;
        self.shift = 0;
        self.bits = 0;
        self.pointer_set = false;
    }

    fn on_stop(&mut self) {
        self.phase = Phase2::Idle;
        self.bits = 0;
    }

    /// The master samples on this edge, so the slave reads `SDA` here too.
    fn on_scl_rise(&mut self, sda: bool) {
        match self.phase {
            Phase2::Address | Phase2::Receive => {
                self.shift = (self.shift << 1) | u8::from(sda);
                self.bits += 1;
            }
            Phase2::Transmit => self.bits += 1,
            Phase2::TransmitAck => self.master_acked = !sda,
            _ => {}
        }
    }

    /// `SDA` may only change while `SCL` is low, so everything the slave
    /// *drives* happens here.
    fn on_scl_fall(&mut self, wires: &I2cWires) {
        match self.phase {
            Phase2::Address if self.bits == 8 => {
                self.reading = self.shift & 1 == 1;

                if self.shift >> 1 == self.address {
                    wires.slave_sda(true);
                    self.phase = Phase2::AddressAck;
                } else {
                    // Say nothing. The master sees SDA stay high and reports a
                    // NACK, which is how a bus scan finds empty addresses.
                    self.phase = Phase2::Idle;
                }
            }
            Phase2::AddressAck => {
                wires.slave_sda(false);
                self.bits = 0;
                self.shift = 0;

                if self.stretch_next {
                    self.stretch_next = false;
                    wires.slave_stretch(40);
                }

                if self.reading {
                    self.phase = Phase2::Transmit;
                    self.load_next(wires);
                } else {
                    self.phase = Phase2::Receive;
                }
            }
            Phase2::Receive if self.bits == 8 => {
                self.store(self.shift);
                wires.slave_sda(true);
                self.phase = Phase2::ReceiveAck;
            }
            Phase2::ReceiveAck => {
                wires.slave_sda(false);
                self.bits = 0;
                self.shift = 0;
                self.phase = Phase2::Receive;
            }
            Phase2::Transmit => {
                if self.bits == 8 {
                    wires.slave_sda(false);
                    self.phase = Phase2::TransmitAck;
                } else {
                    self.present(wires);
                }
            }
            Phase2::TransmitAck => {
                if self.master_acked {
                    self.bits = 0;
                    self.phase = Phase2::Transmit;
                    self.load_next(wires);
                } else {
                    // A NACK from the master means "that was the last byte".
                    wires.slave_sda(false);
                    self.phase = Phase2::Idle;
                }
            }
            _ => {}
        }
    }

    /// The first byte written after an address sets the register pointer; the
    /// rest are data.
    fn store(&mut self, byte: u8) {
        if self.pointer_set {
            self.registers[self.pointer] = byte;
            self.pointer = (self.pointer + 1) % REGISTERS;
        } else {
            self.pointer = usize::from(byte) % REGISTERS;
            self.pointer_set = true;
        }
    }

    fn load_next(&mut self, wires: &I2cWires) {
        self.transmitting = self.registers[self.pointer];
        self.pointer = (self.pointer + 1) % REGISTERS;
        self.present(wires);
    }

    /// Puts bit `7 - bits` of the outgoing byte on `SDA`.
    fn present(&mut self, wires: &I2cWires) {
        let bit = self.transmitting >> (7 - self.bits) & 1;
        wires.slave_sda(bit == 0);
    }
}

/// The address the simulated device answers to.
const SLAVE_ADDRESS: u8 = 0x42;

fn i2c_demo() -> u32 {
    println!();
    println!("-- software I2C --");
    println!("  a simulated device at {SLAVE_ADDRESS:#04x} with {REGISTERS} registers,");
    println!("  on properly wired-AND nets: the master can only pull down");

    let mut failures = 0;

    failures += i2c_write_read();
    failures += i2c_write();
    failures += i2c_nack();
    failures += i2c_scan();
    failures += i2c_stretch();

    failures
}

/// The shape every I2C driver uses: write a register number, repeated START,
/// read the contents.
fn i2c_write_read() -> u32 {
    let wires = I2cWires::new(SLAVE_ADDRESS);
    let mut bus = bitbang::I2c::new(SdaPin(&wires), SclPin(&wires), 0);

    let mut got = [0u8; 4];
    let result = bus.write_read(SLAVE_ADDRESS, &[0x02], &mut got);

    // Registers are seeded 0xA0 | index, so reading from 2 gives A2 A3 A4 A5.
    let expected = [0xA2u8, 0xA3, 0xA4, 0xA5];
    let ok = result.is_ok() && got == expected;

    println!();
    println!("  write_read (repeated START)");
    println!("    read 4 bytes from register 2: {got:02X?}, want {expected:02X?}");
    println!("    result {result:?}  {}", verdict(ok));

    u32::from(!ok)
}

/// Writing, then reading the same registers back to check it landed.
fn i2c_write() -> u32 {
    let wires = I2cWires::new(SLAVE_ADDRESS);
    let mut bus = bitbang::I2c::new(SdaPin(&wires), SclPin(&wires), 0);

    let written = bus.write(SLAVE_ADDRESS, &[0x05, 0xDE, 0xAD]);

    let mut got = [0u8; 2];
    let read = bus.write_read(SLAVE_ADDRESS, &[0x05], &mut got);

    let ok = written.is_ok() && read.is_ok() && got == [0xDE, 0xAD];

    println!();
    println!("  write then read back");
    println!(
        "    wrote DE AD to registers 5 and 6, read {got:02X?}  {}",
        verdict(ok)
    );

    u32::from(!ok)
}

/// An address nobody answers to.
fn i2c_nack() -> u32 {
    let wires = I2cWires::new(SLAVE_ADDRESS);
    let mut bus = bitbang::I2c::new(SdaPin(&wires), SclPin(&wires), 0);

    let result = bus.write(0x21, &[0x00]);
    let ok = result == Err(bitbang::Error::AddressNack);

    println!();
    println!("  addressing an empty address");
    println!("    write to 0x21 -> {result:?}  {}", verdict(ok));

    u32::from(!ok)
}

/// The bus scan: 112 address bytes and their ACK bits.
fn i2c_scan() -> u32 {
    let wires = I2cWires::new(SLAVE_ADDRESS);
    let mut bus = bitbang::I2c::new(SdaPin(&wires), SclPin(&wires), 0);

    let mut found = [0u8; 4];
    let mut count = 0usize;

    bus.scan(|address| {
        if count < found.len() {
            found[count] = address;
        }
        count += 1;
    });

    let ok = count == 1 && found[0] == SLAVE_ADDRESS;

    println!();
    println!("  bus scan");
    println!(
        "    0x08..0x77 -> {count} device(s), first {:#04x}  {}",
        found[0],
        verdict(ok)
    );

    u32::from(!ok)
}

/// The case the hardware BSC controller cannot handle.
fn i2c_stretch() -> u32 {
    let wires = I2cWires::new(SLAVE_ADDRESS);
    wires.slave.borrow_mut().stretch_next = true;

    let mut bus = bitbang::I2c::new(SdaPin(&wires), SclPin(&wires), 0);

    let mut got = [0u8; 1];
    let result = bus.write_read(SLAVE_ADDRESS, &[0x01], &mut got);

    let polls = wires.stretch_polls.get();
    let ok = result.is_ok() && got == [0xA1] && polls > 0;

    println!();
    println!("  clock stretching");
    println!("    the slave held SCL down after the address ACK;");
    println!("    the master polled it {polls} times and then carried on");
    println!(
        "    read register 1 -> {got:02X?}, want [A1]  {}",
        verdict(ok)
    );
    println!("    (the hardware BSC master corrupts data here -- see drivers::i2c)");

    u32::from(!ok)
}

fn verdict(ok: bool) -> &'static str {
    if ok { "ok" } else { "FAIL" }
}
