// SPDX-License-Identifier: GPL-3.0-only
//! The HVS: the display compositor.
//!
//! Each of its three channels reads a display list — a short program of plane
//! descriptions in the HVS's own memory — composites those planes line by
//! line, and hands the result to a pixel valve. Channel 0 feeds DSI0 and DPI,
//! channel 1 HDMI, channel 2 DSI1.
//!
//! The firmware keeps driving channel 1 for its HDMI framebuffer, so this
//! driver touches only the channel it is asked to and leaves the HVS's global
//! set-up alone where the firmware has done it. A channel gets one plane:
//! 32-bit XRGB, unscaled, the same word layout as the HDMI framebuffer. QEMU
//! has no HVS; the register use follows the Linux `vc4_hvs` and `vc4_plane`
//! drivers and is untested on hardware.
#![allow(dead_code)]

use super::mmio::{ReadOnly, ReadWrite, RegisterBlock};
use crate::{
    arch::mmu,
    bsp::memory::{self, bus, map},
};

const DISPCTRL_ENABLE: u32 = 1 << 31;
/// AXI panic below two lines of FIFO, on all three channels.
const DISPCTRL_PANIC: u32 = 2 << 24 | 2 << 26 | 2 << 28;

const CTRLX_ENABLE: u32 = 1 << 31;
const CTRLX_RESET: u32 = 1 << 30;
const CTRLX_WIDTH_SHIFT: u32 = 12;

const BKGND_AUTOHS: u32 = 1 << 31;
const BKGND_FILL: u32 = 1 << 24;

const STATX_MODE_SHIFT: u32 = 30;
const STATX_LINE_MASK: u32 = 0xfff;

// Display-list words.
const CTL0_END: u32 = 1 << 31;
const CTL0_VALID: u32 = 1 << 30;
const CTL0_SIZE_SHIFT: u32 = 24;
const CTL0_ORDER_ABGR: u32 = 3 << 13;
const CTL0_RGBA_EXPAND_ROUND: u32 = 3 << 11;
const CTL0_UNITY: u32 = 1 << 4;
const CTL0_FORMAT_RGBA8888: u32 = 7;
const POS0_ALPHA_OPAQUE: u32 = 0xff << 24;
const POS2_ALPHA_FIXED: u32 = 1 << 30;
/// What the HVS's own context words are initialised to; it overwrites them.
const CONTEXT: u32 = 0xc0c0_c0c0;

/// Words in an unscaled plane: control, position 0 and 2, context, pointer,
/// pointer context, pitch.
const PLANE_WORDS: usize = 7;
/// Index of the pointer word within a plane, which a flip rewrites.
const PLANE_POINTER: usize = 4;

/// Words of display-list memory.
const DLIST_WORDS: usize = 0x4000 / 4;

/// Composite output buffer size in pixels, and Linux's share for channels 1
/// and 2: three 2048-pixel lines each, with channel 0 taking the rest.
const COB_PIXELS: u32 = 20_736;
const COB_LINE: u32 = 2048;

#[repr(C)]
struct ChannelRegisters {
    ctrl: ReadWrite<u32>,  // 0x40 + 0x10n
    bkgnd: ReadWrite<u32>, // 0x44
    stat: ReadOnly<u32>,   // 0x48
    base: ReadWrite<u32>,  // 0x4c
}

#[repr(C)]
struct Registers {
    dispctrl: ReadWrite<u32>,             // 0x0000
    dispstat: ReadWrite<u32>,             // 0x0004
    _res0: [u32; 6],                      // 0x0008..0x0020
    displist: [ReadWrite<u32>; 3],        // 0x0020..0x002c
    _res1: [u32; 5],                      // 0x002c..0x0040
    channels: [ChannelRegisters; 3],      // 0x0040..0x0070
    _res2: [u32; (0x2000 - 0x70) / 4],    // 0x0070..0x2000
    dlist: [ReadWrite<u32>; DLIST_WORDS], // 0x2000..0x6000
}

const _: () = assert!(core::mem::offset_of!(Registers, displist) == 0x20);
const _: () = assert!(core::mem::offset_of!(Registers, channels) == 0x40);
const _: () = assert!(core::mem::offset_of!(Registers, dlist) == 0x2000);

/// An HVS channel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Channel {
    /// Feeds pixel valve 0: DSI0, DPI.
    Zero = 0,
    /// Feeds pixel valve 2: HDMI. The firmware's.
    One = 1,
    /// Feeds pixel valve 1: DSI1.
    Two = 2,
}

impl Channel {
    /// Where this channel's display list lives. The firmware's lists are at
    /// the bottom of the memory, so ours go at the top.
    const fn dlist_start(self) -> usize {
        DLIST_WORDS - 16 * (self as usize + 1)
    }

    /// Linux's composite output buffer share for this channel, as the
    /// `DISPBASE` value: top in the high half, base in the low.
    const fn output_buffer(self) -> u32 {
        let (base, top) = match self {
            Self::Two => (0, COB_LINE * 3),
            Self::One => (COB_LINE * 3, COB_LINE * 6),
            Self::Zero => (COB_LINE * 6, COB_PIXELS),
        };
        (top - 1) << 16 | base
    }
}

/// One plane: an XRGB8888 image in memory, shown unscaled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Plane {
    /// ARM physical address of the first pixel.
    pub address: usize,
    /// Width in pixels.
    pub width: u32,
    /// Height in lines.
    pub height: u32,
    /// Bytes from one line to the next.
    pub pitch: u32,
    /// Where its top-left corner goes on the screen.
    pub x: u32,
    /// Where its top-left corner goes on the screen.
    pub y: u32,
}

impl Plane {
    /// The plane's display-list words.
    const fn words(&self) -> [u32; PLANE_WORDS] {
        [
            CTL0_VALID
                | (PLANE_WORDS as u32) << CTL0_SIZE_SHIFT
                | CTL0_RGBA_EXPAND_ROUND
                | CTL0_ORDER_ABGR
                | CTL0_UNITY
                | CTL0_FORMAT_RGBA8888,
            POS0_ALPHA_OPAQUE | (self.y & 0xfff) << 12 | (self.x & 0xfff),
            POS2_ALPHA_FIXED | (self.height & 0xfff) << 16 | (self.width & 0xfff),
            CONTEXT,
            bus::to_bus(self.address),
            CONTEXT,
            self.pitch & 0xffff,
        ]
    }

    /// Bytes the plane's image spans.
    pub const fn bytes(&self) -> usize {
        self.pitch as usize * self.height as usize
    }
}

// A 640x480 plane at the origin, from 0x1000_0000: the words Linux would
// write for the same unscaled XRGB8888 plane.
const _: () = {
    let words = Plane {
        address: 0x1000_0000,
        width: 640,
        height: 480,
        pitch: 2560,
        x: 0,
        y: 0,
    }
    .words();
    assert!(words[0] == 0x4700_7817);
    assert!(words[1] == 0xff00_0000);
    assert!(words[2] == 0x41e0_0280);
    assert!(words[4] == 0xd000_0000);
    assert!(words[6] == 2560);
};

/// What a channel is doing, from its status register.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Status {
    /// 0 disabled, 1 initialising, 2 running, 3 at end of frame.
    pub mode: u32,
    /// The line being composited.
    pub line: u32,
}

/// The HVS.
pub struct Hvs {
    block: RegisterBlock<Registers>,
}

impl Hvs {
    /// Creates the driver for the HVS at [`map::HVS`].
    ///
    /// # Safety
    ///
    /// Only one instance may exist, and it must be placed behind a lock before
    /// secondary cores are released.
    pub const unsafe fn new() -> Self {
        Self {
            block: unsafe { RegisterBlock::new(map::HVS) },
        }
    }

    /// The `DISPBASE` value of `channel`: the slice of the composite output
    /// buffer it owns, top in the high half and base in the low.
    pub fn output_buffer(&self, channel: Channel) -> u32 {
        self.block.regs().channels[channel as usize].base.read()
    }

    /// Starts `channel` compositing a `width` x `height` screen: `plane` over
    /// a background of `background` (`0xRRGGBB`). With no plane, the screen is
    /// the background alone, which needs no memory at all.
    ///
    /// Enables the HVS if the firmware has not, and gives the channel Linux's
    /// share of the output buffer if the firmware left it none.
    pub fn start(
        &mut self,
        channel: Channel,
        width: u32,
        height: u32,
        plane: Option<&Plane>,
        background: u32,
    ) {
        let regs = self.block.regs();
        if regs.dispctrl.read() & DISPCTRL_ENABLE == 0 {
            regs.dispctrl
                .modify(|ctrl| (ctrl & !(0x3f << 24)) | DISPCTRL_ENABLE | DISPCTRL_PANIC);
        }
        let chan = &regs.channels[channel as usize];
        let base = chan.base.read();
        if base >> 16 <= base & 0xffff {
            chan.base.write(channel.output_buffer());
        }

        let start = channel.dlist_start();
        let mut at = start;
        if let Some(plane) = plane {
            if plane.address < memory::vc_mem_base() {
                // SAFETY: the plane is ARM DRAM the caller owns; the HVS must
                // see what the cores wrote, not what is still in cache.
                unsafe { mmu::clean_invalidate_range(plane.address, plane.bytes()) };
            }
            for word in plane.words() {
                regs.dlist[at].write(word);
                at += 1;
            }
        }
        regs.dlist[at].write(CTL0_END);
        regs.displist[channel as usize].write(start as u32);

        chan.ctrl.write(0);
        chan.ctrl.write(CTRLX_RESET);
        chan.ctrl.write(0);
        chan.ctrl
            .write(CTRLX_ENABLE | (width & 0xfff) << CTRLX_WIDTH_SHIFT | (height & 0xfff));
        chan.bkgnd
            .write(BKGND_AUTOHS | BKGND_FILL | (background & 0xff_ffff));
    }

    /// Points `channel`'s plane at a new image of the same shape, taking
    /// effect from the next frame the HVS starts.
    ///
    /// An image in ARM DRAM is cleaned from the cache first, which is what
    /// makes what the cores drew visible; one in VideoCore memory needs none.
    pub fn flip(&mut self, channel: Channel, address: usize, bytes: usize) {
        if address < memory::vc_mem_base() {
            // SAFETY: as in `start`.
            unsafe { mmu::clean_invalidate_range(address, bytes) };
        }
        let regs = self.block.regs();
        regs.dlist[channel.dlist_start() + PLANE_POINTER].write(bus::to_bus(address));
    }

    /// Changes `channel`'s background colour (`0xRRGGBB`).
    pub fn set_background(&mut self, channel: Channel, background: u32) {
        self.block.regs().channels[channel as usize]
            .bkgnd
            .write(BKGND_AUTOHS | BKGND_FILL | (background & 0xff_ffff));
    }

    /// Stops `channel` and empties its FIFO.
    pub fn stop(&mut self, channel: Channel) {
        let chan = &self.block.regs().channels[channel as usize];
        if chan.ctrl.read() & CTRLX_ENABLE == 0 {
            return;
        }
        chan.ctrl.modify(|ctrl| ctrl | CTRLX_RESET);
        chan.ctrl.modify(|ctrl| ctrl & !CTRLX_ENABLE);
    }

    /// `channel`'s compositing state.
    pub fn status(&self, channel: Channel) -> Status {
        let stat = self.block.regs().channels[channel as usize].stat.read();
        Status {
            mode: stat >> STATX_MODE_SHIFT,
            line: stat & STATX_LINE_MASK,
        }
    }
}
