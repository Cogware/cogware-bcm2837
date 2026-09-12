// SPDX-License-Identifier: GPL-3.0-only
//! Peripheral abstraction: owned pins, buses, and the `embedded-hal` traits.
//!
//! [`crate::drivers`] is the register layer — one singleton per peripheral,
//! behind one lock, addressed by number. This layer adds what the register
//! layer cannot:
//!
//! * **Ownership.** [`gpio::Pins::take`] hands out each of the 54 GPIOs
//!   exactly once, and a mode change consumes the pin by value, so a pin moved
//!   into a driver is unavailable to everything else. See [`gpio`].
//! * **Legality.** A [`gpio::Pin`] carries its mode in the type, so calling
//!   `set_high` on an input does not compile.
//! * **Portability.** The `embedded-hal` traits are implemented, so drivers
//!   written for other chips work — see below.
//!
//! # Buses
//!
//! | Module | Bus | Notes |
//! | --- | --- | --- |
//! | [`crate::drivers::spi`] | hardware SPI0 | 64-byte FIFOs, up to 125 MHz |
//! | [`crate::drivers::i2c`] | hardware BSC0/BSC1 | 16-byte FIFO, 100/400 kHz |
//! | [`bitbang`] | software SPI and I2C | any pins, any speed, no peripheral |
//!
//! The BCM2837's hardware I2C cannot cope with a slave that stretches the
//! clock at an arbitrary point — a documented silicon bug — so for some
//! sensors only the bit-banged bus works. See [`bitbang::I2c`].
//!
//! # Sharing a bus between cores
//!
//! [`shared`] puts the bus in a [`crate::sync::SpinLock`] and hands each user a
//! handle that takes the lock for exactly one transaction — the granularity
//! `embedded-hal`'s `SpiDevice` is designed around.
//!
//! # `embedded-hal` integration
//!
//! Everything here is usable on its own, with infallible signatures that suit
//! bare metal ([`digital`]). With the `embedded-hal` feature — on by default —
//! the 1.0 traits are implemented on top, so a driver crate written for any
//! other chip works here unmodified:
//!
//! ```ignore
//! use cogware_bcm2837::hal::{gpio::Pins, i2c::I2c1};
//!
//! let pins = Pins::take().unwrap();
//! let i2c = I2c1::new(pins.p2, pins.p3, 400_000);
//! let mut sensor = some_driver_crate::Sensor::new(i2c);   // just works
//! ```
//!
//! With the `embedded-hal-async` feature the async traits are implemented too,
//! on top of [`crate::task`]: bus wrappers yield between complete blocking
//! transactions, never while holding chip select or a bus lock. See [`asynch`].
//!
//! # Filesystems on the SD card
//!
//! [`crate::drivers::emmc`] reads and writes 512-byte blocks and knows nothing
//! of partitions or files. The `embedded-sdmmc` feature, off by default,
//! implements that crate's `BlockDevice` on the SD slot, so a FAT16 or FAT32
//! volume can be mounted and read by name. See [`sdmmc`].

pub mod bitbang;
pub mod delay;
pub mod digital;
pub mod gpio;
pub mod shared;

#[cfg(feature = "embedded-hal")]
mod eh;

#[cfg(feature = "embedded-hal-async")]
pub mod asynch;

#[cfg(feature = "embedded-sdmmc")]
pub mod sdmmc;

pub use delay::Delay;
pub use digital::{InputPin, OpenDrainPin, OutputPin, StatefulOutputPin};
pub use gpio::{AnyPin, Pin, Pins};
