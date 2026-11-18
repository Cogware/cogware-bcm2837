// SPDX-License-Identifier: GPL-3.0-only
//! CCI, the camera control interface: I2C with register addresses.
//!
//! Camera sensors (and many display bridges) are configured by writing values
//! to numbered registers over I2C. CCI fixes the framing: the register address
//! goes first, most significant byte first, then the data, also big-endian.
//! Most sensors use 16-bit register addresses; older OmniVision parts use 8.

use crate::{
    hal::shared::{I2cBus, I2cDevice},
    sync::SpinLock,
};

/// The longest payload [`Cci::write`] sends in one transaction.
pub const MAX_WRITE: usize = 32;

/// A device on a CCI bus.
pub struct Cci<B: I2cBus + 'static> {
    device: I2cDevice<B>,
    wide_address: bool,
}

impl<B: I2cBus + 'static> Cci<B> {
    /// A device at 7-bit `address` with 16-bit register addresses.
    pub const fn new(bus: &'static SpinLock<B>, address: u8) -> Self {
        Self {
            device: I2cDevice::new(bus, address),
            wide_address: true,
        }
    }

    /// A device at 7-bit `address` with 8-bit register addresses.
    pub const fn narrow(bus: &'static SpinLock<B>, address: u8) -> Self {
        Self {
            device: I2cDevice::new(bus, address),
            wide_address: false,
        }
    }

    /// The register address as it goes on the wire, and its length.
    fn address_bytes(&self, register: u16) -> ([u8; 2], usize) {
        if self.wide_address {
            (register.to_be_bytes(), 2)
        } else {
            ([register as u8, 0], 1)
        }
    }

    /// Reads `out.len()` consecutive registers from `register` up.
    pub fn read(&mut self, register: u16, out: &mut [u8]) -> Result<(), B::Error> {
        let (address, length) = self.address_bytes(register);
        self.device.write_read(&address[..length], out)
    }

    /// Reads one 8-bit register.
    pub fn read8(&mut self, register: u16) -> Result<u8, B::Error> {
        let mut value = [0];
        self.read(register, &mut value)?;
        Ok(value[0])
    }

    /// Reads a 16-bit value from two registers, high byte first.
    pub fn read16(&mut self, register: u16) -> Result<u16, B::Error> {
        let mut value = [0; 2];
        self.read(register, &mut value)?;
        Ok(u16::from_be_bytes(value))
    }

    /// Writes `data` to consecutive registers from `register` up, in one
    /// transaction. At most [`MAX_WRITE`] bytes; the rest are not sent.
    pub fn write(&mut self, register: u16, data: &[u8]) -> Result<(), B::Error> {
        let (address, length) = self.address_bytes(register);
        let mut frame = [0u8; MAX_WRITE + 2];
        let data = &data[..data.len().min(MAX_WRITE)];
        frame[..length].copy_from_slice(&address[..length]);
        frame[length..length + data.len()].copy_from_slice(data);
        self.device.write(&frame[..length + data.len()])
    }

    /// Writes one 8-bit register.
    pub fn write8(&mut self, register: u16, value: u8) -> Result<(), B::Error> {
        self.write(register, &[value])
    }

    /// Writes a 16-bit value to two registers, high byte first.
    pub fn write16(&mut self, register: u16, value: u16) -> Result<(), B::Error> {
        self.write(register, &value.to_be_bytes())
    }

    /// Writes a table of `(register, value)` pairs in order, the form sensor
    /// vendors publish their mode settings in. On failure, says which entry.
    pub fn write_table(&mut self, table: &[(u16, u8)]) -> Result<(), (usize, B::Error)> {
        for (index, &(register, value)) in table.iter().enumerate() {
            self.write8(register, value)
                .map_err(|error| (index, error))?;
        }
        Ok(())
    }

    /// Sets bits `mask` of 8-bit `register` to `value`, leaving the rest.
    pub fn update8(&mut self, register: u16, mask: u8, value: u8) -> Result<(), B::Error> {
        let old = self.read8(register)?;
        self.write8(register, (old & !mask) | (value & mask))
    }
}
