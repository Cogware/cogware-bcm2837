// SPDX-License-Identifier: GPL-3.0-only
//! Power management: resetting the board from software.
//!
//! The BCM2837 has no reset line a kernel can pull. What it has is a watchdog
//! in the power manager: arm it with a short timeout, tell the reset
//! controller that a watchdog expiry means a full reset, and the SoC restarts
//! as if the power had been cycled. That is the whole of [`reboot`].
//!
//! Every write to this block carries a password in its top byte. A write
//! without it is discarded silently, which is the failure this module exists
//! to make impossible to write by accident.

use super::mmio::{ReadWrite, RegisterBlock};
use crate::bsp::memory::map;

/// The top byte every write to this block must carry, or the hardware drops
/// it without complaint.
const PASSWORD: u32 = 0x5a00_0000;

/// Keep only the bits of `RSTC` that are not the watchdog configuration.
const RSTC_WRCFG_CLR: u32 = 0xffff_ffcf;

/// The `RSTC` configuration that makes a watchdog expiry a full reset.
const RSTC_WRCFG_FULL_RESET: u32 = 0x0000_0020;

/// The `RSTS` bits that name the partition to boot. Cleared so the board comes
/// back on the one it normally uses rather than whatever a previous reset left.
const RSTS_PARTITION: u32 = 0xffff_faaa;

/// Watchdog ticks to wait before the reset fires.
///
/// The counter runs at about 65 kHz, so ten ticks is around 150 microseconds:
/// long enough for these three writes to land, short enough that nothing else
/// happens in between.
const TIMEOUT_TICKS: u32 = 10;

#[repr(C)]
struct Registers {
    /// 0x00..0x1c: the per-domain power registers, which this does not touch.
    _reserved: [ReadWrite<u32>; 7],
    rstc: ReadWrite<u32>, // 0x1c
    rsts: ReadWrite<u32>, // 0x20
    wdog: ReadWrite<u32>, // 0x24
}

const _: () = assert!(core::mem::offset_of!(Registers, rstc) == 0x1c);
const _: () = assert!(core::mem::offset_of!(Registers, wdog) == 0x24);

/// The BCM2837 power manager.
pub struct Power {
    block: RegisterBlock<Registers>,
}

impl Power {
    /// Creates the driver for the power manager at [`map::PM`].
    ///
    /// # Safety
    ///
    /// Only one instance may exist, and it must be placed behind a lock before
    /// secondary cores are released.
    pub const unsafe fn new() -> Self {
        Self {
            block: unsafe { RegisterBlock::new(map::PM) },
        }
    }

    /// Restart the board, and never return.
    ///
    /// The console is drained first, because the watchdog fires in well under
    /// a millisecond and a line takes several to clock out -- without it the
    /// last thing the board says, usually why it is restarting, is cut off
    /// mid-word. Nothing else is: the other cores are not stopped and no card
    /// write is finished, so a caller with work that must land has to finish
    /// it before calling.
    pub fn reboot(&mut self) -> ! {
        // `try_lock`, not `lock`: this is reachable from a panic on a core
        // that already holds the console, and a reset that deadlocked waiting
        // to say why would be worse than one that said nothing.
        if let Some(mut uart) = crate::drivers::UART.try_lock() {
            uart.flush();
        }

        let regs = self.block.regs();

        // Come back on the usual partition, whatever the last reset asked for.
        let rsts = regs.rsts.read() & !RSTS_PARTITION;
        regs.rsts.write(PASSWORD | rsts);

        regs.wdog.write(PASSWORD | TIMEOUT_TICKS);

        let rstc = regs.rstc.read() & RSTC_WRCFG_CLR;
        regs.rstc.write(PASSWORD | rstc | RSTC_WRCFG_FULL_RESET);

        // The watchdog fires in about 150 microseconds. Spinning rather than
        // returning means a caller cannot carry on into code that assumed the
        // reset already happened.
        loop {
            crate::cpu::spin_cycles(1_000);
        }
    }
}

/// Restart the board without taking [`crate::drivers::POWER`].
///
/// For the paths that reset from somewhere a lock cannot be taken -- a panic
/// handler, or a core that already holds it. Nothing survives the call, so
/// there is no state left for a race to corrupt.
pub fn reboot() -> ! {
    // SAFETY: the block is mapped, and every core that reaches here is about
    // to stop existing.
    unsafe { Power::new() }.reboot()
}
