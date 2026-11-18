// SPDX-License-Identifier: GPL-3.0-only
//! The clock manager (CPRMAN): the per-peripheral clock generators.
//!
//! Each generator picks a source, divides it by a 12.12 fixed-point divisor
//! and optionally smooths the fractional part with a MASH noise-shaper. The
//! general-purpose clocks drive pins directly (a camera's MCLK is usually one);
//! the rest feed PWM, PCM, the camera receivers and the DSI escape clocks.
//!
//! Like the power manager, every write carries a password in its top byte,
//! and a write without it is dropped silently.
#![allow(dead_code)]

use super::mmio::{ReadWrite, RegisterBlock};
use crate::{bsp::memory::map, cpu};

/// The top byte every write to this block must carry.
const PASSWORD: u32 = 0x5a00_0000;

const CTL_SRC_MASK: u32 = 0xf;
const CTL_ENAB: u32 = 1 << 4;
const CTL_KILL: u32 = 1 << 5;
const CTL_BUSY: u32 = 1 << 7;
const CTL_MASH_SHIFT: u32 = 9;

/// Polls to make for `BUSY` to clear. A generator stops within a couple of
/// source cycles, so this is generous even from the 19.2 MHz oscillator.
const POLL_LIMIT: u32 = 100_000;

/// Frequency of the crystal oscillator, source 1.
pub const OSCILLATOR_HZ: u32 = 19_200_000;

/// Frequency of PLLD's peripheral output, source 6, as the firmware leaves it.
///
/// The firmware configures PLLD before the kernel runs and never changes it;
/// QEMU emulates the PLL but has no firmware to set it up, so a generator fed
/// from it there runs at whatever its reset state gives.
pub const PLLD_HZ: u32 = 500_000_000;

/// PLLD's oscillator, which every PLLD channel divides. The firmware sets it
/// once and nothing may change it, since the peripheral clocks hang off it.
pub const PLLD_VCO_HZ: u32 = 2_000_000_000;

/// Source number of a DSI PHY's byte clock, for the DSI pixel clocks.
pub const SOURCE_DSI_BYTE: u32 = 8;

/// `CM_PLLD`, relative to the block: the hold and load bits of PLLD's
/// channels.
const CM_PLLD: usize = 0x10c;
const CM_PLLD_LOADDSI1: u32 = 1 << 2;
const CM_PLLD_HOLDDSI1: u32 = 1 << 3;

/// `A2W_PLLD_DSI1`, relative to the block: the PLLD DSI1 channel's divider.
/// The analogue (A2W) registers sit 4 KiB above the clock manager proper.
const A2W_PLLD_DSI1: usize = 0x1640;
const A2W_CHANNEL_DISABLE: u32 = 1 << 8;

/// The PLLD DSI1 channel divider nearest `hz`, 1 to 256.
const fn dsi1_pll_divider(hz: u32) -> u32 {
    let hz = if hz == 0 { 1 } else { hz };
    let divider = (PLLD_VCO_HZ + hz / 2) / hz;
    if divider < 1 {
        1
    } else if divider > 256 {
        256
    } else {
        divider
    }
}

// PLLD / 3, truncated to whole hertz, must come back as 3, not 4.
const _: () = assert!(dsi1_pll_divider(666_666_666) == 3);
const _: () = assert!(dsi1_pll_divider(1_000_000_000) == 2);
const _: () = assert!(dsi1_pll_divider(1) == 256);

/// A clock generator.
///
/// The discriminant is the offset of its control register; the divisor
/// register is the word after it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum ClockId {
    /// Camera receiver 0's low-power clock.
    Cam0 = 0x40,
    /// Camera receiver 1's low-power clock.
    Cam1 = 0x48,
    /// DSI0's escape clock.
    Dsi0Escape = 0x58,
    /// DSI0's pixel clock, fed to pixel valve 0. Selects a source rather than
    /// dividing one: see [`Clocks::select`].
    Dsi0Pixel = 0x60,
    /// DSI1's escape clock.
    Dsi1Escape = 0x158,
    /// DSI1's pixel clock, fed to pixel valve 1. Selects a source rather than
    /// dividing one: see [`Clocks::select`].
    Dsi1Pixel = 0x160,
    /// General-purpose clock 0, on GPIO 4 (ALT0) among others.
    Gp0 = 0x70,
    /// General-purpose clock 1, on GPIO 5 (ALT0). The firmware may own it for
    /// Ethernet on boards that have it; the CM3 does not.
    Gp1 = 0x78,
    /// General-purpose clock 2, on GPIO 6 (ALT0).
    Gp2 = 0x80,
    /// PCM/I2S bit clock source.
    Pcm = 0x98,
    /// PWM controller clock.
    Pwm = 0xa0,
}

impl ClockId {
    const fn pair(self) -> usize {
        self as usize / 8
    }

    /// The widest integer divisor this generator's `DIVI` field holds. The
    /// camera and DSI clocks have four integer bits; the rest twelve.
    const fn divi_max(self) -> u32 {
        match self {
            Self::Cam0 | Self::Cam1 | Self::Dsi0Escape | Self::Dsi1Escape => 0xf,
            Self::Dsi0Pixel | Self::Dsi1Pixel => 0,
            _ => 0xfff,
        }
    }
}

/// Where a generator takes its input from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Source {
    /// The 19.2 MHz crystal. Stable regardless of what the firmware does to
    /// the PLLs, so the right choice unless more speed is needed.
    Oscillator = 1,
    /// PLLD's peripheral output, 500 MHz.
    PllD = 6,
}

impl Source {
    /// The source's frequency, in hertz.
    pub const fn hz(self) -> u32 {
        match self {
            Self::Oscillator => OSCILLATOR_HZ,
            Self::PllD => PLLD_HZ,
        }
    }
}

/// How the fractional part of the divisor is applied.
///
/// Higher stages spread the jitter wider in exchange for a closer average.
/// Each stage needs a larger minimum integer divisor: 1, 2, 3 and 5.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Mash {
    /// Integer division only; the fraction is discarded. Lowest jitter.
    Integer = 0,
    /// First-order noise shaping.
    Stage1 = 1,
    /// Second-order.
    Stage2 = 2,
    /// Third-order.
    Stage3 = 3,
}

impl Mash {
    const fn min_divi(self) -> u32 {
        match self {
            Self::Integer => 1,
            Self::Stage1 => 2,
            Self::Stage2 => 3,
            Self::Stage3 => 5,
        }
    }
}

/// Why a clock could not be set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The generator stayed busy past the poll limit after being disabled.
    Busy,
    /// The requested rate needs a divisor outside what the hardware and the
    /// chosen [`Mash`] allow.
    OutOfRange,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Busy => "clock generator stayed busy",
            Self::OutOfRange => "rate out of range for this source",
        })
    }
}

#[repr(C)]
struct Pair {
    ctl: ReadWrite<u32>,
    div: ReadWrite<u32>,
}

#[repr(C)]
struct Registers {
    pairs: [Pair; 64], // 0x000..0x200, one control/divisor pair every 8 bytes
}

const _: () = assert!(core::mem::size_of::<Registers>() == 0x200);

/// Computes the 12.12 divisor for `hz` from `source`.
///
/// Returns the `DIV` register value and the rate it actually produces.
const fn divisor(source: u32, hz: u32, mash: Mash, divi_max: u32) -> Result<(u32, u32), Error> {
    if hz == 0 {
        return Err(Error::OutOfRange);
    }
    // 12.12 fixed point, rounded to nearest. u64 because source << 12 does not
    // fit in 32 bits for PLLD.
    let mut div = ((source as u64) << 12).saturating_add(hz as u64 / 2) / hz as u64;
    if matches!(mash, Mash::Integer) {
        div = (div + 0x800) & !0xfff;
    }
    let divi = (div >> 12) as u32;
    if divi < mash.min_divi() || divi > divi_max {
        return Err(Error::OutOfRange);
    }
    let actual = (((source as u64) << 12) / div) as u32;
    Ok(((div as u32) & 0x00ff_ffff, actual))
}

// The arithmetic, checked at compile time: 19.2 MHz / 4 is exact, an integer
// divisor rounds to nearest, and an unreachable rate is refused.
const _: () = {
    assert!(matches!(
        divisor(OSCILLATOR_HZ, 4_800_000, Mash::Integer, 0xfff),
        Ok((0x4000, 4_800_000))
    ));
    assert!(matches!(
        divisor(OSCILLATOR_HZ, 5_000_000, Mash::Integer, 0xfff),
        Ok((0x4000, 4_800_000))
    ));
    assert!(matches!(
        divisor(PLLD_HZ, 24_000_000, Mash::Stage1, 0xfff),
        Ok((0x14d55, _))
    ));
    assert!(divisor(OSCILLATOR_HZ, 19_200_000, Mash::Stage1, 0xfff).is_err());
    assert!(divisor(OSCILLATOR_HZ, 1_000, Mash::Integer, 0xfff).is_err());
    assert!(divisor(OSCILLATOR_HZ, 0, Mash::Integer, 0xfff).is_err());
    // The four-bit generators: 500 / 100 fits, 500 / 25 does not.
    assert!(matches!(
        divisor(PLLD_HZ, 100_000_000, Mash::Integer, 0xf),
        Ok((0x5000, 100_000_000))
    ));
    assert!(divisor(PLLD_HZ, 25_000_000, Mash::Integer, 0xf).is_err());
};

/// The BCM2837 clock manager.
pub struct Clocks {
    block: RegisterBlock<Registers>,
}

impl Clocks {
    /// Creates the driver for the clock manager at [`map::CM`].
    ///
    /// # Safety
    ///
    /// Only one instance may exist, and it must be placed behind a lock before
    /// secondary cores are released.
    pub const unsafe fn new() -> Self {
        Self {
            block: unsafe { RegisterBlock::new(map::CM) },
        }
    }

    /// Runs `clock` from `source` as close to `hz` as the divisor allows, and
    /// returns the rate it actually runs at.
    ///
    /// The generator is stopped and waited for before the divisor changes,
    /// because changing it while running can glitch the output.
    pub fn enable(
        &mut self,
        clock: ClockId,
        source: Source,
        hz: u32,
        mash: Mash,
    ) -> Result<u32, Error> {
        let (div, actual) = divisor(source.hz(), hz, mash, clock.divi_max())?;
        self.disable(clock)?;

        let pair = &self.block.regs().pairs[clock.pair()];
        pair.div.write(PASSWORD | div);
        let ctl = (mash as u32) << CTL_MASH_SHIFT | source as u32;
        pair.ctl.write(PASSWORD | ctl);
        pair.ctl.write(PASSWORD | ctl | CTL_ENAB);
        Ok(actual)
    }

    /// Stops `clock` and waits until it has.
    pub fn disable(&mut self, clock: ClockId) -> Result<(), Error> {
        let pair = &self.block.regs().pairs[clock.pair()];
        let ctl = pair.ctl.read() & !(CTL_ENAB | CTL_KILL | CTL_BUSY) & 0x00ff_ffff;
        pair.ctl.write(PASSWORD | ctl);
        for _ in 0..POLL_LIMIT {
            if pair.ctl.read() & CTL_BUSY == 0 {
                return Ok(());
            }
            cpu::spin_cycles(10);
        }
        // Kill resets the generator outright, at the cost of a possible glitch
        // on its output. Better than leaving it running at the old rate.
        pair.ctl.write(PASSWORD | ctl | CTL_KILL);
        pair.ctl.write(PASSWORD | ctl);
        if pair.ctl.read() & CTL_BUSY == 0 {
            Ok(())
        } else {
            Err(Error::Busy)
        }
    }

    /// Runs `clock` straight from source number `source`, undivided. For the
    /// DSI pixel clocks, which take [`SOURCE_DSI_BYTE`] and have no divider.
    pub fn select(&mut self, clock: ClockId, source: u32) -> Result<(), Error> {
        self.disable(clock)?;
        let pair = &self.block.regs().pairs[clock.pair()];
        pair.ctl.write(PASSWORD | (source & CTL_SRC_MASK));
        pair.ctl
            .write(PASSWORD | (source & CTL_SRC_MASK) | CTL_ENAB);
        Ok(())
    }

    /// Divides PLLD down for DSI1's high-speed bit clock, to the rate nearest
    /// `hz`, and returns that rate.
    ///
    /// The divider is an integer, so the rate is [`PLLD_VCO_HZ`] over a whole
    /// number: a display pipeline picks its pixel clock to suit. Nearest
    /// rather than not-above, so a rate already computed as PLLD over a
    /// divider, and truncated to whole hertz, gets that divider back.
    pub fn enable_dsi1_pll(&mut self, hz: u32) -> u32 {
        let divider = dsi1_pll_divider(hz);
        // A divider of 256 is written as zero; the field is eight bits.
        self.write_raw(A2W_PLLD_DSI1, divider & 0xff);
        let cm = self.read_raw(CM_PLLD);
        self.write_raw(CM_PLLD, cm | CM_PLLD_LOADDSI1);
        self.write_raw(CM_PLLD, cm & !CM_PLLD_LOADDSI1);
        let a2w = self.read_raw(A2W_PLLD_DSI1);
        self.write_raw(A2W_PLLD_DSI1, a2w & !A2W_CHANNEL_DISABLE);
        let cm = self.read_raw(CM_PLLD);
        self.write_raw(CM_PLLD, cm & !CM_PLLD_HOLDDSI1);
        PLLD_VCO_HZ / divider
    }

    /// Holds and disables the PLLD DSI1 channel.
    pub fn disable_dsi1_pll(&mut self) {
        let cm = self.read_raw(CM_PLLD);
        self.write_raw(CM_PLLD, (cm & !CM_PLLD_LOADDSI1) | CM_PLLD_HOLDDSI1);
        let a2w = self.read_raw(A2W_PLLD_DSI1);
        self.write_raw(A2W_PLLD_DSI1, a2w | A2W_CHANNEL_DISABLE);
    }

    /// Reads a PLL register outside the generator pairs.
    fn read_raw(&self, offset: usize) -> u32 {
        // SAFETY: both offsets used are registers of this block, which this
        // driver owns; the A2W window is inside the mapped peripheral range.
        unsafe { RegisterBlock::<()>::read_raw(map::CM, offset) }
    }

    /// Writes a PLL register outside the generator pairs, with the password.
    fn write_raw(&mut self, offset: usize, value: u32) {
        // SAFETY: as in `read_raw`.
        unsafe { RegisterBlock::<()>::write_raw(map::CM, offset, PASSWORD | (value & 0x00ff_ffff)) }
    }

    /// Whether `clock` is enabled and running.
    pub fn is_running(&self, clock: ClockId) -> bool {
        let ctl = self.block.regs().pairs[clock.pair()].ctl.read();
        ctl & CTL_ENAB != 0 && ctl & CTL_BUSY != 0
    }

    /// The rate `clock` is programmed for, if its source is one this driver
    /// knows the frequency of.
    pub fn rate(&self, clock: ClockId) -> Option<u32> {
        let pair = &self.block.regs().pairs[clock.pair()];
        let ctl = pair.ctl.read();
        let source = match ctl & CTL_SRC_MASK {
            1 => OSCILLATOR_HZ,
            6 => PLLD_HZ,
            _ => return None,
        };
        let mut div = (pair.div.read() & 0x00ff_ffff) as u64;
        if (ctl >> CTL_MASH_SHIFT) & 3 == 0 {
            div &= !0xfff;
        }
        if div < 0x1000 {
            return None;
        }
        Some((((source as u64) << 12) / div) as u32)
    }
}
