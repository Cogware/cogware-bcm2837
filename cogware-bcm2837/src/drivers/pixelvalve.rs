// SPDX-License-Identifier: GPL-3.0-only
//! The pixel valves: the display timing generators.
//!
//! A pixel valve pulls composited pixels from its HVS channel and paces them
//! out in a raster — active lines, porches and sync pulses — to whatever
//! encoder it is wired to. Valve 0 serves DSI0 and DPI, valve 1 DSI1, valve 2
//! HDMI (the firmware's). QEMU has none; the programming follows the Linux
//! `vc4_crtc` driver and is untested on hardware.
#![allow(dead_code)]

use super::mmio::{ReadWrite, RegisterBlock};
use crate::{bsp::memory::map, time};

// PV_CONTROL
const CONTROL_EN: u32 = 1 << 0;
const CONTROL_FIFO_CLR: u32 = 1 << 1;
const CONTROL_CLK_SELECT_SHIFT: u32 = 2;
const CONTROL_WAIT_HSTART: u32 = 1 << 12;
const CONTROL_TRIGGER_UNDERFLOW: u32 = 1 << 13;
const CONTROL_CLR_AT_START: u32 = 1 << 14;
const CONTROL_FIFO_LEVEL_SHIFT: u32 = 15;
const CONTROL_FORMAT_SHIFT: u32 = 21;
// PV_V_CONTROL
const VCONTROL_VIDEN: u32 = 1 << 0;
const VCONTROL_CONTINUOUS: u32 = 1 << 1;
const VCONTROL_DSI: u32 = 1 << 3;
// PV_INTSTAT
const INT_VFP_START: u32 = 1 << 7;

/// The FIFO is 64 bytes deep; the HVS answers six pixels late, so the valve
/// asks for more while fewer than 64 - 3 * 6 bytes are queued, and one less
/// again because at exactly that level the FIFO overflows.
const FIFO_LEVEL_24BPP: u32 = 64 - 3 * 6 - 1;

#[repr(C)]
struct Registers {
    control: ReadWrite<u32>,     // 0x00
    v_control: ReadWrite<u32>,   // 0x04
    vsyncd_even: ReadWrite<u32>, // 0x08
    horza: ReadWrite<u32>,       // 0x0c
    horzb: ReadWrite<u32>,       // 0x10
    verta: ReadWrite<u32>,       // 0x14
    vertb: ReadWrite<u32>,       // 0x18
    verta_even: ReadWrite<u32>,  // 0x1c
    vertb_even: ReadWrite<u32>,  // 0x20
    inten: ReadWrite<u32>,       // 0x24
    intstat: ReadWrite<u32>,     // 0x28
    stat: ReadWrite<u32>,        // 0x2c
    hact_act: ReadWrite<u32>,    // 0x30
}

const _: () = assert!(core::mem::offset_of!(Registers, hact_act) == 0x30);

/// A display mode: the active area, the blanking around it and the pixel
/// clock. Porches and pulses are in pixels horizontally and lines vertically.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Timing {
    /// Pixel clock, in hertz.
    pub pixel_clock_hz: u32,
    /// Visible pixels per line.
    pub hactive: u32,
    /// Horizontal front porch.
    pub hfp: u32,
    /// Horizontal sync pulse.
    pub hsync: u32,
    /// Horizontal back porch.
    pub hbp: u32,
    /// Visible lines.
    pub vactive: u32,
    /// Vertical front porch.
    pub vfp: u32,
    /// Vertical sync pulse.
    pub vsync: u32,
    /// Vertical back porch.
    pub vbp: u32,
}

impl Timing {
    /// Pixels per line, blanking included.
    pub const fn htotal(&self) -> u32 {
        self.hactive + self.hfp + self.hsync + self.hbp
    }

    /// Lines per frame, blanking included.
    pub const fn vtotal(&self) -> u32 {
        self.vactive + self.vfp + self.vsync + self.vbp
    }

    /// Frames per second, in thousandths.
    pub const fn refresh_millihz(&self) -> u32 {
        let pixels = self.htotal() as u64 * self.vtotal() as u64;
        if pixels == 0 {
            return 0;
        }
        (self.pixel_clock_hz as u64 * 1000 / pixels) as u32
    }
}

/// Which encoder a valve is driving.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Output {
    /// DSI video, 24 bits a pixel into the DSI block.
    Dsi,
    /// DPI parallel RGB, on valve 0.
    Dpi,
}

/// One of the three pixel valves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Valve {
    /// Valve 0: DSI0 and DPI.
    Zero,
    /// Valve 1: DSI1.
    One,
    /// Valve 2: HDMI, the firmware's.
    Two,
}

impl Valve {
    const fn base(self) -> usize {
        match self {
            Self::Zero => map::PIXELVALVE0,
            Self::One => map::PIXELVALVE1,
            Self::Two => map::PIXELVALVE2,
        }
    }

    /// The SoC interrupt it raises.
    pub const fn irq(self) -> u32 {
        match self {
            Self::Zero => 45,
            Self::One => 46,
            Self::Two => 42,
        }
    }
}

/// A pixel valve.
pub struct PixelValve {
    block: RegisterBlock<Registers>,
}

impl PixelValve {
    /// Creates the driver for `valve`.
    ///
    /// # Safety
    ///
    /// Only one instance per valve may exist, and it must be placed behind a
    /// lock before secondary cores are released.
    pub const unsafe fn new(valve: Valve) -> Self {
        Self {
            block: unsafe { RegisterBlock::new(valve.base()) },
        }
    }

    /// Programs `timing` for `output` and starts the valve pulling pixels.
    /// The HVS channel feeding it should already be running.
    pub fn start(&mut self, timing: &Timing, output: Output) {
        self.stop();
        let regs = self.block.regs();
        regs.horza.write(timing.hbp << 16 | timing.hsync);
        regs.horzb.write(timing.hfp << 16 | timing.hactive);
        regs.verta.write(timing.vbp << 16 | timing.vsync);
        regs.vertb.write(timing.vfp << 16 | timing.vactive);
        let (format, clock_select, dsi) = match output {
            // DSIV_24, the DSI clock.
            Output::Dsi => (4, 0, VCONTROL_DSI),
            // Plain 24-bit, the DPI clock.
            Output::Dpi => (0, 1, 0),
        };
        regs.v_control.write(VCONTROL_CONTINUOUS | dsi);
        regs.vsyncd_even.write(0);
        if output == Output::Dsi {
            regs.hact_act.write(timing.hactive);
        }

        regs.control.write(
            CONTROL_FIFO_CLR
                | FIFO_LEVEL_24BPP << CONTROL_FIFO_LEVEL_SHIFT
                | format << CONTROL_FORMAT_SHIFT
                | CONTROL_CLR_AT_START
                | CONTROL_TRIGGER_UNDERFLOW
                | CONTROL_WAIT_HSTART
                | clock_select << CONTROL_CLK_SELECT_SHIFT,
        );
        regs.control.modify(|control| control | CONTROL_EN);
        regs.v_control.modify(|v| v | VCONTROL_VIDEN);
    }

    /// Stops video, waits a frame's worth for the FIFO to drain, and resets
    /// the valve.
    pub fn stop(&mut self) {
        let regs = self.block.regs();
        if regs.v_control.read() & VCONTROL_VIDEN != 0 {
            regs.v_control.modify(|v| v & !VCONTROL_VIDEN);
            // Linux waits 20 ms here so no pixel is left stuck between the
            // valve and the encoder.
            time::spin_for_micros(20_000);
        }
        regs.control.modify(|control| control & !CONTROL_EN);
        regs.control.modify(|control| control | CONTROL_FIFO_CLR);
    }

    /// Waits up to `micros` for the start of the next vertical front porch:
    /// the moment a new frame's plane pointer is safe to change.
    pub fn wait_vblank(&mut self, micros: u64) -> bool {
        let regs = self.block.regs();
        regs.intstat.write(INT_VFP_START);
        let deadline = time::ticks() + micros * time::frequency_hz() / 1_000_000;
        while time::ticks() < deadline {
            if regs.intstat.read() & INT_VFP_START != 0 {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }

    /// The raw status register, for reporting a stalled pipeline.
    pub fn status(&self) -> u32 {
        self.block.regs().stat.read()
    }
}
