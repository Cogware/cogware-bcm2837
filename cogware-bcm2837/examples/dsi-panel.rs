// SPDX-License-Identifier: GPL-3.0-only
//! A MIPI DSI panel brought up in command mode, then filled with colour bars.
//!
//! Run with `cargo chain --example dsi-panel` on a board with a panel on
//! `DISP1`. QEMU has no DSI host, so there it stops at the power step.
//!
//! The sequence here is the generic DCS one: reset, wake, set the pixel
//! format, display on. Real panels almost always want a block of
//! manufacturer commands as well; they go in [`panel_init`].

#![no_std]
#![no_main]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

use cogware_bcm2837::{
    BootInfo, cpu,
    drivers::{
        self,
        dsi::{self, Config, Dsi, Port, dcs},
    },
    entry, println, time,
};

entry!(main);

/// Which host the panel is wired to.
const PORT: Port = Port::Dsi1;

/// Panel size, for the colour bars.
const WIDTH: u16 = 240;
const HEIGHT: u16 = 320;

fn main(_boot: BootInfo) -> ! {
    println!();
    println!("DSI panel: {PORT:?}, command mode, low-power");

    if let Err(error) = dsi::power_on(PORT) {
        println!("  power on : FAIL, {error}");
        println!("  (expected under QEMU, which has no DSI host)");
        cpu::halt();
    }
    println!("  power on : ok");

    let mut host = drivers::DSI1.lock();
    let config = Config {
        lanes: 1,
        ..Config::default()
    };
    if let Err(error) = host.init(config) {
        println!("  init     : FAIL, {error}");
        cpu::halt();
    }
    println!("  init     : ok");

    let steps = bring_up(&mut host);
    println!(
        "  panel    : {}",
        match steps {
            Ok(()) => "on",
            Err(_) => "FAILED",
        }
    );
    if let Err(error) = steps {
        println!("  error    : {error}");
        cpu::halt();
    }

    let mut mode = [0u8; 1];
    match host.dcs_read(dcs::GET_POWER_MODE, &mut mode) {
        Ok(_) => println!(
            "  power    : {:#04x} (0x9c is awake with the display on)",
            mode[0]
        ),
        Err(error) => println!("  power    : unread, {error}"),
    }

    let started = time::uptime();
    match colour_bars(&mut host) {
        Ok(()) => println!("  bars     : drawn in {:?}", time::uptime() - started),
        Err(error) => println!("  bars     : {error}"),
    }
    cpu::halt()
}

/// The generic DCS wake-up, with [`panel_init`] spliced in.
fn bring_up(host: &mut Dsi) -> Result<(), dsi::Error> {
    host.dcs_write(dcs::SOFT_RESET, &[])?;
    time::spin_for_micros(5_000);
    panel_init(host)?;
    host.dcs_write(dcs::EXIT_SLEEP_MODE, &[])?;
    time::spin_for_micros(120_000);
    // 16 bits per pixel, RGB 5-6-5.
    host.dcs_write(dcs::SET_PIXEL_FORMAT, &[0x55])?;
    host.dcs_write(dcs::SET_DISPLAY_ON, &[])
}

/// The panel controller's own commands, sent before it leaves sleep.
///
/// Empty on purpose: copy them from the panel vendor's initialisation table,
/// as `host.generic_write(&[...])` or `host.dcs_write(command, &[...])`.
fn panel_init(_host: &mut Dsi) -> Result<(), dsi::Error> {
    Ok(())
}

/// Eight vertical bars, one line at a time so nothing large is on the stack.
fn colour_bars(host: &mut Dsi) -> Result<(), dsi::Error> {
    const BARS: [u16; 8] = [
        0xffff, 0xffe0, 0x07ff, 0x07e0, 0xf81f, 0xf800, 0x001f, 0x0000,
    ];
    let mut line = [0u8; WIDTH as usize * 2];
    for (x, pixel) in line.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        *pixel = BARS[x * BARS.len() / WIDTH as usize].to_be_bytes();
    }
    for y in 0..HEIGHT {
        host.set_window(0, y, WIDTH - 1, y)?;
        host.write_memory(&line)?;
    }
    Ok(())
}
