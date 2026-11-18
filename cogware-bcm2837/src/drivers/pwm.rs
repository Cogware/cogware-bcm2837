// SPDX-License-Identifier: GPL-3.0-only
//! The PWM controller: two channels sharing one clock and one FIFO.
//!
//! Each channel counts its input clock up to `range` and drives its pin from
//! either a data register or the shared FIFO. In mark-space mode the output is
//! high for the first `data` counts of each period, which is what servos and
//! LED dimming want; in balanced mode the same duty is spread as evenly as
//! possible across the period, which filters better into an analogue level.
//! Serialiser mode instead shifts each 32-bit word out MSB first, one bit per
//! clock, which is how WS2812 strips and PWM audio are fed — usually by DMA
//! through [`Pwm::fifo_bus_address`].
//!
//! # Pins
//!
//! | Channel | GPIO (function) |
//! | --- | --- |
//! | [`Channel::One`] | 12 (ALT0), 18 (ALT5), 40 (ALT0), 52 (ALT1) |
//! | [`Channel::Two`] | 13 (ALT0), 19 (ALT5), 41 (ALT0), 45 (ALT0), 53 (ALT1) |
#![allow(dead_code)]

use super::{
    clock::{self, ClockId, Mash, Source},
    gpio::Function,
    mmio::{ReadWrite, RegisterBlock, WriteOnly},
};
use crate::{
    bsp::memory::{bus, map},
    cpu,
    drivers::{CLOCKS, GPIO},
};

const CTL_PWEN: u32 = 1 << 0;
const CTL_MODE_SERIAL: u32 = 1 << 1;
const CTL_RPTL: u32 = 1 << 2;
const CTL_SBIT: u32 = 1 << 3;
const CTL_POLA: u32 = 1 << 4;
const CTL_USEF: u32 = 1 << 5;
const CTL_CLRF: u32 = 1 << 6;
const CTL_MSEN: u32 = 1 << 7;
/// Channel two's control bits are channel one's, shifted up a byte.
const CTL_CHANNEL_SHIFT: u32 = 8;

const STA_FULL: u32 = 1 << 0;
const STA_EMPTY: u32 = 1 << 1;
/// Write error, read error, both gap flags and the bus error: all
/// write-1-to-clear.
const STA_ERRORS: u32 = 0b1_0011_1100;

const DMAC_ENAB: u32 = 1 << 31;

/// Polls to make for FIFO space. The FIFO is sixteen words deep and drains at
/// the PWM clock rate over the channel's range, so this only expires if the
/// channel is not running at all.
const POLL_LIMIT: u32 = 1_000_000;

#[repr(C)]
struct Registers {
    ctl: ReadWrite<u32>,  // 0x00
    sta: ReadWrite<u32>,  // 0x04
    dmac: ReadWrite<u32>, // 0x08
    _res0: u32,           // 0x0c
    rng1: ReadWrite<u32>, // 0x10
    dat1: ReadWrite<u32>, // 0x14
    fif1: WriteOnly<u32>, // 0x18
    _res1: u32,           // 0x1c
    rng2: ReadWrite<u32>, // 0x20
    dat2: ReadWrite<u32>, // 0x24
}

const _: () = assert!(core::mem::offset_of!(Registers, fif1) == 0x18);
const _: () = assert!(core::mem::offset_of!(Registers, dat2) == 0x24);

/// FIFO offset, for [`Pwm::fifo_bus_address`].
const FIF1_OFFSET: usize = 0x18;

/// A PWM channel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Channel {
    /// Channel 1 (`PWM0` in the pin tables).
    One,
    /// Channel 2 (`PWM1` in the pin tables).
    Two,
}

impl Channel {
    const fn shift(self) -> u32 {
        match self {
            Self::One => 0,
            Self::Two => CTL_CHANNEL_SHIFT,
        }
    }
}

/// What a channel's output does with each value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// High for the first `data` counts of `range`, then low.
    MarkSpace,
    /// `data` high counts of `range`, spread evenly over the period.
    Balanced,
    /// Each value is shifted out MSB first, `range` bits of it, one per clock.
    Serialiser,
}

/// How a channel is set up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// Counts per period, or bits per word in [`Mode::Serialiser`].
    pub range: u32,
    /// The output algorithm.
    pub mode: Mode,
    /// Invert the output.
    pub invert: bool,
    /// The level the pin rests at when there is nothing to send.
    pub idle_high: bool,
    /// Take values from the shared FIFO rather than the data register.
    pub use_fifo: bool,
    /// When the FIFO runs dry, keep repeating the last value rather than
    /// going idle.
    pub repeat_last: bool,
}

impl Config {
    /// A mark-space channel of `range` counts, fed from the data register.
    pub const fn mark_space(range: u32) -> Self {
        Self {
            range,
            mode: Mode::MarkSpace,
            invert: false,
            idle_high: false,
            use_fifo: false,
            repeat_last: false,
        }
    }
}

/// Why a PWM operation failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The clock manager refused the rate.
    Clock(clock::Error),
    /// The pin carries neither PWM channel.
    NotAPwmPin(u8),
    /// The FIFO stayed full: the channel is not running or not using it.
    FifoStalled,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Clock(error) => write!(f, "PWM clock: {error}"),
            Self::NotAPwmPin(pin) => write!(f, "GPIO {pin} has no PWM function"),
            Self::FifoStalled => f.write_str("PWM FIFO stalled"),
        }
    }
}

impl From<clock::Error> for Error {
    fn from(error: clock::Error) -> Self {
        Self::Clock(error)
    }
}

/// The channel and function a pin carries PWM on, if it does.
pub const fn pin_function(pin: u8) -> Option<(Channel, Function)> {
    Some(match pin {
        12 | 40 => (Channel::One, Function::Alt0),
        18 => (Channel::One, Function::Alt5),
        52 => (Channel::One, Function::Alt1),
        13 | 41 | 45 => (Channel::Two, Function::Alt0),
        19 => (Channel::Two, Function::Alt5),
        53 => (Channel::Two, Function::Alt1),
        _ => return None,
    })
}

/// The BCM2837 PWM controller.
pub struct Pwm {
    block: RegisterBlock<Registers>,
    clock_hz: u32,
}

impl Pwm {
    /// Creates the driver for the PWM block at [`map::PWM`].
    ///
    /// # Safety
    ///
    /// Only one instance may exist, and it must be placed behind a lock before
    /// secondary cores are released.
    pub const unsafe fn new() -> Self {
        Self {
            block: unsafe { RegisterBlock::new(map::PWM) },
            clock_hz: 0,
        }
    }

    /// Stops both channels and clocks the controller as near `hz` as possible,
    /// returning the rate it actually runs at. Takes [`CLOCKS`].
    ///
    /// The oscillator is used up to half its rate and PLLD above that; both
    /// with an integer divisor, so every period is the same length.
    pub fn init(&mut self, hz: u32) -> Result<u32, Error> {
        let regs = self.block.regs();
        regs.ctl.write(0);
        regs.dmac.write(0);
        regs.sta.write(STA_ERRORS);

        let source = if hz <= clock::OSCILLATOR_HZ / 2 {
            Source::Oscillator
        } else {
            Source::PllD
        };
        self.clock_hz = CLOCKS
            .lock()
            .enable(ClockId::Pwm, source, hz, Mash::Integer)?;
        // The controller is clocked by what it is configuring; give the new
        // clock a few cycles to arrive before the registers are written.
        cpu::spin_cycles(1_000);
        Ok(self.clock_hz)
    }

    /// The controller's clock rate, as set by [`init`](Self::init).
    pub fn clock_hz(&self) -> u32 {
        self.clock_hz
    }

    /// Switches `pin` to its PWM function and returns the channel it carries.
    /// Takes [`GPIO`].
    pub fn route(&mut self, pin: u8) -> Result<Channel, Error> {
        let (channel, function) = pin_function(pin).ok_or(Error::NotAPwmPin(pin))?;
        GPIO.lock().set_function(pin, function);
        Ok(channel)
    }

    /// Sets `channel` up as `config` describes and starts it.
    pub fn configure(&mut self, channel: Channel, config: Config) {
        let regs = self.block.regs();
        let mut bits = CTL_PWEN;
        match config.mode {
            Mode::MarkSpace => bits |= CTL_MSEN,
            Mode::Balanced => {}
            Mode::Serialiser => bits |= CTL_MODE_SERIAL,
        }
        for (set, bit) in [
            (config.invert, CTL_POLA),
            (config.idle_high, CTL_SBIT),
            (config.use_fifo, CTL_USEF),
            (config.repeat_last, CTL_RPTL),
        ] {
            if set {
                bits |= bit;
            }
        }

        let mask = 0xff << channel.shift();
        regs.ctl.modify(|ctl| ctl & !mask & !CTL_CLRF);
        match channel {
            Channel::One => regs.rng1.write(config.range),
            Channel::Two => regs.rng2.write(config.range),
        }
        regs.ctl
            .modify(|ctl| (ctl & !mask & !CTL_CLRF) | bits << channel.shift());
    }

    /// Sets `channel`'s data register: high counts per period.
    pub fn set_duty(&mut self, channel: Channel, data: u32) {
        let regs = self.block.regs();
        match channel {
            Channel::One => regs.dat1.write(data),
            Channel::Two => regs.dat2.write(data),
        }
    }

    /// Configures `channel` as mark-space for an output of `hz` and returns
    /// the range, so a duty of `range / 2` is a square wave.
    pub fn set_frequency(&mut self, channel: Channel, hz: u32) -> u32 {
        let range = (self.clock_hz / hz.max(1)).max(1);
        self.configure(channel, Config::mark_space(range));
        range
    }

    /// Stops `channel`; the pin goes to its idle level.
    pub fn disable(&mut self, channel: Channel) {
        self.block
            .regs()
            .ctl
            .modify(|ctl| ctl & !CTL_CLRF & !(CTL_PWEN << channel.shift()));
    }

    /// Pushes `words` into the FIFO, waiting for space as it drains.
    pub fn write_fifo(&mut self, words: &[u32]) -> Result<(), Error> {
        let regs = self.block.regs();
        for &word in words {
            let mut polls = 0;
            while regs.sta.read() & STA_FULL != 0 {
                polls += 1;
                if polls == POLL_LIMIT {
                    return Err(Error::FifoStalled);
                }
                cpu::spin_cycles(10);
            }
            regs.fif1.write(word);
        }
        Ok(())
    }

    /// Discards whatever is waiting in the FIFO.
    pub fn clear_fifo(&mut self) {
        self.block.regs().ctl.modify(|ctl| ctl | CTL_CLRF);
    }

    /// Whether the FIFO has drained.
    pub fn fifo_empty(&self) -> bool {
        self.block.regs().sta.read() & STA_EMPTY != 0
    }

    /// Lets a DMA channel paced by [`super::dma::Dreq::Pwm`] feed the FIFO,
    /// raising its request while fewer than `threshold` words are queued.
    pub fn enable_dma(&mut self, threshold: u8) {
        // Panic threshold equal to the request threshold, as the reset value
        // has it.
        let threshold = u32::from(threshold.clamp(1, 15));
        self.block
            .regs()
            .dmac
            .write(DMAC_ENAB | threshold << 8 | threshold);
    }

    /// Stops DMA requests.
    pub fn disable_dma(&mut self) {
        self.block.regs().dmac.write(0);
    }

    /// Bus address of the FIFO, for a DMA control block's destination.
    pub fn fifo_bus_address(&self) -> u32 {
        bus::peripheral(map::PWM + FIF1_OFFSET)
    }
}
