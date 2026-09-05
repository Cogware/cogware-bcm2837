// SPDX-License-Identifier: MIT OR Apache-2.0
//! One bus, many owners.
//!
//! A bus is a single physical resource wanted by four cores and any number of
//! tasks. It goes in a [`SpinLock`] and each user gets a *handle* that locks it
//! for the duration of one transaction and lets go again.
//!
//! A transaction is the unit that must not be interleaved: an I2C register read
//! is a write of the register number and a read of its contents with no STOP
//! between them, and splitting it across two lock acquisitions lets another
//! core address a different device in the middle and get back somebody else's
//! data.
//!
//! ```ignore
//! use cogware_bcm2837::{drivers::I2C1, hal::shared::I2cDevice};
//!
//! // Every core can build one of these; they all point at the same bus.
//! let mut sensor = I2cDevice::new(&I2C1, 0x68);
//! let mut seconds = [0u8; 1];
//! sensor.write_read(&[0x00], &mut seconds)?;
//! ```
//!
//! # Two traits
//!
//! [`SpiBus`] and [`I2cBus`] exist so the sharing wrappers do not care whether
//! the bus underneath is the hardware peripheral or a bit-banged one. Both are
//! implemented for [`crate::drivers::spi::Spi0`], [`crate::drivers::i2c::I2c`],
//! [`crate::hal::bitbang::Spi`] and [`crate::hal::bitbang::I2c`], so a driver
//! written against a shared handle runs on any of them.
//!
//! # Lock ordering
//!
//! A bus lock may be taken while holding nothing else, and `init` on either
//! hardware bus takes the `GPIO` lock underneath it. So the order is
//! **bus → GPIO**, and code holding the GPIO lock must not then reach for a
//! bus. Nothing in the crate does.
//!
//! `println!` takes the `UART` lock: printing while holding a bus lock is
//! fine, but a UART driver that talked over I2C would not be.

use core::convert::Infallible;

use crate::{hal::digital::OutputPin, sync::SpinLock};

// ---------------------------------------------------------------------------
// Bus traits
// ---------------------------------------------------------------------------

/// The operations every SPI bus offers, hardware or bit-banged.
///
/// SPI is full duplex: a byte goes out for every byte that comes in.
/// [`SpiBus::write`] and [`SpiBus::read`] are the cases where one direction is
/// ignored, not cases where a direction is idle.
pub trait SpiBus {
    /// What can go wrong. [`Infallible`] for the bit-banged bus, which has no
    /// FIFO to overrun and no peripheral to be misconfigured.
    type Error;

    /// Clocks `write` out and `read` in at the same time.
    fn transfer(&mut self, read: &mut [u8], write: &[u8]) -> Result<(), Self::Error>;

    /// Clocks `data` out, discarding what comes back.
    fn write(&mut self, data: &[u8]) -> Result<(), Self::Error>;

    /// Clocks zeros out and `data` in.
    fn read(&mut self, data: &mut [u8]) -> Result<(), Self::Error>;

    /// Clocks `data` out and overwrites it with what came back.
    fn transfer_in_place(&mut self, data: &mut [u8]) -> Result<(), Self::Error>;

    /// Runs a complete transaction without releasing hardware chip select.
    ///
    /// Software buses need no extra state. Hardware buses override this to
    /// keep their controller active across transfers and delays in `body`.
    fn transaction<R>(&mut self, body: impl FnOnce(&mut Self) -> R) -> R
    where
        Self: Sized,
    {
        body(self)
    }
}

/// One buffer in an I2C transaction.
pub enum I2cOperation<'a> {
    /// Read bytes, acknowledging all but the last byte of this read phase.
    Read(&'a mut [u8]),
    /// Write bytes without a new address between adjacent write buffers.
    Write(&'a [u8]),
}

impl I2cOperation<'_> {
    pub(crate) fn is_read(&self) -> bool {
        matches!(self, Self::Read(_))
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Self::Read(bytes) => bytes.len(),
            Self::Write(bytes) => bytes.len(),
        }
    }
}

/// A reborrowable operation list, without allocation or a buffer-count limit.
///
/// Adapters translate their operation type one buffer at a time. The list's
/// length, directions and buffer lengths must remain unchanged during a call.
pub trait I2cOperations {
    /// Number of operations.
    fn len(&self) -> usize;

    /// Whether there are no operations.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reborrows the operation at `index`, which must be less than `len()`.
    fn operation(&mut self, index: usize) -> I2cOperation<'_>;
}

impl I2cOperations for [I2cOperation<'_>] {
    fn len(&self) -> usize {
        <[I2cOperation<'_>]>::len(self)
    }

    fn operation(&mut self, index: usize) -> I2cOperation<'_> {
        match &mut self[index] {
            I2cOperation::Read(bytes) => I2cOperation::Read(bytes),
            I2cOperation::Write(bytes) => I2cOperation::Write(bytes),
        }
    }
}

impl<const N: usize> I2cOperations for [I2cOperation<'_>; N] {
    fn len(&self) -> usize {
        N
    }

    fn operation(&mut self, index: usize) -> I2cOperation<'_> {
        self.as_mut_slice().operation(index)
    }
}

pub(crate) struct I2cPhase {
    pub end: usize,
    pub read: bool,
    pub length: Option<usize>,
    pub last_nonempty: usize,
}

pub(crate) fn i2c_phase(operations: &mut (impl I2cOperations + ?Sized), start: usize) -> I2cPhase {
    let read = operations.operation(start).is_read();
    let mut phase = I2cPhase {
        end: start,
        read,
        length: Some(0),
        last_nonempty: start,
    };
    while phase.end < operations.len() {
        let operation = operations.operation(phase.end);
        if operation.is_read() != read {
            break;
        }
        let length = operation.len();
        if length != 0 {
            phase.last_nonempty = phase.end;
        }
        phase.length = phase.length.and_then(|total| total.checked_add(length));
        phase.end += 1;
    }
    phase
}

/// The operations every I2C bus offers, hardware or bit-banged.
///
/// Addresses are seven-bit and given unshifted: pass `0x68`, not `0xD0`.
pub trait I2cBus {
    /// What can go wrong. Both implementations can at least be NACKed.
    type Error;

    /// Writes `data` to `address`.
    fn write(&mut self, address: u8, data: &[u8]) -> Result<(), Self::Error>;

    /// Reads `data.len()` bytes from `address`.
    fn read(&mut self, address: u8, data: &mut [u8]) -> Result<(), Self::Error>;

    /// Writes `write` then reads `read`, without releasing the bus between.
    fn write_read(&mut self, address: u8, write: &[u8], read: &mut [u8])
    -> Result<(), Self::Error>;
}

/// An I2C bus that preserves the complete operation-list transaction contract.
///
/// Adjacent buffers of the same direction are contiguous; direction changes
/// use repeated START, and only the final phase ends in STOP. A controller
/// unable to implement a requested sequence must return an error, not split it.
///
/// Separate from [`I2cBus`] so existing implementations of the simpler bus
/// trait keep compiling without silently claiming transaction support.
pub trait I2cTransaction: I2cBus {
    /// Executes `operations` as one bus transaction.
    fn transaction(
        &mut self,
        address: u8,
        operations: &mut (impl I2cOperations + ?Sized),
    ) -> Result<(), Self::Error>;
}

// -- hardware ---------------------------------------------------------------

impl SpiBus for crate::drivers::spi::Spi0 {
    type Error = crate::drivers::spi::Error;

    #[inline]
    fn transfer(&mut self, read: &mut [u8], write: &[u8]) -> Result<(), Self::Error> {
        Self::transfer(self, read, write)
    }

    #[inline]
    fn write(&mut self, data: &[u8]) -> Result<(), Self::Error> {
        Self::write(self, data)
    }

    #[inline]
    fn read(&mut self, data: &mut [u8]) -> Result<(), Self::Error> {
        Self::read(self, data)
    }

    #[inline]
    fn transfer_in_place(&mut self, data: &mut [u8]) -> Result<(), Self::Error> {
        Self::transfer_in_place(self, data)
    }

    fn transaction<R>(&mut self, body: impl FnOnce(&mut Self) -> R) -> R {
        Self::transaction(self, body)
    }
}

impl I2cTransaction for crate::drivers::i2c::I2c {
    fn transaction(
        &mut self,
        address: u8,
        operations: &mut (impl I2cOperations + ?Sized),
    ) -> Result<(), Self::Error> {
        Self::transaction(self, address, operations)
    }
}

impl I2cBus for crate::drivers::i2c::I2c {
    type Error = crate::drivers::i2c::Error;

    #[inline]
    fn write(&mut self, address: u8, data: &[u8]) -> Result<(), Self::Error> {
        Self::write(self, address, data)
    }

    #[inline]
    fn read(&mut self, address: u8, data: &mut [u8]) -> Result<(), Self::Error> {
        Self::read(self, address, data)
    }

    #[inline]
    fn write_read(
        &mut self,
        address: u8,
        write: &[u8],
        read: &mut [u8],
    ) -> Result<(), Self::Error> {
        Self::write_read(self, address, write, read)
    }
}

// -- bit-banged -------------------------------------------------------------

impl<SCK, MOSI, MISO> SpiBus for crate::hal::bitbang::Spi<SCK, MOSI, MISO>
where
    SCK: OutputPin,
    MOSI: OutputPin,
    MISO: crate::hal::digital::InputPin,
{
    type Error = Infallible;

    #[inline]
    fn transfer(&mut self, read: &mut [u8], write: &[u8]) -> Result<(), Self::Error> {
        Self::transfer(self, read, write);
        Ok(())
    }

    #[inline]
    fn write(&mut self, data: &[u8]) -> Result<(), Self::Error> {
        Self::write(self, data);
        Ok(())
    }

    #[inline]
    fn read(&mut self, data: &mut [u8]) -> Result<(), Self::Error> {
        Self::read(self, data);
        Ok(())
    }

    #[inline]
    fn transfer_in_place(&mut self, data: &mut [u8]) -> Result<(), Self::Error> {
        Self::transfer_in_place(self, data);
        Ok(())
    }
}

impl<SDA, SCL> I2cTransaction for crate::hal::bitbang::I2c<SDA, SCL>
where
    SDA: crate::hal::digital::OpenDrainPin,
    SCL: crate::hal::digital::OpenDrainPin,
{
    fn transaction(
        &mut self,
        address: u8,
        operations: &mut (impl I2cOperations + ?Sized),
    ) -> Result<(), Self::Error> {
        Self::transaction(self, address, operations)
    }
}

impl<SDA, SCL> I2cBus for crate::hal::bitbang::I2c<SDA, SCL>
where
    SDA: crate::hal::digital::OpenDrainPin,
    SCL: crate::hal::digital::OpenDrainPin,
{
    type Error = crate::hal::bitbang::Error;

    #[inline]
    fn write(&mut self, address: u8, data: &[u8]) -> Result<(), Self::Error> {
        Self::write(self, address, data)
    }

    #[inline]
    fn read(&mut self, address: u8, data: &mut [u8]) -> Result<(), Self::Error> {
        Self::read(self, address, data)
    }

    #[inline]
    fn write_read(
        &mut self,
        address: u8,
        write: &[u8],
        read: &mut [u8],
    ) -> Result<(), Self::Error> {
        Self::write_read(self, address, write, read)
    }
}

// ---------------------------------------------------------------------------
// Shared handles
// ---------------------------------------------------------------------------

/// A handle to an I2C bus somebody else owns.
///
/// Copyable and `'static`, so every core can hold one and they all refer to
/// the same bus. Each call takes the lock, runs one transaction, and releases
/// it.
///
/// With the `embedded-hal` feature it implements `embedded_hal::i2c::I2c`.
pub struct SharedI2c<B: I2cBus + 'static> {
    bus: &'static SpinLock<B>,
}

impl<B: I2cBus + 'static> SharedI2c<B> {
    /// Wraps a locked bus.
    pub const fn new(bus: &'static SpinLock<B>) -> Self {
        Self { bus }
    }

    /// Runs several operations under one lock acquisition.
    ///
    /// A single [`I2cBus::write_read`] is already atomic; reach for this when a
    /// device needs a longer sequence that must not be interrupted — a write, a
    /// delay, and a read of the result, say.
    ///
    /// Nothing else can use the bus until the closure returns, so keep it
    /// short.
    pub fn transaction<R>(&self, body: impl FnOnce(&mut B) -> R) -> R {
        self.bus.with(body)
    }
}

impl<B: I2cTransaction + 'static> I2cTransaction for SharedI2c<B> {
    fn transaction(
        &mut self,
        address: u8,
        operations: &mut (impl I2cOperations + ?Sized),
    ) -> Result<(), Self::Error> {
        self.bus
            .with(|bus| I2cTransaction::transaction(bus, address, operations))
    }
}

impl<B: I2cBus + 'static> I2cBus for SharedI2c<B> {
    type Error = B::Error;

    #[inline]
    fn write(&mut self, address: u8, data: &[u8]) -> Result<(), Self::Error> {
        self.bus.with(|bus| bus.write(address, data))
    }

    #[inline]
    fn read(&mut self, address: u8, data: &mut [u8]) -> Result<(), Self::Error> {
        self.bus.with(|bus| bus.read(address, data))
    }

    #[inline]
    fn write_read(
        &mut self,
        address: u8,
        write: &[u8],
        read: &mut [u8],
    ) -> Result<(), Self::Error> {
        self.bus.with(|bus| bus.write_read(address, write, read))
    }
}

impl<B: I2cBus + 'static> Clone for SharedI2c<B> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<B: I2cBus + 'static> Copy for SharedI2c<B> {}

/// A handle to one device on a shared I2C bus.
///
/// Unlike [`SharedI2c`] the address is baked in, so the caller cannot
/// accidentally address the wrong chip. Several of these may point at the same
/// bus with different addresses.
pub struct I2cDevice<B: I2cBus + 'static> {
    bus: &'static SpinLock<B>,
    address: u8,
}

impl<B: I2cBus + 'static> I2cDevice<B> {
    /// Binds a seven-bit address on a locked bus.
    pub const fn new(bus: &'static SpinLock<B>, address: u8) -> Self {
        Self { bus, address }
    }

    /// The address this handle talks to.
    pub const fn address(&self) -> u8 {
        self.address
    }

    /// Writes to the device.
    pub fn write(&mut self, data: &[u8]) -> Result<(), B::Error> {
        self.bus.with(|bus| bus.write(self.address, data))
    }

    /// Reads from the device.
    pub fn read(&mut self, data: &mut [u8]) -> Result<(), B::Error> {
        self.bus.with(|bus| bus.read(self.address, data))
    }

    /// Writes then reads, atomically — the usual "read register N" shape.
    pub fn write_read(&mut self, write: &[u8], read: &mut [u8]) -> Result<(), B::Error> {
        self.bus
            .with(|bus| bus.write_read(self.address, write, read))
    }

    /// Runs several operations under one lock acquisition.
    pub fn transaction<R>(&self, body: impl FnOnce(&mut B) -> R) -> R {
        self.bus.with(body)
    }
}

impl<B: I2cBus + 'static> Clone for I2cDevice<B> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<B: I2cBus + 'static> Copy for I2cDevice<B> {}

// ---------------------------------------------------------------------------
// SPI devices
// ---------------------------------------------------------------------------

/// What can go wrong talking to one device on a shared SPI bus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpiDeviceError<E> {
    /// The bus itself failed.
    Bus(E),
}

/// Asserts and releases a device's chip select.
///
/// SPI has no addressing: a device listens because its select line is low.
/// Which line that is depends on the board, and on this chip there are two
/// ways to drive it — the peripheral's own `CE0`/`CE1`, or any GPIO. This
/// trait covers both, plus the case where there is nothing to drive.
pub trait ChipSelect {
    /// Selects the device. Called before the first byte.
    fn select(&mut self);

    /// Deselects it. Called after the last byte, including on the error path.
    fn deselect(&mut self);
}

/// A chip select driven by an ordinary GPIO pin.
///
/// Active low, which is what essentially every SPI device uses.
pub struct GpioCs<P: OutputPin>(pub P);

impl<P: OutputPin> ChipSelect for GpioCs<P> {
    #[inline]
    fn select(&mut self) {
        self.0.set_low();
    }

    #[inline]
    fn deselect(&mut self) {
        self.0.set_high();
    }
}

/// No chip select at all.
///
/// For a bus with exactly one device whose select is tied low, or for the
/// hardware peripheral driving `CE0`/`CE1` itself — the controller asserts the
/// line around the whole [`SpiBus::transaction`] and nothing needs to happen
/// on a separate GPIO.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoCs;

impl ChipSelect for NoCs {
    #[inline]
    fn select(&mut self) {}

    #[inline]
    fn deselect(&mut self) {}
}

/// A handle to one device on a shared SPI bus.
///
/// Locks the bus, asserts chip select, runs the transfer, releases both. The
/// select is released even if the transfer fails: a device left selected would
/// interpret the *next* device's traffic as its own.
///
/// With the `embedded-hal` feature this implements
/// `embedded_hal::spi::SpiDevice`.
pub struct SpiDevice<B: SpiBus + 'static, CS: ChipSelect> {
    bus: &'static SpinLock<B>,
    cs: CS,
}

struct Selected<'a, CS: ChipSelect>(&'a mut CS);

impl<CS: ChipSelect> Drop for Selected<'_, CS> {
    fn drop(&mut self) {
        self.0.deselect();
    }
}

impl<B: SpiBus + 'static, CS: ChipSelect> SpiDevice<B, CS> {
    /// Binds a chip select to a locked bus.
    pub const fn new(bus: &'static SpinLock<B>, cs: CS) -> Self {
        Self { bus, cs }
    }

    /// Runs `body` with the bus locked and this device selected.
    ///
    /// Everything else here is written in terms of this. Use it directly when a
    /// device needs several transfers with the select held low throughout.
    pub fn transaction<R>(&mut self, body: impl FnOnce(&mut B) -> R) -> R {
        let cs = &mut self.cs;

        self.bus.with(|bus| {
            cs.select();
            let _selected = Selected(cs);
            SpiBus::transaction(bus, body)
        })
    }

    /// Clocks `write` out and `read` in.
    pub fn transfer(&mut self, read: &mut [u8], write: &[u8]) -> Result<(), B::Error> {
        self.transaction(|bus| bus.transfer(read, write))
    }

    /// Clocks `data` out, discarding what comes back.
    pub fn write(&mut self, data: &[u8]) -> Result<(), B::Error> {
        self.transaction(|bus| bus.write(data))
    }

    /// Clocks zeros out and `data` in.
    pub fn read(&mut self, data: &mut [u8]) -> Result<(), B::Error> {
        self.transaction(|bus| bus.read(data))
    }

    /// Clocks `data` out and overwrites it with what came back.
    pub fn transfer_in_place(&mut self, data: &mut [u8]) -> Result<(), B::Error> {
        self.transaction(|bus| bus.transfer_in_place(data))
    }
}
