// SPDX-License-Identifier: GPL-3.0-only
//! ARM PL011 UART (UART0), the kernel console.
//!
//! On the Raspberry Pi 3 the PL011 is the "real" UART; the mini UART (AUX) is
//! tied to the VideoCore clock and drifts. QEMU maps `-serial` index 0 to this
//! peripheral, so the same driver serves emulation and hardware.
//!
//! The interrupt-enable paths are unused: the console is polled today.
#![allow(dead_code)]

use core::fmt;

use super::mmio::{ReadOnly, ReadWrite, RegisterBlock, WriteOnly};
use crate::bsp::memory::map;

mod fr {
    /// Transmitter busy: set while a character is still being shifted out.
    pub const BUSY: u32 = 1 << 3;
    /// Receive FIFO empty.
    pub const RXFE: u32 = 1 << 4;
    /// Transmit FIFO full.
    pub const TXFF: u32 = 1 << 5;
}

mod lcr_h {
    /// Enable the transmit and receive FIFOs.
    pub const FEN: u32 = 1 << 4;
    /// Word length 8 bits.
    pub const WLEN_8: u32 = 0b11 << 5;
}

mod cr {
    /// UART enable.
    pub const UARTEN: u32 = 1 << 0;
    /// Transmit enable.
    pub const TXE: u32 = 1 << 8;
    /// Receive enable.
    pub const RXE: u32 = 1 << 9;
}

mod imsc {
    /// Receive interrupt mask.
    pub const RXIM: u32 = 1 << 4;
    /// Receive timeout interrupt mask.
    pub const RTIM: u32 = 1 << 6;
}

/// Value that clears every interrupt in `ICR`.
const ICR_ALL: u32 = 0x7FF;

#[repr(C)]
struct Registers {
    dr: ReadWrite<u32>,      // 0x00 data
    rsr_ecr: ReadWrite<u32>, // 0x04 receive status / error clear
    _res0: [u32; 4],         // 0x08
    fr: ReadOnly<u32>,       // 0x18 flag
    _res1: u32,              // 0x1C
    ilpr: ReadWrite<u32>,    // 0x20 IrDA low power
    ibrd: WriteOnly<u32>,    // 0x24 integer baud rate divisor
    fbrd: WriteOnly<u32>,    // 0x28 fractional baud rate divisor
    lcr_h: WriteOnly<u32>,   // 0x2C line control
    cr: WriteOnly<u32>,      // 0x30 control
    ifls: ReadWrite<u32>,    // 0x34 interrupt FIFO level select
    imsc: ReadWrite<u32>,    // 0x38 interrupt mask set/clear
    ris: ReadOnly<u32>,      // 0x3C raw interrupt status
    mis: ReadOnly<u32>,      // 0x40 masked interrupt status
    icr: WriteOnly<u32>,     // 0x44 interrupt clear
    dmacr: ReadWrite<u32>,   // 0x48 DMA control
}

const _: () = assert!(core::mem::offset_of!(Registers, fr) == 0x18);
const _: () = assert!(core::mem::offset_of!(Registers, ibrd) == 0x24);
const _: () = assert!(core::mem::offset_of!(Registers, cr) == 0x30);
const _: () = assert!(core::mem::offset_of!(Registers, icr) == 0x44);

/// The PL011 baud rate divisors for a given reference clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BaudDivisors {
    /// Integer part, written to `IBRD`.
    pub integer: u32,
    /// Fractional part in 1/64ths, written to `FBRD`.
    pub fraction: u32,
}

impl BaudDivisors {
    /// Computes `IBRD`/`FBRD` for `baud` from a `clock_hz` reference.
    ///
    /// `BRD = clock / (16 * baud)`, `FBRD = round(frac(BRD) * 64)`, computed in
    /// integer arithmetic so it works in `const` contexts.
    ///
    /// For the Pi 3 default of 48 MHz at 115200 baud this yields `IBRD = 26`,
    /// `FBRD = 3`.
    pub const fn new(clock_hz: u32, baud: u32) -> Self {
        // Work in 64ths of a divisor so the fraction falls out exactly.
        let scaled = (clock_hz as u64 * 4 + (baud as u64 / 2)) / baud as u64;
        Self {
            integer: (scaled / 64) as u32,
            fraction: (scaled % 64) as u32,
        }
    }
}

/// PL011 UART driver.
pub struct Pl011Uart {
    block: RegisterBlock<Registers>,
    chars_written: u64,
    chars_read: u64,
}

impl Pl011Uart {
    /// Creates the driver for UART0 at [`map::PL011_UART`].
    ///
    /// # Safety
    ///
    /// Only one instance may exist. It must be behind a lock before secondary
    /// cores are released.
    pub const unsafe fn new() -> Self {
        Self {
            block: unsafe { RegisterBlock::new(map::PL011_UART) },
            chars_written: 0,
            chars_read: 0,
        }
    }

    /// Brings the UART up in 8N1 with FIFOs enabled.
    ///
    /// The caller must have routed GPIO 14/15 to ALT0 first (see
    /// [`super::gpio::Gpio::route_pl011`]); doing it the other way round
    /// glitches the line.
    ///
    /// `IBRD`, `FBRD` and `LCR_H` form one 30-bit register that is latched by
    /// the `LCR_H` write, so the write order below is mandatory.
    pub fn init(&mut self, clock_hz: u32, baud: u32) {
        let regs = self.block.regs();

        // Turn the UART off and let any in-flight character drain;
        // reconfiguring mid-frame corrupts it.
        regs.cr.write(0);
        while regs.fr.read() & fr::BUSY != 0 {
            core::hint::spin_loop();
        }

        // Clearing FEN flushes the FIFOs.
        regs.lcr_h.write(0);

        regs.icr.write(ICR_ALL);

        let divisors = BaudDivisors::new(clock_hz, baud);
        regs.ibrd.write(divisors.integer);
        regs.fbrd.write(divisors.fraction);
        regs.lcr_h.write(lcr_h::WLEN_8 | lcr_h::FEN);

        regs.cr.write(cr::UARTEN | cr::TXE | cr::RXE);
    }

    /// Enables receive and receive-timeout interrupts.
    ///
    /// Only useful once an interrupt controller is wired up; see
    /// `docs/07-exceptions-and-irq.md`.
    pub fn enable_rx_interrupts(&mut self) {
        self.block
            .regs()
            .imsc
            .modify(|value| value | imsc::RXIM | imsc::RTIM);
    }

    /// Acknowledges every pending interrupt source.
    pub fn clear_interrupts(&mut self) {
        self.block.regs().icr.write(ICR_ALL);
    }

    /// Writes one byte, blocking while the transmit FIFO is full.
    pub fn write_byte(&mut self, byte: u8) {
        let regs = self.block.regs();
        while regs.fr.read() & fr::TXFF != 0 {
            core::hint::spin_loop();
        }
        regs.dr.write(u32::from(byte));
        self.chars_written += 1;
    }

    /// Writes a string, expanding `\n` to `\r\n`.
    pub fn write_str_crlf(&mut self, s: &str) {
        for byte in s.bytes() {
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
    }

    /// Reads one byte if the receive FIFO is non-empty.
    pub fn read_byte(&mut self) -> Option<u8> {
        let regs = self.block.regs();
        if regs.fr.read() & fr::RXFE != 0 {
            return None;
        }
        self.chars_read += 1;
        Some(regs.dr.read() as u8)
    }

    /// Blocks until the transmitter has drained completely.
    pub fn flush(&mut self) {
        let regs = self.block.regs();
        while regs.fr.read() & fr::BUSY != 0 {
            core::hint::spin_loop();
        }
    }

    /// Number of bytes transmitted since construction.
    pub fn chars_written(&self) -> u64 {
        self.chars_written
    }

    /// Number of bytes received since construction.
    pub fn chars_read(&self) -> u64 {
        self.chars_read
    }
}

impl fmt::Write for Pl011Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_str_crlf(s);
        Ok(())
    }
}

/// Diagnostic output which never borrows the shared driver or its counters.
pub(crate) struct EmergencyWriter {
    failed: bool,
}

impl EmergencyWriter {
    pub(crate) fn new() -> Self {
        Self { failed: false }
    }

    fn wait_clear(&mut self, mask: u32) -> fmt::Result {
        if !self.failed {
            for _ in 0..1_000_000 {
                // SAFETY: FR is an aligned, read-only PL011 MMIO register,
                // mapped even before the MMU is enabled. No driver RAM is read.
                let flags = unsafe {
                    RegisterBlock::<Registers>::read_raw(
                        map::PL011_UART,
                        core::mem::offset_of!(Registers, fr),
                    )
                };
                if flags & mask == 0 {
                    return Ok(());
                }
                core::hint::spin_loop();
            }
        }
        self.failed = true;
        Err(fmt::Error)
    }

    fn write_byte(&mut self, byte: u8) -> fmt::Result {
        self.wait_clear(fr::TXFF)?;
        // SAFETY: DR is aligned, mapped external MMIO. An emergency write can
        // interleave with another transmitter, but cannot alias driver RAM.
        unsafe {
            RegisterBlock::<Registers>::write_raw(
                map::PL011_UART,
                core::mem::offset_of!(Registers, dr),
                u32::from(byte),
            )
        };
        Ok(())
    }

    pub(crate) fn flush(&mut self) -> fmt::Result {
        self.wait_clear(fr::BUSY)
    }
}

impl fmt::Write for EmergencyWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            if byte == b'\n' {
                self.write_byte(b'\r')?;
            }
            self.write_byte(byte)?;
        }
        Ok(())
    }
}

// Divisor cross-checks against the worked examples in the PL011 TRM and the
// values the Raspberry Pi firmware programs.
const _: () = {
    let d = BaudDivisors::new(48_000_000, 115_200);
    assert!(d.integer == 26 && d.fraction == 3);

    let d = BaudDivisors::new(48_000_000, 921_600);
    assert!(d.integer == 3 && d.fraction == 16);

    let d = BaudDivisors::new(3_000_000, 115_200);
    assert!(d.integer == 1 && d.fraction == 40);
};
