// SPDX-License-Identifier: GPL-3.0-only
//! The hardware random number generator.
//!
//! A ring-oscillator source feeding a small FIFO of 32-bit words. Its first
//! output after power-up is less random than the rest, so [`Rng::init`] has
//! the hardware discard a warm-up run before anything is read. Nothing about
//! its quality is documented, so treat it as a seed source, not as a vetted
//! cryptographic generator.
#![allow(dead_code)]

use super::mmio::{ReadOnly, ReadWrite, RegisterBlock};
use crate::{bsp::memory::map, cpu};

const CTRL_ENABLE: u32 = 1 << 0;

/// Words to discard after enabling, written to `STATUS`. Linux uses the same.
const WARMUP_COUNT: u32 = 0x4_0000;

const INT_MASK_OFF: u32 = 1 << 0;

/// Polls to make for a word before giving up. The FIFO refills in tens of
/// microseconds once warm; the warm-up itself takes much longer.
const POLL_LIMIT: u32 = 4_000_000;

#[repr(C)]
struct Registers {
    ctrl: ReadWrite<u32>,     // 0x00
    status: ReadWrite<u32>,   // 0x04
    data: ReadOnly<u32>,      // 0x08
    _ff_threshold: u32,       // 0x0c
    int_mask: ReadWrite<u32>, // 0x10
}

const _: () = assert!(core::mem::offset_of!(Registers, int_mask) == 0x10);

/// The generator produced nothing within the poll limit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Timeout;

impl core::fmt::Display for Timeout {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("random number generator timed out")
    }
}

/// The BCM2837 hardware RNG.
pub struct Rng {
    block: RegisterBlock<Registers>,
}

impl Rng {
    /// Creates the driver for the generator at [`map::RNG`].
    ///
    /// # Safety
    ///
    /// Only one instance may exist, and it must be placed behind a lock before
    /// secondary cores are released.
    pub const unsafe fn new() -> Self {
        Self {
            block: unsafe { RegisterBlock::new(map::RNG) },
        }
    }

    /// Starts the generator, with its interrupt masked. Harmless to repeat.
    pub fn init(&mut self) {
        let regs = self.block.regs();
        if regs.ctrl.read() & CTRL_ENABLE != 0 {
            return;
        }
        regs.status.write(WARMUP_COUNT);
        regs.int_mask.modify(|mask| mask | INT_MASK_OFF);
        regs.ctrl.modify(|ctrl| ctrl | CTRL_ENABLE);
    }

    /// The next random word, waiting for one if the FIFO is empty.
    pub fn next_u32(&mut self) -> Result<u32, Timeout> {
        let regs = self.block.regs();
        for _ in 0..POLL_LIMIT {
            if regs.status.read() >> 24 != 0 {
                return Ok(regs.data.read());
            }
            cpu::spin_cycles(10);
        }
        Err(Timeout)
    }

    /// Fills `buffer` with random bytes.
    pub fn fill(&mut self, buffer: &mut [u8]) -> Result<(), Timeout> {
        for chunk in buffer.chunks_mut(4) {
            let word = self.next_u32()?.to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
        Ok(())
    }
}
