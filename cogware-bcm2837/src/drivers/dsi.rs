// SPDX-License-Identifier: GPL-3.0-only
//! The two MIPI DSI hosts.
//!
//! A host sends packets — DCS commands, generic writes, reads with a bus
//! turnaround — to a panel's controller, always in low-power escape mode,
//! which needs only the 100 MHz escape clock. That is enough to initialise any
//! panel and, for one with its own frame memory, to push pixels with
//! [`Dsi::write_memory`]. With a high-speed clock as well, the host streams
//! video from a pixel valve: [`super::display`] puts that pipeline together.
//!
//! Nothing here is exercised by QEMU, which has no DSI; the sequence follows
//! the Linux `vc4_dsi` driver and is untested on hardware.
//!
//! DSI0 and DSI1 differ in register layout, so registers are reached by
//! offset rather than through one `#[repr(C)]` map. DSI1 also has an AXI slave
//! that ignores writes from the ARM and takes them only from the DMA engine,
//! so every DSI1 register write goes through a DMA channel the driver claims.
#![allow(dead_code)]

use super::{
    clock::{self, ClockId, Mash, Source},
    dma,
    mailbox::{self, Domain},
    mmio::RegisterBlock,
};
use crate::{
    bsp::memory::{bus, map},
    time,
};

/// DCS and generic packet data types.
pub mod data_type {
    /// Generic short write, no parameters.
    pub const GENERIC_SHORT_WRITE_0: u8 = 0x03;
    /// Generic short write, one parameter.
    pub const GENERIC_SHORT_WRITE_1: u8 = 0x13;
    /// Generic short write, two parameters.
    pub const GENERIC_SHORT_WRITE_2: u8 = 0x23;
    /// Generic read, no parameters.
    pub const GENERIC_READ_0: u8 = 0x04;
    /// Generic read, one parameter.
    pub const GENERIC_READ_1: u8 = 0x14;
    /// Generic read, two parameters.
    pub const GENERIC_READ_2: u8 = 0x24;
    /// DCS short write, no parameter.
    pub const DCS_SHORT_WRITE_0: u8 = 0x05;
    /// DCS short write, one parameter.
    pub const DCS_SHORT_WRITE_1: u8 = 0x15;
    /// DCS read, no parameter.
    pub const DCS_READ: u8 = 0x06;
    /// Set the largest packet the panel may return.
    pub const SET_MAX_RETURN_SIZE: u8 = 0x37;
    /// Generic long write.
    pub const GENERIC_LONG_WRITE: u8 = 0x29;
    /// DCS long write.
    pub const DCS_LONG_WRITE: u8 = 0x39;
}

/// Standard DCS commands.
pub mod dcs {
    /// Software reset.
    pub const SOFT_RESET: u8 = 0x01;
    /// Leave sleep mode; most panels then need 120 ms.
    pub const EXIT_SLEEP_MODE: u8 = 0x11;
    /// Turn the display output off.
    pub const SET_DISPLAY_OFF: u8 = 0x28;
    /// Turn the display output on.
    pub const SET_DISPLAY_ON: u8 = 0x29;
    /// Column address window: start and end, 16 bits each.
    pub const SET_COLUMN_ADDRESS: u8 = 0x2a;
    /// Page (row) address window.
    pub const SET_PAGE_ADDRESS: u8 = 0x2b;
    /// Pixels follow, from the start of the window.
    pub const WRITE_MEMORY_START: u8 = 0x2c;
    /// Pixels follow, carrying on where the last packet stopped.
    pub const WRITE_MEMORY_CONTINUE: u8 = 0x3c;
    /// Pixel format: 0x55 for 16 bits, 0x66 for 18, 0x77 for 24.
    pub const SET_PIXEL_FORMAT: u8 = 0x3a;
    /// Read the power mode byte.
    pub const GET_POWER_MODE: u8 = 0x0a;
}

/// Escape clock rate; Linux's timings assume it.
const ESCAPE_HZ: u32 = 100_000_000;
/// One escape-clock period, in nanoseconds.
const ESCAPE_NS: u32 = 1_000_000_000 / ESCAPE_HZ;

/// The command FIFO's depth in bytes; longer payloads also use the pixel FIFO.
const CMD_FIFO_BYTES: usize = 16;
/// The pixel FIFO's depth in 32-bit words.
const PIX_FIFO_WORDS: usize = 256;
/// The longest payload one packet can carry through both FIFOs.
pub const MAX_PAYLOAD: usize = (PIX_FIFO_WORDS - 1) * 4 + 3;

// TXPKT1C
const PKTC_EN: u32 = 1 << 0;
const PKTC_TYPE_LONG: u32 = 1 << 2;
const PKTC_MODE_LP: u32 = 1 << 3;
const PKTC_CTRL_RX: u32 = 1 << 4;
const PKTC_DISPLAY_SECONDARY: u32 = 2 << 8;
const PKTC_REPEAT_ONCE: u32 = 1 << 10;
// RXPKT1H
const RXH_PKT_TYPE_LONG: u32 = 1 << 24;
const RXH_ERRORS: u32 = 0xf << 28;
// PHYC
const PHYC_DLANE_ENABLE: [u32; 4] = [1 << 0, 1 << 4, 1 << 8, 1 << 12];
// DISP1: enabled, 32-bit little-endian words, to carry long payloads.
const DISP1_32BIT_LE: u32 = 2 << 1 | 1;
// DISP0: the video stream.
const DISP0_ENABLE: u32 = 1 << 0;
const DISP0_COMMAND_MODE: u32 = 1 << 1;
const DISP0_PFORMAT_SHIFT: u32 = 2;
const DISP0_ST_END: u32 = 1 << 4;
const DISP0_CHANNEL_SHIFT: u32 = 5;
const DISP0_LP_STOP_PERFRAME: u32 = 2 << 11;
const DISP0_PIX_CLK_DIV_SHIFT: u32 = 13;
// CTRL
const CTRL_RX_LPDT_EOT_DISABLE: u32 = 1 << 13;
const CTRL_HSDT_EOT_DISABLE: u32 = 1 << 11;
const CTRL_SOFT_RESET_CFG: u32 = 1 << 10;
const CTRL_CAL_BYTE: u32 = 1 << 9;
const CTRL_CLR_LDF: u32 = 1 << 7;

/// A DSI host.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Port {
    /// DSI0, two data lanes (`DISP0` on the Compute Module).
    Dsi0,
    /// DSI1, four data lanes (`DISP1`).
    Dsi1,
}

/// Where each register sits on one port.
struct Layout {
    base: usize,
    txpkt1c: usize,
    txpkt1h: usize,
    rxpkt1h: usize,
    cmd_fifo: usize,
    pix_fifo: usize,
    rx_fifo: Option<usize>,
    disp0_ctrl: usize,
    disp1_ctrl: usize,
    int_stat: usize,
    stat: usize,
    int_en: usize,
    hstx_to_cnt: usize,
    phyc: usize,
    /// The eight timing registers HS_CLT0..HS_DLT7, consecutive.
    hs_clt0: usize,
    afec0: usize,
    afec1: usize,
    id: usize,
    /// INT_STAT: packet sent; turnaround back from the panel; the errors.
    int_tx_done: u32,
    int_rx_done: u32,
    int_errors: u32,
    ctrl_enable: u32,
    ctrl_reset_fifos: u32,
    phyc_clane_enable: u32,
    phyc_esc_shift: u32,
    phyc_hs_continuous: u32,
    afec0_reset: u32,
}

const DSI0: Layout = Layout {
    base: map::DSI0,
    txpkt1c: 0x04,
    txpkt1h: 0x08,
    rxpkt1h: 0x0c,
    cmd_fifo: 0x14,
    pix_fifo: 0x20,
    rx_fifo: None,
    disp0_ctrl: 0x18,
    disp1_ctrl: 0x1c,
    int_stat: 0x24,
    stat: 0x2c,
    int_en: 0x28,
    hstx_to_cnt: 0x30,
    phyc: 0x40,
    hs_clt0: 0x44,
    afec0: 0x64,
    afec1: 0x68,
    id: 0x74,
    int_tx_done: 1 << 23,
    int_rx_done: 1 << 22,
    int_errors: 0xff << 3,
    ctrl_enable: 1 << 0,
    ctrl_reset_fifos: CTRL_CLR_LDF | 0b1111 << 3,
    phyc_clane_enable: 1 << 8,
    phyc_esc_shift: 12,
    phyc_hs_continuous: 1 << 10,
    afec0_reset: 1 << 11,
};

const DSI1: Layout = Layout {
    base: map::DSI1,
    txpkt1c: 0x04,
    txpkt1h: 0x08,
    rxpkt1h: 0x14,
    cmd_fifo: 0x1c,
    pix_fifo: 0x20,
    rx_fifo: Some(0x24),
    disp0_ctrl: 0x28,
    disp1_ctrl: 0x2c,
    int_stat: 0x30,
    stat: 0x38,
    int_en: 0x34,
    hstx_to_cnt: 0x3c,
    phyc: 0x4c,
    hs_clt0: 0x50,
    afec0: 0x70,
    afec1: 0x74,
    id: 0x8c,
    int_tx_done: 1 << 1,
    int_rx_done: 1 << 21,
    int_errors: 0xff << 6,
    ctrl_enable: 1 << 0,
    ctrl_reset_fifos: CTRL_CLR_LDF | 0b111 << 4,
    phyc_clane_enable: 1 << 16,
    phyc_esc_shift: 20,
    phyc_hs_continuous: 1 << 18,
    afec0_reset: 1 << 13,
};

/// What every DSI block's ID register reads: "dsi" in ASCII.
pub const ID_VALUE: u32 = 0x0064_7369;

impl Port {
    const fn layout(self) -> &'static Layout {
        match self {
            Self::Dsi0 => &DSI0,
            Self::Dsi1 => &DSI1,
        }
    }

    /// The most data lanes this host has.
    pub const fn max_lanes(self) -> u8 {
        match self {
            Self::Dsi0 => 2,
            Self::Dsi1 => 4,
        }
    }

    const fn domain(self) -> Domain {
        match self {
            Self::Dsi0 => Domain::Dsi0,
            Self::Dsi1 => Domain::Dsi1,
        }
    }

    const fn escape_clock(self) -> ClockId {
        match self {
            Self::Dsi0 => ClockId::Dsi0Escape,
            Self::Dsi1 => ClockId::Dsi1Escape,
        }
    }

    /// The clock that carries this host's byte clock to its pixel valve.
    pub const fn pixel_clock(self) -> ClockId {
        match self {
            Self::Dsi0 => ClockId::Dsi0Pixel,
            Self::Dsi1 => ClockId::Dsi1Pixel,
        }
    }
}

/// How pixels are packed on the link in video mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum PixelFormat {
    /// 16 bits per pixel.
    Rgb565 = 0,
    /// 18 bits per pixel, packed with no padding.
    Rgb666Packed = 1,
    /// 18 bits per pixel, each colour padded to a byte.
    Rgb666 = 2,
    /// 24 bits per pixel.
    Rgb888 = 3,
}

impl PixelFormat {
    /// Link bits each pixel occupies.
    pub const fn bits(self) -> u32 {
        match self {
            Self::Rgb565 => 16,
            Self::Rgb666Packed => 18,
            Self::Rgb666 | Self::Rgb888 => 24,
        }
    }
}

/// How a host is set up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// Data lanes wired to the panel, 1 up to [`Port::max_lanes`]. Only lane
    /// 0 carries low-power traffic, but every wired lane is held in its stop
    /// state.
    pub lanes: u8,
    /// The virtual channel packets are addressed to; almost always 0.
    pub virtual_channel: u8,
    /// The rate of the PLL channel clocking the PHY, if one is running; the
    /// D-PHY timings are computed from it. `None` for escape mode only.
    pub hs_clock_hz: Option<u32>,
    /// Keep the clock lane in high-speed mode between packets. Most video
    /// panels want it; ignored without a high-speed clock.
    pub continuous_clock: bool,
}

impl Config {
    /// Escape mode only, on `lanes` lanes: command mode with no PLL.
    pub const fn low_power(lanes: u8) -> Self {
        Self {
            lanes,
            virtual_channel: 0,
            hs_clock_hz: None,
            continuous_clock: false,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::low_power(1)
    }
}

/// Why a DSI operation failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The firmware would not power the host.
    Power(mailbox::Error),
    /// The clock manager would not provide the escape clock.
    Clock(clock::Error),
    /// DSI1 needs a DMA channel for its register writes, and none was free.
    NoDmaChannel,
    /// A DMA register write failed.
    Dma(dma::Error),
    /// The ID register did not read back [`ID_VALUE`]: unpowered, or absent.
    NotPresent(u32),
    /// More lanes than the port has, or none.
    Lanes,
    /// [`Dsi::init`] has not run.
    NotInitialised,
    /// The payload is longer than [`MAX_PAYLOAD`], or a read longer than this
    /// port can return.
    TooLong,
    /// The packet did not go, or the panel did not answer, in time.
    Timeout,
    /// The host flagged a bus error; the interrupt status bits.
    Bus(u32),
    /// The panel answered with a different length than asked for.
    ShortRead(usize),
    /// High-speed mode on DSI0, whose PLL channel (PLLA) is not driven here.
    Unsupported,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Power(error) => write!(f, "DSI power: {error}"),
            Self::Clock(error) => write!(f, "DSI escape clock: {error}"),
            Self::NoDmaChannel => f.write_str("no DMA channel for DSI1 register writes"),
            Self::Dma(error) => write!(f, "DSI register write: {error}"),
            Self::NotPresent(id) => write!(f, "DSI ID reads {id:#010x}"),
            Self::Lanes => f.write_str("lane count not supported by this port"),
            Self::NotInitialised => f.write_str("DSI host not initialised"),
            Self::TooLong => f.write_str("DSI packet too long"),
            Self::Timeout => f.write_str("DSI transfer timed out"),
            Self::Bus(bits) => write!(f, "DSI bus error (status {bits:#x})"),
            Self::ShortRead(got) => write!(f, "DSI read returned {got} bytes"),
            Self::Unsupported => f.write_str("high-speed DSI0 is not supported"),
        }
    }
}

/// Powers `port` and starts its escape clock. Takes [`super::MAILBOX`] and
/// [`super::CLOCKS`], one after the other.
pub fn power_on(port: Port) -> Result<(), Error> {
    super::MAILBOX
        .lock()
        .set_domain_state(port.domain(), true)
        .map_err(Error::Power)?;
    super::CLOCKS
        .lock()
        .enable(port.escape_clock(), Source::PllD, ESCAPE_HZ, Mash::Integer)
        .map_err(Error::Clock)?;
    Ok(())
}

/// Stops `port`'s escape clock and removes its power.
pub fn power_off(port: Port) -> Result<(), Error> {
    super::CLOCKS
        .lock()
        .disable(port.escape_clock())
        .map_err(Error::Clock)?;
    super::MAILBOX
        .lock()
        .set_domain_state(port.domain(), false)
        .map_err(Error::Power)
}

/// Converts a duration of `ns` nanoseconds plus `ui` unit intervals into unit
/// intervals, rounded up to a whole byte clock as the timing registers count.
const fn hs_timing(ui_ns: u32, ns: u32, ui: u32) -> u32 {
    (ui + ns.div_ceil(ui_ns)).next_multiple_of(8)
}

/// A DSI host in command mode.
pub struct Dsi {
    port: Port,
    config: Config,
    /// DSI1's register-write channel.
    dma: Option<dma::Channel>,
    initialised: bool,
}

impl Dsi {
    /// Creates the driver for `port`.
    ///
    /// # Safety
    ///
    /// Only one instance per port may exist, and it must be placed behind a
    /// lock before secondary cores are released.
    pub const unsafe fn new(port: Port) -> Self {
        Self {
            port,
            config: Config::low_power(1),
            dma: None,
            initialised: false,
        }
    }

    fn read(&self, offset: usize) -> u32 {
        // SAFETY: every layout offset is a register inside the port's mapped
        // block, which this driver owns.
        unsafe { RegisterBlock::<()>::read_raw(self.port.layout().base, offset) }
    }

    fn write(&mut self, offset: usize, value: u32) -> Result<(), Error> {
        let base = self.port.layout().base;
        match &mut self.dma {
            // SAFETY: as in `read`; the engine reaches the register at its bus
            // address.
            Some(channel) => unsafe { channel.write_word(bus::peripheral(base + offset), value) }
                .map_err(Error::Dma),
            None => {
                // SAFETY: as in `read`.
                unsafe { RegisterBlock::<()>::write_raw(base, offset, value) };
                Ok(())
            }
        }
    }

    fn modify(&mut self, offset: usize, f: impl FnOnce(u32) -> u32) -> Result<(), Error> {
        let value = f(self.read(offset));
        self.write(offset, value)
    }

    /// Brings up the D-PHY and the packet engine. The port must have been
    /// [`power_on`]ed. Claims a DMA channel for DSI1.
    ///
    /// With [`Config::hs_clock_hz`], also starts the PLL channel at the
    /// fastest rate not above it and the pixel clock, taking
    /// [`super::CLOCKS`] inside this host's lock; [`hs_clock_hz`](Self::hs_clock_hz)
    /// reports what the PLL gave.
    pub fn init(&mut self, config: Config) -> Result<(), Error> {
        if config.lanes == 0 || config.lanes > self.port.max_lanes() {
            return Err(Error::Lanes);
        }
        let layout = self.port.layout();
        let id = self.read(layout.id);
        if id != ID_VALUE {
            return Err(Error::NotPresent(id));
        }
        if self.port == Port::Dsi1 && self.dma.is_none() {
            self.dma = Some(dma::claim(dma::Kind::Lite).ok_or(Error::NoDmaChannel)?);
        }
        self.config = config;
        let lanes = u32::from(config.lanes);

        if config.hs_clock_hz.is_some() && self.port == Port::Dsi0 {
            return Err(Error::Unsupported);
        }

        // Reset the block and its FIFOs, then leave end-of-transmission
        // packets off in both directions, as Linux does.
        self.write(0, CTRL_SOFT_RESET_CFG | layout.ctrl_reset_fifos)?;
        self.write(0, CTRL_HSDT_EOT_DISABLE | CTRL_RX_LPDT_EOT_DISABLE)?;
        let stat = self.read(layout.stat);
        self.write(layout.stat, stat)?;

        // Analogue front end: bias trims, lane drive on DSI1, unused lanes
        // powered down, held in reset for a millisecond.
        let afec0 = match self.port {
            Port::Dsi0 => {
                let unused = if lanes < 2 { 1 << 8 } else { 0 };
                7 << 4 | 7 | unused | layout.afec0_reset
            }
            Port::Dsi1 => {
                let drive = 6 << 17 | 6 << 20 | 6 << 23 | 6 << 26 | 6 << 29;
                let unused = [(2, 1 << 10), (3, 1 << 9), (4, 1 << 8)]
                    .into_iter()
                    .filter(|&(lane, _)| lanes < lane)
                    .fold(0, |bits, (_, power_down)| bits | power_down);
                7 << 4 | 7 | drive | unused | layout.afec0_reset
            }
        };
        self.write(layout.afec0, afec0)?;
        let afec1 = match self.port {
            Port::Dsi0 => 6 << 8 | 6 << 4 | 6,
            Port::Dsi1 => 0,
        };
        self.write(layout.afec1, afec1)?;
        time::spin_for_micros(1_000);

        // The PLL channel, then the pixel clock from the PHY's byte clock,
        // both while the front end is still held in reset, as Linux orders it.
        if let Some(hz) = config.hs_clock_hz {
            let mut clocks = super::CLOCKS.lock();
            let actual = clocks.enable_dsi1_pll(hz);
            clocks
                .select(self.port.pixel_clock(), clock::SOURCE_DSI_BYTE)
                .map_err(Error::Clock)?;
            self.config.hs_clock_hz = Some(actual);
        }

        // The timing registers, Linux's formulas, in unit intervals. The PHY
        // clock is double data rate, hence half a nanosecond per hertz.
        let ui_ns = 500_000_000_u32
            .div_ceil(self.config.hs_clock_hz.unwrap_or(500_000_000).max(1))
            .max(1);
        let lpx = 60u32.div_ceil(ESCAPE_NS);
        let timings = [
            hs_timing(ui_ns, 262, 0) << 18 | hs_timing(ui_ns, 0, 8) << 9 | hs_timing(ui_ns, 38, 0),
            hs_timing(ui_ns, 60, 0) << 9 | hs_timing(ui_ns, 60, 52),
            hs_timing(ui_ns, 1_000_000, 0) & 0xff_ffff,
            hs_timing(ui_ns, 100, 0) << 18
                | hs_timing(ui_ns, 105, 6) << 9
                | hs_timing(ui_ns, 40, 4),
            hs_timing(ui_ns, 0, 8).max(hs_timing(ui_ns, 60, 4)) << 9
                | hs_timing(ui_ns, lpx * ESCAPE_NS, 0),
            // T_INIT: 5 ms of stop state after power-up, the firmware's
            // conservative figure, where the spec asks only 100 us.
            hs_timing(ui_ns, 5_000_000, 0) & 0xff_ffff,
            (lpx * 5) << 24 | lpx << 16 | (lpx * 4) << 8 | lpx,
            1_000_000u32.div_ceil(ESCAPE_NS) & 0xff_ffff,
        ];
        for (index, value) in timings.into_iter().enumerate() {
            self.write(layout.hs_clt0 + index * 4, value)?;
        }

        // Without a high-speed clock the clock lane must stay in its
        // low-power stop state, so never continuous.
        let mut phyc = layout.phyc_clane_enable | (lpx - 1) << layout.phyc_esc_shift;
        if config.hs_clock_hz.is_some() && config.continuous_clock {
            phyc |= layout.phyc_hs_continuous;
        }
        for enable in &PHYC_DLANE_ENABLE[..config.lanes as usize] {
            phyc |= enable;
        }
        self.write(layout.phyc, phyc)?;
        self.modify(0, |ctrl| ctrl | CTRL_CAL_BYTE)?;

        // High-speed transmit timeout off; generous receive, turnaround and
        // reset timeouts, all as Linux sets them.
        self.write(layout.hstx_to_cnt, 0)?;
        self.write(layout.hstx_to_cnt + 4, 0xff_ffff)?;
        self.write(layout.hstx_to_cnt + 8, 100_000)?;
        self.write(layout.hstx_to_cnt + 12, 100_000)?;
        self.write(layout.disp1_ctrl, DISP1_32BIT_LE)?;

        self.modify(0, |ctrl| ctrl | layout.ctrl_enable)?;
        self.modify(layout.afec0, |afec0| afec0 & !layout.afec0_reset)?;
        self.write(layout.int_en, 0)?;
        self.write(layout.disp0_ctrl, DISP0_COMMAND_MODE)?;
        self.initialised = true;
        Ok(())
    }

    /// The PLL rate [`init`](Self::init) got, if it started one.
    pub fn hs_clock_hz(&self) -> Option<u32> {
        self.config.hs_clock_hz
    }

    /// Readies the video stream: pixels of `format` arriving from the pixel
    /// valve, stopping to low power once per frame so commands can still go
    /// in the vertical blank. [`start_video`](Self::start_video) turns it on.
    pub fn configure_video(&mut self, format: PixelFormat) -> Result<(), Error> {
        if !self.initialised {
            return Err(Error::NotInitialised);
        }
        if self.config.hs_clock_hz.is_none() {
            return Err(Error::Unsupported);
        }
        let divider = format.bits() / u32::from(self.config.lanes);
        let value = divider << DISP0_PIX_CLK_DIV_SHIFT
            | (format as u32) << DISP0_PFORMAT_SHIFT
            | u32::from(self.config.virtual_channel & 3) << DISP0_CHANNEL_SHIFT
            | DISP0_LP_STOP_PERFRAME
            | DISP0_ST_END;
        self.write(self.port.layout().disp0_ctrl, value)
    }

    /// Starts sending the pixel valve's stream to the panel.
    pub fn start_video(&mut self) -> Result<(), Error> {
        let disp0 = self.port.layout().disp0_ctrl;
        self.modify(disp0, |ctrl| ctrl | DISP0_ENABLE)
    }

    /// Stops the video stream; commands still go.
    pub fn stop_video(&mut self) -> Result<(), Error> {
        let disp0 = self.port.layout().disp0_ctrl;
        self.modify(disp0, |ctrl| ctrl & !DISP0_ENABLE)
    }

    /// Sends one packet, and with `rx` reads the panel's answer into it.
    ///
    /// `data_type` is one of [`data_type`]; a long type sends `payload`
    /// whole, a short type sends its first two bytes as the parameters.
    /// Returns the number of bytes read.
    pub fn transfer(
        &mut self,
        data_type: u8,
        payload: &[u8],
        rx: &mut [u8],
    ) -> Result<usize, Error> {
        if !self.initialised {
            return Err(Error::NotInitialised);
        }
        let layout = self.port.layout();
        // The long data types: low nibble 9, or C to E.
        let long = matches!(data_type & 0x0f, 0x09 | 0x0c..=0x0e);
        if payload.len() > MAX_PAYLOAD || (layout.rx_fifo.is_none() && rx.len() > 2) {
            return Err(Error::TooLong);
        }

        let id = u32::from(self.config.virtual_channel & 3) << 6 | u32::from(data_type & 0x3f);
        let mut header = id;
        let mut control = PKTC_EN | PKTC_MODE_LP | PKTC_REPEAT_ONCE;
        if long {
            // Whatever does not divide into words goes through the byte-wide
            // command FIFO first; the rest through the pixel FIFO.
            let cmd_bytes = if payload.len() <= CMD_FIFO_BYTES {
                payload.len()
            } else {
                payload.len() % 4
            };
            header |= (payload.len() as u32) << 8 | (cmd_bytes as u32) << 24;
            control |= PKTC_TYPE_LONG;
            for &byte in &payload[..cmd_bytes] {
                self.write(layout.cmd_fifo, u32::from(byte))?;
            }
            for word in payload[cmd_bytes..].as_chunks::<4>().0 {
                self.write(layout.pix_fifo, u32::from_le_bytes(*word))?;
            }
            if payload.len() > cmd_bytes {
                control |= PKTC_DISPLAY_SECONDARY;
            }
        } else {
            let p0 = payload.first().copied().unwrap_or(0);
            let p1 = payload.get(1).copied().unwrap_or(0);
            header |= u32::from(p0) << 8 | u32::from(p1) << 16;
        }
        let done = if rx.is_empty() {
            layout.int_tx_done
        } else {
            control |= PKTC_CTRL_RX;
            layout.int_rx_done
        };

        self.write(layout.int_stat, done | layout.int_errors)?;
        self.write(layout.txpkt1h, header)?;
        self.write(layout.txpkt1c, control)?;

        let result = self.wait(done).and_then(|()| self.receive(rx));
        if result.is_err() {
            self.reset_fifos()?;
        }
        result
    }

    /// Polls for `done`, for up to a second as Linux allows.
    fn wait(&self, done: u32) -> Result<(), Error> {
        let layout = self.port.layout();
        let deadline = time::ticks() + time::frequency_hz();
        loop {
            let status = self.read(layout.int_stat);
            if status & layout.int_errors != 0 {
                return Err(Error::Bus(status & layout.int_errors));
            }
            if status & done != 0 {
                return Ok(());
            }
            if time::ticks() > deadline {
                return Err(Error::Timeout);
            }
            core::hint::spin_loop();
        }
    }

    /// Copies the panel's answer out of the receive header or FIFO.
    fn receive(&self, rx: &mut [u8]) -> Result<usize, Error> {
        if rx.is_empty() {
            return Ok(0);
        }
        let layout = self.port.layout();
        let header = self.read(layout.rxpkt1h);
        if header & RXH_ERRORS != 0 {
            return Err(Error::Bus(header & RXH_ERRORS));
        }
        if header & RXH_PKT_TYPE_LONG != 0 {
            let length = ((header >> 8) & 0xffff) as usize;
            let fifo = layout.rx_fifo.ok_or(Error::TooLong)?;
            if length != rx.len() {
                return Err(Error::ShortRead(length));
            }
            for byte in rx.iter_mut() {
                *byte = self.read(fifo) as u8;
            }
            Ok(length)
        } else {
            let short = [(header >> 8) as u8, (header >> 16) as u8];
            let length = rx.len().min(2);
            rx[..length].copy_from_slice(&short[..length]);
            Ok(length)
        }
    }

    /// Abandons a failed packet and empties the FIFOs.
    fn reset_fifos(&mut self) -> Result<(), Error> {
        let layout = self.port.layout();
        self.modify(layout.txpkt1c, |control| control & !PKTC_EN)?;
        time::spin_for_micros(1);
        self.modify(0, |ctrl| ctrl | layout.ctrl_reset_fifos)
    }

    /// Sends DCS command `command` with `params`, picking the short or long
    /// form by length.
    pub fn dcs_write(&mut self, command: u8, params: &[u8]) -> Result<(), Error> {
        match params {
            [] => self.transfer(data_type::DCS_SHORT_WRITE_0, &[command], &mut []),
            [param] => self.transfer(data_type::DCS_SHORT_WRITE_1, &[command, *param], &mut []),
            _ => {
                let mut packet = [0u8; MAX_PAYLOAD];
                let length = params.len() + 1;
                if length > MAX_PAYLOAD {
                    return Err(Error::TooLong);
                }
                packet[0] = command;
                packet[1..length].copy_from_slice(params);
                self.transfer(data_type::DCS_LONG_WRITE, &packet[..length], &mut [])
            }
        }
        .map(|_| ())
    }

    /// Sends a generic (manufacturer-defined) write of `params`.
    pub fn generic_write(&mut self, params: &[u8]) -> Result<(), Error> {
        let data_type = match params.len() {
            0 => data_type::GENERIC_SHORT_WRITE_0,
            1 => data_type::GENERIC_SHORT_WRITE_1,
            2 => data_type::GENERIC_SHORT_WRITE_2,
            _ => data_type::GENERIC_LONG_WRITE,
        };
        self.transfer(data_type, params, &mut []).map(|_| ())
    }

    /// Reads `buffer.len()` bytes in answer to DCS command `command`, telling
    /// the panel the size first.
    pub fn dcs_read(&mut self, command: u8, buffer: &mut [u8]) -> Result<usize, Error> {
        let size = buffer.len() as u16;
        self.transfer(data_type::SET_MAX_RETURN_SIZE, &size.to_le_bytes(), &mut [])?;
        self.transfer(data_type::DCS_READ, &[command], buffer)
    }

    /// Writes `pixels` into the panel's frame memory from the start of the
    /// current column and page window, as many packets as it takes.
    ///
    /// Chunks are a multiple of six bytes, so neither 16- nor 24-bit pixels
    /// are split between packets.
    pub fn write_memory(&mut self, pixels: &[u8]) -> Result<(), Error> {
        const CHUNK: usize = (MAX_PAYLOAD - 1) / 6 * 6;
        let mut command = dcs::WRITE_MEMORY_START;
        for chunk in pixels.chunks(CHUNK) {
            self.dcs_write(command, chunk)?;
            command = dcs::WRITE_MEMORY_CONTINUE;
        }
        Ok(())
    }

    /// Sets the column and page window [`write_memory`](Self::write_memory)
    /// fills, inclusive at both ends.
    pub fn set_window(&mut self, x0: u16, y0: u16, x1: u16, y1: u16) -> Result<(), Error> {
        let [a, b] = x0.to_be_bytes();
        let [c, d] = x1.to_be_bytes();
        self.dcs_write(dcs::SET_COLUMN_ADDRESS, &[a, b, c, d])?;
        let [a, b] = y0.to_be_bytes();
        let [c, d] = y1.to_be_bytes();
        self.dcs_write(dcs::SET_PAGE_ADDRESS, &[a, b, c, d])
    }

    /// Stops the packet engine and holds the D-PHY in reset. The port may be
    /// [`power_off`]ed afterwards.
    pub fn shutdown(&mut self) -> Result<(), Error> {
        if !self.initialised {
            return Ok(());
        }
        let layout = self.port.layout();
        self.modify(layout.disp0_ctrl, |ctrl| ctrl & !DISP0_ENABLE)?;
        self.modify(0, |ctrl| ctrl & !layout.ctrl_enable)?;
        self.modify(layout.afec0, |afec0| afec0 | layout.afec0_reset)?;
        if self.config.hs_clock_hz.take().is_some() {
            let mut clocks = super::CLOCKS.lock();
            let _ = clocks.disable(self.port.pixel_clock());
            clocks.disable_dsi1_pll();
        }
        self.initialised = false;
        Ok(())
    }
}
