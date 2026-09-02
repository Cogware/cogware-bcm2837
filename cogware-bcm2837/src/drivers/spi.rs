// SPDX-License-Identifier: MIT OR Apache-2.0
//! BCM2835 SPI0 master.
//!
//! SPI0 is the full master: 64-byte TX and RX FIFOs, three chip selects with
//! programmable polarity, and a clock dividing the 250 MHz core clock from
//! 125 MHz down to 3.8 kHz. SPI1 and SPI2 live in the AUX block and are not
//! implemented here.
//!
//! # Pins
//!
//! SPI0 is on GPIO 7-11, all ALT0. [`Spi0::init`] routes them.
//!
//! | GPIO | Signal |
//! | --- | --- |
//! | 7 | `CE1` (chip enable 1, active low) |
//! | 8 | `CE0` (chip enable 0, active low) |
//! | 9 | `MISO` |
//! | 10 | `MOSI` |
//! | 11 | `SCLK` |
//!
//! # Chip select
//!
//! The controller can drive `CE0`/`CE1` itself, asserting for the duration of
//! each transfer. This needs no extra pins but limits to two devices and
//! deasserts between the two halves of a write-then-read.
//!
//! [`ChipSelect::None`] leaves both lines free for a GPIO-driven chip select,
//! as [`crate::hal::shared::SpiDevice`] does: any number of devices, with chip
//! select held across a whole transaction.
//!
//! # What "done" means
//!
//! SPI is full duplex: every byte clocked out clocks one in. A write-only
//! transfer still fills the RX FIFO, which stalls the controller at 64 bytes if
//! not drained. Every routine here reads back as many bytes as it writes,
//! discarding what the caller did not ask for.
//!
//! Clearing `TA` also writes `DONE`, which the datasheet describes as read-
//! only. Writing it is load-bearing: transmission sometimes breaks otherwise,
//! so the bit is really write-1-to-clear or a silicon bug. Linux does the same.
#![allow(dead_code)]

use super::mmio::{ReadWrite, RegisterBlock};
use crate::{
    bsp::memory::map,
    drivers::{GPIO, gpio::Function},
};

/// The VideoCore core clock, which SPI0 divides down.
///
/// 250 MHz in the default firmware configuration. The datasheet's "nominally
/// 150 MHz" is a documented erratum and wrong for this board. Change this if
/// `core_freq` in `config.txt` changes.
pub const CORE_CLOCK_HZ: u32 = 250_000_000;

/// Turns a `CDIV` field value into the bit rate it produces.
///
/// `CDIV` is 16 bits wide and the datasheet gives zero a special meaning: it
/// divides by 65536 rather than being invalid.
const fn clock_from_divider(divider: u32) -> u32 {
    CORE_CLOCK_HZ / if divider == 0 { 65_536 } else { divider }
}

/// GPIO pins SPI0 uses, in the order CE1, CE0, MISO, MOSI, SCLK.
pub const PINS: [u8; 5] = [7, 8, 9, 10, 11];

/// Clock polarity: the level `SCLK` rests at between transfers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Polarity {
    /// `SCLK` idles low. The leading edge of each bit is a rising edge.
    IdleLow,
    /// `SCLK` idles high. The leading edge of each bit is a falling edge.
    IdleHigh,
}

/// Clock phase: which clock edge of each bit period samples the data.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Data is sampled on the leading edge and changed on the trailing edge.
    CaptureOnFirstTransition,
    /// Data is changed on the leading edge and sampled on the trailing edge.
    CaptureOnSecondTransition,
}

/// One of the four standard SPI modes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mode {
    /// Which level `SCLK` idles at.
    pub polarity: Polarity,
    /// Which edge samples data.
    pub phase: Phase,
}

/// Mode 0: idle low, sample on the rising edge. The common default.
pub const MODE_0: Mode = Mode {
    polarity: Polarity::IdleLow,
    phase: Phase::CaptureOnFirstTransition,
};

/// Mode 1: idle low, sample on the falling edge.
pub const MODE_1: Mode = Mode {
    polarity: Polarity::IdleLow,
    phase: Phase::CaptureOnSecondTransition,
};

/// Mode 2: idle high, sample on the falling edge.
pub const MODE_2: Mode = Mode {
    polarity: Polarity::IdleHigh,
    phase: Phase::CaptureOnFirstTransition,
};

/// Mode 3: idle high, sample on the rising edge.
pub const MODE_3: Mode = Mode {
    polarity: Polarity::IdleHigh,
    phase: Phase::CaptureOnSecondTransition,
};

/// Which chip-enable line the controller should drive.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChipSelect {
    /// Assert `CE0` (GPIO 8) for the duration of each transfer.
    Ce0,
    /// Assert `CE1` (GPIO 7) for the duration of each transfer.
    Ce1,
    /// Drive neither; the caller handles chip select. For more than two devices
    /// or a multi-part transaction.
    None,
}

impl ChipSelect {
    /// The `CS[1:0]` field value. `None` parks on CE2, which is not routed to
    /// any pin on this board, so nothing is asserted.
    const fn bits(self) -> u32 {
        match self {
            ChipSelect::Ce0 => 0b00,
            ChipSelect::Ce1 => 0b01,
            ChipSelect::None => 0b10,
        }
    }
}

/// How the bus should be set up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// Requested bit clock. Rounded to an achievable divider by
    /// [`Spi0::set_speed_hz`].
    pub speed_hz: u32,
    /// Clock polarity and phase.
    pub mode: Mode,
    /// Which chip-enable line, if any, the controller drives.
    pub chip_select: ChipSelect,
    /// Whether chip select is active high. Almost every device wants `false`.
    pub cs_active_high: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            speed_hz: 1_000_000,
            mode: MODE_0,
            chip_select: ChipSelect::Ce0,
            cs_active_high: false,
        }
    }
}

/// Something went wrong on the bus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// A transfer did not complete within the timeout.
    ///
    /// Only reachable with an uninitialised controller or a stopped clock.
    Timeout,
    /// The transfer was longer than the 16-bit length the controller accepts.
    TooLong,
}

// CS register bits (datasheet section 10.5).
const CS_CS_MASK: u32 = 0b11;
const CS_CPHA: u32 = 1 << 2;
const CS_CPOL: u32 = 1 << 3;
const CS_CLEAR_TX: u32 = 1 << 4;
const CS_CLEAR_RX: u32 = 1 << 5;
const CS_CSPOL: u32 = 1 << 6;
const CS_TA: u32 = 1 << 7;
const CS_DMAEN: u32 = 1 << 8;
const CS_INTD: u32 = 1 << 9;
const CS_INTR: u32 = 1 << 10;
const CS_ADCS: u32 = 1 << 11;
const CS_REN: u32 = 1 << 12;
const CS_LEN: u32 = 1 << 13;
const CS_DONE: u32 = 1 << 16;
const CS_RXD: u32 = 1 << 17;
const CS_TXD: u32 = 1 << 18;
const CS_RXR: u32 = 1 << 19;
const CS_RXF: u32 = 1 << 20;
const CS_CSPOL0: u32 = 1 << 21;

/// Iterations of the polling loop before a transfer is declared stuck.
///
/// Generous: the slowest configurable clock is 3.8 kHz, where one byte takes
/// about 2 ms.
const POLL_LIMIT: u32 = 20_000_000;

#[repr(C)]
struct Registers {
    cs: ReadWrite<u32>,   // 0x00
    fifo: ReadWrite<u32>, // 0x04
    clk: ReadWrite<u32>,  // 0x08
    dlen: ReadWrite<u32>, // 0x0C
    ltoh: ReadWrite<u32>, // 0x10
    dc: ReadWrite<u32>,   // 0x14
}

const _: () = assert!(core::mem::offset_of!(Registers, clk) == 0x08);
const _: () = assert!(core::mem::offset_of!(Registers, dc) == 0x14);

/// The SPI0 master.
pub struct Spi0 {
    block: RegisterBlock<Registers>,
    initialised: bool,
}

impl Spi0 {
    /// Creates the driver for the SPI0 block at [`map::SPI0`].
    ///
    /// # Safety
    ///
    /// Only one instance may exist, and it must be behind a lock before
    /// secondary cores are released.
    pub const unsafe fn new() -> Self {
        Self {
            block: unsafe { RegisterBlock::new(map::SPI0) },
            initialised: false,
        }
    }

    /// Routes GPIO 7-11 to SPI0 and applies `config`.
    ///
    /// Safe to call more than once; a second call just reconfigures.
    ///
    /// # Lock order
    ///
    /// Takes [`crate::drivers::GPIO`] while the caller holds
    /// [`crate::drivers::SPI0`]. Nothing may take them the other way round.
    pub fn init(&mut self, config: Config) {
        {
            let mut gpio = GPIO.lock();
            for pin in PINS {
                gpio.set_function(pin, Function::Alt0);
            }
        }

        let regs = self.block.regs();

        // Park the controller: TA off, FIFOs empty, interrupts and DMA off.
        // `DONE` is written for the reason in the module documentation.
        regs.cs.write(CS_CLEAR_TX | CS_CLEAR_RX | CS_DONE);

        self.set_mode(config.mode);
        self.set_speed_hz(config.speed_hz);
        self.set_chip_select(config.chip_select, config.cs_active_high);

        self.initialised = true;
    }

    /// Whether [`Spi0::init`] has run.
    pub const fn is_initialised(&self) -> bool {
        self.initialised
    }

    /// Sets the clock polarity and phase.
    pub fn set_mode(&mut self, mode: Mode) {
        let mut bits = 0;
        if mode.polarity == Polarity::IdleHigh {
            bits |= CS_CPOL;
        }
        if mode.phase == Phase::CaptureOnSecondTransition {
            bits |= CS_CPHA;
        }

        self.block
            .regs()
            .cs
            .modify(|value| (value & !(CS_CPOL | CS_CPHA)) | bits);
    }

    /// Selects which chip-enable line the controller drives, and its polarity.
    pub fn set_chip_select(&mut self, chip_select: ChipSelect, active_high: bool) {
        let mut bits = chip_select.bits();
        if active_high {
            bits |= CS_CSPOL | CS_CSPOL0;
        }

        self.block
            .regs()
            .cs
            .modify(|value| (value & !(CS_CS_MASK | CS_CSPOL | CS_CSPOL0)) | bits);
    }

    /// Sets the bit clock as close to `speed_hz` as the divider allows, and
    /// returns the frequency actually programmed.
    ///
    /// `SCLK = 250 MHz / CDIV`. The datasheet claims `CDIV` must be a power of
    /// two, but the hardware ignores the low bit, so this rounds up to an even
    /// divider. A `CDIV` of 0 means 65536, the slowest setting, about 3.8 kHz.
    pub fn set_speed_hz(&mut self, speed_hz: u32) -> u32 {
        let mut divider = if speed_hz == 0 {
            0
        } else {
            CORE_CLOCK_HZ.div_ceil(speed_hz)
        };

        // The low bit is ignored by hardware; round up so the clock is never
        // faster than asked for.
        divider += divider & 1;

        // The field is 16 bits and zero means 65536.
        if divider >= 65_536 {
            divider = 0;
        }

        self.block.regs().clk.write(divider);

        clock_from_divider(divider)
    }

    /// The frequency currently programmed, in Hz.
    pub fn speed_hz(&self) -> u32 {
        clock_from_divider(self.block.regs().clk.read() & 0xFFFF)
    }

    /// Clocks `write` out and `read` in, simultaneously.
    ///
    /// The two slices may differ in length: the shorter one is padded, sending
    /// zeros once `write` runs out and discarding bytes once `read` is full.
    /// The transfer is as long as the longer slice.
    pub fn transfer(&mut self, read: &mut [u8], write: &[u8]) -> Result<(), Error> {
        let length = read.len().max(write.len());
        if length == 0 {
            return Ok(());
        }

        self.begin()?;
        let result = self.pump(read, write, length);
        self.end();
        result
    }

    /// Clocks `data` out, discarding everything that comes back.
    pub fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        self.transfer(&mut [], data)
    }

    /// Clocks zeros out and `data` in.
    pub fn read(&mut self, data: &mut [u8]) -> Result<(), Error> {
        self.transfer(data, &[])
    }

    /// Clocks `data` out and overwrites it with what came back.
    pub fn transfer_in_place(&mut self, data: &mut [u8]) -> Result<(), Error> {
        if data.is_empty() {
            return Ok(());
        }

        self.begin()?;

        let mut sent = 0;
        let mut received = 0;
        let mut spins = 0u32;

        while received < data.len() {
            let regs = self.block.regs();

            let mut progressed = false;
            while sent < data.len() && regs.cs.read() & CS_TXD != 0 {
                regs.fifo.write(u32::from(data[sent]));
                sent += 1;
                progressed = true;
            }
            while received < sent && regs.cs.read() & CS_RXD != 0 {
                data[received] = (regs.fifo.read() & 0xFF) as u8;
                received += 1;
                progressed = true;
            }

            if progressed {
                spins = 0;
            } else {
                spins += 1;
                if spins > POLL_LIMIT {
                    self.end();
                    return Err(Error::Timeout);
                }
                core::hint::spin_loop();
            }
        }

        self.end();
        Ok(())
    }

    /// Waits for the shift register to drain.
    ///
    /// Every routine here already leaves the bus idle, so this is a no-op kept
    /// for the `embedded-hal` `SpiBus::flush` contract.
    pub fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// Empties both FIFOs and asserts `TA`, starting a transfer.
    fn begin(&mut self) -> Result<(), Error> {
        let regs = self.block.regs();
        regs.cs.modify(|value| {
            (value & !(CS_DMAEN | CS_INTR | CS_INTD | CS_REN | CS_LEN))
                | CS_CLEAR_TX
                | CS_CLEAR_RX
                | CS_DONE
        });
        regs.cs.modify(|value| value | CS_TA);
        Ok(())
    }

    /// Deasserts `TA` and clears the FIFOs, ending a transfer.
    ///
    /// Writes `DONE` on the way past — see the module documentation.
    fn end(&mut self) {
        self.block
            .regs()
            .cs
            .modify(|value| (value & !CS_TA) | CS_CLEAR_TX | CS_CLEAR_RX | CS_DONE);
    }

    /// The full-duplex inner loop: keep the TX FIFO fed and the RX FIFO drained
    /// until `length` bytes have gone each way.
    fn pump(&mut self, read: &mut [u8], write: &[u8], length: usize) -> Result<(), Error> {
        let regs = self.block.regs();

        let mut sent = 0;
        let mut received = 0;
        let mut spins = 0u32;

        while received < length {
            let mut progressed = false;

            // Feed. Past the end of `write`, send zeros to keep clocking.
            while sent < length && regs.cs.read() & CS_TXD != 0 {
                let byte = write.get(sent).copied().unwrap_or(0);
                regs.fifo.write(u32::from(byte));
                sent += 1;
                progressed = true;
            }

            // Drain. Bytes past the end of `read` must still be removed, or the
            // RX FIFO fills and the transfer stalls.
            while received < sent && regs.cs.read() & CS_RXD != 0 {
                let byte = (regs.fifo.read() & 0xFF) as u8;
                if let Some(slot) = read.get_mut(received) {
                    *slot = byte;
                }
                received += 1;
                progressed = true;
            }

            if progressed {
                spins = 0;
            } else {
                spins += 1;
                if spins > POLL_LIMIT {
                    return Err(Error::Timeout);
                }
                core::hint::spin_loop();
            }
        }

        Ok(())
    }
}
