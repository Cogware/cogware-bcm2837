// SPDX-License-Identifier: MIT OR Apache-2.0
//! BCM2837 external mass media controller (EMMC / Arasan SDHCI).
//!
//! Drives the Arasan-compatible SDHCI controller wired to the Raspberry Pi 3
//! micro-SD slot: brings the card up from reset, negotiates bus width and
//! clock rate, and services 512-byte block reads/writes through the
//! controller's programmed-I/O FIFO (`EMMC_DATA`). DMA is not implemented.
//!
//! The command sequence, register writes, interrupt-wait discipline and CSD /
//! CID decoding are adapted from `SDCard.c` by Leon de Boer (LdB) and treated
//! as the specification. State lives on the [`Emmc`] struct, which the driver
//! layer wraps in a [`crate::sync::SpinLock`].
//!
//! # Locking
//!
//! Rule 5.5 forbids holding a lock across a spin-wait; this driver is the
//! documented exception, since an SD/MMC transaction is itself a bounded poll
//! of the controller. Every wait has an explicit microsecond deadline (see
//! `wait_until`) so a wedged card cannot hold the lock forever.
//!
//! # GPIO
//!
//! Firmware routes the SD interface to GPIO 48-53 before the ARM cores start,
//! so this driver never touches the GPIO block; doing so would also require
//! the GPIO lock and risk a lock-ordering hazard.
//!
//! # QEMU
//!
//! QEMU's `raspi3b` SDHCI model answers CMD0..CMD9 against a raw image passed
//! with `-drive`, so card identification works under emulation. Timing-
//! sensitive paths (ACMD41 busy polling, clock stabilisation) differ from real
//! silicon and must be tested on a Pi 3.
//!
//! # Warnings and dead code
//!
//! Per instructions.md rule 2.3, this module carries a top-level
//! `#![allow(dead_code)]`: the command table, register bit constants and
//! [`Error`] enum are intentionally complete descriptions of the hardware and
//! its failure modes.
#![allow(dead_code)]

use core::fmt;

use super::mmio::{ReadWrite, RegisterBlock};
use crate::{bsp::memory::map, time};

/// SD block size in bytes. Fixed at 512; CMD16 (`SET_BLOCKLEN`) programs 512
/// during initialisation and SDHC/SDXC cards address by block.
pub const BLOCK_SIZE: usize = 512;

// ---------------------------------------------------------------------------
// Register block
// ---------------------------------------------------------------------------

#[repr(C)]
struct Registers {
    arg2: ReadWrite<u32>,        // 0x00
    blksizecnt: ReadWrite<u32>,  // 0x04
    arg1: ReadWrite<u32>,        // 0x08
    cmdtm: ReadWrite<u32>,       // 0x0c
    resp0: ReadWrite<u32>,       // 0x10
    resp1: ReadWrite<u32>,       // 0x14
    resp2: ReadWrite<u32>,       // 0x18
    resp3: ReadWrite<u32>,       // 0x1c
    data: ReadWrite<u32>,        // 0x20
    status: ReadWrite<u32>,      // 0x24
    control0: ReadWrite<u32>,    // 0x28
    control1: ReadWrite<u32>,    // 0x2c
    interrupt: ReadWrite<u32>,   // 0x30
    irpt_mask: ReadWrite<u32>,   // 0x34
    irpt_en: ReadWrite<u32>,     // 0x38
    control2: ReadWrite<u32>,    // 0x3c
    _res0: [u32; 18],            // 0x40..0x88
    tune_step: ReadWrite<u32>,   // 0x88
    _res1: [u32; 28],            // 0x8c..0xfc
    slotisr_ver: ReadWrite<u32>, // 0xfc
}

const _: () = assert!(core::mem::offset_of!(Registers, cmdtm) == 0x0c);
const _: () = assert!(core::mem::offset_of!(Registers, data) == 0x20);
const _: () = assert!(core::mem::offset_of!(Registers, control0) == 0x28);
const _: () = assert!(core::mem::offset_of!(Registers, interrupt) == 0x30);
const _: () = assert!(core::mem::offset_of!(Registers, tune_step) == 0x88);
const _: () = assert!(core::mem::offset_of!(Registers, slotisr_ver) == 0xfc);

// ---------------------------------------------------------------------------
// Bit constants for the controller registers.
//
// Named after the reference (LdB's `register_bitfields!` blocks and its
// `rpi4_constants` module). All private.
// ---------------------------------------------------------------------------

mod cmdtm_bits {
    pub const TM_BLKCNT_EN: u32 = 1 << 1;
    pub const TM_DAT_DIR: u32 = 1 << 4;
    pub const TM_MULTI_BLOCK: u32 = 1 << 5;
    pub const RSP_NONE: u32 = 0 << 16;
    pub const RSP_136: u32 = 1 << 16;
    pub const RSP_48: u32 = 2 << 16;
    pub const RSP_BUSY48: u32 = 3 << 16;
    pub const RSP_MASK: u32 = 3 << 16;
    pub const CMD_ISDATA: u32 = 1 << 21;

    pub const fn cmd(index: u32) -> u32 {
        index << 24
    }
    pub const fn rsp_type(code: u32) -> u32 {
        (code >> 16) & 0x3
    }
    pub const fn index_of(code: u32) -> u32 {
        (code >> 24) & 0x3F
    }
}

mod status_bits {
    pub const CMD_INHIBIT: u32 = 1 << 0;
    pub const DAT_INHIBIT: u32 = 1 << 1;
    pub const READ_TRANSFER: u32 = 1 << 9;
}

mod control0_bits {
    pub const HCTL_DWIDTH: u32 = 1 << 1;
    pub const BUSPOWER: u32 = 1 << 8;
    pub const BUSVOLTAGE_V3_3: u32 = 0b111 << 9;
}

mod control1_bits {
    pub const CLK_INTLEN: u32 = 1 << 0;
    pub const CLK_STABLE: u32 = 1 << 1;
    pub const CLK_EN: u32 = 1 << 2;
    pub const SRST_HC: u32 = 1 << 24;
    pub const SRST_DATA: u32 = 1 << 26;
    // Data-timeout unit exponent; 14 matches the reference's `DTO = 14`.
    pub const DATA_TOUNIT_MAX: u32 = 14 << 16;

    pub const fn clock_divider(div: u32) -> u32 {
        // 10-bit divider spread across two disjoint fields: low 8 bits in
        // `CLK_FREQ8` (bits 8-15), high 2 bits in `CLK_FREQ_MS2` (bits 6-7).
        let lo = (div & 0xFF) << 8;
        let hi = ((div >> 8) & 0x3) << 6;
        lo | hi
    }
}

mod int_bits {
    pub const CMD_DONE: u32 = 1 << 0;
    pub const DATA_DONE: u32 = 1 << 1;
    pub const WRITE_RDY: u32 = 1 << 4;
    pub const READ_RDY: u32 = 1 << 5;
    pub const CARD_INT: u32 = 1 << 8;
    pub const CTO_ERR: u32 = 1 << 16;
    pub const CCRC_ERR: u32 = 1 << 17;
    pub const CEND_ERR: u32 = 1 << 18;
    pub const CBAD_ERR: u32 = 1 << 19;
    pub const DCRC_ERR: u32 = 1 << 21;
    pub const DEND_ERR: u32 = 1 << 22;
    pub const ACMD_ERR: u32 = 1 << 24;

    // Excludes `DTO_ERR` and the generic `ERR` bit, matching the reference so
    // command-timeout handling stays identical.
    pub const ERROR_MASK: u32 = CCRC_ERR | CEND_ERR | CBAD_ERR | DCRC_ERR | DEND_ERR | ACMD_ERR;
}

// R1 status bits carrying error information from the card. Anything set in
// this mask means the transfer must be aborted.
const R1_ERRORS_MASK: u32 = 0xFFF9_C004;

// Status bit set by the card when it has just accepted an APP_CMD (CMD55).
const ST_APP_CMD: u32 = 0x0000_0020;

// What the programmable divider divides, in Hz.
//
// Not the rate the mailbox reports for the EMMC clock, and the difference has
// cost enough to be worth writing down. On this board `GET_CLOCK_RATE` for
// EMMC answers 200 MHz, which is the clock delivered to the block; the divider
// sits behind a fixed prescale and divides this instead. Feeding it the
// mailbox figure makes every divider 4.8 times too large -- card
// initialisation, specified at 400 kHz, lands at 83 kHz, and the ACMD41
// power-up poll stalls the controller for longer than the driver waits.
//
// Measured, not assumed: 41.67 MHz is the rate at which this controller
// produces the frequencies it is asked for on this hardware. The mailbox rate
// is reported next to it at boot so a board where that stops being true is
// visible rather than mysterious.
const BASE_CLOCK: u32 = 41_666_666;

// The divider field is ten bits wide.
const MAX_DIVIDER: u32 = 1023;
// SD initialisation runs at ≤ 400 kHz per the SD Physical Layer Specification.
const FREQ_SETUP: u32 = 400_000;
// Full-speed clock once the card is in the transfer state.
const FREQ_NORMAL: u32 = 20_000_000;

// ACMD41 argument bits. See SD Physical Layer Specification §4.2.
const ACMD41_HCS: u32 = 0x4000_0000;
const ACMD41_SDXC_POWER: u32 = 0x1000_0000;
const ACMD41_VOLTAGE: u32 = 0x00FF_8000;
const ACMD41_ARG_HC: u32 = ACMD41_HCS | ACMD41_SDXC_POWER | ACMD41_VOLTAGE;
const ACMD41_ARG_SC: u32 = ACMD41_VOLTAGE;

// OCR bits used during initialisation.
const OCR_POWER_UP_BUSY: u32 = 1 << 31;
const OCR_CARD_CAPACITY: u32 = 1 << 30;
const OCR_VOLTAGE_3V2_3V3: u32 = 1 << 20;
const OCR_VOLTAGE_3V3_3V4: u32 = 1 << 21;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why an EMMC operation failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// A bounded wait for a controller state change ran out of time.
    Timeout,
    /// The command-timeout bit (`CTO_ERR`) was raised by the controller.
    CommandTimeout,
    /// The controller reported `CMD_INHIBIT` or `DAT_INHIBIT` for too long.
    /// The controller never came free. Carries the interrupt register as it
    /// stood: zero means nothing was flagged and the wait simply expired,
    /// which says the bus is too slow or the card is not answering. Bits set
    /// mean the opposite -- the card answered and the answer was mangled,
    /// which is what being clocked too fast looks like.
    Busy(u32),
    /// The card did not respond, or produced a response the driver could not
    /// interpret.
    NoResponse,
    /// The controller failed to clear `SRST_HC` inside its reset deadline.
    ResetFailed,
    /// The controller failed to report a stable SD clock after reprogramming.
    ClockFailed,
    /// ACMD41 returned an OCR that does not include a 3.3 V window. The Pi 3
    /// is 3.3 V only, so this card cannot be used.
    UnsupportedVoltage,
    /// The card refused an APP_CMD (CMD55), or answered a following ACMD
    /// without the `APP_CMD` status bit set.
    AppCommandFailed,
    /// [`Emmc::init`] found no usable card in the slot.
    NoCard,
    /// The card reported an error in its R1 status; the wrapped value is the
    /// masked R1 word.
    CardStatus(u32),
    /// The card responded but reported itself as unusable — CMD8 check
    /// pattern mismatched, ACMD41 never left the busy state, or CSD decoding
    /// produced a capacity of zero.
    Unusable,
    /// A read or write was issued with an invalid buffer: empty, or not a
    /// whole number of 512-byte blocks.
    InvalidBuffer,
    /// A read or write started at a block that does not exist on the card.
    OutOfRange,
    /// A method that only makes sense on an initialised driver was called
    /// before [`Emmc::init`] succeeded.
    NotInitialised,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Timeout => "emmc timed out",
            Self::CommandTimeout => "command timeout",
            Self::Busy(_) => "controller busy",
            Self::NoResponse => "no response from card",
            Self::ResetFailed => "controller reset failed",
            Self::ClockFailed => "clock did not stabilise",
            Self::UnsupportedVoltage => "card does not support 3.3 v",
            Self::AppCommandFailed => "app command not accepted",
            Self::NoCard => "no sd card present",
            Self::CardStatus(_) => "card reported error status",
            Self::Unusable => "card is unusable",
            Self::InvalidBuffer => "buffer is not a whole number of blocks",
            Self::OutOfRange => "block address out of range",
            Self::NotInitialised => "driver has not been initialised",
        };
        f.write_str(text)?;
        // The status word is the whole content of this error. Folding it away
        // leaves "something went wrong" and a reboot to find out, which is a
        // long way to travel for a number the card already handed over.
        if let Self::Busy(bits) = self {
            if *bits == 0 {
                // Nothing flagged: the wait ran out. The card never answered,
                // which points at a bus too slow or a card not there rather
                // than at a signal the controller could not read.
                f.write_str(" (timed out, no error flagged)")?;
            } else {
                write!(f, " irpt 0x{bits:08x}")?;
                let mut first = true;
                for (mask, name) in IRPT_ERROR_NAMES {
                    if bits & mask != 0 {
                        f.write_str(if first { " (" } else { ", " })?;
                        f.write_str(name)?;
                        first = false;
                    }
                }
                if !first {
                    f.write_str(")")?;
                }
            }
        }
        if let Self::CardStatus(bits) = self {
            write!(f, " 0x{bits:08x}")?;
            let mut first = true;
            for (mask, name) in R1_ERROR_NAMES {
                if bits & mask != 0 {
                    f.write_str(if first { " (" } else { ", " })?;
                    f.write_str(name)?;
                    first = false;
                }
            }
            if !first {
                f.write_str(")")?;
            }
        }
        Ok(())
    }
}

/// A snapshot of the controller, for when it will not answer.
#[derive(Clone, Copy, Debug)]
pub struct Diagnostics {
    /// `STATUS`: the inhibit and card-presence bits.
    pub status: u32,
    /// `CONTROL0`: bus power and width.
    pub control0: u32,
    /// `CONTROL1`: the clock divider and enables actually in force.
    pub control1: u32,
    /// `INTERRUPT`: anything the controller has flagged.
    pub interrupt: u32,
    /// `SLOTISR_VER`: host controller version and vendor.
    pub slotisr_ver: u32,
    /// The rate the driver believes it is dividing.
    pub base_clock: u32,
    /// The last command it tried to send.
    pub last_command: &'static str,
}

impl fmt::Display for Diagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "status 0x{:08x}{}{}{} control1 0x{:08x} (div {}{}{}{}) irpt 0x{:08x} ver 0x{:08x} base {} Hz last {}",
            self.status,
            if self.status & (1 << 0) != 0 {
                " CMD_INHIBIT"
            } else {
                ""
            },
            if self.status & (1 << 1) != 0 {
                " DAT_INHIBIT"
            } else {
                ""
            },
            if self.status & (1 << 16) != 0 {
                " CARD_INSERTED"
            } else {
                " NO_CARD"
            },
            self.control1,
            // The divider as the controller holds it: low eight bits at 15..8,
            // high two at 7..6. Printed decoded because reading it back out of
            // the word by hand is where a wrong clock hides.
            ((self.control1 >> 8) & 0xFF) | (((self.control1 >> 6) & 0x3) << 8),
            if self.control1 & (1 << 0) != 0 {
                " INTLEN"
            } else {
                ""
            },
            if self.control1 & (1 << 1) != 0 {
                " STABLE"
            } else {
                ""
            },
            if self.control1 & (1 << 2) != 0 {
                " CLK_EN"
            } else {
                ""
            },
            self.interrupt,
            self.slotisr_ver,
            self.base_clock,
            if self.last_command.is_empty() {
                "(none)"
            } else {
                self.last_command
            }
        )
    }
}

/// The controller's own error interrupts, by name.
///
/// The distinction that matters when a bus will not work: a CRC or end-bit
/// error means the card replied and the reply could not be read, which is what
/// clocking it too fast looks like. A timeout with none of these set means it
/// never replied at all.
const IRPT_ERROR_NAMES: &[(u32, &str)] = &[
    (1 << 16, "CTO_ERR/command timeout"),
    (1 << 17, "CCRC_ERR/command crc"),
    (1 << 18, "CEND_ERR/command end bit"),
    (1 << 19, "CBAD_ERR/command index"),
    (1 << 20, "DTO_ERR/data timeout"),
    (1 << 21, "DCRC_ERR/data crc"),
    (1 << 22, "DEND_ERR/data end bit"),
    (1 << 24, "ACMD_ERR/auto command"),
];

/// The R1 error bits, by the names the SD Physical Layer Specification uses.
///
/// A card that refuses a command says why in this word, and the name is the
/// difference between a diagnosis and another boot. `COM_CRC_ERROR` in
/// particular means the bus is being clocked outside what the card can follow,
/// which is a driver problem and not a card one.
const R1_ERROR_NAMES: &[(u32, &str)] = &[
    (1 << 31, "OUT_OF_RANGE"),
    (1 << 30, "ADDRESS_ERROR"),
    (1 << 29, "BLOCK_LEN_ERROR"),
    (1 << 28, "ERASE_SEQ_ERROR"),
    (1 << 27, "ERASE_PARAM"),
    (1 << 26, "WP_VIOLATION"),
    (1 << 25, "CARD_IS_LOCKED"),
    (1 << 24, "LOCK_UNLOCK_FAILED"),
    (1 << 23, "COM_CRC_ERROR"),
    (1 << 22, "ILLEGAL_COMMAND"),
    (1 << 21, "CARD_ECC_FAILED"),
    (1 << 20, "CC_ERROR"),
    (1 << 19, "ERROR"),
    (1 << 16, "CSD_OVERWRITE"),
    (1 << 15, "WP_ERASE_SKIP"),
    (1 << 3, "AKE_SEQ_ERROR"),
];

// ---------------------------------------------------------------------------
// Card type / info
// ---------------------------------------------------------------------------

/// SD card family, as inferred during initialisation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CardType {
    /// Not yet detected, or CMD0 / CMD8 / ACMD41 did not resolve.
    Unknown,
    /// Legacy MMC card. Not currently driven.
    Mmc,
    /// SD version 1.x card, standard capacity.
    V1,
    /// SD version 2.x standard-capacity card (byte-addressed).
    V2Sc,
    /// SD version 2.x high- or extended-capacity card (block-addressed).
    V2Hc,
}

impl CardType {
    /// Human-readable label used by the shell's `sd` command.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Mmc => "mmc",
            Self::V1 => "sd v1",
            Self::V2Sc => "sd v2 sc",
            Self::V2Hc => "sd v2 hc",
        }
    }
}

/// Descriptive information about the currently-initialised card.
///
/// Populated from the CID, CSD, OCR and SCR responses during [`Emmc::init`].
/// Held by value so it can be handed out as plain data with no `&self` borrow
/// retained.
#[derive(Clone, Copy)]
pub struct CardInfo {
    /// Card family (SD v1, SD v2 SC, SD v2 HC, MMC, unknown).
    pub card_type: CardType,
    /// Card capacity in bytes.
    pub capacity_bytes: u64,
    /// Relative card address, as returned by CMD3. High 16 bits carry the RCA
    /// itself; the low bits are the R6 status flags.
    pub rca: u32,
    /// Operating conditions register, from ACMD41.
    pub ocr: u32,
    /// CSD structure version: 1 for CSD 1.0 (SC), 2 for CSD 2.0 (HC/XC).
    pub csd_version: u8,
    /// Negotiated bus width in data lines (1 or 4).
    pub bus_width: u8,
    /// Manufacturer ID from the CID.
    pub manufacturer_id: u8,
    /// Two-byte OEM/application ID (ASCII).
    pub oem_id: [u8; 2],
    /// Five-byte product name (ASCII).
    pub product_name: [u8; 5],
    /// Product revision, major digit (BCD).
    pub product_revision_major: u8,
    /// Product revision, minor digit (BCD).
    pub product_revision_minor: u8,
    /// Manufacturing month, 1..=12.
    pub manufacture_month: u8,
    /// Manufacturing year, 4-digit AD.
    pub manufacture_year: u16,
    /// Card serial number.
    pub serial: u32,
}

// ---------------------------------------------------------------------------
// SD command table
// ---------------------------------------------------------------------------

/// A single row of the command table.
#[derive(Clone, Copy)]
struct Cmd {
    name: &'static str,
    code: u32,
    use_rca: bool,
    delay_us: u32,
}

impl Cmd {
    const fn new(name: &'static str, code: u32, use_rca: bool, delay_us: u32) -> Self {
        Self {
            name,
            code,
            use_rca,
            delay_us,
        }
    }
}

// Anything from `AppCmdStart` onwards is issued as an ACMD (CMD55 first).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SdCommand {
    GoIdleState,
    AllSendCid,
    SendRelAddr,
    CardSelect,
    SendIfCond,
    SendCsd,
    StopTrans,
    SendStatus,
    SetBlocklen,
    ReadSingle,
    ReadMulti,
    WriteSingle,
    WriteMulti,
    AppCmd,
    AppCmdRca,
    // Sentinel: any variant with a higher discriminant needs an APP_CMD first.
    AppCmdStart,
    SetBusWidth,
    AppSendOpCond,
    SendScr,
}

impl SdCommand {
    fn is_app_command(&self) -> bool {
        *self > Self::AppCmdStart
    }

    fn descriptor(&self) -> Cmd {
        use cmdtm_bits::*;
        match self {
            Self::GoIdleState => Cmd::new("GO_IDLE_STATE", cmd(0x00) | RSP_NONE, false, 0),
            Self::AllSendCid => Cmd::new("ALL_SEND_CID", cmd(0x02) | RSP_136, false, 0),
            Self::SendRelAddr => Cmd::new("SEND_REL_ADDR", cmd(0x03) | RSP_48, false, 0),
            Self::CardSelect => Cmd::new("CARD_SELECT", cmd(0x07) | RSP_BUSY48, true, 0),
            Self::SendIfCond => Cmd::new("SEND_IF_COND", cmd(0x08) | RSP_48, false, 100),
            Self::SendCsd => Cmd::new("SEND_CSD", cmd(0x09) | RSP_136, true, 0),
            Self::StopTrans => Cmd::new("STOP_TRANS", cmd(0x0c) | RSP_BUSY48, false, 0),
            Self::SendStatus => Cmd::new("SEND_STATUS", cmd(0x0d) | RSP_48, true, 0),
            Self::SetBlocklen => Cmd::new("SET_BLOCKLEN", cmd(0x10) | RSP_NONE, false, 0),
            Self::ReadSingle => Cmd::new(
                "READ_SINGLE",
                cmd(0x11) | RSP_48 | CMD_ISDATA | TM_DAT_DIR,
                false,
                0,
            ),
            Self::ReadMulti => Cmd::new(
                "READ_MULTI",
                cmd(0x12) | RSP_48 | CMD_ISDATA | TM_DAT_DIR | TM_BLKCNT_EN | TM_MULTI_BLOCK,
                false,
                0,
            ),
            Self::WriteSingle => {
                Cmd::new("WRITE_SINGLE", cmd(0x18) | RSP_48 | CMD_ISDATA, false, 0)
            }
            Self::WriteMulti => Cmd::new(
                "WRITE_MULTI",
                cmd(0x19) | RSP_48 | CMD_ISDATA | TM_BLKCNT_EN | TM_MULTI_BLOCK,
                false,
                0,
            ),
            Self::AppCmd => Cmd::new("APP_CMD", cmd(0x37) | RSP_NONE, false, 100),
            Self::AppCmdRca => Cmd::new("APP_CMD_RCA", cmd(0x37) | RSP_48, true, 0),
            Self::AppCmdStart => Cmd::new("APP_CMD_START", 0, false, 0),
            Self::SetBusWidth => Cmd::new("SET_BUS_WIDTH", cmd(0x06) | RSP_48, false, 0),
            Self::AppSendOpCond => Cmd::new("APP_SEND_OP_COND", cmd(0x29) | RSP_48, false, 1000),
            Self::SendScr => Cmd::new(
                "SEND_SCR",
                cmd(0x33) | RSP_48 | CMD_ISDATA | TM_DAT_DIR,
                false,
                0,
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Timing helper
// ---------------------------------------------------------------------------

/// Waits up to `deadline_us` microseconds for `cond` to become true.
///
/// Returns `true` if the condition was observed true before the deadline,
/// `false` if the wait expired. Reads `CNTFRQ_EL0` through [`time::frequency_hz`]
/// on every call so the same binary is correct under QEMU (62.5 MHz) and on a
/// real Pi 3 (19.2 MHz).
fn wait_until<F: FnMut() -> bool>(deadline_us: u64, mut cond: F) -> bool {
    let freq = time::frequency_hz();
    let deadline_ticks = if freq == 0 {
        // No timer: fall back to a fixed spin count. `saturating_mul` avoids
        // overflow on very long deadlines.
        deadline_us.saturating_mul(1_000)
    } else {
        deadline_us.saturating_mul(freq) / 1_000_000
    };

    let start = time::ticks();
    loop {
        if cond() {
            return true;
        }
        if time::ticks().wrapping_sub(start) >= deadline_ticks {
            // Final check: the counter may have crossed the deadline since the
            // previous test.
            return cond();
        }
        core::hint::spin_loop();
    }
}

// ---------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------

/// Driver for the BCM2837 EMMC / Arasan SDHCI controller.
pub struct Emmc {
    block: RegisterBlock<Registers>,
    initialised: bool,
    card_type: CardType,
    rca: u32,
    ocr: u32,
    status: u32,
    scr: u64,
    capacity_bytes: u64,
    bus_width: u8,
    csd_version: u8,
    // CID words from RESP3..RESP0 (RESP3 top, RESP0 bottom), kept raw for
    // [`CardInfo`].
    cid_raw: [u32; 4],
    // Last command name; debug aid printed in the shell's error message.
    last_cmd_name: &'static str,
    // What the firmware says this block is clocked at, learned at `init`.
    base_clock: u32,
}

impl Emmc {
    /// Creates the driver bound to the EMMC block at [`map::EMMC`].
    ///
    /// # Safety
    ///
    /// Only one instance of this driver may exist and it must be behind a
    /// [`crate::sync::SpinLock`] before any secondary core is released, per
    /// rules 4.1 and 5.4.
    pub const unsafe fn new() -> Self {
        Self {
            // SAFETY: forwarded to the caller of `Emmc::new`; the single-owner
            // invariant is cited above.
            block: unsafe { RegisterBlock::new(map::EMMC) },
            base_clock: 0,
            initialised: false,
            card_type: CardType::Unknown,
            rca: 0,
            ocr: 0,
            status: 0,
            scr: 0,
            capacity_bytes: 0,
            bus_width: 1,
            csd_version: 0,
            cid_raw: [0; 4],
            last_cmd_name: "",
        }
    }

    #[inline(always)]
    fn regs(&self) -> &Registers {
        self.block.regs()
    }

    /// Whether the last call to [`Self::init`] completed successfully.
    pub fn is_initialised(&self) -> bool {
        self.initialised
    }

    /// Snapshot of the currently-initialised card, or [`None`] if the driver
    /// has never seen one come up successfully.
    pub fn info(&self) -> Option<CardInfo> {
        if !self.initialised {
            return None;
        }

        // Top 32 CID bits (Pi's stripped CRC byte reads zero) in `cid_raw[0]`,
        // bottom 32 bits in `cid_raw[3]`.
        let cid0 = self.cid_raw[0];
        let cid1 = self.cid_raw[1];
        let cid2 = self.cid_raw[2];
        let cid3 = self.cid_raw[3];

        let manufacturer_id = ((cid0 >> 16) & 0xFF) as u8;
        let oem_hi = ((cid0 >> 8) & 0xFF) as u8;
        let oem_lo = (cid0 & 0xFF) as u8;
        let prod_name = [
            ((cid1 >> 24) & 0xFF) as u8,
            ((cid1 >> 16) & 0xFF) as u8,
            ((cid1 >> 8) & 0xFF) as u8,
            (cid1 & 0xFF) as u8,
            ((cid2 >> 24) & 0xFF) as u8,
        ];
        let rev_major = ((cid2 >> 20) & 0xF) as u8;
        let rev_minor = ((cid2 >> 16) & 0xF) as u8;
        let serial_hi = cid2 & 0xFFFF;
        let serial_lo = (cid3 >> 16) & 0xFFFF;
        let serial = (serial_hi << 16) | serial_lo;
        let month = (cid3 & 0xF) as u8;
        // Year is an offset from 2000; widen so 255 + 2000 fits the u16.
        let year_offset = ((cid3 >> 4) & 0xFF) as u16;

        Some(CardInfo {
            card_type: self.card_type,
            capacity_bytes: self.capacity_bytes,
            rca: self.rca,
            ocr: self.ocr,
            csd_version: self.csd_version,
            bus_width: self.bus_width,
            manufacturer_id,
            oem_id: [oem_hi, oem_lo],
            product_name: prod_name,
            product_revision_major: rev_major,
            product_revision_minor: rev_minor,
            manufacture_month: month,
            manufacture_year: 2000u16.saturating_add(year_offset),
            serial,
        })
    }

    /// Total number of 512-byte blocks on the card, or 0 if uninitialised.
    pub fn block_count(&self) -> u64 {
        if !self.initialised {
            0
        } else {
            self.capacity_bytes / BLOCK_SIZE as u64
        }
    }

    // -----------------------------------------------------------------------
    // Low-level waits.
    // -----------------------------------------------------------------------

    fn wait_for_interrupt(&mut self, mask: u32) -> Result<(), Error> {
        // Wait for `mask`, but any error bit also ends the wait. Command
        // timeout is reported separately so callers can retry.
        let watched = mask | int_bits::ERROR_MASK | int_bits::CTO_ERR;
        let observed = wait_until(1_000_000, || self.regs().interrupt.read() & watched != 0);

        let ival = self.regs().interrupt.read();

        if !observed {
            self.regs().interrupt.write(ival);
            return Err(Error::Timeout);
        }
        if (ival & int_bits::CTO_ERR) != 0 {
            self.regs().interrupt.write(ival);
            return Err(Error::CommandTimeout);
        }
        if (ival & int_bits::ERROR_MASK) != 0 {
            self.regs().interrupt.write(ival);
            return Err(Error::NoResponse);
        }

        // Only acknowledge the bit(s) the caller asked about; leave anything
        // else set so the next wait sees it.
        self.regs().interrupt.write(mask);
        Ok(())
    }

    fn wait_for_command(&self) -> Result<(), Error> {
        let observed = wait_until(1_000_000, || {
            let status = self.regs().status.read();
            let interrupt = self.regs().interrupt.read();
            (status & status_bits::CMD_INHIBIT) == 0 || (interrupt & int_bits::ERROR_MASK) != 0
        });
        let interrupt = self.regs().interrupt.read();
        if !observed || (interrupt & int_bits::ERROR_MASK) != 0 {
            Err(Error::Busy(interrupt))
        } else {
            Ok(())
        }
    }

    fn wait_for_data(&self) -> Result<(), Error> {
        let observed = wait_until(1_000_000, || {
            let status = self.regs().status.read();
            let interrupt = self.regs().interrupt.read();
            (status & status_bits::DAT_INHIBIT) == 0 || (interrupt & int_bits::ERROR_MASK) != 0
        });
        let interrupt = self.regs().interrupt.read();
        if !observed || (interrupt & int_bits::ERROR_MASK) != 0 {
            Err(Error::Busy(0))
        } else {
            Ok(())
        }
    }

    // -----------------------------------------------------------------------
    // Command layer.
    // -----------------------------------------------------------------------

    fn send_command_raw(&mut self, cmd: Cmd, arg: u32) -> Result<(), Error> {
        self.wait_for_command()?;
        self.last_cmd_name = cmd.name;

        // Write-1-to-clear: writing the current bits back clears all pending
        // interrupts before issuing a new command.
        let pending = self.regs().interrupt.read();
        self.regs().interrupt.write(pending);

        self.regs().arg1.write(arg);
        self.regs().cmdtm.write(cmd.code);
        if cmd.delay_us != 0 {
            time::spin_for_micros(cmd.delay_us as u64);
        }

        self.wait_for_interrupt(int_bits::CMD_DONE)?;

        let resp0 = self.regs().resp0.read();

        match cmdtm_bits::rsp_type(cmd.code) {
            0 => Ok(()),
            3 => {
                self.status = resp0;
                if resp0 & R1_ERRORS_MASK == 0 {
                    Ok(())
                } else {
                    Err(Error::CardStatus(resp0 & R1_ERRORS_MASK))
                }
            }
            2 => match cmdtm_bits::index_of(cmd.code) {
                0x03 => {
                    // CMD3 R6: RCA in top 16 bits, status flags packed into
                    // the low 16.
                    self.rca = resp0 & 0xFFFF_0000;
                    self.status = (resp0 & 0x0000_1FFF)
                        | ((resp0 & 0x0000_2000) << 6)
                        | ((resp0 & 0x0000_4000) << 8)
                        | ((resp0 & 0x0000_8000) << 8);
                    if self.status & R1_ERRORS_MASK == 0 {
                        Ok(())
                    } else {
                        Err(Error::CardStatus(self.status & R1_ERRORS_MASK))
                    }
                }
                0x08 => {
                    // CMD8: response must echo the argument exactly. A mismatch
                    // means the card did not understand CMD8 (typical of SD v1)
                    // or answered on the wrong voltage.
                    self.status = 0;
                    if resp0 == arg {
                        Ok(())
                    } else {
                        Err(Error::NoResponse)
                    }
                }
                0x29 => {
                    // ACMD41: RESP0 holds OCR.
                    self.status = 0;
                    self.ocr = resp0;
                    Ok(())
                }
                _ => {
                    self.status = resp0;
                    if resp0 & R1_ERRORS_MASK == 0 {
                        Ok(())
                    } else {
                        Err(Error::CardStatus(resp0 & R1_ERRORS_MASK))
                    }
                }
            },
            1 => {
                self.status = 0;
                let resp1 = self.regs().resp1.read();
                let resp2 = self.regs().resp2.read();
                let resp3 = self.regs().resp3.read();
                if cmdtm_bits::index_of(cmd.code) == 0x09 {
                    self.unpack_csd(resp0, resp1, resp2, resp3);
                } else {
                    // CID: RESP3 = top word, RESP0 = bottom word.
                    self.cid_raw = [resp3, resp2, resp1, resp0];
                }
                Ok(())
            }
            _ => Err(Error::NoResponse),
        }
    }

    fn send_app_command(&mut self) -> Result<(), Error> {
        if self.rca == 0 {
            // No RCA yet: CMD55 is issued unadorned, then a 100 µs pause so the
            // following ACMD does not race the controller.
            self.send_command_raw(SdCommand::AppCmd.descriptor(), 0)?;
            time::spin_for_micros(100);
            Ok(())
        } else {
            self.send_command_raw(SdCommand::AppCmdRca.descriptor(), self.rca)?;
            if self.status & ST_APP_CMD == 0 {
                Err(Error::AppCommandFailed)
            } else {
                Ok(())
            }
        }
    }

    fn send_command(&mut self, cmd: SdCommand) -> Result<(), Error> {
        if cmd.is_app_command() {
            self.send_app_command()?;
        }
        let descriptor = cmd.descriptor();
        let arg = if descriptor.use_rca { self.rca } else { 0 };
        self.send_command_raw(descriptor, arg)?;
        if cmd.is_app_command() && self.rca != 0 && (self.status & ST_APP_CMD) == 0 {
            return Err(Error::AppCommandFailed);
        }
        Ok(())
    }

    fn send_command_arg(&mut self, cmd: SdCommand, arg: u32) -> Result<(), Error> {
        if cmd.is_app_command() {
            self.send_app_command()?;
        }
        self.send_command_raw(cmd.descriptor(), arg)?;
        if cmd.is_app_command() && self.rca != 0 && (self.status & ST_APP_CMD) == 0 {
            return Err(Error::AppCommandFailed);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Clock / reset.
    // -----------------------------------------------------------------------

    fn set_clock(&mut self, freq: u32) -> Result<(), Error> {
        // A divider of zero halts the clock. The datasheet describes the
        // 10-bit divider as `base / (2 * div)`, but the reference uses the
        // integer ratio directly; match it.
        let base = if self.base_clock == 0 {
            BASE_CLOCK
        } else {
            self.base_clock
        };
        if freq == 0 || freq >= base {
            return Err(Error::ClockFailed);
        }
        // Rounded up, so the card is never clocked faster than it was asked
        // for. Rounding down overclocks by up to one divider step, and the
        // step that matters is the first one: initialisation has a hard
        // ceiling of 400 kHz and a card is entitled to ignore anything above
        // it.
        let div = base.div_ceil(freq).min(MAX_DIVIDER);

        // Stop the clock to the card before touching the divider. A frequency
        // change made while the output is running is not required to be
        // honoured, and on this controller it is not.
        let running = self.regs().control1.read();
        self.regs().control1.write(running & !control1_bits::CLK_EN);

        // Internal clock on and divided, but not yet released to the card.
        let control1 = control1_bits::clock_divider(div)
            | control1_bits::DATA_TOUNIT_MAX
            | control1_bits::CLK_INTLEN;
        self.regs().control1.write(control1);

        // Only once it is stable. Enabling the output during the settling
        // window hands the card a malformed clock, and the window grows with
        // the divider -- so the fault appears exactly when the driver starts
        // asking for the slow rate that initialisation is specified at, and
        // hides at every faster one.
        let stable = wait_until(1_000_000, || {
            self.regs().control1.read() & control1_bits::CLK_STABLE != 0
        });
        if !stable {
            return Err(Error::ClockFailed);
        }
        self.regs().control1.write(control1 | control1_bits::CLK_EN);

        // The card is entitled to 74 clock cycles before the first command.
        // At 400 kHz that is 185 microseconds, which no other delay in this
        // sequence covers; at the rate the driver used to run it by accident,
        // it was over in eight.
        let cycles_us = 74_000_000u64 / u64::from(freq.max(1));
        time::spin_for_micros(cycles_us.max(200));
        Ok(())
    }

    fn reset_card(&mut self) -> Result<(), Error> {
        self.regs().control1.write(control1_bits::SRST_HC);
        time::spin_for_micros(10);

        let cleared = wait_until(1_000_000, || {
            self.regs().control1.read() & control1_bits::SRST_HC == 0
        });
        if !cleared {
            return Err(Error::ResetFailed);
        }

        self.regs().control1.write(control1_bits::SRST_DATA);
        time::spin_for_micros(10);
        self.regs().control1.write(0);

        // Explicit 3.3 V bus-power assertion: required by the RPi 4 controller
        // and tolerated by the RPi 3.
        self.regs().control0.write(0);
        time::spin_for_micros(1);
        self.regs()
            .control0
            .write(control0_bits::BUSVOLTAGE_V3_3 | control0_bits::BUSPOWER);
        self.regs().control1.write(0);
        time::spin_for_micros(1);

        self.set_clock(FREQ_SETUP)?;

        // Unmask every source except `CARD_INT` (SDIO only, unused by mass
        // storage), clear stale flags, then disable delivery since the driver
        // polls.
        self.regs().irpt_en.write(0);
        self.regs().irpt_mask.write(!int_bits::CARD_INT);
        self.regs().interrupt.write(!0);
        let int_en = self.regs().irpt_en.read() & !self.regs().interrupt.read();
        self.regs().irpt_en.write(int_en);

        self.rca = 0;
        self.ocr = 0;
        self.status = 0;
        self.card_type = CardType::Unknown;
        self.last_cmd_name = "";

        self.send_command(SdCommand::GoIdleState)
    }

    fn app_send_op_cond(&mut self, arg: u32) -> Result<(), Error> {
        // The SD spec requires ACMD41 to be polled for > 1 s; six retries of
        // 400 ms cover that with margin.
        let mut attempt = self.send_command_arg(SdCommand::AppSendOpCond, arg);
        let mut remaining = 6;
        while remaining > 0 {
            match &attempt {
                Ok(()) | Err(Error::Timeout) | Err(Error::CommandTimeout) => {}
                Err(err) => return Err(*err),
            }
            if self.ocr & OCR_POWER_UP_BUSY != 0 {
                break;
            }
            time::spin_for_micros(400_000);
            attempt = self.send_command_arg(SdCommand::AppSendOpCond, arg);
            remaining -= 1;
        }
        // Propagate the last hard error if the card never left the busy state.
        if let Err(err) = attempt
            && !matches!(err, Error::Timeout | Error::CommandTimeout)
        {
            return Err(err);
        }
        if self.ocr & OCR_POWER_UP_BUSY == 0 {
            return Err(Error::Timeout);
        }
        if self.ocr & (OCR_VOLTAGE_3V2_3V3 | OCR_VOLTAGE_3V3_3V4) == 0 {
            return Err(Error::UnsupportedVoltage);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // CSD decoding — kept faithful to LdB's `unpack_csd`.
    // -----------------------------------------------------------------------

    fn unpack_csd(&mut self, resp0: u32, resp1: u32, resp2: u32, resp3: u32) {
        // The controller drops the CRC7 and stop bit of the 136-bit R2, so the
        // response registers hold CSD[127:8] right-aligned. Reassembling them
        // little-endian gives this byte-to-CSD-bit mapping, which every mask
        // below is derived from:
        //
        //   buffer[0]  = CSD[111:104]   buffer[8]  = CSD[47:40]
        //   buffer[1]  = CSD[119:112]   buffer[9]  = CSD[55:48]
        //   buffer[2]  = CSD[127:120]   buffer[10] = CSD[63:56]
        //   buffer[3]  = unused         buffer[11] = CSD[71:64]
        //   buffer[4]  = CSD[79:72]     buffer[12] = CSD[15:8]
        //   buffer[5]  = CSD[87:80]     buffer[13] = CSD[23:16]
        //   buffer[6]  = CSD[95:88]     buffer[14] = CSD[31:24]
        //   buffer[7]  = CSD[103:96]    buffer[15] = CSD[39:32]
        let mut buffer = [0u8; 16];
        buffer[0..4].copy_from_slice(&resp3.to_le_bytes());
        buffer[4..8].copy_from_slice(&resp2.to_le_bytes());
        buffer[8..12].copy_from_slice(&resp1.to_le_bytes());
        buffer[12..16].copy_from_slice(&resp0.to_le_bytes());

        // CSD_STRUCTURE is CSD[127:126].
        let structure = (buffer[2] & 0xC0) >> 6;
        self.csd_version = if structure == 1 { 2 } else { 1 };

        if structure == 1 {
            // CSD v2.0 (SDHC/SDXC): C_SIZE is CSD[69:48]; capacity is
            // (C_SIZE + 1) blocks of 512 KiB.
            let c_size = (u32::from(buffer[11] & 0x3F) << 16)
                | (u32::from(buffer[10]) << 8)
                | u32::from(buffer[9]);
            self.capacity_bytes = u64::from(c_size + 1) * 512 * 1024;
        } else {
            // CSD v1.0 (SDSC):
            //   capacity = (C_SIZE + 1) * 2^(C_SIZE_MULT + 2) * 2^READ_BL_LEN
            //
            // C_SIZE      is CSD[73:62]: 2 bits from buffer[4], 8 from
            //             buffer[11], 2 from the top of buffer[10].
            // C_SIZE_MULT is CSD[49:47]: 2 bits from buffer[9], 1 from the top
            //             of buffer[8].
            // READ_BL_LEN is CSD[83:80], the low nibble of buffer[5].
            let c_size = (u32::from(buffer[4] & 0x03) << 10)
                | (u32::from(buffer[11]) << 2)
                | (u32::from(buffer[10] & 0xC0) >> 6);
            let c_size_mult =
                (u32::from(buffer[9] & 0x03) << 1) | (u32::from(buffer[8] & 0x80) >> 7);
            let read_bl_len = u32::from(buffer[5]) & 0x0F;
            // Split the shift so it cannot overflow: `c_size_mult` is 3 bits
            // (shift ≤ 9) and `read_bl_len` 4 bits (shift ≤ 15), so the
            // product fits in u64.
            let block = 1u64 << read_bl_len;
            let mult = 1u64 << (c_size_mult + 2);
            self.capacity_bytes = u64::from(c_size + 1) * block * mult;
        }
    }

    // -----------------------------------------------------------------------
    // SCR read (used to learn which bus widths the card supports).
    // -----------------------------------------------------------------------

    fn read_scr(&mut self) -> Result<(), Error> {
        self.wait_for_data()?;

        // BLKSIZECNT is one 8-byte block for ACMD51.
        self.regs().blksizecnt.write((1 << 16) | 8);

        self.send_command(SdCommand::SendScr)?;
        self.wait_for_interrupt(int_bits::READ_RDY)?;

        // Two 32-bit reads with a bounded poll on `READ_TRANSFER` between them.
        let mut words = [0u32; 2];
        let mut count = 0usize;
        let deadline_us = 100_000u64;
        while count < 2 {
            let observed = wait_until(deadline_us, || {
                self.regs().status.read() & status_bits::READ_TRANSFER != 0
            });
            if !observed {
                return Err(Error::Timeout);
            }
            words[count] = self.regs().data.read();
            count += 1;
        }

        self.scr = u64::from(words[0]) | (u64::from(words[1]) << 32);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Block transfer.
    // -----------------------------------------------------------------------

    fn transfer_blocks(
        &mut self,
        start_block: u32,
        num_blocks: u32,
        mut transfer: Transfer<'_>,
    ) -> Result<(), Error> {
        if self.card_type == CardType::Unknown {
            return Err(Error::NotInitialised);
        }
        if num_blocks == 0 {
            return Ok(());
        }
        if transfer.len() != (num_blocks as usize) * BLOCK_SIZE {
            return Err(Error::InvalidBuffer);
        }

        self.wait_for_data()?;

        let write = transfer.is_write();
        let ready_int = if write {
            int_bits::WRITE_RDY
        } else {
            int_bits::READ_RDY
        };
        let transfer_cmd = if write {
            if num_blocks == 1 {
                SdCommand::WriteSingle
            } else {
                SdCommand::WriteMulti
            }
        } else if num_blocks == 1 {
            SdCommand::ReadSingle
        } else {
            SdCommand::ReadMulti
        };

        // SC cards address bytes, HC/XC cards address 512-byte blocks; only
        // the argument differs.
        let block_address = if self.card_type == CardType::V2Sc {
            start_block.checked_shl(9).ok_or(Error::OutOfRange)?
        } else {
            start_block
        };

        // BLKSIZECNT: `BLKCNT` in bits 16-31, `BLKSIZE` in bits 0-9.
        self.regs()
            .blksizecnt
            .write((num_blocks << 16) | (BLOCK_SIZE as u32 & 0x3FF));

        self.send_command_arg(transfer_cmd, block_address)?;

        for block in 0..num_blocks as usize {
            self.wait_for_interrupt(ready_int)?;
            let range = block * BLOCK_SIZE..(block + 1) * BLOCK_SIZE;
            match &mut transfer {
                Transfer::Write(source) => {
                    for chunk in source[range].as_chunks::<4>().0 {
                        self.regs().data.write(u32::from_le_bytes(*chunk));
                    }
                }
                Transfer::Read(destination) => {
                    for chunk in destination[range].as_chunks_mut::<4>().0 {
                        *chunk = self.regs().data.read().to_le_bytes();
                    }
                }
            }
        }

        if write {
            self.wait_for_interrupt(int_bits::DATA_DONE)?;
        }

        // Multi-block transfers without a preceding `SET_BLOCK_COUNT` need an
        // explicit `STOP_TRANS` to leave the card in the transfer state.
        if num_blocks > 1 {
            self.send_command(SdCommand::StopTrans)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Full initialisation.
    // -----------------------------------------------------------------------

    /// The controller's registers as they stand, for reporting a failure.
    ///
    /// A stalled controller says nothing about why through the error type
    /// alone, and reproducing it costs a reboot. This is every register that
    /// bears on the question, read in one go so a caller can print them
    /// together and be done.
    #[must_use]
    pub fn diagnostics(&self) -> Diagnostics {
        Diagnostics {
            status: self.regs().status.read(),
            control0: self.regs().control0.read(),
            control1: self.regs().control1.read(),
            interrupt: self.regs().interrupt.read(),
            slotisr_ver: self.regs().slotisr_ver.read(),
            base_clock: self.base_clock,
            last_command: self.last_cmd_name,
        }
    }

    /// What the driver is dividing to reach the bus clock, in Hz.
    ///
    /// Zero until [`Self::init`] has asked. Worth reporting next to the rate
    /// the firmware advertises: if the two disagree the card is being clocked
    /// wrongly, and every symptom after that is downstream of it.
    #[must_use]
    pub fn base_clock(&self) -> u32 {
        self.base_clock
    }

    /// Brings the card up: reset, CMD8 probe, ACMD41 busy-loop, CMD2 (CID),
    /// CMD3 (RCA), CMD9 (CSD), CMD7 (select), ACMD51 (SCR), optional 4-bit
    /// bus, CMD16 (block length 512).
    ///
    /// On success the card is in the transfer state at `FREQ_NORMAL` with
    /// [`Self::info`] populated.
    pub fn init(&mut self) -> Result<(), Error> {
        self.base_clock = BASE_CLOCK;
        self.initialised = false;
        self.capacity_bytes = 0;
        self.bus_width = 1;
        self.scr = 0;
        self.cid_raw = [0; 4];
        self.csd_version = 0;

        self.reset_card()?;

        // CMD8: SD 2.0 probe. Argument 0x1AA = VHS 2.7-3.6 V, check pattern
        // 0xAA; a matching echo means at least a v2 card.
        let cmd8 = self.send_command_arg(SdCommand::SendIfCond, 0x0000_01AA);
        match cmd8 {
            Ok(()) => {
                self.app_send_op_cond(ACMD41_ARG_HC)?;
                self.card_type = if self.ocr & OCR_CARD_CAPACITY != 0 {
                    CardType::V2Hc
                } else {
                    CardType::V2Sc
                };
            }
            Err(Error::Busy(bits)) => return Err(Error::Busy(bits)),
            Err(_) => {
                // Legacy card: reset again, then ACMD41 with the SC argument.
                self.reset_card()?;
                if self.regs().status.read() & status_bits::CMD_INHIBIT != 0 {
                    return Err(Error::Busy(self.regs().interrupt.read()));
                }
                self.app_send_op_cond(ACMD41_ARG_SC)?;
                self.card_type = CardType::V1;
            }
        }

        self.send_command(SdCommand::AllSendCid)?;
        self.send_command(SdCommand::SendRelAddr)?;
        self.send_command(SdCommand::SendCsd)?;

        if self.capacity_bytes == 0 {
            return Err(Error::Unusable);
        }

        self.set_clock(FREQ_NORMAL)?;
        self.send_command(SdCommand::CardSelect)?;
        self.read_scr()?;

        // SCR bits 8-11 carry `BUS_WIDTH`; 5 (0b0101) means both 1-bit and
        // 4-bit are supported. Otherwise the driver stays at 1-bit.
        let bus_width_field = ((self.scr >> 8) & 0xF) as u32;
        if bus_width_field == 5 {
            self.send_command_arg(SdCommand::SetBusWidth, self.rca | 2)?;
            let control0 = self.regs().control0.read();
            self.regs()
                .control0
                .write(control0 | control0_bits::HCTL_DWIDTH);
            self.bus_width = 4;
        }

        self.send_command_arg(SdCommand::SetBlocklen, BLOCK_SIZE as u32)?;

        if self.capacity_bytes < BLOCK_SIZE as u64 {
            return Err(Error::Unusable);
        }

        self.initialised = true;
        Ok(())
    }

    /// Reads `buffer.len() / 512` consecutive blocks into `buffer`.
    ///
    /// `buffer` must be a non-zero multiple of [`BLOCK_SIZE`]; otherwise
    /// [`Error::InvalidBuffer`] is returned and no I/O happens.
    pub fn read_blocks(&mut self, start_block: u32, buffer: &mut [u8]) -> Result<(), Error> {
        if !self.initialised {
            return Err(Error::NotInitialised);
        }
        if buffer.is_empty() || !buffer.len().is_multiple_of(BLOCK_SIZE) {
            return Err(Error::InvalidBuffer);
        }
        let num_blocks_usize = buffer.len() / BLOCK_SIZE;
        let num_blocks: u32 = num_blocks_usize
            .try_into()
            .map_err(|_| Error::InvalidBuffer)?;
        let end = u64::from(start_block) + u64::from(num_blocks);
        if end > self.block_count() {
            return Err(Error::OutOfRange);
        }
        self.transfer_blocks(start_block, num_blocks, Transfer::Read(buffer))
    }

    /// Writes `buffer.len() / 512` consecutive blocks from `buffer`.
    ///
    /// `buffer` must be a non-zero multiple of [`BLOCK_SIZE`]; otherwise
    /// [`Error::InvalidBuffer`] is returned and no I/O happens.
    pub fn write_blocks(&mut self, start_block: u32, buffer: &[u8]) -> Result<(), Error> {
        if !self.initialised {
            return Err(Error::NotInitialised);
        }
        if buffer.is_empty() || !buffer.len().is_multiple_of(BLOCK_SIZE) {
            return Err(Error::InvalidBuffer);
        }
        let num_blocks_usize = buffer.len() / BLOCK_SIZE;
        let num_blocks: u32 = num_blocks_usize
            .try_into()
            .map_err(|_| Error::InvalidBuffer)?;
        let end = u64::from(start_block) + u64::from(num_blocks);
        if end > self.block_count() {
            return Err(Error::OutOfRange);
        }
        self.transfer_blocks(start_block, num_blocks, Transfer::Write(buffer))
    }
}

/// Which way a PIO transfer goes, and the buffer it uses.
///
/// The directions need different mutability: a `&[u8]` write buffer may live
/// in `.rodata`, so fabricating a `&mut` to it would be undefined behaviour.
enum Transfer<'a> {
    /// Card to memory.
    Read(&'a mut [u8]),
    /// Memory to card.
    Write(&'a [u8]),
}

impl Transfer<'_> {
    /// Length of the underlying buffer in bytes.
    fn len(&self) -> usize {
        match self {
            Self::Read(buffer) => buffer.len(),
            Self::Write(buffer) => buffer.len(),
        }
    }

    /// Whether this transfer sends data to the card.
    fn is_write(&self) -> bool {
        matches!(self, Self::Write(_))
    }
}
