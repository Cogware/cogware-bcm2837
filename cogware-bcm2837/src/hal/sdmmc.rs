// SPDX-License-Identifier: GPL-3.0-only
//! `embedded-sdmmc` block device implementation.
//!
//! The adapter that lets the `embedded-sdmmc` crate mount a FAT filesystem on
//! the card in the Pi's SD slot. Everything it does is a restatement of
//! [`crate::drivers::emmc::Emmc::read_blocks`] and its write counterpart,
//! wrapped in the types the traits require.
//!
//! Compiled only with the `embedded-sdmmc` feature, which is **off** by
//! default: unlike `embedded-hal` this dependency is not a pure trait crate,
//! and brings a FAT implementation and transitive crates with it. See
//! `instructions.md` rule 1.1.
//!
//! # Usage
//!
//! ```ignore
//! use cogware_bcm2837::{drivers, hal::sdmmc::{NoClock, SdCard}};
//! use embedded_sdmmc::{Mode, VolumeIdx, VolumeManager};
//!
//! drivers::EMMC.lock().init()?;
//!
//! let manager = VolumeManager::new(SdCard::read_only(), NoClock);
//! let volume = manager.open_volume(VolumeIdx(0))?;
//! let root = volume.open_root_dir()?;
//! let file = root.open_file_in_dir("CONFIG.TXT", Mode::ReadOnly)?;
//! ```
//!
//! [`crate::drivers::EMMC`] must be initialised first. This type only borrows
//! the controller for the duration of each transfer.

use core::fmt;

use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx, TimeSource, Timestamp};

use crate::drivers::{self, emmc};

/// Why a block transfer failed.
///
/// [`BlockDevice::Error`] must implement [`core::error::Error`], which
/// [`emmc::Error`] does not, so it is wrapped here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The EMMC driver rejected the transfer.
    Emmc(emmc::Error),
    /// A write was attempted on a card opened with [`SdCard::read_only`].
    ReadOnly,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Emmc(error) => write!(f, "{error}"),
            Self::ReadOnly => write!(f, "card is open read-only"),
        }
    }
}

impl core::error::Error for Error {}

impl From<emmc::Error> for Error {
    fn from(error: emmc::Error) -> Self {
        Self::Emmc(error)
    }
}

/// The SD slot, as an `embedded-sdmmc` block device.
///
/// A zero-sized handle rather than an owned peripheral: there is one slot,
/// [`drivers::EMMC`] is the single controller behind a spin lock, and the lock
/// is taken for each transfer. Several of these may exist and be used from
/// different cores; they all address the same card.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SdCard {
    writable: bool,
}

impl SdCard {
    /// A read-write view of the card.
    #[must_use]
    pub const fn new() -> Self {
        Self { writable: true }
    }

    /// A view that fails every write at the block layer.
    ///
    /// Writes fail with [`Error::ReadOnly`] at the block layer, so a mistake
    /// further up cannot corrupt the volume.
    #[must_use]
    pub const fn read_only() -> Self {
        Self { writable: false }
    }
}

impl Default for SdCard {
    fn default() -> Self {
        Self::new()
    }
}

/// Turns a block offset into an absolute LBA, or reports the overflow.
fn lba(start: BlockIdx, offset: usize) -> Result<u32, Error> {
    u32::try_from(offset)
        .ok()
        .and_then(|offset| start.0.checked_add(offset))
        .ok_or(Error::Emmc(emmc::Error::OutOfRange))
}

impl BlockDevice for SdCard {
    type Error = Error;

    fn read(&self, blocks: &mut [Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        let mut emmc = drivers::EMMC.lock();
        for (offset, block) in blocks.iter_mut().enumerate() {
            // One block per call rather than one multi-block transfer: `Block`
            // carries no layout guarantee, so a `[Block]` cannot soundly be
            // reinterpreted as one contiguous byte buffer.
            emmc.read_blocks(lba(start_block_idx, offset)?, &mut block.contents)?;
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        if !self.writable {
            return Err(Error::ReadOnly);
        }
        let mut emmc = drivers::EMMC.lock();
        for (offset, block) in blocks.iter().enumerate() {
            emmc.write_blocks(lba(start_block_idx, offset)?, &block.contents)?;
        }
        Ok(())
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        let blocks = drivers::EMMC.lock().block_count();
        // embedded-sdmmc counts blocks in a u32, so it tops out at 2 TiB.
        // Saturate rather than wrap to a tiny volume.
        Ok(BlockCount(u32::try_from(blocks).unwrap_or(u32::MAX)))
    }
}

/// A [`TimeSource`] that always reports the epoch.
///
/// The BCM2837 has no battery-backed real-time clock, so there is no correct
/// answer at boot; 1970-01-01 is visibly wrong rather than plausibly wrong.
/// Supply your own [`TimeSource`] if the board has an RTC on I2C, or if the
/// time has been fetched from elsewhere.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct NoClock;

impl TimeSource for NoClock {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 0,
            zero_indexed_month: 0,
            zero_indexed_day: 0,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}
