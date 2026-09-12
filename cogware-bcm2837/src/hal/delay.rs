// SPDX-License-Identifier: GPL-3.0-only
//! Delays measured against the ARM generic timer.
//!
//! Bit-banging a bus means asserting things about wall-clock time — "hold SCL
//! low for at least 4.7 µs" — which an instruction-counting loop cannot
//! honour: the A53 is superscalar and out-of-order, and the core clock changes
//! when the firmware scales it.
//!
//! `CNTPCT_EL0` is a free-running counter at the frequency the hardware
//! publishes in `CNTFRQ_EL0` (19.2 MHz on a Pi 3, 62.5 MHz under QEMU); it
//! does not stop when the CPU clock changes and reading it costs a couple of
//! cycles. Everything here is built on [`crate::time`], which reads that
//! frequency rather than assuming it.
//!
//! The resolution floor is one tick — 52 ns on hardware — so a request for
//! 10 ns waits ~52 ns. Delays always round *up*, never undershooting a bus
//! timing minimum.

use core::time::Duration;

use crate::time;

/// A blocking delay source.
///
/// Zero-sized and freely copyable: there is one system counter and reading it
/// needs no state.
///
/// ```ignore
/// use cogware_bcm2837::hal::delay::Delay;
///
/// let mut delay = Delay;
/// delay.delay_micros(5);
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct Delay;

impl Delay {
    /// Creates a delay source.
    #[inline]
    pub const fn new() -> Self {
        Self
    }

    /// Busy-waits for at least `ns` nanoseconds.
    ///
    /// Rounds up to the next whole tick of the system counter, so short
    /// requests take one tick rather than none.
    #[inline]
    pub fn delay_nanos(&mut self, ns: u64) {
        let frequency = time::frequency_hz();
        if frequency == 0 {
            return;
        }

        // Round up: (ns * f + 1e9 - 1) / 1e9, in u128 so the product cannot
        // overflow.
        let ticks = (u128::from(ns) * u128::from(frequency)).div_ceil(1_000_000_000);
        spin_ticks(u64::try_from(ticks).unwrap_or(u64::MAX));
    }

    /// Busy-waits for at least `us` microseconds.
    #[inline]
    pub fn delay_micros(&mut self, us: u64) {
        self.delay_nanos(us.saturating_mul(1_000));
    }

    /// Busy-waits for at least `ms` milliseconds.
    #[inline]
    pub fn delay_millis(&mut self, ms: u64) {
        self.delay_nanos(ms.saturating_mul(1_000_000));
    }

    /// Busy-waits for at least `duration`.
    #[inline]
    pub fn delay(&mut self, duration: Duration) {
        time::spin_for(duration);
    }
}

/// Busy-waits for `ticks` ticks of the system counter.
///
/// Split out so a bit-bang loop that has precomputed its half-period in ticks
/// can skip the frequency division.
#[inline]
pub fn spin_ticks(ticks: u64) {
    if ticks == 0 {
        return;
    }

    let start = time::ticks();
    while time::ticks().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

/// Converts nanoseconds to system-counter ticks, rounding up.
///
/// Hoists the division out of a bit-bang loop: work out the half-period once
/// when the bus is configured, then call [`spin_ticks`] per edge.
#[inline]
pub fn nanos_to_ticks(ns: u64) -> u64 {
    let frequency = time::frequency_hz();
    if frequency == 0 {
        return 0;
    }

    let ticks = (u128::from(ns) * u128::from(frequency)).div_ceil(1_000_000_000);
    u64::try_from(ticks).unwrap_or(u64::MAX)
}
