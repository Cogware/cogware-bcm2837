// SPDX-License-Identifier: MIT OR Apache-2.0
//! BCM2837 GPIO controller.
//!
//! The single [`Gpio`] instance lives inside a [`crate::sync::SpinLock`] in
//! [`crate::drivers::GPIO`], and every core goes through that lock:
//! read-modify-write on `GPFSELn` (six pins share one 32-bit register)
//! corrupts silently when two cores race.
//!
//! The full pin API (all six alternate functions, both pull directions,
//! set/clear/level) is implemented even though boot only needs ALT0 and ALT4.
#![allow(dead_code)]

use super::mmio::{ReadOnly, ReadWrite, RegisterBlock, WriteOnly};
use crate::{bsp::memory::map, cpu};

/// Alternate-function encoding of a pin, as programmed into `GPFSELn`.
///
/// The numbering is not sequential; it comes from the BCM2835 ARM Peripherals
/// datasheet, section 6.1.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Function {
    /// Pin is a GPIO input.
    Input = 0b000,
    /// Pin is a GPIO output.
    Output = 0b001,
    /// Alternate function 0.
    Alt0 = 0b100,
    /// Alternate function 1.
    Alt1 = 0b101,
    /// Alternate function 2.
    Alt2 = 0b110,
    /// Alternate function 3.
    Alt3 = 0b111,
    /// Alternate function 4.
    Alt4 = 0b011,
    /// Alternate function 5.
    Alt5 = 0b010,
}

/// Internal pull resistor selection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Pull {
    /// Disable both pull-up and pull-down.
    None = 0b00,
    /// Enable the pull-down resistor.
    Down = 0b01,
    /// Enable the pull-up resistor.
    Up = 0b10,
}

/// Highest GPIO number on the BCM2837.
pub const MAX_PIN: u8 = 53;

#[repr(C)]
struct Registers {
    gpfsel: [ReadWrite<u32>; 6],   // 0x00
    _res0: u32,                    // 0x18
    gpset: [WriteOnly<u32>; 2],    // 0x1C
    _res1: u32,                    // 0x24
    gpclr: [WriteOnly<u32>; 2],    // 0x28
    _res2: u32,                    // 0x30
    gplev: [ReadOnly<u32>; 2],     // 0x34
    _res3: u32,                    // 0x3C
    gpeds: [ReadWrite<u32>; 2],    // 0x40
    _res4: u32,                    // 0x48
    gpren: [ReadWrite<u32>; 2],    // 0x4C
    _res5: u32,                    // 0x54
    gpfen: [ReadWrite<u32>; 2],    // 0x58
    _res6: u32,                    // 0x60
    gphen: [ReadWrite<u32>; 2],    // 0x64
    _res7: u32,                    // 0x6C
    gplen: [ReadWrite<u32>; 2],    // 0x70
    _res8: u32,                    // 0x78
    gparen: [ReadWrite<u32>; 2],   // 0x7C
    _res9: u32,                    // 0x84
    gpafen: [ReadWrite<u32>; 2],   // 0x88
    _res10: u32,                   // 0x90
    gppud: ReadWrite<u32>,         // 0x94
    gppudclk: [ReadWrite<u32>; 2], // 0x98
}

const _: () = assert!(core::mem::offset_of!(Registers, gppud) == 0x94);
const _: () = assert!(core::mem::offset_of!(Registers, gplev) == 0x34);

/// The BCM2837 GPIO block.
pub struct Gpio {
    block: RegisterBlock<Registers>,
}

impl Gpio {
    /// Creates the driver for the GPIO block at [`map::GPIO`].
    ///
    /// # Safety
    ///
    /// Only one instance may exist, and it must be placed behind a lock before
    /// secondary cores are released.
    pub const unsafe fn new() -> Self {
        Self {
            block: unsafe { RegisterBlock::new(map::GPIO) },
        }
    }

    /// Selects the function of a single pin.
    ///
    /// # Panics
    ///
    /// If `pin` exceeds [`MAX_PIN`].
    pub fn set_function(&mut self, pin: u8, function: Function) {
        assert!(pin <= MAX_PIN, "GPIO pin {pin} out of range");

        let register = (pin / 10) as usize;
        let shift = u32::from(pin % 10) * 3;

        self.block.regs().gpfsel[register]
            .modify(|value| (value & !(0b111 << shift)) | ((function as u32) << shift));
    }

    /// Selects the same function for a group of pins.
    pub fn set_functions(&mut self, pins: &[u8], function: Function) {
        for &pin in pins {
            self.set_function(pin, function);
        }
    }

    /// Reads back the function currently programmed for a pin.
    pub fn function(&self, pin: u8) -> u32 {
        assert!(pin <= MAX_PIN, "GPIO pin {pin} out of range");
        let register = (pin / 10) as usize;
        let shift = u32::from(pin % 10) * 3;
        (self.block.regs().gpfsel[register].read() >> shift) & 0b111
    }

    /// Applies a pull configuration to a set of pins.
    ///
    /// The BCM2837 uses the legacy two-step `GPPUD` + `GPPUDCLK` protocol,
    /// which requires a settling delay of at least 150 core clock cycles both
    /// after programming the control signal and after applying the clock.
    pub fn set_pull(&mut self, pins: &[u8], pull: Pull) {
        const SETTLE_CYCLES: usize = 150;

        let regs = self.block.regs();

        regs.gppud.write(pull as u32);
        cpu::spin_cycles(SETTLE_CYCLES);

        let mut clocks = [0u32; 2];
        for &pin in pins {
            assert!(pin <= MAX_PIN, "GPIO pin {pin} out of range");
            clocks[(pin / 32) as usize] |= 1 << (pin % 32);
        }

        regs.gppudclk[0].write(clocks[0]);
        regs.gppudclk[1].write(clocks[1]);
        cpu::spin_cycles(SETTLE_CYCLES);

        regs.gppud.write(0);
        regs.gppudclk[0].write(0);
        regs.gppudclk[1].write(0);
    }

    /// Drives an output pin high.
    pub fn set_high(&mut self, pin: u8) {
        assert!(pin <= MAX_PIN, "GPIO pin {pin} out of range");
        self.block.regs().gpset[(pin / 32) as usize].write(1 << (pin % 32));
    }

    /// Drives an output pin low.
    pub fn set_low(&mut self, pin: u8) {
        assert!(pin <= MAX_PIN, "GPIO pin {pin} out of range");
        self.block.regs().gpclr[(pin / 32) as usize].write(1 << (pin % 32));
    }

    /// Samples the current level of a pin.
    pub fn level(&self, pin: u8) -> bool {
        assert!(pin <= MAX_PIN, "GPIO pin {pin} out of range");
        (self.block.regs().gplev[(pin / 32) as usize].read() >> (pin % 32)) & 1 == 1
    }

    /// Enables or disables synchronous edge detection for a pin.
    ///
    /// A detected edge latches a sticky bit in `GPEDS`, readable with
    /// [`read_event`] and cleared with [`clear_event`]. Detection is
    /// level-independent: both directions may be armed at once, and disarming
    /// both leaves the pin generating no events.
    ///
    /// These are the *synchronous* detect registers, which run the pin through
    /// a two-stage synchroniser before sampling, rejecting glitches shorter
    /// than a couple of core clocks. The asynchronous registers
    /// (`GPAREN`/`GPAFEN`) catch shorter pulses and are not exposed here.
    ///
    /// # Panics
    ///
    /// If `pin` exceeds [`MAX_PIN`].
    pub fn set_edge_detect(&mut self, pin: u8, rising: bool, falling: bool) {
        assert!(pin <= MAX_PIN, "GPIO pin {pin} out of range");

        let register = (pin / 32) as usize;
        let mask = 1u32 << (pin % 32);
        let regs = self.block.regs();

        regs.gpren[register].modify(|value| if rising { value | mask } else { value & !mask });
        regs.gpfen[register].modify(|value| if falling { value | mask } else { value & !mask });

        // An edge may have latched while the enables were being written; clear
        // it so the caller starts from a clean slate.
        regs.gpeds[register].write(mask);
    }

    /// Routes GPIO 14/15 to the PL011 (`TXD0`/`RXD0` are ALT0) and disables
    /// their pulls, as the UART expects on a push-pull line.
    pub fn route_pl011(&mut self) {
        self.set_functions(&[14, 15], Function::Alt0);
        self.set_pull(&[14, 15], Pull::None);
    }
}

// ---------------------------------------------------------------------------
// Lock-free single-pin access
// ---------------------------------------------------------------------------
//
// Everything above needs [`crate::drivers::GPIO`] held, because `GPFSELn` and
// `GPPUD` are read-modify-write: six pins share one function-select register,
// so two cores reconfiguring different pins can lose each other's edits.
//
// `GPSETn` and `GPCLRn` are *write-1-to-act*: a write affects only the bits
// that are set, and writing zero to a bit does nothing. There is no read step,
// so two cores writing different bits both take effect in either order.
// `GPLEVn` is a pure read.
//
// These three therefore need no lock, which matters for bit-banged buses that
// toggle a pin once per clock edge: a global spin lock would serialise all
// four cores, and holding one mid-transfer risks being preempted while a line
// is held low.
//
// This second, unlocked path to the peripheral is sound only for these three
// registers. Do not extend it to anything that reads before it writes.

/// The GPIO register block, without going through the lock.
///
/// SAFETY: the caller must restrict itself to registers where a write is
/// write-1-to-act or the access is a pure read.
#[inline(always)]
fn regs_unlocked() -> &'static Registers {
    // SAFETY: `map::GPIO` is mapped Device-nGnRnE for the whole of boot and is
    // never unmapped, so the reference is valid for `'static`.
    unsafe { &*(map::GPIO as *const Registers) }
}

/// Drives `pin` high, without taking [`crate::drivers::GPIO`].
///
/// Safe and race-free because `GPSETn` is write-1-to-act: the write touches
/// only this pin's bit.
///
/// The pin must already have been set to [`Function::Output`], which *does*
/// need the lock. Calling this on an input pin has no effect.
///
/// # Panics
///
/// If `pin` exceeds [`MAX_PIN`].
#[inline]
pub fn write_high(pin: u8) {
    assert!(pin <= MAX_PIN, "GPIO pin {pin} out of range");
    regs_unlocked().gpset[(pin / 32) as usize].write(1 << (pin % 32));
}

/// Drives `pin` low, without taking [`crate::drivers::GPIO`].
///
/// The `GPCLRn` counterpart of [`write_high`]; the same reasoning applies.
///
/// # Panics
///
/// If `pin` exceeds [`MAX_PIN`].
#[inline]
pub fn write_low(pin: u8) {
    assert!(pin <= MAX_PIN, "GPIO pin {pin} out of range");
    regs_unlocked().gpclr[(pin / 32) as usize].write(1 << (pin % 32));
}

/// Samples the level of `pin`, without taking [`crate::drivers::GPIO`].
///
/// `GPLEVn` is read-only, so there is nothing to serialise. Reads the actual
/// pad: for an output pin that is normally the value being driven, for an
/// input pin the externally applied level.
///
/// # Panics
///
/// If `pin` exceeds [`MAX_PIN`].
#[inline]
pub fn read_level(pin: u8) -> bool {
    assert!(pin <= MAX_PIN, "GPIO pin {pin} out of range");
    (regs_unlocked().gplev[(pin / 32) as usize].read() >> (pin % 32)) & 1 == 1
}

/// Reports whether an edge has been detected on `pin` since the last
/// [`clear_event`], without taking [`crate::drivers::GPIO`].
///
/// Requires [`Gpio::set_edge_detect`] to have armed the pin first. The bit is
/// sticky: it stays set until explicitly cleared, so an edge cannot be missed
/// between polls.
///
/// # Panics
///
/// If `pin` exceeds [`MAX_PIN`].
#[inline]
pub fn read_event(pin: u8) -> bool {
    assert!(pin <= MAX_PIN, "GPIO pin {pin} out of range");
    (regs_unlocked().gpeds[(pin / 32) as usize].read() >> (pin % 32)) & 1 == 1
}

/// Clears the latched edge-detect bit for `pin`, without taking
/// [`crate::drivers::GPIO`].
///
/// `GPEDS` is write-1-to-clear, a form of write-1-to-act, so this needs no
/// lock for the same reason [`write_high`] does not.
///
/// # Panics
///
/// If `pin` exceeds [`MAX_PIN`].
#[inline]
pub fn clear_event(pin: u8) {
    assert!(pin <= MAX_PIN, "GPIO pin {pin} out of range");
    regs_unlocked().gpeds[(pin / 32) as usize].write(1 << (pin % 32));
}
