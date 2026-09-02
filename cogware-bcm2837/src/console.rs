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
//! bypasser.

use core::{
    fmt::{self, Write},
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use crate::{
    arch::mmu,
    bsp::memory,
    drivers::{GPIO, UART, pl011::Pl011Uart},
    task,
};

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

    // SAFETY: as above.
    let uart = unsafe { UART.bypass() };
    uart.init(memory::PL011_CLOCK_HZ, memory::CONSOLE_BAUD);
}

/// Whether the console may take its lock on this core.
fn locking_available() -> bool {
    mmu::is_enabled()
}

/// Backing function for [`crate::print!`] and [`crate::println!`].
#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    if locking_available() {
        let _ = UART.lock().write_fmt(args);
    } else {
        // SAFETY: the MMU being off implies core 0 is the only core running.
        let uart = unsafe { UART.bypass() };
        let _ = uart.write_fmt(args);
    }
}

/// Blocks until every queued byte has left the transmitter.
pub fn flush() {
    if locking_available() {
        UART.lock().flush();
    } else {
        // SAFETY: as in `_print`.
        unsafe { UART.bypass() }.flush();
    }
}

/// Reads one byte from the console if any is waiting.
pub fn read_byte() -> Option<u8> {
    if locking_available() {
        UART.lock().read_byte()
    } else {
        // SAFETY: as in `_print`.
        unsafe { UART.bypass() }.read_byte()
    }
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

/// Number of bytes written to and read from the console so far.
pub fn statistics() -> (u64, u64) {
    if locking_available() {
        let uart = UART.lock();
        (uart.chars_written(), uart.chars_read())
    } else {
        // SAFETY: as in `_print`.
        let uart = unsafe { UART.bypass() };
        (uart.chars_written(), uart.chars_read())
    }
}

/// Forcibly releases the console lock.
///
/// # Safety
///
/// Panic path only. Recovers a console whose holder will never run again, at
/// the cost of possibly interleaving output.
pub unsafe fn force_unlock() {
    unsafe { UART.force_unlock() };
}

/// Runs `f` with the UART held for the whole closure, so a multi-line report
/// cannot be interleaved with another core's output.
///
/// Uses `try_lock`: the callers are the fault dump and the panic handler, which
/// may run on a core that already holds the lock from an enclosing `println!`,
/// where blocking would deadlock.
pub fn with_writer<R>(f: impl FnOnce(&mut Pl011Uart) -> R) -> R {
    if locking_available()
        && let Some(mut guard) = UART.try_lock()
    {
        return f(&mut guard);
    }

    // SAFETY: either the MMU is off, so core 0 is the only core running, or the
    // lock is contended and this is an error path that must not block.
    f(unsafe { UART.bypass() })
}
