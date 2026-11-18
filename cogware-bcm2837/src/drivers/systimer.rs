// SPDX-License-Identifier: GPL-3.0-only
//! The system timer: a free-running 64-bit 1 MHz counter with four compare
//! channels.
//!
//! Unlike the architectural timer this one is a single SoC-wide counter, so
//! every core reads the same value, and its match outputs are interrupt
//! sources at the SoC interrupt controller. The VideoCore uses channels 0 and
//! 2; only [`Channel::One`] and [`Channel::Three`] are the ARM's.
//!
//! Nothing here takes a lock: the counter is read-only, the match flags are
//! write-1-to-clear, and each compare register belongs to whoever owns that
//! channel.
#![allow(dead_code)]

use super::mmio::{ReadOnly, ReadWrite};
use crate::bsp::memory::map;

/// Counter rate. Fixed by the hardware, independent of the core clock.
pub const HZ: u32 = 1_000_000;

#[repr(C)]
struct Registers {
    cs: ReadWrite<u32>,     // 0x00
    clo: ReadOnly<u32>,     // 0x04
    chi: ReadOnly<u32>,     // 0x08
    c: [ReadWrite<u32>; 4], // 0x0c..0x1c
}

const _: () = assert!(core::mem::offset_of!(Registers, c) == 0x0c);

/// A compare channel the ARM may use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub enum Channel {
    /// Channel 1, SoC interrupt 1.
    One = 1,
    /// Channel 3, SoC interrupt 3.
    Three = 3,
}

impl Channel {
    /// The SoC interrupt number its match raises.
    pub const fn irq(self) -> u32 {
        self as u32
    }
}

fn regs() -> &'static Registers {
    // SAFETY: `map::SYSTEM_TIMER` is mapped Device-nGnRnE for the whole of boot
    // and never unmapped, so the reference is valid for `'static`.
    unsafe { &*(map::SYSTEM_TIMER as *const Registers) }
}

/// Microseconds since the SoC came out of reset.
///
/// The two halves are separate registers, so the high word is read either
/// side of the low one and the read retried if the low word wrapped between.
pub fn micros() -> u64 {
    let regs = regs();
    loop {
        let hi = regs.chi.read();
        let lo = regs.clo.read();
        if regs.chi.read() == hi {
            return (u64::from(hi) << 32) | u64::from(lo);
        }
    }
}

/// Arms `channel` to match when the low 32 bits of the counter reach `at`,
/// and clears any match already pending on it.
///
/// Only the low word is compared, so a deadline more than about 71 minutes
/// away wraps.
pub fn set_compare(channel: Channel, at: u32) {
    let regs = regs();
    regs.c[channel as usize].write(at);
    regs.cs.write(1 << channel as u32);
}

/// Arms `channel` to match `micros` from now.
pub fn set_compare_in(channel: Channel, micros: u32) {
    set_compare(channel, regs().clo.read().wrapping_add(micros));
}

/// Whether `channel` has matched since it was last armed or cleared.
pub fn matched(channel: Channel) -> bool {
    regs().cs.read() & (1 << channel as u32) != 0
}

/// Acknowledges a match on `channel`, which also drops its interrupt line.
pub fn clear(channel: Channel) {
    regs().cs.write(1 << channel as u32);
}
