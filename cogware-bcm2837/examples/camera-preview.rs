// SPDX-License-Identifier: GPL-3.0-only
//! A camera frame on the HDMI screen: capture through a [`Sensor`], render
//! the raw Bayer data, show it.
//!
//! Run with `cargo chain --example camera-preview` on a board. A sensor
//! driver comes from a device crate; plug it in at [`sensor`]. Without one —
//! and always under QEMU — the example renders a synthetic RAW10 frame
//! instead, four coloured quadrants on a known filter layout, and checks the
//! rendered colours, which exercises everything after the receiver.

#![no_std]
#![no_main]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::vec;

use cogware_bcm2837::{
    BootInfo, cpu,
    devices::{
        bayer::{self, BayerOrder, Gains, Raw},
        sensor::{self, Sensor, SensorMode},
    },
    drivers::{
        self,
        framebuffer::{Color, WHITE},
        unicam::{Port, data_type},
    },
    entry, println,
};

entry!(main);

/// The receiver the sensor is wired to.
const PORT: Port = Port::Cam1;

fn main(_boot: BootInfo) -> ! {
    println!();
    println!("Camera preview");

    let mut fb = drivers::FRAMEBUFFER.lock();
    if let Err(error) = fb.init(1280, 720, 1) {
        drop(fb);
        println!("  display  : FAIL, {error}");
        cpu::halt();
    }
    fb.clear(Color::rgb(16, 16, 16));
    drop(fb);

    let shown = match capture_live() {
        Ok(()) => "live frame",
        Err(why) => {
            println!("  camera   : {why}; rendering a synthetic frame");
            if synthetic() {
                "synthetic frame, colours checked"
            } else {
                "synthetic frame, colours WRONG"
            }
        }
    };
    if let Some(mut canvas) = drivers::FRAMEBUFFER.lock().canvas() {
        canvas.draw_text_scaled(16, 690, shown, WHITE, None, 2);
    }
    println!("  shown    : {shown}");
    cpu::halt()
}

/// The sensor driver, from a device crate. None here.
fn sensor() -> Option<impl Sensor<Error = &'static str>> {
    None::<NoSensor>
}

/// Stands in for a real driver's type.
struct NoSensor;

impl Sensor for NoSensor {
    type Error = &'static str;
    fn identify(&mut self) -> Result<u32, Self::Error> {
        Err("no sensor")
    }
    fn configure(&mut self) -> Result<SensorMode, Self::Error> {
        Err("no sensor")
    }
    fn start(&mut self) -> Result<(), Self::Error> {
        Err("no sensor")
    }
    fn stop(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// One frame from the real sensor, rendered to the screen.
fn capture_live() -> Result<(), &'static str> {
    let mut sensor = sensor().ok_or("no sensor driver plugged in")?;
    // Big enough for an 8 MP RAW10 frame; u128 for the receiver's alignment.
    let mut buffer = vec![0u128; 3280 * 2464 * 10 / 8 / 16 + 1024];
    let bytes = buffer.len() * 16;
    // SAFETY: the buffer is not touched until `capture` returns.
    let mode = unsafe {
        sensor::capture(
            &mut sensor,
            &drivers::CAM1,
            PORT,
            buffer.as_mut_ptr() as usize,
            bytes,
            1_000_000,
        )
    }
    .map_err(|_| "capture failed")?;
    let order = mode.bayer.ok_or("not a raw mode")?;
    // SAFETY: plain memory again now the receiver has stopped.
    let data = unsafe { core::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), bytes) };
    let raw = Raw {
        data,
        width: mode.width,
        height: mode.height,
        stride: mode.stride(),
        bits: mode.bits,
        order,
    };
    let mut fb = drivers::FRAMEBUFFER.lock();
    let mut canvas = fb.canvas().ok_or("no display")?;
    bayer::render(&raw, &mut canvas, Gains::UNITY);
    Ok(())
}

/// Renders a synthetic RGGB RAW10 frame whose four quadrants are red, green,
/// blue and white, and checks one pixel from each.
fn synthetic() -> bool {
    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;
    let mode = SensorMode {
        width: WIDTH,
        height: HEIGHT,
        data_type: data_type::RAW10,
        bits: 10,
        lanes: 2,
        bayer: Some(BayerOrder::Rggb),
        continuous_clock: true,
    };
    let stride = mode.stride();
    let mut data = vec![0u8; (stride * HEIGHT) as usize];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let (red, green, blue) = match (x < WIDTH / 2, y < HEIGHT / 2) {
                (true, true) => (240, 0, 0),
                (false, true) => (0, 240, 0),
                (true, false) => (0, 0, 240),
                (false, false) => (240, 240, 240),
            };
            let value = match (x % 2, y % 2) {
                (0, 0) => red,
                (1, 1) => blue,
                _ => green,
            };
            // RAW10 keeps each pixel's high byte whole; the fifth byte of each
            // group holds the low bits, left zero.
            data[(y * stride + x / 4 * 5 + x % 4) as usize] = value;
        }
    }
    let raw = Raw {
        data: &data,
        width: WIDTH,
        height: HEIGHT,
        stride,
        bits: 10,
        order: BayerOrder::Rggb,
    };

    let mut fb = drivers::FRAMEBUFFER.lock();
    let Some(mut canvas) = fb.canvas() else {
        return false;
    };
    let (w, h) = bayer::render(&raw, &mut canvas, Gains::UNITY);
    let expected = [
        (w / 4, h / 4, Color::rgb(240, 0, 0)),
        (3 * w / 4, h / 4, Color::rgb(0, 240, 0)),
        (w / 4, 3 * h / 4, Color::rgb(0, 0, 240)),
        (3 * w / 4, 3 * h / 4, Color::rgb(240, 240, 240)),
    ];
    let ok = expected
        .iter()
        .all(|&(x, y, color)| canvas.read(x, y) == Some(color));
    drop(fb);
    println!(
        "  rendered : {w}x{h} from {WIDTH}x{HEIGHT} RAW10, quadrants {}",
        if ok { "ok" } else { "FAIL" }
    );
    ok
}
