// SPDX-License-Identifier: GPL-3.0-only
//! A DSI panel in video mode: the background alone first, then a drawn plane,
//! then a page flip.
//!
//! Run with `cargo chain --example dsi-video` on a board with a panel on
//! `DISP1`. QEMU has no DSI host, so there it stops at the power step.
//!
//! The panel here is a placeholder: [`ExamplePanel`] has a mode and no
//! commands. A real panel's driver, from a device crate, implements
//! [`Panel`] with its own mode and initialisation table and drops in where
//! `ExamplePanel` is.

#![no_std]
#![no_main]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::vec;

use cogware_bcm2837::{
    BootInfo, cpu,
    devices::panel::{self, Panel},
    drivers::{
        self,
        display::{self, DsiVideo},
        dsi::{self, Dsi, PixelFormat, Port},
        framebuffer::{Canvas, Color, PixelOrder, WHITE},
        hvs::Plane,
        pixelvalve::Timing,
    },
    entry, println, time,
};

entry!(main);

/// An 800x480, one-lane, 24-bit video panel with no commands of its own.
struct ExamplePanel;

impl Panel for ExamplePanel {
    type Error = dsi::Error;

    fn video(&self) -> DsiVideo {
        DsiVideo {
            port: Port::Dsi1,
            lanes: 1,
            format: PixelFormat::Rgb888,
            timing: Timing {
                pixel_clock_hz: 27_000_000,
                hactive: 800,
                hfp: 40,
                hsync: 2,
                hbp: 46,
                vactive: 480,
                vfp: 7,
                vsync: 2,
                vbp: 21,
            },
            continuous_clock: true,
        }
    }

    fn prepare(&mut self, _host: &mut Dsi) -> Result<(), dsi::Error> {
        Ok(())
    }
}

fn main(_boot: BootInfo) -> ! {
    println!();
    println!("DSI video");

    let mut panel = ExamplePanel;
    let video = panel.video();

    // Stage 1: the compositor's background alone. No image memory is
    // involved, so a solid colour on the panel proves the PLL, the PHY, the
    // valve and the stream.
    let timing = match panel::start(&mut panel, None, 0x00_40_80) {
        Ok(timing) => timing,
        Err(error) => {
            println!("  start    : FAIL, {error}");
            println!("  (expected under QEMU, which has no DSI host)");
            cpu::halt();
        }
    };
    let pll = drivers::DSI1.lock().hs_clock_hz().unwrap_or(0);
    println!(
        "  running  : {}x{} at {}.{:03} Hz, pixel clock {} Hz, link {} Hz",
        timing.hactive,
        timing.vactive,
        timing.refresh_millihz() / 1000,
        timing.refresh_millihz() % 1000,
        timing.pixel_clock_hz,
        pll
    );
    let (output, status) = {
        let hvs = drivers::HVS.lock();
        let channel = drivers::hvs::Channel::Two;
        (hvs.output_buffer(channel), hvs.status(channel))
    };
    println!("  HVS      : output buffer {output:#010x}, {status:?}");
    time::spin_for_micros(2_000_000);

    // Stage 2: two pages of plane, drawn with the same primitives as the HDMI
    // framebuffer, shown one after the other.
    let (width, height) = (timing.hactive, timing.vactive);
    let words = (width * height) as usize;
    let mut pages = vec![0u32; words * 2];
    let (first, second) = pages.split_at_mut(words);
    draw(first, width, height, "page one", Color::rgb(160, 0, 0));
    draw(second, width, height, "page two", Color::rgb(0, 120, 0));

    let plane = Plane {
        address: first.as_ptr() as usize,
        width,
        height,
        pitch: width * 4,
        x: 0,
        y: 0,
    };
    let restart = display::stop(video.port)
        .and_then(|()| display::bring_up(&video))
        .and_then(|timing| display::start(video.port, &timing, Some(&plane), 0));
    println!("  plane    : {restart:?}");
    time::spin_for_micros(2_000_000);

    display::flip(video.port, second.as_ptr() as usize, words * 4);
    println!("  flip     : page two");
    cpu::halt()
}

/// Colour bars, a label and a border.
fn draw(pixels: &mut [u32], width: u32, height: u32, label: &str, border: Color) {
    let Some(mut canvas) = Canvas::new(pixels, width, height, width as usize, PixelOrder::Bgr)
    else {
        return;
    };
    const BARS: [Color; 7] = [
        Color::rgb(255, 255, 255),
        Color::rgb(255, 255, 0),
        Color::rgb(0, 255, 255),
        Color::rgb(0, 255, 0),
        Color::rgb(255, 0, 255),
        Color::rgb(255, 0, 0),
        Color::rgb(0, 0, 255),
    ];
    let bar = width / BARS.len() as u32;
    for (index, color) in BARS.iter().enumerate() {
        canvas.fill_rect(index as u32 * bar, 0, bar, height, *color);
    }
    for inset in 0..8 {
        canvas.draw_rect(inset, inset, width - 2 * inset, height - 2 * inset, border);
    }
    canvas.draw_text_scaled(24, height - 48, label, WHITE, Some(border), 3);
}
