// SPDX-License-Identifier: GPL-3.0-only
//! Unicam: the two MIPI CSI-2 camera receivers.
//!
//! A receiver takes packets off the D-PHY lanes, keeps the ones whose data
//! type and virtual channel match, and writes their payload line by line into
//! a frame buffer in DRAM. That is all: there is no image processing on the
//! ARM side, so what lands is the sensor's raw output — Bayer, usually — and
//! configuring the sensor itself over I2C is the caller's job.
//!
//! `CAM0` has two data lanes and `CAM1` four; the lanes are dedicated pins, so
//! nothing here touches the GPIO block. Nothing here is exercised by QEMU,
//! which has no Unicam; the sequence follows the Linux `bcm2835-unicam`
//! driver and is untested on hardware.
//!
//! The order is [`power_on`], [`Unicam::start`], then [`service`] (polled, or
//! from the receiver's interrupt) for frame events, then [`Unicam::stop`].
#![allow(dead_code)]

use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
    task::{Context, Poll},
};

use super::{
    clock::{self, ClockId, Mash, Source},
    mailbox::{self, Domain},
    mmio::{ReadWrite, RegisterBlock},
};
use crate::{
    arch::mmu,
    bsp::memory::{bus, map},
    task::{self, TaskSet},
    time,
};

/// CSI-2 data types, for [`Config::data_type`].
pub mod data_type {
    /// YUV 4:2:2, 8 bits per sample.
    pub const YUV422_8: u8 = 0x1e;
    /// RGB 5-6-5.
    pub const RGB565: u8 = 0x22;
    /// RGB 8-8-8.
    pub const RGB888: u8 = 0x24;
    /// Raw, 8 bits per pixel.
    pub const RAW8: u8 = 0x2a;
    /// Raw, 10 bits per pixel, four pixels in five bytes.
    pub const RAW10: u8 = 0x2b;
    /// Raw, 12 bits per pixel, two pixels in three bytes.
    pub const RAW12: u8 = 0x2c;
}

// CTRL
const CTRL_CPE: u32 = 1 << 0;
const CTRL_MEM: u32 = 1 << 1;
const CTRL_CPR: u32 = 1 << 2;
const CTRL_SOE: u32 = 1 << 4;
const CTRL_PFT_SHIFT: u32 = 8;
const CTRL_OET_SHIFT: u32 = 12;
// STA: the error flags, and the ones Linux clears on start.
const STA_SBE: u32 = 1 << 2;
const STA_PBE: u32 = 1 << 3;
const STA_HOE: u32 = 1 << 4;
const STA_PLE: u32 = 1 << 5;
const STA_SSC: u32 = 1 << 6;
const STA_CRCE: u32 = 1 << 7;
const STA_IFO: u32 = 1 << 9;
const STA_OFO: u32 = 1 << 10;
const STA_DL: u32 = 1 << 12;
const STA_PS: u32 = 1 << 13;
const STA_PI0: u32 = 1 << 15;
const STA_PI1: u32 = 1 << 16;
const STA_ERRORS: u32 =
    STA_SBE | STA_PBE | STA_HOE | STA_PLE | STA_SSC | STA_CRCE | STA_IFO | STA_OFO;
const STA_CLEAR: u32 = STA_ERRORS | STA_DL | STA_PS | STA_PI0 | STA_PI1;
// ANA
const ANA_AR: u32 = 1 << 2;
const ANA_DDL: u32 = 1 << 3;
const ANA_CTATADJ_SHIFT: u32 = 4;
const ANA_PTATADJ_SHIFT: u32 = 8;
// PRI: panic priority 0xe, normal priority 8, panic threshold 2, enabled.
const PRI_VALUE: u32 = 0xe << 8 | 8 << 4 | 2 << 1 | 1;
// CLK and DATn lane control: enable, low-power receive, termination, HS.
const LANE_ENABLE: u32 = 1 << 0;
const LANE_LP: u32 = 1 << 2;
const LANE_HS: u32 = 1 << 3;
const LANE_TERMINATE: u32 = 1 << 4;
// ICTL
const ICTL_FSIE: u32 = 1 << 0;
const ICTL_FEIE: u32 = 1 << 1;
const ICTL_IBOB: u32 = 1 << 2;
const ICTL_FCM: u32 = 1 << 3;
const ICTL_TFC: u32 = 1 << 4;
const ICTL_LIP: u32 = 1 << 5;
const ICTL_LCIE_SHIFT: u32 = 16;
// ISTA
const ISTA_FSI: u32 = 1 << 0;
const ISTA_FEI: u32 = 1 << 1;
const ISTA_LCI: u32 = 1 << 2;
const ISTA_ALL: u32 = ISTA_FSI | ISTA_FEI | ISTA_LCI;
// CMP0: compare on frame-end short packets, so no frame end is missed.
const CMP_FRAME_END: u32 = 1 << 31 | 1 << 9 | 1 << 8 | 1;
// MISC
const MISC_FL0: u32 = 1 << 6;
const MISC_FL1: u32 = 1 << 9;

/// The low-power clock rate the receiver wants, as Linux sets it.
const LP_CLOCK_HZ: u32 = 100_000_000;

/// The frame buffer's start, stride and end all need this alignment.
pub const BUFFER_ALIGN: usize = 16;

/// The receiver's register map, sparse; only what is driven is named.
#[repr(C)]
struct Registers {
    ctrl: ReadWrite<u32>,     // 0x000
    sta: ReadWrite<u32>,      // 0x004
    ana: ReadWrite<u32>,      // 0x008
    pri: ReadWrite<u32>,      // 0x00c
    clk: ReadWrite<u32>,      // 0x010
    clt: ReadWrite<u32>,      // 0x014
    dat: [ReadWrite<u32>; 4], // 0x018..0x028
    dlt: ReadWrite<u32>,      // 0x028
    cmp0: ReadWrite<u32>,     // 0x02c
    _res0: [u32; 52],         // 0x030..0x100
    ictl: ReadWrite<u32>,     // 0x100
    ista: ReadWrite<u32>,     // 0x104
    idi0: ReadWrite<u32>,     // 0x108
    ipipe: ReadWrite<u32>,    // 0x10c
    ibsa0: ReadWrite<u32>,    // 0x110
    ibea0: ReadWrite<u32>,    // 0x114
    ibls: ReadWrite<u32>,     // 0x118
    ibwp: ReadWrite<u32>,     // 0x11c
    ihwin: ReadWrite<u32>,    // 0x120
    _ihsta: ReadWrite<u32>,   // 0x124
    ivwin: ReadWrite<u32>,    // 0x128
    _res1: [u32; 53],         // 0x12c..0x200
    dcs: ReadWrite<u32>,      // 0x200
    _res2: [u32; 127],        // 0x204..0x400
    misc: ReadWrite<u32>,     // 0x400
}

const _: () = assert!(core::mem::offset_of!(Registers, dlt) == 0x028);
const _: () = assert!(core::mem::offset_of!(Registers, ictl) == 0x100);
const _: () = assert!(core::mem::offset_of!(Registers, ivwin) == 0x128);
const _: () = assert!(core::mem::offset_of!(Registers, dcs) == 0x200);
const _: () = assert!(core::mem::offset_of!(Registers, misc) == 0x400);

/// One of the two receivers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Port {
    /// `CAM0`, two data lanes.
    Cam0,
    /// `CAM1`, four data lanes.
    Cam1,
}

impl Port {
    const fn index(self) -> usize {
        self as usize
    }

    const fn base(self) -> usize {
        match self {
            Self::Cam0 => map::CAM0,
            Self::Cam1 => map::CAM1,
        }
    }

    /// The most data lanes this receiver has.
    pub const fn max_lanes(self) -> u8 {
        match self {
            Self::Cam0 => 2,
            Self::Cam1 => 4,
        }
    }

    /// The SoC interrupt it raises.
    pub const fn irq(self) -> u32 {
        match self {
            Self::Cam0 => 38,
            Self::Cam1 => 39,
        }
    }

    const fn domain(self) -> Domain {
        match self {
            Self::Cam0 => Domain::Unicam0,
            Self::Cam1 => Domain::Unicam1,
        }
    }

    const fn lp_clock(self) -> ClockId {
        match self {
            Self::Cam0 => ClockId::Cam0,
            Self::Cam1 => ClockId::Cam1,
        }
    }
}

/// What lands in memory for each pixel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Packing {
    /// Exactly as sent: RAW10 stays four pixels to five bytes.
    AsReceived,
    /// Each pixel widened to 16 bits, from the given bits per pixel (6, 7, 8,
    /// 10, 12 or 14).
    Unpack16(u8),
}

impl Packing {
    fn ipipe(self) -> u32 {
        match self {
            Self::AsReceived => 0,
            Self::Unpack16(bits) => {
                let unpack = match bits {
                    6 => 1,
                    7 => 2,
                    8 => 3,
                    10 => 4,
                    12 => 5,
                    _ => 6,
                };
                // PACK16 in the pack field, bits 7-9.
                unpack | 5 << 7
            }
        }
    }
}

/// How a receiver is set up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// Data lanes the sensor drives, 1 up to [`Port::max_lanes`].
    pub lanes: u8,
    /// The CSI-2 data type to keep, see [`data_type`].
    pub data_type: u8,
    /// The CSI-2 virtual channel to keep, 0-3.
    pub virtual_channel: u8,
    /// Bytes from the start of one line in memory to the next; a multiple of
    /// [`BUFFER_ALIGN`] no smaller than one line's payload.
    pub stride: u32,
    /// Lines per frame.
    pub height: u32,
    /// How pixels are laid out in memory.
    pub packing: Packing,
    /// Whether the sensor keeps its clock lane in high-speed mode between
    /// lines. Most do; turning it off suits sensors in non-continuous mode.
    pub continuous_clock: bool,
    /// Keep capturing every frame, rather than one per [`Unicam::trigger`].
    pub continuous: bool,
}

impl Config {
    /// Bytes one frame occupies in memory.
    pub const fn frame_bytes(&self) -> usize {
        self.stride as usize * self.height as usize
    }
}

/// Why a receiver could not be used.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The firmware would not power the receiver.
    Power(mailbox::Error),
    /// The clock manager would not provide the low-power clock.
    Clock(clock::Error),
    /// The configuration asks for more lanes than the port has, or none.
    Lanes,
    /// The buffer is misaligned, or too small for one frame.
    Buffer,
    /// No frame arrived in time.
    Timeout,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Power(error) => write!(f, "camera power: {error}"),
            Self::Clock(error) => write!(f, "camera clock: {error}"),
            Self::Lanes => f.write_str("lane count not supported by this port"),
            Self::Buffer => f.write_str("frame buffer misaligned or too small"),
            Self::Timeout => f.write_str("no frame arrived"),
        }
    }
}

/// Powers `port` and starts its low-power clock. Takes [`super::MAILBOX`] and
/// [`super::CLOCKS`], one after the other.
pub fn power_on(port: Port) -> Result<(), Error> {
    super::MAILBOX
        .lock()
        .set_domain_state(port.domain(), true)
        .map_err(Error::Power)?;
    super::CLOCKS
        .lock()
        .enable(port.lp_clock(), Source::PllD, LP_CLOCK_HZ, Mash::Integer)
        .map_err(Error::Clock)?;
    Ok(())
}

/// Stops `port`'s clock and removes its power. The receiver must be stopped.
pub fn power_off(port: Port) -> Result<(), Error> {
    super::CLOCKS
        .lock()
        .disable(port.lp_clock())
        .map_err(Error::Clock)?;
    super::MAILBOX
        .lock()
        .set_domain_state(port.domain(), false)
        .map_err(Error::Power)
}

/// What [`service`] found.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Events {
    /// A frame started.
    pub frame_start: bool,
    /// A frame finished landing in the buffer.
    pub frame_end: bool,
    /// The receiver's error flags, see [`Events::error_text`].
    pub errors: u32,
}

impl Events {
    /// A short description of the first error flag set, if any.
    pub fn error_text(&self) -> Option<&'static str> {
        [
            (STA_SBE, "single-bit ECC error corrected"),
            (STA_PBE, "multi-bit packet header error"),
            (STA_HOE, "packet header overflow"),
            (STA_PLE, "packet length error"),
            (STA_SSC, "sync lost"),
            (STA_CRCE, "payload CRC error"),
            (STA_IFO, "input FIFO overflow"),
            (STA_OFO, "output FIFO overflow"),
        ]
        .into_iter()
        .find(|(bit, _)| self.errors & bit != 0)
        .map(|(_, text)| text)
    }
}

/// Frames completed per port, for [`Unicam::frames`] and [`NextFrame`].
static FRAMES: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];

/// Error flags accumulated per port since last read.
static ERRORS: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];

/// Whether each port should leave trigger mode after its first frame.
static CONTINUOUS: [AtomicBool; 2] = [const { AtomicBool::new(false) }; 2];

/// Tasks waiting on each port's next frame.
static WAITERS: [TaskSet; 2] = [const { TaskSet::new() }; 2];

/// Reads and acknowledges `port`'s events, counts frames and wakes waiters.
///
/// Takes no lock, so it is both the receiver's interrupt handler body and the
/// polled path; the flags it clears are write-1-to-clear.
pub fn service(port: Port) -> Events {
    // SAFETY: the receiver's registers are mapped for the whole of boot; STA
    // and ISTA are write-1-to-clear, and ICTL is only modified here while a
    // capture runs.
    let regs = unsafe { &*(port.base() as *const Registers) };
    let sta = regs.sta.read();
    let ista = regs.ista.read();
    regs.sta.write(sta & STA_CLEAR);
    regs.ista.write(ista & ISTA_ALL);

    let events = Events {
        frame_start: ista & ISTA_FSI != 0,
        frame_end: ista & ISTA_FEI != 0,
        errors: sta & STA_ERRORS,
    };
    let index = port.index();
    if events.errors != 0 {
        ERRORS[index].fetch_or(events.errors, Ordering::Relaxed);
    }
    if ista != 0 && CONTINUOUS[index].load(Ordering::Relaxed) {
        let ictl = regs.ictl.read();
        if ictl & ICTL_FCM != 0 {
            // The first frame was captured in trigger mode to lock onto a
            // frame start; from here on every frame is wanted.
            regs.ictl.write((ictl & !ICTL_FCM) | ICTL_TFC);
        }
    }
    if events.frame_end {
        // Release: whoever sees the count must see the frame's data.
        FRAMES[index].fetch_add(1, Ordering::Release);
        WAITERS[index].wake_all();
    }
    events
}

/// Interrupt handler for `CAM0`, for [`super::interrupt::register`].
pub fn handle_cam0() {
    service(Port::Cam0);
}

/// Interrupt handler for `CAM1`.
pub fn handle_cam1() {
    service(Port::Cam1);
}

/// A CSI-2 receiver.
pub struct Unicam {
    port: Port,
    block: RegisterBlock<Registers>,
    running: bool,
}

impl Unicam {
    /// Creates the driver for `port`.
    ///
    /// # Safety
    ///
    /// Only one instance per port may exist, and it must be placed behind a
    /// lock before secondary cores are released.
    pub const unsafe fn new(port: Port) -> Self {
        Self {
            port,
            block: unsafe { RegisterBlock::new(port.base()) },
            running: false,
        }
    }

    /// Which receiver this is.
    pub fn port(&self) -> Port {
        self.port
    }

    /// Frames completed since boot.
    pub fn frames(&self) -> u32 {
        FRAMES[self.port.index()].load(Ordering::Acquire)
    }

    /// Error flags seen since the last call, see [`Events::error_text`].
    pub fn take_errors(&self) -> u32 {
        ERRORS[self.port.index()].swap(0, Ordering::Relaxed)
    }

    /// Brings the lanes up and starts writing frames to `buffer`.
    ///
    /// The port must have been [`power_on`]ed, and the sensor told to stream
    /// after this returns, so the receiver is listening for its first frame
    /// start. Without [`Config::continuous`], frames are captured one per
    /// [`trigger`](Self::trigger); the first is triggered here.
    ///
    /// # Safety
    ///
    /// `buffer..buffer + len` must be ARM DRAM that nothing else touches,
    /// beyond reading completed frames, until [`stop`](Self::stop) returns:
    /// the receiver writes it behind the cores' backs.
    pub unsafe fn start(
        &mut self,
        config: &Config,
        buffer: usize,
        len: usize,
    ) -> Result<(), Error> {
        if config.lanes == 0 || config.lanes > self.port.max_lanes() {
            return Err(Error::Lanes);
        }
        if !buffer.is_multiple_of(BUFFER_ALIGN)
            || !(config.stride as usize).is_multiple_of(BUFFER_ALIGN)
            || len < config.frame_bytes()
        {
            return Err(Error::Buffer);
        }
        if self.running {
            self.stop();
        }
        // SAFETY: the caller hands the buffer over; nothing in it is wanted.
        unsafe { mmu::clean_invalidate_range(buffer, len) };

        let regs = self.block.regs();
        let index = self.port.index();
        CONTINUOUS[index].store(config.continuous, Ordering::Relaxed);
        ERRORS[index].store(0, Ordering::Relaxed);

        // One clock-gate bit pair per lane, the clock lane first.
        let gates = (0..config.lanes).fold(1u32, |gates, _| gates << 2 | 1);
        self.write_clock_gate(gates);

        regs.ctrl.write(CTRL_MEM);
        regs.ana
            .write(ANA_AR | 7 << ANA_CTATADJ_SHIFT | 7 << ANA_PTATADJ_SHIFT);
        time::spin_for_micros(1_000);
        regs.ana.modify(|ana| ana & !ANA_AR);

        regs.ctrl.modify(|ctrl| ctrl | CTRL_CPR);
        regs.ctrl.modify(|ctrl| ctrl & !CTRL_CPR);
        regs.ctrl.modify(|ctrl| ctrl & !CTRL_CPE);

        // CSI-2 (CPM and DCM both zero), packet framer timeout 15, output
        // engine timeout 128.
        regs.ctrl.modify(|ctrl| {
            (ctrl & !(0x1ff << CTRL_OET_SHIFT | 0xf << CTRL_PFT_SHIFT | 0b10_1000))
                | 0xf << CTRL_PFT_SHIFT
                | 128 << CTRL_OET_SHIFT
        });
        regs.ihwin.write(0);
        regs.ivwin.write(0);
        regs.pri.write(PRI_VALUE);
        regs.ana.modify(|ana| ana & !ANA_DDL);

        // Trigger mode to begin with, so the first frame captured starts at a
        // real frame start; `service` leaves it once one has arrived.
        let line_interrupt = (config.height >> 2).max(128) & 0x1fff;
        regs.ictl.write(
            ICTL_FSIE | ICTL_FEIE | ICTL_FCM | ICTL_IBOB | line_interrupt << ICTL_LCIE_SHIFT,
        );
        regs.sta.write(STA_CLEAR);
        regs.ista.write(ISTA_ALL);

        // D-PHY timings as Linux uses them: clock and data termination enable
        // 2, settle 6, receive enable 0, in escape-clock cycles.
        regs.clt.write(6 << 8 | 2);
        regs.dlt.write(6 << 8 | 2);
        regs.ctrl.modify(|ctrl| ctrl & !CTRL_SOE);
        regs.cmp0.write(CMP_FRAME_END);

        let hs = if config.continuous_clock {
            LANE_TERMINATE | LANE_HS
        } else {
            0
        };
        regs.clk.write(LANE_ENABLE | LANE_LP | hs);
        for (lane, dat) in regs
            .dat
            .iter()
            .enumerate()
            .take(self.port.max_lanes() as usize)
        {
            dat.write(if lane < config.lanes as usize {
                LANE_ENABLE | LANE_LP | hs
            } else {
                0
            });
        }

        regs.ibls.write(config.stride);
        let start = bus::to_bus(buffer);
        regs.ibsa0.write(start);
        regs.ibea0.write(start + config.frame_bytes() as u32);
        regs.ipipe.write(config.packing.ipipe());
        regs.idi0
            .write(u32::from(config.virtual_channel & 3) << 6 | u32::from(config.data_type & 0x3f));
        regs.misc.modify(|misc| misc | MISC_FL0 | MISC_FL1);

        regs.ctrl.modify(|ctrl| ctrl | CTRL_CPE);
        regs.ictl.modify(|ictl| ictl | ICTL_LIP);
        regs.ictl.modify(|ictl| ictl | ICTL_TFC);
        self.running = true;
        Ok(())
    }

    /// Points the receiver at a new buffer of `len` bytes, latched at the next
    /// frame start. Call between a frame end and the next frame start to swap
    /// buffers without tearing.
    ///
    /// The new buffer is subject to the same contract as in
    /// [`start`](Self::start).
    pub fn set_buffer(&mut self, buffer: usize, len: usize) {
        let regs = self.block.regs();
        let start = bus::to_bus(buffer);
        regs.ibsa0.write(start);
        regs.ibea0.write(start + len as u32);
    }

    /// Captures one more frame, when not [`Config::continuous`].
    pub fn trigger(&mut self) {
        self.block.regs().ictl.modify(|ictl| ictl | ICTL_TFC);
    }

    /// Bytes of the current frame written so far: the write pointer's offset
    /// from the buffer start, or zero before the first line lands.
    pub fn write_offset(&self) -> u32 {
        let regs = self.block.regs();
        regs.ibwp.read().saturating_sub(regs.ibsa0.read())
    }

    /// Waits up to `micros` for the next frame by polling, so no interrupt is
    /// needed. The frame is in the buffer when this returns `Ok`.
    pub fn wait_frame(&mut self, micros: u64) -> Result<Events, Error> {
        let deadline = time::ticks() + micros * time::frequency_hz() / 1_000_000;
        loop {
            let events = service(self.port);
            if events.frame_end {
                return Ok(events);
            }
            if time::ticks() > deadline {
                return Err(Error::Timeout);
            }
            core::hint::spin_loop();
        }
    }

    /// Awaits the next frame end. Wakes on the receiver's interrupt if it is
    /// enabled, and on the polling tick otherwise.
    pub fn next_frame(&self) -> NextFrame {
        NextFrame {
            port: self.port,
            after: self.frames(),
        }
    }

    /// Stops the receiver and powers its lanes down. The buffer is the
    /// caller's again once this returns.
    pub fn stop(&mut self) {
        let regs = self.block.regs();
        regs.ana.modify(|ana| ana | ANA_DDL);
        regs.ctrl.modify(|ctrl| ctrl | CTRL_SOE);
        for dat in &regs.dat {
            dat.write(0);
        }
        regs.ctrl.modify(|ctrl| ctrl | CTRL_CPR);
        time::spin_for_micros(100);
        regs.ctrl.modify(|ctrl| ctrl & !CTRL_CPR);
        regs.ctrl.modify(|ctrl| ctrl & !CTRL_CPE);
        regs.dcs.write(0);
        self.write_clock_gate(0);
        self.running = false;
    }

    /// Writes this port's lane clock gates, which carry the clock manager's
    /// password.
    fn write_clock_gate(&self, gates: u32) {
        // SAFETY: each port's gate word is touched only by that port's driver,
        // which the caller holds exclusively.
        unsafe {
            RegisterBlock::<()>::write_raw(map::CMI, self.port.index() * 4, 0x5a00_0000 | gates)
        };
    }
}

/// Future returned by [`Unicam::next_frame`].
#[must_use = "futures do nothing unless awaited"]
pub struct NextFrame {
    port: Port,
    after: u32,
}

impl Future for NextFrame {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        let index = self.port.index();
        // Register before re-checking, so a frame end between the two still
        // wakes this task.
        WAITERS[index].insert_current();
        if FRAMES[index].load(Ordering::Acquire) != self.after {
            return Poll::Ready(());
        }
        // Without the interrupt, nothing else would look.
        service(self.port);
        if FRAMES[index].load(Ordering::Acquire) != self.after {
            return Poll::Ready(());
        }
        task::poll_again();
        Poll::Pending
    }
}
