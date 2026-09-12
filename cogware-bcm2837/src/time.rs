// SPDX-License-Identifier: GPL-3.0-only
//! Generic counter based timekeeping.
//!
//! The ARM generic timer is per-core, always running, and publishes its
//! frequency in `CNTFRQ_EL0` — 19.2 MHz on the Raspberry Pi 3, 62.5 MHz under
//! QEMU. Reading `CNTFRQ_EL0` rather than assuming a value keeps one binary
//! correct on both.
#![allow(dead_code)]

use core::time::Duration;

use crate::arch::cpu::{self, read_sysreg};

/// Counter frequency in Hz, from `CNTFRQ_EL0`.
///
/// `CNTHCTL_EL2.EL1PCTEN` is set by the boot code so EL1 may read this.
#[inline]
pub fn frequency_hz() -> u64 {
    read_sysreg!("CNTFRQ_EL0")
}

/// Current value of the physical counter, `CNTPCT_EL0`.
#[inline]
pub fn ticks() -> u64 {
    // `isb` stops the read from being speculated ahead of preceding work,
    // which matters when timing a code sequence.
    cpu::isb();
    read_sysreg!("CNTPCT_EL0")
}

/// Time since the counter started.
pub fn uptime() -> Duration {
    let frequency = frequency_hz();
    if frequency == 0 {
        return Duration::ZERO;
    }

    let ticks = ticks();
    Duration::new(
        ticks / frequency,
        ((ticks % frequency) * 1_000_000_000 / frequency) as u32,
    )
}

/// Busy-waits for at least `duration`.
///
/// `wfe` is not used: an unrelated `sev` from another core would cut the wait
/// short.
pub fn spin_for(duration: Duration) {
    let frequency = frequency_hz();
    if frequency == 0 {
        return;
    }

    let wait = duration.as_nanos().saturating_mul(u128::from(frequency)) / 1_000_000_000;
    let wait = u64::try_from(wait).unwrap_or(u64::MAX);

    let start = ticks();
    while ticks().wrapping_sub(start) < wait {
        core::hint::spin_loop();
    }
}

/// Busy-waits for `micros` microseconds.
pub fn spin_for_micros(micros: u64) {
    spin_for(Duration::from_micros(micros));
}
