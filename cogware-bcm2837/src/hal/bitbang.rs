// SPDX-License-Identifier: MIT OR Apache-2.0
//! SPI and I2C driven directly from GPIO pins, with no peripheral involved.
//!
//! The CPU generates every edge, so a transfer is a busy loop. Three reasons
//! to want it anyway:
//!
//! * **Any pins.** SPI0 is fixed to GPIO 7-11 and BSC1 to GPIO 2-3; a
//!   bit-banged bus can use any GPIO pin.
//! * **Multiple buses.** There is one usable hardware SPI and one usable
//!   hardware I2C; a bit-banged bus is limited only by available pins.
//! * **Clock stretching works.** The BCM2837's hardware I2C master cannot
//!   cope with a slave stretching the clock at an arbitrary point — a
//!   silicon bug, described in [`crate::drivers::i2c`]. [`I2c`] watches `SCL`
//!   and waits for it, so devices that break the hardware master work here.
//!
//! Both buses are generic over the pin traits in [`crate::hal::digital`] rather
//! than over concrete GPIO types, which lets the `bitbang` example drive them
//! with simulated pins and check every edge without hardware.
//!
//! # Timing
//!
//! Speed is given as a bit rate and converted once, at construction, into a
//! half-period in system-counter ticks, so every edge costs one comparison
//! against `CNTPCT_EL0` rather than a division. See [`crate::hal::delay`].
//!
//! The achievable rate is bounded by how fast a pin can be toggled, which on a
//! 1.2 GHz A53 storing to Device-nGnRnE memory is a few MHz. A higher request
//! yields the fastest achievable rate, not an error.

use crate::hal::{
    delay::{nanos_to_ticks, spin_ticks},
    digital::{InputPin, OpenDrainPin, OutputPin},
};

pub use crate::drivers::spi::{MODE_0, MODE_1, MODE_2, MODE_3, Mode, Phase, Polarity};

// ---------------------------------------------------------------------------
// A pin that is not there
// ---------------------------------------------------------------------------

/// A placeholder for a signal the bus does not use.
///
/// A write-only SPI device has no `MISO` to connect. Pass a `NoPin`: reads
/// come back high, as an unconnected pulled-up line does, and writes go
/// nowhere.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoPin;

impl InputPin for NoPin {
    #[inline]
    fn is_high(&self) -> bool {
        true
    }
}

impl OutputPin for NoPin {
    #[inline]
    fn set_high(&mut self) {}

    #[inline]
    fn set_low(&mut self) {}
}

// ---------------------------------------------------------------------------
// SPI
// ---------------------------------------------------------------------------

/// A bit-banged SPI master.
///
/// Chip select is not handled here: a bus with more than one device needs a
/// select line per device, so use [`crate::hal::shared::SpiDevice`], or drive
/// an [`OutputPin`] yourself around the transfer.
pub struct Spi<SCK, MOSI, MISO> {
    sck: SCK,
    mosi: MOSI,
    miso: MISO,
    mode: Mode,
    half_period: u64,
}

impl<SCK, MOSI, MISO> Spi<SCK, MOSI, MISO>
where
    SCK: OutputPin,
    MOSI: OutputPin,
    MISO: InputPin,
{
    /// Builds a bus from three pins, a mode and a bit rate.
    ///
    /// `SCK` is parked at the idle level for the mode, so the first transfer
    /// starts from a defined state.
    pub fn new(sck: SCK, mosi: MOSI, miso: MISO, mode: Mode, speed_hz: u32) -> Self {
        let half_period = half_period_ticks(speed_hz);

        let mut bus = Self {
            sck,
            mosi,
            miso,
            mode,
            half_period,
        };
        bus.idle_clock();
        bus
    }

    /// Changes the bit rate.
    pub fn set_speed_hz(&mut self, speed_hz: u32) {
        self.half_period = half_period_ticks(speed_hz);
    }

    /// Changes the clock polarity and phase, and re-parks the clock.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.idle_clock();
    }

    /// Gives the pins back.
    pub fn release(self) -> (SCK, MOSI, MISO) {
        (self.sck, self.mosi, self.miso)
    }

    /// Clocks `write` out and `read` in, simultaneously.
    ///
    /// As with the hardware bus the slices may differ in length: zeros are sent
    /// once `write` runs out, and bytes past the end of `read` are dropped.
    pub fn transfer(&mut self, read: &mut [u8], write: &[u8]) {
        let length = read.len().max(write.len());

        for index in 0..length {
            let out = write.get(index).copied().unwrap_or(0);
            let back = self.transfer_byte(out);
            if let Some(slot) = read.get_mut(index) {
                *slot = back;
            }
        }
    }

    /// Clocks `data` out, discarding what comes back.
    pub fn write(&mut self, data: &[u8]) {
        for &byte in data {
            self.transfer_byte(byte);
        }
    }

    /// Clocks zeros out and `data` in.
    pub fn read(&mut self, data: &mut [u8]) {
        for slot in data.iter_mut() {
            *slot = self.transfer_byte(0);
        }
    }

    /// Clocks `data` out and overwrites it with what came back.
    pub fn transfer_in_place(&mut self, data: &mut [u8]) {
        for slot in data.iter_mut() {
            *slot = self.transfer_byte(*slot);
        }
    }

    /// Shifts one byte out and one byte in, most significant bit first.
    ///
    /// The two phases differ in *when* data changes relative to the clock:
    ///
    /// * `CPHA = 0` — the bit is placed on `MOSI` while the clock is idle, the
    ///   leading edge samples it, and the trailing edge returns the clock to
    ///   idle.
    /// * `CPHA = 1` — the leading edge changes `MOSI`, and the trailing edge
    ///   samples.
    ///
    /// `CPOL` decides which physical direction "leading" is: with an idle-low
    /// clock the leading edge rises, with idle-high it falls.
    pub fn transfer_byte(&mut self, byte: u8) -> u8 {
        let idle_high = self.mode.polarity == Polarity::IdleHigh;
        let sample_on_leading = self.mode.phase == Phase::CaptureOnFirstTransition;

        let mut input = 0u8;

        for bit in (0..8).rev() {
            let out = (byte >> bit) & 1 == 1;

            if sample_on_leading {
                self.mosi.set_state(out);
                spin_ticks(self.half_period);

                self.sck.set_state(!idle_high); // leading edge
                input = (input << 1) | u8::from(self.miso.is_high());
                spin_ticks(self.half_period);

                self.sck.set_state(idle_high); // trailing edge
            } else {
                self.sck.set_state(!idle_high); // leading edge
                self.mosi.set_state(out);
                spin_ticks(self.half_period);

                self.sck.set_state(idle_high); // trailing edge
                input = (input << 1) | u8::from(self.miso.is_high());
                spin_ticks(self.half_period);
            }
        }

        input
    }

    /// Parks `SCK` at the idle level for the current mode.
    fn idle_clock(&mut self) {
        let idle_high = self.mode.polarity == Polarity::IdleHigh;
        self.sck.set_state(idle_high);
    }
}

// ---------------------------------------------------------------------------
// I2C
// ---------------------------------------------------------------------------

/// Something went wrong on a bit-banged I2C bus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// Nothing acknowledged the address.
    AddressNack,
    /// A device acknowledged the address but then stopped acknowledging data.
    ///
    /// Carries the number of data bytes that *were* acknowledged, which shows
    /// where the device gave up.
    DataNack(usize),
    /// A device held `SCL` low for longer than the stretch limit.
    ClockTimeout,
    /// A line was still low when the bus should have been idle.
    ///
    /// Either a device is mid-transaction and confused, or `SDA`/`SCL` have no
    /// pull-up and are floating low.
    BusStuck,
}

/// A bit-banged I2C master.
///
/// Both pins must be [`OpenDrainPin`]s: I2C lines are pulled up externally and
/// only ever driven low. [`crate::hal::gpio::OpenDrain`] explains how that is
/// arranged on a chip whose pads are push-pull.
///
/// Unlike the hardware master this one honours clock stretching: after
/// releasing `SCL` it waits until the line reads high before timing the
/// clock-high period.
pub struct I2c<SDA, SCL> {
    sda: SDA,
    scl: SCL,
    half_period: u64,
    stretch_limit: u32,
}

impl<SDA, SCL> I2c<SDA, SCL>
where
    SDA: OpenDrainPin,
    SCL: OpenDrainPin,
{
    /// How many spins to wait for a stretched clock before giving up.
    const DEFAULT_STRETCH_LIMIT: u32 = 1_000_000;

    /// Builds a bus from two open-drain pins and a bit rate.
    ///
    /// Both lines are released, which is the idle state.
    pub fn new(sda: SDA, scl: SCL, speed_hz: u32) -> Self {
        let mut bus = Self {
            sda,
            scl,
            half_period: half_period_ticks(speed_hz),
            stretch_limit: Self::DEFAULT_STRETCH_LIMIT,
        };
        bus.sda.release();
        bus.scl.release();
        bus
    }

    /// Changes the bit rate.
    pub fn set_speed_hz(&mut self, speed_hz: u32) {
        self.half_period = half_period_ticks(speed_hz);
    }

    /// Changes how long to tolerate a stretched clock.
    pub fn set_stretch_limit(&mut self, spins: u32) {
        self.stretch_limit = spins;
    }

    /// Gives the pins back.
    pub fn release(self) -> (SDA, SCL) {
        (self.sda, self.scl)
    }

    /// Writes `data` to the device at seven-bit address `address`.
    pub fn write(&mut self, address: u8, data: &[u8]) -> Result<(), Error> {
        self.start()?;
        let result = self.write_addressed(address, data);
        self.stop();
        result
    }

    /// Reads `data.len()` bytes from the device at `address`.
    pub fn read(&mut self, address: u8, data: &mut [u8]) -> Result<(), Error> {
        self.start()?;
        let result = self.read_addressed(address, data);
        self.stop();
        result
    }

    /// Writes `write`, then reads `read`, with a repeated START in between.
    ///
    /// The bus is never released, so nothing can interleave a transaction
    /// between the register number and the data.
    pub fn write_read(&mut self, address: u8, write: &[u8], read: &mut [u8]) -> Result<(), Error> {
        self.start()?;

        if let Err(error) = self.write_addressed(address, write) {
            self.stop();
            return Err(error);
        }

        self.restart()?;

        let result = self.read_addressed(address, read);
        self.stop();
        result
    }

    /// Reports whether anything answers at `address`.
    ///
    /// Sends an address byte and nothing else, so unlike the hardware master's
    /// [`crate::drivers::i2c::I2c::probe`] this consumes no data and has no
    /// side effect on the device.
    pub fn probe(&mut self, address: u8) -> bool {
        let found = self
            .start()
            .and_then(|()| self.write_byte(address << 1))
            .unwrap_or(false);
        self.stop();
        found
    }

    /// Scans the bus and calls `found` for every address that answers.
    pub fn scan(&mut self, mut found: impl FnMut(u8)) {
        for address in 0x08..=0x77 {
            if self.probe(address) {
                found(address);
            }
        }
    }

    // -- transaction pieces --------------------------------------------------

    /// Sends the address with the write bit, then the data.
    fn write_addressed(&mut self, address: u8, data: &[u8]) -> Result<(), Error> {
        if !self.write_byte(address << 1)? {
            return Err(Error::AddressNack);
        }

        for (index, &byte) in data.iter().enumerate() {
            if !self.write_byte(byte)? {
                return Err(Error::DataNack(index));
            }
        }

        Ok(())
    }

    /// Sends the address with the read bit, then clocks the data in.
    ///
    /// Every byte but the last is acknowledged; the last is not, which is how
    /// the master tells the device to stop talking.
    fn read_addressed(&mut self, address: u8, data: &mut [u8]) -> Result<(), Error> {
        if !self.write_byte((address << 1) | 1)? {
            return Err(Error::AddressNack);
        }

        let last = data.len().saturating_sub(1);
        for (index, byte) in data.iter_mut().enumerate() {
            *byte = self.read_byte(index != last)?;
        }

        Ok(())
    }

    /// `SDA` falls while `SCL` is high.
    fn start(&mut self) -> Result<(), Error> {
        self.sda.release();
        self.release_scl()?;
        self.wait_half();

        if self.sda.is_low() {
            // Somebody else is holding the line down; starting now would
            // corrupt their transaction.
            return Err(Error::BusStuck);
        }

        self.sda.drive_low();
        self.wait_half();
        self.scl.drive_low();
        self.wait_half();

        Ok(())
    }

    /// A START with no preceding STOP: bring both lines back up, then drop
    /// `SDA` again while `SCL` is high.
    ///
    /// `SCL` must be driven low first: the transaction so far ended on the ACK
    /// of the previous byte, where the *slave* is pulling `SDA` down and `SCL`
    /// is still high. The slave only lets go of `SDA` when `SCL` falls, so
    /// without this the falling edge of `SDA` that makes a START never happens
    /// and the repeated START is silently lost.
    fn restart(&mut self) -> Result<(), Error> {
        self.scl.drive_low();
        self.sda.release();
        self.wait_half();

        self.release_scl()?;
        self.wait_half();

        self.sda.drive_low();
        self.wait_half();
        self.scl.drive_low();
        self.wait_half();

        Ok(())
    }

    /// `SDA` rises while `SCL` is high.
    ///
    /// Infallible on purpose: it runs on the error path, where a second error
    /// would only mask the first. A stuck bus stays stuck and the next
    /// [`I2c::start`] reports it.
    fn stop(&mut self) {
        self.scl.drive_low();
        self.sda.drive_low();
        self.wait_half();

        let _ = self.release_scl();
        self.wait_half();

        self.sda.release();
        self.wait_half();
    }

    /// Shifts a byte out, most significant bit first, and returns whether the
    /// device acknowledged it.
    fn write_byte(&mut self, byte: u8) -> Result<bool, Error> {
        for bit in (0..8).rev() {
            self.write_bit((byte >> bit) & 1 == 1)?;
        }

        // The ninth bit belongs to the device: it pulls `SDA` low to ACK.
        Ok(!self.read_bit()?)
    }

    /// Shifts a byte in, then acknowledges it or does not.
    fn read_byte(&mut self, ack: bool) -> Result<u8, Error> {
        let mut byte = 0u8;
        for _ in 0..8 {
            byte = (byte << 1) | u8::from(self.read_bit()?);
        }

        // ACK is a low bit, so acknowledging means writing false.
        self.write_bit(!ack)?;
        Ok(byte)
    }

    /// Puts one bit on `SDA` and clocks it.
    ///
    /// `SDA` only ever changes while `SCL` is low; a transition while the clock
    /// is high is a START or a STOP, not data.
    fn write_bit(&mut self, bit: bool) -> Result<(), Error> {
        self.scl.drive_low();

        if bit {
            self.sda.release();
        } else {
            self.sda.drive_low();
        }

        self.wait_half();
        self.release_scl()?;
        self.wait_half();

        Ok(())
    }

    /// Releases `SDA` so the device can drive it, and samples one bit.
    fn read_bit(&mut self) -> Result<bool, Error> {
        self.scl.drive_low();
        self.sda.release();
        self.wait_half();

        self.release_scl()?;
        // Sample while the clock is high, which is when the device guarantees
        // the line is stable.
        let bit = self.sda.is_high();
        self.wait_half();

        Ok(bit)
    }

    /// Releases `SCL` and waits for it to rise.
    ///
    /// This is the whole of clock-stretch support: a device that needs more
    /// time holds `SCL` down after the master releases it, and the master
    /// must not start timing the high period until the line is high.
    fn release_scl(&mut self) -> Result<(), Error> {
        self.scl.release();

        let mut spins = 0u32;
        while self.scl.is_low() {
            spins += 1;
            if spins > self.stretch_limit {
                return Err(Error::ClockTimeout);
            }
            core::hint::spin_loop();
        }

        Ok(())
    }

    /// Waits half a bit period.
    #[inline]
    fn wait_half(&self) {
        spin_ticks(self.half_period);
    }
}

/// Converts a bit rate into half a bit period, in system-counter ticks.
///
/// Zero means "as fast as the pins will go": the delay drops out and the rate
/// becomes whatever a store to the GPIO register costs.
fn half_period_ticks(speed_hz: u32) -> u64 {
    if speed_hz == 0 {
        return 0;
    }

    let half_period_ns = 500_000_000u64 / u64::from(speed_hz);
    nanos_to_ticks(half_period_ns)
}
