// SPDX-License-Identifier: MIT OR Apache-2.0
//! Kernel console on top of the PL011 UART.
//!
//! # Locking strategy
//!
//! [`crate::sync::SpinLock`] needs `LDAXR`/`STLXR`, which are only defined on
//! Normal cacheable memory, i.e. once the MMU is on. The console picks its
//! access mode from `SCTLR_EL1.M`:
//!
//! * MMU off — bypass the lock and touch the UART directly;
//! * MMU on  — take the lock.
//!
//! This relies on a boot-order invariant: the only code that prints with the
//! MMU off is core 0, before [`crate::smp::start_secondaries`] releases anyone,
//! and secondary cores enable their MMU before their first `println!`. So "MMU
//! off" implies "single-threaded", and a lock holder never coexists with a lock
//! bypasser. A plain-load/store borrow guard also rejects reentrant early
//! console callbacks before they can create overlapping mutable references.

use core::{
    fmt::{self, Write},
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll},
};

use crate::{
    arch::mmu,
    bsp::memory,
    drivers::{
        GPIO, UART,
        pl011::{EmergencyWriter, Pl011Uart},
    },
    task,
};

static EARLY_BORROWED: AtomicBool = AtomicBool::new(false);

struct EarlyBorrow;

impl EarlyBorrow {
    fn new() -> Self {
        assert!(
            !EARLY_BORROWED.load(Ordering::Relaxed),
            "pre-MMU console access is not reentrant"
        );
        // Only core 0 uses the early console; RMW atomics need the MMU.
        EARLY_BORROWED.store(true, Ordering::Relaxed);
        Self
    }
}

impl Drop for EarlyBorrow {
    fn drop(&mut self) {
        EARLY_BORROWED.store(false, Ordering::Relaxed);
    }
}

/// Brings up GPIO 14/15 and the PL011.
///
/// # Safety
///
/// Must be called exactly once, by core 0, with the MMU still off and no other
/// core running.
pub unsafe fn init() {
    // SAFETY: single-threaded with the MMU off, so exclusives are unavailable
    // and the lock would be UNPREDICTABLE. See the module docs.
    let gpio = unsafe { GPIO.bypass() };
    gpio.route_pl011();

    with_writer(|uart| uart.init(memory::PL011_CLOCK_HZ, memory::CONSOLE_BAUD));
}

/// Whether the console may take its lock on this core.
fn locking_available() -> bool {
    mmu::is_enabled()
}

/// Backing function for [`crate::print!`] and [`crate::println!`].
#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    with_writer(|uart| {
        let _ = uart.write_fmt(args);
    });
}

/// Blocks until every queued byte has left the transmitter.
pub fn flush() {
    with_writer(Pl011Uart::flush);
}

/// Reads one byte from the console if any is waiting.
pub fn read_byte() -> Option<u8> {
    with_writer(Pl011Uart::read_byte)
}

/// Future returned by [`next_byte`].
#[derive(Debug)]
#[must_use = "futures do nothing unless awaited"]
pub struct NextByte {
    /// Zero-sized, but kept private so the type cannot be built by hand.
    _private: (),
}

impl Future for NextByte {
    type Output = u8;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<u8> {
        if let Some(byte) = read_byte() {
            return Poll::Ready(byte);
        }

        // The PL011 receive interrupt is not wired up, so this is a polled
        // driver: ask the executor for another look on the next tick.
        task::poll_again();
        Poll::Pending
    }
}

/// Waits for one byte from the console.
///
/// The core idles in `wfe` between polls, costing one FIFO read per executor
/// tick rather than a spin loop.
pub fn next_byte() -> NextByte {
    NextByte { _private: () }
}

/// Number of ordinary bytes written to and read from the console so far.
///
/// Emergency output deliberately does not touch these shared counters.
pub fn statistics() -> (u64, u64) {
    with_writer(|uart| (uart.chars_written(), uart.chars_read()))
}

/// Forcibly releases the console lock.
///
/// # Safety
///
/// The conditions of [`crate::sync::SpinLock::force_unlock`] must hold: no
/// outstanding guard may be used or dropped and no data reference may remain
/// live. Panic handlers should use [`with_emergency_writer`] instead.
pub unsafe fn force_unlock() {
    unsafe { UART.force_unlock() };
}

/// Runs `f` with the UART held for the whole closure, so a multi-line report
/// cannot be interleaved with another core's output.
///
/// This is ordinary, blocking console access. Exception and panic handlers
/// must use [`with_emergency_writer`] rather than waiting for interrupted code.
///
/// # Panics
///
/// If this core already holds or borrows the console, including through an
/// early-boot callback before the MMU is enabled.
pub fn with_writer<R>(f: impl FnOnce(&mut Pl011Uart) -> R) -> R {
    if locking_available() {
        return f(&mut UART.lock());
    }

    let _borrow = EarlyBorrow::new();
    // SAFETY: only core 0 uses the early console, and the borrow guard prevents
    // callbacks from creating a second reference through any ordinary API.
    f(unsafe { UART.bypass() })
}

/// Writes an exception or panic report without waiting for the console lock.
///
/// Takes the lock if it is immediately available; otherwise bytes may
/// interleave with ordinary output. The writer touches only external MMIO,
/// never the shared UART driver or its counters, even if a guard is outstanding.
///
/// Returns a formatting error if `f` fails or a bounded UART wait expires.
/// The transmitter is flushed before returning successfully.
pub fn with_emergency_writer(f: impl FnOnce(&mut dyn Write) -> fmt::Result) -> fmt::Result {
    let _guard = if locking_available() {
        UART.try_lock()
    } else {
        None
    };
    let mut writer = EmergencyWriter::new();
    let result = f(&mut writer);
    let flushed = writer.flush();
    result.and(flushed)
}
