// SPDX-License-Identifier: MIT OR Apache-2.0
//! `embedded-hal` 1.0 implementations.
//!
//! The adapter that lets driver crates written for any other chip run here
//! unmodified. Everything it implements is a thin restatement of the
//! infallible API in [`crate::hal::digital`] and the buses in
//! [`crate::drivers`] and [`crate::hal::bitbang`], wrapped in the `Result`s the
//! traits require.
//!
//! Compiled only with the `embedded-hal` feature, which is on by default. Turn
//! it off and everything else in the crate still works.
//!
//! # What maps to what
//!
//! | Ours | `embedded-hal` |
//! | --- | --- |
//! | [`Pin`]`<N, Input<_>>` | `digital::InputPin` |
//! | [`Pin`]`<N, Output>` | `digital::OutputPin`, `StatefulOutputPin`, `InputPin` |
//! | [`Pin`]`<N, OpenDrain>` | `digital::OutputPin`, `InputPin` |
//! | [`Delay`] | `delay::DelayNs` |
//! | [`Spi0`], [`bitbang::Spi`] | `spi::SpiBus` |
//! | [`shared::SpiDevice`] | `spi::SpiDevice` |
//! | [`I2c`], [`bitbang::I2c`], [`shared::SharedI2c`] | `i2c::I2c` |
//!
//! Two mappings need explaining.
//!
//! **An output pin also implements `InputPin`.** Reading a pin configured as
//! an output reads the pad, not a remembered value, and the BCM2837 allows
//! that, so drivers that toggle a line and check it landed work.
//!
//! **An open-drain pin implements `OutputPin`, not some open-drain trait**,
//! because `embedded-hal` has no open-drain trait. `set_low` drives the line
//! down and `set_high` releases it, which is the convention every other HAL
//! uses and is electrically what open drain means.
//!
//! # Errors
//!
//! Pin operations are [`Infallible`], so `unwrap` on them cannot panic. Bus
//! errors map onto `ErrorKind` as closely as the hardware allows: the BSC
//! controller sets one error flag for every kind of NACK, so
//! [`crate::drivers::i2c::Error::Nack`] becomes
//! `NoAcknowledgeSource::Unknown`, while the bit-banged bus watches the ACK
//! bit itself and can tell an address NACK from a data NACK.

use core::convert::Infallible;

use embedded_hal::{delay, digital, i2c, spi};

use crate::{
    drivers::{i2c::I2c, spi::Spi0},
    hal::{
        bitbang,
        delay::Delay,
        digital::OpenDrainPin as _,
        gpio::{AnyPin, OpenDrain, Output, Pin},
        shared::{self, ChipSelect},
    },
};

// ---------------------------------------------------------------------------
// Pins
// ---------------------------------------------------------------------------

impl<const N: u8, MODE> digital::ErrorType for Pin<N, MODE> {
    type Error = Infallible;
}

impl<MODE> digital::ErrorType for AnyPin<MODE> {
    type Error = Infallible;
}

// Readable in any mode that can be read: inputs, outputs and open-drain pins.
// The bound is on our own trait, which is only implemented for those modes, so
// an `Alt<2>` pin still cannot be sampled.

impl<const N: u8, MODE> digital::InputPin for Pin<N, MODE>
where
    Pin<N, MODE>: crate::hal::digital::InputPin,
{
    #[inline]
    fn is_high(&mut self) -> Result<bool, Self::Error> {
        Ok(crate::hal::digital::InputPin::is_high(self))
    }

    #[inline]
    fn is_low(&mut self) -> Result<bool, Self::Error> {
        Ok(crate::hal::digital::InputPin::is_low(self))
    }
}

impl<MODE> digital::InputPin for AnyPin<MODE>
where
    AnyPin<MODE>: crate::hal::digital::InputPin,
{
    #[inline]
    fn is_high(&mut self) -> Result<bool, Self::Error> {
        Ok(crate::hal::digital::InputPin::is_high(self))
    }

    #[inline]
    fn is_low(&mut self) -> Result<bool, Self::Error> {
        Ok(crate::hal::digital::InputPin::is_low(self))
    }
}

impl<const N: u8> digital::OutputPin for Pin<N, Output> {
    #[inline]
    fn set_high(&mut self) -> Result<(), Self::Error> {
        crate::hal::digital::OutputPin::set_high(self);
        Ok(())
    }

    #[inline]
    fn set_low(&mut self) -> Result<(), Self::Error> {
        crate::hal::digital::OutputPin::set_low(self);
        Ok(())
    }
}

impl digital::OutputPin for AnyPin<Output> {
    #[inline]
    fn set_high(&mut self) -> Result<(), Self::Error> {
        crate::hal::digital::OutputPin::set_high(self);
        Ok(())
    }

    #[inline]
    fn set_low(&mut self) -> Result<(), Self::Error> {
        crate::hal::digital::OutputPin::set_low(self);
        Ok(())
    }
}

impl<const N: u8> digital::StatefulOutputPin for Pin<N, Output> {
    #[inline]
    fn is_set_high(&mut self) -> Result<bool, Self::Error> {
        Ok(crate::hal::digital::StatefulOutputPin::is_set_high(self))
    }

    #[inline]
    fn is_set_low(&mut self) -> Result<bool, Self::Error> {
        Ok(crate::hal::digital::StatefulOutputPin::is_set_low(self))
    }
}

impl digital::StatefulOutputPin for AnyPin<Output> {
    #[inline]
    fn is_set_high(&mut self) -> Result<bool, Self::Error> {
        Ok(crate::hal::digital::StatefulOutputPin::is_set_high(self))
    }

    #[inline]
    fn is_set_low(&mut self) -> Result<bool, Self::Error> {
        Ok(crate::hal::digital::StatefulOutputPin::is_set_low(self))
    }
}

// Open drain: low means drive, high means let go. `embedded-hal` has no trait
// that says so, and this is how every HAL spells it.

impl<const N: u8> digital::OutputPin for Pin<N, OpenDrain> {
    #[inline]
    fn set_high(&mut self) -> Result<(), Self::Error> {
        self.release();
        Ok(())
    }

    #[inline]
    fn set_low(&mut self) -> Result<(), Self::Error> {
        self.drive_low();
        Ok(())
    }
}

impl digital::OutputPin for AnyPin<OpenDrain> {
    #[inline]
    fn set_high(&mut self) -> Result<(), Self::Error> {
        self.release();
        Ok(())
    }

    #[inline]
    fn set_low(&mut self) -> Result<(), Self::Error> {
        self.drive_low();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Delay
// ---------------------------------------------------------------------------

impl delay::DelayNs for Delay {
    #[inline]
    fn delay_ns(&mut self, ns: u32) {
        self.delay_nanos(u64::from(ns));
    }

    #[inline]
    fn delay_us(&mut self, us: u32) {
        self.delay_micros(u64::from(us));
    }

    #[inline]
    fn delay_ms(&mut self, ms: u32) {
        self.delay_millis(u64::from(ms));
    }
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

impl spi::Error for crate::drivers::spi::Error {
    fn kind(&self) -> spi::ErrorKind {
        // Neither variant has an `embedded-hal` equivalent: `Timeout` means the
        // controller stopped responding and `TooLong` is a caller mistake
        // caught before any clock edge.
        spi::ErrorKind::Other
    }
}

impl i2c::Error for crate::drivers::i2c::Error {
    fn kind(&self) -> i2c::ErrorKind {
        use crate::drivers::i2c::Error;

        match self {
            // The BSC controller raises one `ERR` flag whatever was not
            // acknowledged, so the source is unknown.
            Error::Nack => i2c::ErrorKind::NoAcknowledge(i2c::NoAcknowledgeSource::Unknown),
            Error::ClockTimeout | Error::Timeout => i2c::ErrorKind::Other,
            Error::TooLong | Error::UnsupportedTransaction | Error::RepeatedStartLost => {
                i2c::ErrorKind::Other
            }
        }
    }
}

impl i2c::Error for bitbang::Error {
    fn kind(&self) -> i2c::ErrorKind {
        match self {
            // Bit-banging reads the ACK bit at a known point in the frame, so
            // it knows exactly what was refused.
            bitbang::Error::AddressNack => {
                i2c::ErrorKind::NoAcknowledge(i2c::NoAcknowledgeSource::Address)
            }
            bitbang::Error::DataNack(_) => {
                i2c::ErrorKind::NoAcknowledge(i2c::NoAcknowledgeSource::Data)
            }
            bitbang::Error::ClockTimeout => i2c::ErrorKind::Other,
            bitbang::Error::BusStuck => i2c::ErrorKind::Bus,
            bitbang::Error::EmptyRead => i2c::ErrorKind::Other,
        }
    }
}

// ---------------------------------------------------------------------------
// SPI buses
// ---------------------------------------------------------------------------

impl spi::ErrorType for Spi0 {
    type Error = crate::drivers::spi::Error;
}

impl spi::SpiBus<u8> for Spi0 {
    #[inline]
    fn read(&mut self, words: &mut [u8]) -> Result<(), Self::Error> {
        Spi0::read(self, words)
    }

    #[inline]
    fn write(&mut self, words: &[u8]) -> Result<(), Self::Error> {
        Spi0::write(self, words)
    }

    #[inline]
    fn transfer(&mut self, read: &mut [u8], write: &[u8]) -> Result<(), Self::Error> {
        Spi0::transfer(self, read, write)
    }

    #[inline]
    fn transfer_in_place(&mut self, words: &mut [u8]) -> Result<(), Self::Error> {
        Spi0::transfer_in_place(self, words)
    }

    #[inline]
    fn flush(&mut self) -> Result<(), Self::Error> {
        Spi0::flush(self)
    }
}

impl<SCK, MOSI, MISO> spi::ErrorType for bitbang::Spi<SCK, MOSI, MISO> {
    type Error = Infallible;
}

impl<SCK, MOSI, MISO> spi::SpiBus<u8> for bitbang::Spi<SCK, MOSI, MISO>
where
    SCK: crate::hal::digital::OutputPin,
    MOSI: crate::hal::digital::OutputPin,
    MISO: crate::hal::digital::InputPin,
{
    #[inline]
    fn read(&mut self, words: &mut [u8]) -> Result<(), Self::Error> {
        bitbang::Spi::read(self, words);
        Ok(())
    }

    #[inline]
    fn write(&mut self, words: &[u8]) -> Result<(), Self::Error> {
        bitbang::Spi::write(self, words);
        Ok(())
    }

    #[inline]
    fn transfer(&mut self, read: &mut [u8], write: &[u8]) -> Result<(), Self::Error> {
        bitbang::Spi::transfer(self, read, write);
        Ok(())
    }

    #[inline]
    fn transfer_in_place(&mut self, words: &mut [u8]) -> Result<(), Self::Error> {
        bitbang::Spi::transfer_in_place(self, words);
        Ok(())
    }

    #[inline]
    fn flush(&mut self) -> Result<(), Self::Error> {
        // Nothing is buffered: `transfer_byte` returns only once the last edge
        // has been driven.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SPI devices
// ---------------------------------------------------------------------------

impl<B, CS> spi::ErrorType for shared::SpiDevice<B, CS>
where
    B: shared::SpiBus + 'static,
    B::Error: spi::Error,
    CS: ChipSelect,
{
    type Error = B::Error;
}

impl<B, CS> spi::SpiDevice<u8> for shared::SpiDevice<B, CS>
where
    B: shared::SpiBus + 'static,
    B::Error: spi::Error,
    CS: ChipSelect,
{
    /// Runs every operation with the bus locked and chip select asserted
    /// throughout, as `embedded-hal` specifies.
    ///
    /// The first error ends the sequence, but chip select is still released:
    /// a device left selected would answer the next device's traffic.
    fn transaction(
        &mut self,
        operations: &mut [spi::Operation<'_, u8>],
    ) -> Result<(), Self::Error> {
        self.transaction(|bus| {
            for operation in operations.iter_mut() {
                match operation {
                    spi::Operation::Read(buffer) => bus.read(buffer)?,
                    spi::Operation::Write(buffer) => bus.write(buffer)?,
                    spi::Operation::Transfer(read, write) => bus.transfer(read, write)?,
                    spi::Operation::TransferInPlace(buffer) => bus.transfer_in_place(buffer)?,
                    spi::Operation::DelayNs(ns) => {
                        Delay::new().delay_nanos(u64::from(*ns));
                    }
                }
            }
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// I2C buses
// ---------------------------------------------------------------------------

impl i2c::ErrorType for I2c {
    type Error = crate::drivers::i2c::Error;
}

impl i2c::I2c<i2c::SevenBitAddress> for I2c {
    #[inline]
    fn read(&mut self, address: u8, read: &mut [u8]) -> Result<(), Self::Error> {
        I2c::read(self, address, read)
    }

    #[inline]
    fn write(&mut self, address: u8, write: &[u8]) -> Result<(), Self::Error> {
        I2c::write(self, address, write)
    }

    #[inline]
    fn write_read(
        &mut self,
        address: u8,
        write: &[u8],
        read: &mut [u8],
    ) -> Result<(), Self::Error> {
        I2c::write_read(self, address, write, read)
    }

    fn transaction(
        &mut self,
        address: u8,
        operations: &mut [i2c::Operation<'_>],
    ) -> Result<(), Self::Error> {
        I2c::transaction(self, address, &mut Operations(operations))
    }
}

impl<SDA, SCL> i2c::ErrorType for bitbang::I2c<SDA, SCL> {
    type Error = bitbang::Error;
}

impl<SDA, SCL> i2c::I2c<i2c::SevenBitAddress> for bitbang::I2c<SDA, SCL>
where
    SDA: crate::hal::digital::OpenDrainPin,
    SCL: crate::hal::digital::OpenDrainPin,
{
    #[inline]
    fn read(&mut self, address: u8, read: &mut [u8]) -> Result<(), Self::Error> {
        bitbang::I2c::read(self, address, read)
    }

    #[inline]
    fn write(&mut self, address: u8, write: &[u8]) -> Result<(), Self::Error> {
        bitbang::I2c::write(self, address, write)
    }

    #[inline]
    fn write_read(
        &mut self,
        address: u8,
        write: &[u8],
        read: &mut [u8],
    ) -> Result<(), Self::Error> {
        bitbang::I2c::write_read(self, address, write, read)
    }

    fn transaction(
        &mut self,
        address: u8,
        operations: &mut [i2c::Operation<'_>],
    ) -> Result<(), Self::Error> {
        bitbang::I2c::transaction(self, address, &mut Operations(operations))
    }
}

impl<B> i2c::ErrorType for shared::SharedI2c<B>
where
    B: shared::I2cBus + 'static,
    B::Error: i2c::Error,
{
    type Error = B::Error;
}

impl<B> i2c::I2c<i2c::SevenBitAddress> for shared::SharedI2c<B>
where
    B: shared::I2cTransaction + 'static,
    B::Error: i2c::Error,
{
    #[inline]
    fn read(&mut self, address: u8, read: &mut [u8]) -> Result<(), Self::Error> {
        shared::I2cBus::read(self, address, read)
    }

    #[inline]
    fn write(&mut self, address: u8, write: &[u8]) -> Result<(), Self::Error> {
        shared::I2cBus::write(self, address, write)
    }

    #[inline]
    fn write_read(
        &mut self,
        address: u8,
        write: &[u8],
        read: &mut [u8],
    ) -> Result<(), Self::Error> {
        shared::I2cBus::write_read(self, address, write, read)
    }

    /// Holds the bus lock for the whole sequence, so nothing on another core
    /// can address a different device part way through.
    fn transaction(
        &mut self,
        address: u8,
        operations: &mut [i2c::Operation<'_>],
    ) -> Result<(), Self::Error> {
        shared::I2cTransaction::transaction(self, address, &mut Operations(operations))
    }
}

// Reborrow rather than copy the list: no allocation or artificial capacity.
struct Operations<'a, 'b>(&'a mut [i2c::Operation<'b>]);

impl shared::I2cOperations for Operations<'_, '_> {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn operation(&mut self, index: usize) -> shared::I2cOperation<'_> {
        match &mut self.0[index] {
            i2c::Operation::Write(bytes) => shared::I2cOperation::Write(bytes),
            i2c::Operation::Read(bytes) => shared::I2cOperation::Read(bytes),
        }
    }
}
