// SPDX-License-Identifier: GPL-3.0-only
//! BCM2835 BSC (Broadcom Serial Controller) I2C master.
//!
//! Two of the three BSC blocks are useful here: **BSC0** on GPIO 0/1 and
//! **BSC1** on GPIO 2/3, the header I2C bus on pins 3 and 5. BSC2 is wired to
//! the HDMI interface and is not exposed.
//!
//! Both are 7-bit-addressing masters with a 16-byte FIFO and a 16-bit transfer
//! length, driven by writing a length, an address and a direction and then
//! pushing bytes through the FIFO until `DONE`.
//!
//! # The clock-stretching bug
//!
//! The BCM2835 I2C master cannot handle a slave that stretches the clock at an
//! arbitrary point. It tolerates stretching only immediately after the ACK
//! phase of a read, and only if the stretch is at least half a clock period. A
//! slave that holds `SCL` down mid-byte — which the I2C specification permits —
//! has its data corrupted, because the master clocks the next bit regardless.
//!
//! This is silicon, not software; Linux marks the adapter `NO_CLK_STRETCH`. It
//! affects sensors and ADCs that stall while converting.
//!
//! A device that needs clock stretching should use [`crate::hal::bitbang::I2c`]
//! instead: slower and CPU-bound, but it watches `SCL` and waits.
//!
//! # Repeated START
//!
//! Reading a device register means writing a register number and then reading
//! without releasing the bus — a repeated START. This controller has no
//! repeated-START control; it emits a STOP whenever `DLEN` reaches zero.
//!
//! The workaround is a race: start the write without prefilling the FIFO, wait
//! for `S.TA` to confirm the START and address have gone out, push the write
//! bytes, then set `ST` again for the read before the write drains. With `TA`
//! still set the controller emits a repeated START rather than a STOP.
//!
//! [`I2c::write_read`] implements it and returns [`Error::RepeatedStartLost`]
//! if the write completes before the read is armed. It never silently
//! substitutes STOP/START. QEMU currently completes writes synchronously and
//! cannot model this chaining, so combined transfers fail there.
//!
//! The operation-list API coalesces adjacent same-direction buffers. BSC can
//! chain a write phase into a final read, but cannot preserve the contract for
//! a non-final read or an empty chained phase: those requests are rejected
//! before touching the bus. Use the bit-banged bus for arbitrary sequences.
#![allow(dead_code)]

use super::mmio::{ReadWrite, RegisterBlock};
use crate::{
    bsp::memory::map,
    drivers::{GPIO, gpio::Function},
    hal::shared::{I2cOperation, I2cOperations, I2cPhase, i2c_phase},
};

/// The VideoCore core clock, which the BSC divides down to make `SCL`.
///
/// 250 MHz. The datasheet's "nominally 150 MHz" is a known erratum; believing
/// it runs the bus 67% too fast.
pub const CORE_CLOCK_HZ: u32 = 250_000_000;

/// Standard-mode bit rate, 100 kHz.
pub const SPEED_STANDARD: u32 = 100_000;

/// Fast-mode bit rate, 400 kHz.
pub const SPEED_FAST: u32 = 400_000;

/// Something went wrong on the bus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// No device acknowledged the address, or a slave stopped acknowledging
    /// data part way through.
    Nack,
    /// A slave held `SCL` low for longer than `CLKT` allows.
    ///
    /// Only possible if the timeout is re-enabled; [`I2c::init`] disables it,
    /// since the controller's clock-stretch handling is broken anyway.
    ClockTimeout,
    /// The controller never reported the transfer finished.
    Timeout,
    /// The transfer was longer than the 16-bit `DLEN` field allows.
    TooLong,
    /// The BSC cannot chain this sequence without an intermediate STOP.
    ///
    /// Only a write phase followed by a final read phase is supported, and
    /// both chained phases must contain data. An entirely empty read phase is
    /// also rejected before bus activity.
    UnsupportedTransaction,
    /// The write ended before a repeated START could be confirmed.
    ///
    /// The write may already have reached the device. Do not blindly retry a
    /// non-idempotent transaction. QEMU does not model BSC repeated START.
    RepeatedStartLost,
}

// Control register bits.
const C_READ: u32 = 1 << 0;
const C_CLEAR: u32 = 0b11 << 4;
const C_ST: u32 = 1 << 7;
const C_INTD: u32 = 1 << 8;
const C_INTT: u32 = 1 << 9;
const C_INTR: u32 = 1 << 10;
const C_I2CEN: u32 = 1 << 15;

// Status register bits.
const S_TA: u32 = 1 << 0;
const S_DONE: u32 = 1 << 1;
const S_TXW: u32 = 1 << 2;
const S_RXR: u32 = 1 << 3;
const S_TXD: u32 = 1 << 4;
const S_RXD: u32 = 1 << 5;
const S_TXE: u32 = 1 << 6;
const S_RXF: u32 = 1 << 7;
const S_ERR: u32 = 1 << 8;
const S_CLKT: u32 = 1 << 9;

/// Everything that is write-1-to-clear in the status register.
const S_CLEAR_ALL: u32 = S_DONE | S_ERR | S_CLKT;

/// Iterations of the polling loop before a transfer is declared stuck.
const POLL_LIMIT: u32 = 20_000_000;

#[repr(C)]
struct Registers {
    c: ReadWrite<u32>,    // 0x00 control
    s: ReadWrite<u32>,    // 0x04 status
    dlen: ReadWrite<u32>, // 0x08 data length
    a: ReadWrite<u32>,    // 0x0C slave address
    fifo: ReadWrite<u32>, // 0x10 data FIFO
    div: ReadWrite<u32>,  // 0x14 clock divider
    del: ReadWrite<u32>,  // 0x18 data delay
    clkt: ReadWrite<u32>, // 0x1C clock stretch timeout
}

const _: () = assert!(core::mem::offset_of!(Registers, fifo) == 0x10);
const _: () = assert!(core::mem::offset_of!(Registers, clkt) == 0x1C);

/// One BSC master.
pub struct I2c {
    block: RegisterBlock<Registers>,
    sda: u8,
    scl: u8,
    initialised: bool,
}

fn phase_length(phase: &I2cPhase) -> Result<u16, Error> {
    phase
        .length
        .and_then(|length| u16::try_from(length).ok())
        .ok_or(Error::TooLong)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_registers(test: impl FnOnce(&mut I2c, &Registers)) {
        // SAFETY: every field is a ReadWrite<u32>, for which zero is valid.
        let registers: Registers = unsafe { core::mem::zeroed() };
        let mut bus = I2c {
            // SAFETY: these register cells outlive the unique driver used
            // during this call and are never moved while it is alive.
            block: unsafe { RegisterBlock::new(core::ptr::from_ref(&registers).addr()) },
            sda: 2,
            scl: 3,
            initialised: true,
        };
        test(&mut bus, &registers);
    }

    #[test]
    fn fifo_space_is_not_transfer_active() {
        with_registers(|bus, registers| {
            registers.s.write(S_TXD);
            assert_eq!(bus.wait_active(), Err(Error::Timeout));
            registers.s.write(S_TXD | S_TA);
            assert_eq!(bus.wait_active(), Ok(()));
            registers.s.write(S_TA | S_DONE);
            assert_eq!(bus.wait_active(), Err(Error::RepeatedStartLost));
        });
    }

    #[test]
    fn unsupported_shapes_are_rejected_before_start() {
        with_registers(|bus, registers| {
            let mut read = [0; 1];
            assert_eq!(
                bus.transaction(
                    0x50,
                    &mut [I2cOperation::Read(&mut read), I2cOperation::Write(&[0]),]
                ),
                Err(Error::UnsupportedTransaction)
            );
            assert_eq!(registers.c.read(), 0);
            assert_eq!(
                bus.write_read(0x50, &[], &mut read),
                Err(Error::UnsupportedTransaction)
            );
            assert_eq!(registers.c.read(), 0);
            let bytes = [0; 65535];
            assert_eq!(
                bus.transaction(
                    0x50,
                    &mut [I2cOperation::Write(&bytes), I2cOperation::Write(&[0]),]
                ),
                Err(Error::TooLong)
            );
            assert_eq!(registers.c.read(), 0);
            assert_eq!(bus.transaction(0x50, &mut []), Ok(()));
            assert_eq!(registers.c.read(), 0);
        });
    }

    #[test]
    fn abort_does_not_issue_another_start() {
        with_registers(|bus, registers| {
            registers.c.write(C_I2CEN | C_ST | C_READ);
            bus.abort();
            assert_eq!(registers.c.read(), C_CLEAR);
            assert_eq!(registers.c.read() & (C_I2CEN | C_ST), 0);
        });
    }
}

impl I2c {
    /// Creates the driver for BSC0, on GPIO 0 (`SDA`) and GPIO 1 (`SCL`).
    ///
    /// # Safety
    ///
    /// Only one instance may exist, and it must be behind a lock before
    /// secondary cores are released.
    pub const unsafe fn new_bsc0() -> Self {
        Self {
            block: unsafe { RegisterBlock::new(map::I2C0) },
            sda: 0,
            scl: 1,
            initialised: false,
        }
    }

    /// Creates the driver for BSC1, on GPIO 2 (`SDA`) and GPIO 3 (`SCL`) —
    /// the bus on the 40-pin header.
    ///
    /// # Safety
    ///
    /// As [`I2c::new_bsc0`].
    pub const unsafe fn new_bsc1() -> Self {
        Self {
            block: unsafe { RegisterBlock::new(map::I2C1) },
            sda: 2,
            scl: 3,
            initialised: false,
        }
    }

    /// The `SDA` and `SCL` pin numbers this instance uses.
    pub const fn pins(&self) -> (u8, u8) {
        (self.sda, self.scl)
    }

    /// Routes this block's pins and sets the bus to `speed_hz`.
    ///
    /// # Lock order
    ///
    /// Takes [`crate::drivers::GPIO`] while the caller holds the I2C lock.
    /// Nothing may take them the other way round.
    pub fn init(&mut self, speed_hz: u32) {
        {
            let mut gpio = GPIO.lock();
            gpio.set_function(self.sda, Function::Alt0);
            gpio.set_function(self.scl, Function::Alt0);
            // A real bus needs external pull-ups; the internal ones are too
            // weak to meet the rise-time spec, but enabling them helps a bench
            // setup with short jumpers.
            gpio.set_pull(&[self.sda, self.scl], crate::drivers::gpio::Pull::Up);
        }

        {
            let regs = self.block.regs();
            regs.c.write(0);
            regs.s.write(S_CLEAR_ALL);

            // Disable the clock-stretch timeout: the controller's stretch
            // handling is broken (see the module docs), so it only produces
            // spurious errors.
            regs.clkt.write(0);
        }

        self.set_speed_hz(speed_hz);

        self.block.regs().c.write(C_I2CEN);
        self.initialised = true;
    }

    /// Whether [`I2c::init`] has run.
    pub const fn is_initialised(&self) -> bool {
        self.initialised
    }

    /// Sets the bit rate as close to `speed_hz` as the divider allows, and
    /// returns what was actually programmed.
    ///
    /// `SCL = 250 MHz / DIV`, with `DIV` even and at least 2.
    pub fn set_speed_hz(&mut self, speed_hz: u32) -> u32 {
        let mut divider = if speed_hz == 0 {
            0xFFFE
        } else {
            CORE_CLOCK_HZ.div_ceil(speed_hz)
        };

        // The hardware ignores the low bit; round up so the bus is never faster
        // than asked for.
        divider += divider & 1;
        divider = divider.clamp(2, 0xFFFE);

        let regs = self.block.regs();
        regs.div.write(divider);

        // Sampling and driving delays, in core clocks either side of the SCL
        // edge: an eighth of a half-period before driving, half a half-period
        // before sampling.
        let falling = (divider / 16).max(1);
        let rising = (divider / 4).max(1);
        regs.del.write((falling << 16) | rising);

        CORE_CLOCK_HZ / divider
    }

    /// The bit rate currently programmed, in Hz.
    ///
    /// Zero if `DIV` has never been programmed; the datasheet assigns no
    /// meaning to a zero I2C divider.
    pub fn speed_hz(&self) -> u32 {
        let divider = self.block.regs().div.read() & 0xFFFF;
        CORE_CLOCK_HZ.checked_div(divider).unwrap_or(0)
    }

    /// Writes `data` to the device at seven-bit address `address`.
    pub fn write(&mut self, address: u8, data: &[u8]) -> Result<(), Error> {
        self.transaction(address, &mut [I2cOperation::Write(data)])
    }

    /// Reads `data.len()` bytes from the device at `address`.
    pub fn read(&mut self, address: u8, data: &mut [u8]) -> Result<(), Error> {
        self.transaction(address, &mut [I2cOperation::Read(data)])
    }

    /// Writes `write`, then reads `read`, without an intermediate STOP.
    ///
    /// The "read register N" shape most sensors use. See the module docs for
    /// how the repeated START is produced and the race it depends on.
    pub fn write_read(&mut self, address: u8, write: &[u8], read: &mut [u8]) -> Result<(), Error> {
        self.transaction(
            address,
            &mut [I2cOperation::Write(write), I2cOperation::Read(read)],
        )
    }

    /// Executes a complete operation list without allocating.
    ///
    /// Adjacent same-direction buffers are one phase, with one address and
    /// one total `DLEN` (at most 65535 bytes). A write may precede a final read.
    /// Other direction sequences and empty chained phases are rejected before
    /// START; arbitrary sequences are supported by [`crate::hal::bitbang::I2c`].
    /// A read phase must contain at least one byte.
    pub fn transaction(
        &mut self,
        address: u8,
        operations: &mut (impl I2cOperations + ?Sized),
    ) -> Result<(), Error> {
        if operations.is_empty() {
            return Ok(());
        }
        let first = i2c_phase(operations, 0);
        let first_length = phase_length(&first)?;
        if first.read && first_length == 0 {
            return Err(Error::UnsupportedTransaction);
        }
        let second = if first.end < operations.len() {
            let second = i2c_phase(operations, first.end);
            let second_length = phase_length(&second)?;
            if first.read
                || second.end != operations.len()
                || first_length == 0
                || second_length == 0
            {
                return Err(Error::UnsupportedTransaction);
            }
            Some((second, second_length))
        } else {
            None
        };

        self.start(address, first_length, first.read);
        let result = (|| {
            if second.is_some() {
                // TXD only means FIFO space, not that START/address completed.
                // Broadcom's combined-transfer sequence requires TA first.
                self.wait_active()?;
            }
            self.transfer_phase(operations, 0, &first)?;
            if let Some((second, length)) = second {
                let regs = self.block.regs();
                let status = regs.s.read();
                self.check(status)?;
                if status & (S_TA | S_DONE) != S_TA {
                    return Err(Error::RepeatedStartLost);
                }
                regs.dlen.write(u32::from(length));
                regs.c.write(C_I2CEN | C_ST | C_READ);

                // DONE is sticky. Do not clear it between phases: it catches
                // a STOP in the window between the TA check and the ST write.
                // An unusually fast completed read is conservatively rejected.
                let status = regs.s.read();
                self.check(status)?;
                if status & S_DONE != 0 {
                    return Err(Error::RepeatedStartLost);
                }
                self.transfer_phase(operations, first.end, &second)?;
            }
            self.wait_done()?;
            self.finish()
        })();
        if result.is_err() {
            self.abort();
        }
        result
    }

    fn transfer_phase(
        &self,
        operations: &mut (impl I2cOperations + ?Sized),
        start: usize,
        phase: &I2cPhase,
    ) -> Result<(), Error> {
        for index in start..phase.end {
            match operations.operation(index) {
                I2cOperation::Write(bytes) => {
                    for &byte in bytes {
                        self.wait_fifo(S_TXD)?;
                        self.block.regs().fifo.write(u32::from(byte));
                    }
                }
                I2cOperation::Read(bytes) => {
                    for byte in bytes {
                        self.wait_fifo(S_RXD)?;
                        *byte = self.block.regs().fifo.read() as u8;
                    }
                }
            }
        }
        Ok(())
    }

    fn wait_active(&self) -> Result<(), Error> {
        for _ in 0..POLL_LIMIT {
            let status = self.block.regs().s.read();
            self.check(status)?;
            if status & S_DONE != 0 {
                return Err(Error::RepeatedStartLost);
            }
            if status & S_TA != 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(Error::Timeout)
    }

    fn wait_fifo(&self, ready: u32) -> Result<(), Error> {
        for _ in 0..POLL_LIMIT {
            let status = self.block.regs().s.read();
            self.check(status)?;
            // Reads may still have bytes in the FIFO after DONE.
            if status & ready != 0 && (ready == S_RXD || status & S_DONE == 0) {
                return Ok(());
            }
            if status & S_DONE != 0 {
                return Err(Error::Timeout);
            }
            core::hint::spin_loop();
        }
        Err(Error::Timeout)
    }

    fn wait_done(&self) -> Result<(), Error> {
        for _ in 0..POLL_LIMIT {
            let status = self.block.regs().s.read();
            self.check(status)?;
            if status & S_DONE != 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(Error::Timeout)
    }

    fn abort(&mut self) {
        let regs = self.block.regs();
        // Disable before clearing, without I2CEN/ST: this must not start a
        // new frame to the old address (including under QEMU).
        regs.c.write(0);
        regs.c.write(C_CLEAR);
        regs.s.write(S_CLEAR_ALL);
    }

    /// Reports whether anything answers at `address`.
    ///
    /// Performs a one-byte read and watches for an address NACK. Caveat: on a
    /// device with an auto-incrementing read pointer, probing consumes a byte.
    pub fn probe(&mut self, address: u8) -> bool {
        let mut byte = [0u8; 1];
        self.read(address, &mut byte).is_ok()
    }

    /// Scans the bus and calls `found` for each address that answers.
    ///
    /// Only the 0x08-0x77 range is scanned; the rest is reserved by the I2C
    /// specification for things like ten-bit addressing and general call.
    pub fn scan(&mut self, mut found: impl FnMut(u8)) {
        for address in 0x08..=0x77 {
            if self.probe(address) {
                found(address);
            }
        }
    }

    /// Programs a transfer and sets it going.
    ///
    /// The FIFO clear is folded into the same `C` write as `ST`, which the
    /// datasheet sanctions: "If CLEAR and ST are both set in the same
    /// operation, the FIFO is cleared before the new frame is started." This
    /// also matters under emulation: QEMU's `bcm2835_i2c` begins a transfer on
    /// any `C` write with `I2CEN` set, so a separate clear-only write opens a
    /// stray transfer to the previous address that is never ended. Its I2C core
    /// only appends to `current_devs`, so the next address answers too and a
    /// bus scan reports every device twice.
    fn start(&mut self, address: u8, length: u16, read: bool) {
        let regs = self.block.regs();

        regs.s.write(S_CLEAR_ALL);
        regs.a.write(u32::from(address & 0x7F));
        regs.dlen.write(u32::from(length));

        let direction = if read { C_READ } else { 0 };
        regs.c.write(C_I2CEN | C_CLEAR | C_ST | direction);
    }

    /// Turns the sticky error bits into an [`Error`].
    fn check(&self, status: u32) -> Result<(), Error> {
        if status & S_ERR != 0 {
            return Err(Error::Nack);
        }
        if status & S_CLKT != 0 {
            return Err(Error::ClockTimeout);
        }

        Ok(())
    }

    /// Waits for the controller to go idle and clears the status bits.
    fn finish(&mut self) -> Result<(), Error> {
        let regs = self.block.regs();
        let mut spins = 0u32;

        while regs.s.read() & S_TA != 0 {
            spins += 1;
            if spins > POLL_LIMIT {
                return Err(Error::Timeout);
            }
            core::hint::spin_loop();
        }

        let status = regs.s.read();
        regs.s.write(S_CLEAR_ALL);

        if status & S_ERR != 0 {
            Err(Error::Nack)
        } else if status & S_CLKT != 0 {
            Err(Error::ClockTimeout)
        } else {
            Ok(())
        }
    }
}
