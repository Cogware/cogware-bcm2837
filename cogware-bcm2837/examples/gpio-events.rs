// SPDX-License-Identifier: GPL-3.0-only
//! GPIO edge interrupts and pull resistors, driven from outside.
//!
//! Run with `cargo run --release --example gpio-events`, then drive the pins:
//! on a board with a jumper or a button, under the cogware QEMU with
//! `scripts/qemu-scenario.py` and `scripts/scenarios/gpio-events.txt`.
//!
//! GPIO 17 is an input with a pull-down and both edges armed; every edge
//! raises GPIO bank 0's interrupt, and each one is reported with the level of
//! GPIO 17 and of GPIO 22, an input with a pull-up.

#![no_std]
#![no_main]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

use core::sync::atomic::{AtomicU32, Ordering};

use cogware_bcm2837::{
    BootInfo, cpu,
    drivers::{
        GPIO,
        gpio::{self, Function, Pull},
        interrupt,
    },
    entry, println,
};

entry!(main);

/// The pin the edges come in on.
const EDGE_PIN: u8 = 17;
/// A pin left to its pull-up.
const PULLED_PIN: u8 = 22;
/// GPIO bank 0's interrupt: pins 0-27.
const BANK0_IRQ: u32 = 49;

static EDGES: AtomicU32 = AtomicU32::new(0);

fn main(_boot: BootInfo) -> ! {
    println!();
    println!("GPIO events");
    {
        let mut gpio = GPIO.lock();
        gpio.set_function(EDGE_PIN, Function::Input);
        gpio.set_pull(&[EDGE_PIN], Pull::Down);
        gpio.set_function(PULLED_PIN, Function::Input);
        gpio.set_pull(&[PULLED_PIN], Pull::Up);
        gpio.set_edge_detect(EDGE_PIN, true, true);
    }

    interrupt::init();
    let ok = interrupt::register(BANK0_IRQ, on_edge).and_then(|()| interrupt::enable(BANK0_IRQ));
    // SAFETY: the dispatcher is installed, and the only handler enabled takes
    // no lock.
    unsafe { interrupt::unmask() };
    println!(
        "  ready: GPIO {EDGE_PIN}={} GPIO {PULLED_PIN}={} ({ok:?})",
        u8::from(gpio::read_level(EDGE_PIN)),
        u8::from(gpio::read_level(PULLED_PIN))
    );

    let mut seen = 0;
    loop {
        let edges = EDGES.load(Ordering::Acquire);
        if edges != seen {
            seen = edges;
            println!(
                "  edge {seen}: GPIO {EDGE_PIN}={} GPIO {PULLED_PIN}={}",
                u8::from(gpio::read_level(EDGE_PIN)),
                u8::from(gpio::read_level(PULLED_PIN))
            );
        }
        cpu::wfe();
    }
}

/// Acknowledges the edge and counts it; the main loop does the reporting.
fn on_edge() {
    if gpio::read_event(EDGE_PIN) {
        gpio::clear_event(EDGE_PIN);
        EDGES.fetch_add(1, Ordering::Release);
        cpu::sev();
    }
}
