// SPDX-License-Identifier: GPL-3.0-only
//! A CSI-2 camera receiver capturing one frame.
//!
//! Run with `cargo chain --example camera` on a board with a sensor on
//! `CAM1`. QEMU has no camera receiver, so there it stops at the power step.
//!
//! The receiver only receives: the sensor has to be told, over I2C, what to
//! send and to start sending. That is [`start_sensor`], deliberately empty —
//! every sensor's register list is different. Set [`CONFIG`] to match what it
//! is told to send.

#![no_std]
#![no_main]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::vec;

use cogware_bcm2837::{
    BootInfo, cpu,
    drivers::{
        self,
        unicam::{self, Config, Packing, Port, data_type},
    },
    entry, println,
};

entry!(main);

/// Which receiver the sensor is wired to.
const PORT: Port = Port::Cam1;

/// A 1640x1232 RAW10 frame on two lanes, as an IMX219 sends in its binned
/// mode. Change to match the sensor.
const CONFIG: Config = Config {
    lanes: 2,
    data_type: data_type::RAW10,
    virtual_channel: 0,
    // 1640 pixels at 10 bits is 2050 bytes, rounded up to the alignment.
    stride: 2064,
    height: 1232,
    packing: Packing::AsReceived,
    continuous_clock: true,
    continuous: false,
};

fn main(_boot: BootInfo) -> ! {
    println!();
    println!("Camera receiver: {PORT:?}, {} lanes", CONFIG.lanes);

    if let Err(error) = unicam::power_on(PORT) {
        println!("  power on : FAIL, {error}");
        println!("  (expected under QEMU, which has no camera receiver)");
        cpu::halt();
    }
    println!("  power on : ok");

    // u128 for the receiver's 16-byte alignment.
    let mut buffer = vec![0u128; CONFIG.frame_bytes().div_ceil(16)];
    let bytes = buffer.len() * 16;
    let address = buffer.as_mut_ptr() as usize;

    let mut cam = drivers::CAM1.lock();
    // SAFETY: the buffer is not touched again until `stop` returns below.
    if let Err(error) = unsafe { cam.start(&CONFIG, address, bytes) } {
        println!("  start    : FAIL, {error}");
        cpu::halt();
    }
    println!("  start    : ok, {} KiB buffer", bytes / 1024);

    match start_sensor() {
        Ok(()) => println!("  sensor   : streaming"),
        Err(why) => println!("  sensor   : not started ({why})"),
    }

    let result = cam.wait_frame(1_000_000);
    let written = cam.write_offset();
    let errors = cam.take_errors();
    cam.stop();
    drop(cam);

    match result {
        Ok(_) => println!("  frame    : ok, {written} bytes written"),
        Err(error) => println!("  frame    : {error} ({written} bytes written)"),
    }
    let events = unicam::Events {
        errors,
        ..Default::default()
    };
    if let Some(text) = events.error_text() {
        println!("  errors   : {errors:#x}, first: {text}");
    }

    // The buffer is plain memory again: look at the first line.
    let first: &[u8] = bytemuck(&buffer[..2]);
    println!("  line 0   : {first:02x?}");

    let _ = unicam::power_off(PORT);
    cpu::halt()
}

/// Configures the sensor and starts it streaming.
///
/// Empty on purpose: fill in the sensor's I2C register sequence here, through
/// `drivers::I2C0` (GPIO 0/1, or 28/29 on a Compute Module's CAM1 wiring).
fn start_sensor() -> Result<(), &'static str> {
    Err("no sensor driver in this example")
}

/// The bytes of `words`, without a dependency for it.
fn bytemuck(words: &[u128]) -> &[u8] {
    // SAFETY: u8 has no alignment or validity requirements, and the length
    // covers exactly the same memory.
    unsafe { core::slice::from_raw_parts(words.as_ptr().cast(), core::mem::size_of_val(words)) }
}
