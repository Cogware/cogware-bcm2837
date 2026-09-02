// SPDX-License-Identifier: MIT OR Apache-2.0
//! A small bootable display demo: ask the VideoCore for a framebuffer and draw.
//!
//! Run with `cargo run --release --example display`.
//!
//! The scene keeps to the public BSP API: a double-buffered framebuffer mode,
//! an alpha-composited QOI logo, text drawn over image pixels, a copied opaque
//! image and a few primitive shapes. Everything is reported over UART, so a
//! headless QEMU run still shows the framebuffer path reached the firmware.
//!
//! Cores 1-3 are left parked at the MMU gate; see the `multicore` example for
//! how to release them.

#![no_std]
#![no_main]

extern crate alloc;

use cogware_bcm2837::{
    BootInfo, cpu,
    drivers::{
        self,
        framebuffer::{BLACK, BLUE, Color, Framebuffer, GREEN, Image, RED, WHITE},
    },
    entry, println, time,
};

entry!(main);

const MODE_WIDTH: u32 = 1280;
const MODE_HEIGHT: u32 = 720;
const MODE_PAGES: u32 = 2;
const TILE_SIZE: u32 = 64;
const TILE_PIXELS: usize = (TILE_SIZE as usize) * (TILE_SIZE as usize);
const OPAQUE_TILE: [u32; TILE_PIXELS] = opaque_tile();

fn main(boot: BootInfo) -> ! {
    println!();
    println!("CogWare BCM2837 framebuffer example");
    println!();
    println!("  core            : {}", boot.core_id);
    println!("  exception level : EL{}", boot.exception_level);

    let started = time::ticks();
    let init = {
        let mut fb = drivers::FRAMEBUFFER.lock();
        fb.init(MODE_WIDTH, MODE_HEIGHT, MODE_PAGES)
    };
    if let Err(error) = init {
        println!("  display         : mode set failed ({error})");
        println!("  no framebuffer is required; halting over UART.");
        cpu::halt()
    }

    let Some(info) = drivers::FRAMEBUFFER.lock().info() else {
        println!("  display         : mode set succeeded but no mode is live");
        cpu::halt()
    };
    report_mode(info);

    let logo_started = time::ticks();
    let logo = drivers::qoi::decode(drivers::LOGO_QOI);
    let logo_ticks = time::ticks() - logo_started;

    let draw_result = draw_scene(&logo);
    let draw_ticks = time::ticks() - started;

    println!();
    match draw_result {
        Ok(()) => println!("Scene drawn and presented."),
        Err(error) => println!("Scene drawn, but the page flip failed: {error}"),
    }
    match &logo {
        Ok((header, _)) => println!(
            "  logo            : {}x{} QOI decoded in {} us",
            header.width,
            header.height,
            micros(logo_ticks)
        ),
        Err(error) => println!("  logo            : decode failed ({error})"),
    }
    println!("  total time      : {} us", micros(draw_ticks));
    println!("  framebuffer API : clear, fill_rect, draw_rect, blit, blit_blend, text,");
    println!("                    scanline, read");

    cpu::halt()
}

fn report_mode(info: drivers::framebuffer::Info) {
    println!("  display         : ready");
    println!(
        "  mode            : {}x{}x{}",
        info.width, info.height, info.depth
    );
    println!("  pitch           : {} bytes", info.pitch);
    println!("  pixel order     : {:?}", info.order);
    println!(
        "  buffer          : ARM {:#012x}, bus {:#010x}, {} bytes",
        info.base, info.bus_address, info.size
    );
    println!(
        "  pages           : {} (draw {}, show {})",
        info.pages, info.draw_page, info.visible_page
    );
}

fn draw_scene(
    logo: &Result<(drivers::qoi::Header, alloc::vec::Vec<u32>), drivers::qoi::Error>,
) -> Result<(), drivers::framebuffer::Error> {
    let mut fb = drivers::FRAMEBUFFER.lock();
    if fb
        .info()
        .is_some_and(|info| info.pages > 1 && info.draw_page == info.visible_page)
    {
        fb.flip()?;
    }

    let (width, height) = (fb.width(), fb.height());
    fb.clear(Color::rgb(9, 14, 34));
    draw_gradient(&mut fb, width, height);
    draw_logo(&mut fb, logo, width, height);
    draw_per_pixel_band(&mut fb, width, height);
    draw_primitives(&mut fb, width, height);
    fb.draw_rect(0, 0, width, height, Color::rgb(90, 100, 130));

    let result = fb.flip();
    drop(fb);
    result
}

/// Draws a band whose every pixel is a different colour, through borrowed rows.
///
/// This is the case `fill_rect` cannot express — the colour is a function of
/// both axes — and the case [`Framebuffer::pixel`] is worst at, being one call
/// per pixel. [`Framebuffer::scanline`] hands over the row instead, so the
/// inner loop is a plain slice write.
///
/// Note where the encoding happens. The row borrow is exclusive for as long as
/// it lives, so anything else the loop needs from the framebuffer has to be
/// taken first: the palette is encoded up front, once, and the inner loop only
/// indexes it. That is not a workaround, it is the borrow checker enforcing
/// what the hardware already required.
fn draw_per_pixel_band(fb: &mut Framebuffer, width: u32, height: u32) {
    /// Colours in the precomputed palette.
    const SHADES: usize = 64;

    let top = height.saturating_sub(height / 12);
    let depth = height - top;

    let mut palette = [0u32; SHADES];
    for (index, slot) in palette.iter_mut().enumerate() {
        let level = (index * 255 / (SHADES - 1)) as u8;
        *slot = fb.encode(Color::rgb(level / 3, level, 255 - level));
    }

    for y in top..height {
        let down = ((y - top) * 255 / depth.max(1)) as usize;
        let Some(row) = fb.scanline(y) else { continue };
        for (x, slot) in row.iter_mut().enumerate() {
            // An interference pattern: the point is only that it is per pixel
            // and cheap, so the row write is what is being demonstrated.
            let across = x * 255 / width.max(1) as usize;
            *slot = palette[((across ^ down) >> 2) % SHADES];
        }
    }

    // Read one back. Without this there is no way for a caller to composite
    // over what it has already drawn without the driver's own blend.
    let sample = (width / 2, top + depth / 2);
    match fb.read(sample.0, sample.1) {
        Some(colour) => println!(
            "  read back       : ({}, {}) = #{:02x}{:02x}{:02x}",
            sample.0,
            sample.1,
            colour.red(),
            colour.green(),
            colour.blue()
        ),
        None => println!("  read back       : outside the surface"),
    }
}

fn draw_gradient(fb: &mut Framebuffer, width: u32, height: u32) {
    let top = Color::rgb(12, 18, 44);
    let bottom = Color::rgb(2, 3, 12);
    for y in 0..height {
        let amount = (y * 255 / height.max(1)) as u8;
        fb.fill_rect(0, y, width, 1, top.mix(bottom, amount));
    }

    let bars = [
        WHITE,
        Color::rgb(255, 220, 40),
        Color::rgb(40, 220, 255),
        GREEN,
        Color::rgb(255, 80, 220),
        RED,
        BLUE,
        BLACK,
    ];
    let bar_width = width / bars.len() as u32;
    for (index, colour) in bars.iter().enumerate() {
        let x = index as u32 * bar_width;
        let this_width = if index == bars.len() - 1 {
            width - x
        } else {
            bar_width
        };
        fb.fill_rect(x, 0, this_width, 54, *colour);
    }
}

fn draw_logo(
    fb: &mut Framebuffer,
    logo: &Result<(drivers::qoi::Header, alloc::vec::Vec<u32>), drivers::qoi::Error>,
    width: u32,
    height: u32,
) {
    let title = "VideoCore framebuffer";
    fb.draw_text_scaled(
        (width.saturating_sub(Framebuffer::text_width(title, 2))) / 2,
        86,
        title,
        WHITE,
        None,
        2,
    );

    match logo {
        Ok((header, pixels)) => {
            let Some(image) = Image::new(pixels, header.width, header.height) else {
                fb.draw_text(32, 120, "logo geometry was rejected", RED, None);
                return;
            };
            let Some(art) = image.trimmed() else {
                fb.draw_text(32, 120, "logo contains no opaque pixels", RED, None);
                return;
            };
            let x = (width.saturating_sub(art.width)) / 2;
            let y = ((height.saturating_sub(art.height)) / 2).saturating_sub(12);
            fb.blit_blend(x, y, &art);

            let caption = "text over alpha";
            fb.draw_text_scaled(
                x + (art
                    .width
                    .saturating_sub(Framebuffer::text_width(caption, 3)))
                    / 2,
                y + art.height / 2 - 12,
                caption,
                WHITE,
                None,
                3,
            );
        }
        Err(error) => fb.draw_fmt(
            32,
            120,
            format_args!("logo decode failed: {error}"),
            RED,
            None,
        ),
    }
}

fn draw_primitives(fb: &mut Framebuffer, width: u32, height: u32) {
    let Some(tile) = Image::new(&OPAQUE_TILE, TILE_SIZE, TILE_SIZE) else {
        return;
    };
    let y = height.saturating_sub(150);
    let x = 72;

    fb.blit(x, y, &tile);
    fb.draw_rect(x - 1, y - 1, TILE_SIZE + 2, TILE_SIZE + 2, WHITE);
    fb.draw_text(x, y + TILE_SIZE + 10, "blit: opaque copy", WHITE, None);

    let panel_x = x + 260;
    fb.fill_rect(panel_x, y, 168, 64, Color::argb(210, 20, 40, 90));
    fb.draw_rect(panel_x, y, 168, 64, Color::rgb(140, 180, 255));
    fb.draw_text(panel_x + 12, y + 20, "filled rect", WHITE, None);

    let line_x = width.saturating_sub(300);
    for step in 0..8 {
        let offset = step * 8;
        fb.line(
            line_x as i32,
            (y + offset) as i32,
            (line_x + 220) as i32,
            (y + 100 - offset) as i32,
            Color::rgb(255, 190, 80),
        );
    }
    fb.draw_rect(line_x, y, 220, 100, Color::rgb(255, 190, 80));
    fb.draw_text(line_x, y + 112, "lines + clipped edges", WHITE, None);
}

const fn opaque_tile() -> [u32; TILE_PIXELS] {
    let mut pixels = [0; TILE_PIXELS];
    let mut index = 0;
    while index < TILE_PIXELS {
        let x = (index as u32) % TILE_SIZE;
        let y = (index as u32) / TILE_SIZE;
        let red = (60 + x * 3) as u8;
        let green = (40 + y * 3) as u8;
        let blue = if ((x / 8) + (y / 8)).is_multiple_of(2) {
            190
        } else {
            80
        };
        pixels[index] = Color::rgb(red, green, blue).to_argb();
        index += 1;
    }
    pixels
}

fn micros(ticks: u64) -> u64 {
    ticks * 1_000_000 / time::frequency_hz().max(1)
}
