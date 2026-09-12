// SPDX-License-Identifier: GPL-3.0-only
//! Async pins and buses, on top of [`crate::task`].
//!
//! The blocking API spins; this one yields. A task waiting for a pin to change
//! or for a bus transfer to finish hands the core back to the executor.
//!
//! Compiled only with the `embedded-hal-async` feature, which is on by
//! default.
//!
//! # Waiting for a pin
//!
//! [`WaitForEdge`] arms the GPIO edge detector, then polls the latch. Because
//! the edge is *latched* in `GPEDSn`, an edge that happens between two polls is
//! not lost: a 1 µs pulse is still caught by a task polled every 50 µs.
//!
//! There is no GPIO interrupt handler behind it. Each poll asks the executor
//! to poll again ([`crate::task::poll_again`]), so the core drops to `wfe`
//! between rounds rather than spinning at full tilt, and wakes on the timer
//! tick.
//!
//! **QEMU models neither `GPEDSn` nor the edge-enable registers.** Under
//! emulation the latch never sets and these futures never complete; on real
//! silicon they do. The `bitbang` example therefore tests protocol logic with
//! simulated pins rather than waiting on real edges.
//!
//! # Buses
//!
//! An async bus transfer is still driven by the CPU — there is no DMA here —
//! so the transfer itself is blocking. The async wrappers add a yield
//! *between* transactions. Shared buses hold their spin lock only during the
//! blocking transaction, never across an await. The operation list has exactly
//! the same chip-select and repeated-START semantics as the blocking adapter.
//!
//! A 16-byte I2C read at 100 kHz takes 1.4 ms; spinning through that on core 0
//! while core 1 holds the bus wastes 1.7 million cycles.

use core::{
    future::Future,
    pin::Pin as PinRef,
    task::{Context, Poll},
};

use embedded_hal::digital::ErrorType;
use embedded_hal_async::{delay, digital, i2c, spi};

use crate::{
    drivers::gpio::{clear_event, read_event, read_level},
    hal::{
        delay::Delay,
        gpio::{AnyPin, Output, Pin},
        shared::{self, ChipSelect},
    },
    task::{poll_again, sleep_micros, yield_now},
};

// ---------------------------------------------------------------------------
// Waiting on a pin
// ---------------------------------------------------------------------------

/// Which transition [`WaitForEdge`] is waiting for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    /// Low to high.
    Rising,
    /// High to low.
    Falling,
    /// Either.
    Any,
    /// Not an edge at all: complete as soon as the pin reads high.
    ///
    /// Completes immediately if it is already high, as `wait_for_high` is
    /// specified to do.
    High,
    /// Complete as soon as the pin reads low.
    Low,
}

impl Edge {
    /// Whether this condition needs the hardware edge detector armed.
    const fn is_edge(self) -> bool {
        matches!(self, Edge::Rising | Edge::Falling | Edge::Any)
    }

    /// `(rising, falling)` for [`crate::drivers::gpio::Gpio::set_edge_detect`].
    const fn detect(self) -> (bool, bool) {
        match self {
            Edge::Rising => (true, false),
            Edge::Falling => (false, true),
            Edge::Any => (true, true),
            Edge::High | Edge::Low => (false, false),
        }
    }
}

/// A future that completes when a pin does something.
///
/// Borrows the pin for as long as it is alive, so the pin cannot be
/// reconfigured — or waited on twice — while a wait is outstanding.
///
/// Arming happens on the first poll rather than at construction, so building
/// one and dropping it without awaiting has no effect on the hardware. Drop
/// disarms.
#[must_use = "futures do nothing unless awaited"]
pub struct WaitForEdge<'a, P: ?Sized> {
    pin: &'a mut P,
    number: u8,
    edge: Edge,
    armed: bool,
}

impl<'a, P: ?Sized> WaitForEdge<'a, P> {
    /// Waits on the pin numbered `number`.
    ///
    /// `number` must be the pin `pin` actually refers to; the typed
    /// constructors below guarantee that. `pub` so that a driver holding an
    /// erased pin can use it too.
    pub fn new(pin: &'a mut P, number: u8, edge: Edge) -> Self {
        Self {
            pin,
            number,
            edge,
            armed: false,
        }
    }

    /// The pin being waited on.
    pub fn pin(&self) -> &P {
        self.pin
    }
}

impl<P: ?Sized> Future for WaitForEdge<'_, P> {
    type Output = ();

    fn poll(self: PinRef<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();

        // Level conditions do not involve the latch at all.
        match this.edge {
            Edge::High => {
                return if read_level(this.number) {
                    Poll::Ready(())
                } else {
                    poll_again();
                    Poll::Pending
                };
            }
            Edge::Low => {
                return if read_level(this.number) {
                    poll_again();
                    Poll::Pending
                } else {
                    Poll::Ready(())
                };
            }
            _ => {}
        }

        if !this.armed {
            let (rising, falling) = this.edge.detect();
            let mut gpio = crate::drivers::GPIO.lock();
            gpio.set_edge_detect(this.number, rising, falling);
            drop(gpio);

            // `set_edge_detect` clears the latch, so anything seen from here
            // on happened after we started waiting.
            this.armed = true;
        }

        if read_event(this.number) {
            clear_event(this.number);
            return Poll::Ready(());
        }

        poll_again();
        Poll::Pending
    }
}

impl<P: ?Sized> Drop for WaitForEdge<'_, P> {
    fn drop(&mut self) {
        if self.armed && self.edge.is_edge() {
            // Leaving the detector armed would keep latching edges nobody is
            // waiting for, and the next waiter would see a stale one.
            crate::drivers::GPIO
                .lock()
                .set_edge_detect(self.number, false, false);
        }
    }
}

impl<const N: u8, MODE> Pin<N, MODE>
where
    Pin<N, MODE>: crate::hal::digital::InputPin,
{
    /// Waits for a transition on this pin.
    ///
    /// ```ignore
    /// button.wait_for(Edge::Falling).await;
    /// ```
    ///
    /// Does not complete under QEMU, which does not model the edge registers.
    pub fn wait_for(&mut self, edge: Edge) -> WaitForEdge<'_, Self> {
        WaitForEdge::new(self, N, edge)
    }
}

impl<MODE> AnyPin<MODE>
where
    AnyPin<MODE>: crate::hal::digital::InputPin,
{
    /// Waits for a transition on this pin.
    pub fn wait_for(&mut self, edge: Edge) -> WaitForEdge<'_, Self> {
        let number = self.number();
        WaitForEdge::new(self, number, edge)
    }
}

// -- the `embedded-hal-async` trait -----------------------------------------

impl<const N: u8, MODE> digital::Wait for Pin<N, MODE>
where
    Pin<N, MODE>: crate::hal::digital::InputPin + ErrorType,
{
    async fn wait_for_high(&mut self) -> Result<(), Self::Error> {
        WaitForEdge::new(self, N, Edge::High).await;
        Ok(())
    }

    async fn wait_for_low(&mut self) -> Result<(), Self::Error> {
        WaitForEdge::new(self, N, Edge::Low).await;
        Ok(())
    }

    async fn wait_for_rising_edge(&mut self) -> Result<(), Self::Error> {
        WaitForEdge::new(self, N, Edge::Rising).await;
        Ok(())
    }

    async fn wait_for_falling_edge(&mut self) -> Result<(), Self::Error> {
        WaitForEdge::new(self, N, Edge::Falling).await;
        Ok(())
    }

    async fn wait_for_any_edge(&mut self) -> Result<(), Self::Error> {
        WaitForEdge::new(self, N, Edge::Any).await;
        Ok(())
    }
}

/// Whether the running machine latches GPIO edges.
///
/// There is no way to ask the hardware, so this is a runtime probe: arm both
/// edges on a pin the caller owns, toggle it, and see whether `GPEDSn` caught
/// it. Real silicon says `true`; QEMU says `false`.
///
/// Use it to skip an edge-detect demo rather than hang in one.
pub fn edge_detection_works(pin: &mut crate::hal::gpio::Pin<{ PROBE_PIN }, Output>) -> bool {
    use crate::hal::digital::OutputPin as _;

    pin.set_low();
    {
        let mut gpio = crate::drivers::GPIO.lock();
        gpio.set_edge_detect(PROBE_PIN, true, true);
    }

    pin.set_high();
    let caught = read_event(PROBE_PIN);

    {
        let mut gpio = crate::drivers::GPIO.lock();
        gpio.set_edge_detect(PROBE_PIN, false, false);
    }

    caught
}

/// The pin [`edge_detection_works`] probes: GPIO 4, which the boot ROM leaves
/// alone and no peripheral here claims.
pub const PROBE_PIN: u8 = 4;

// ---------------------------------------------------------------------------
// Delay
// ---------------------------------------------------------------------------

impl delay::DelayNs for Delay {
    /// Sub-microsecond waits spin, because a round trip through the executor
    /// costs far more than the delay itself.
    async fn delay_ns(&mut self, ns: u32) {
        if ns < 1_000 {
            self.delay_nanos(u64::from(ns));
        } else {
            sleep_micros(u64::from(ns).div_ceil(1_000)).await;
        }
    }

    async fn delay_us(&mut self, us: u32) {
        sleep_micros(u64::from(us)).await;
    }

    async fn delay_ms(&mut self, ms: u32) {
        sleep_micros(u64::from(ms) * 1_000).await;
    }
}

// ---------------------------------------------------------------------------
// Buses
// ---------------------------------------------------------------------------

impl<B, CS> spi::SpiDevice<u8> for shared::SpiDevice<B, CS>
where
    B: shared::SpiBus + 'static,
    B::Error: embedded_hal::spi::Error,
    CS: ChipSelect,
{
    /// Yields once before taking the bus, then runs the whole transaction
    /// without yielding.
    ///
    /// The transaction cannot yield in the middle: chip select is asserted
    /// throughout, and a device left selected while another task drives the
    /// bus would see traffic meant for somebody else. The yield beforehand
    /// keeps a task issuing back-to-back transfers from starving its core.
    async fn transaction(
        &mut self,
        operations: &mut [spi::Operation<'_, u8>],
    ) -> Result<(), Self::Error> {
        yield_now().await;
        embedded_hal::spi::SpiDevice::transaction(self, operations)
    }
}

impl<B> i2c::I2c<i2c::SevenBitAddress> for shared::SharedI2c<B>
where
    B: shared::I2cTransaction + 'static,
    B::Error: embedded_hal::i2c::Error,
{
    /// Yields once, then runs the transaction with the bus locked.
    ///
    /// As with SPI the transaction itself is indivisible: the point of
    /// `write_read` is that nothing gets between the write and the read.
    async fn transaction(
        &mut self,
        address: u8,
        operations: &mut [i2c::Operation<'_>],
    ) -> Result<(), Self::Error> {
        yield_now().await;
        embedded_hal::i2c::I2c::transaction(self, address, operations)
    }
}
