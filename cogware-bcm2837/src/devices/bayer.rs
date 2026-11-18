// SPDX-License-Identifier: GPL-3.0-only
//! Turning a raw sensor frame into something to look at.
//!
//! A raw frame is one colour per pixel through a 2x2 filter mosaic. The
//! simplest honest rendering takes each 2x2 cell as one output pixel — red,
//! the mean of the two greens, blue — which halves the resolution and needs
//! no interpolation; [`render`] does that, and decimates further to fit the
//! canvas. It is a preview, not an image pipeline: no black level, no lens
//! shading, no colour matrix, only per-channel gains.

use crate::drivers::framebuffer::{Canvas, Color};

/// Which colour sits at each position of the 2x2 filter cell, read left to
/// right then top to bottom.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BayerOrder {
    /// Red, green / green, blue.
    Rggb,
    /// Green, red / blue, green.
    Grbg,
    /// Green, blue / red, green.
    Gbrg,
    /// Blue, green / green, red.
    Bggr,
}

impl BayerOrder {
    /// The (column, row) of red and of blue within the cell.
    const fn red_blue(self) -> ((u32, u32), (u32, u32)) {
        match self {
            Self::Rggb => ((0, 0), (1, 1)),
            Self::Grbg => ((1, 0), (0, 1)),
            Self::Gbrg => ((0, 1), (1, 0)),
            Self::Bggr => ((1, 1), (0, 0)),
        }
    }
}

/// A raw frame in memory, as the receiver wrote it.
#[derive(Clone, Copy, Debug)]
pub struct Raw<'a> {
    /// The frame's bytes.
    pub data: &'a [u8],
    /// Pixels per line.
    pub width: u32,
    /// Lines.
    pub height: u32,
    /// Bytes from one line to the next.
    pub stride: u32,
    /// Bits per pixel, packed CSI-2 style: 8, 10 or 12.
    pub bits: u8,
    /// The filter layout.
    pub order: BayerOrder,
}

impl Raw<'_> {
    /// The top eight bits of pixel (x, y), or zero outside the data.
    ///
    /// CSI-2 packing keeps each pixel's high byte whole — RAW10 is four high
    /// bytes then one byte of the four pairs of low bits, RAW12 two then one —
    /// so the high byte is a single load.
    fn sample(&self, x: u32, y: u32) -> u8 {
        let column = match self.bits {
            10 => x / 4 * 5 + x % 4,
            12 => x / 2 * 3 + x % 2,
            _ => x,
        } as usize;
        self.data
            .get(y as usize * self.stride as usize + column)
            .copied()
            .unwrap_or(0)
    }
}

/// White-balance gains, 8.8 fixed point: 256 is unity.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Gains {
    /// Red gain.
    pub red: u16,
    /// Green gain.
    pub green: u16,
    /// Blue gain.
    pub blue: u16,
}

impl Gains {
    /// No correction.
    pub const UNITY: Self = Self {
        red: 256,
        green: 256,
        blue: 256,
    };
}

/// Draws `raw` into the top-left of `canvas`, one pixel per 2x2 cell,
/// skipping cells as needed to fit. Returns the size drawn.
pub fn render(raw: &Raw<'_>, canvas: &mut Canvas<'_>, gains: Gains) -> (u32, u32) {
    let (cells_x, cells_y) = (raw.width / 2, raw.height / 2);
    if cells_x == 0 || cells_y == 0 || canvas.width() == 0 || canvas.height() == 0 {
        return (0, 0);
    }
    let step = cells_x
        .div_ceil(canvas.width())
        .max(cells_y.div_ceil(canvas.height()))
        .max(1);
    let (out_w, out_h) = (cells_x / step, cells_y / step);
    let ((rx, ry), (bx, by)) = raw.order.red_blue();
    let scale = |value: u32, gain: u16| (value * u32::from(gain) / 256).min(255) as u8;

    let order = canvas.order();
    for oy in 0..out_h {
        let y = oy * step * 2;
        let Some(line) = canvas.scanline(oy) else {
            break;
        };
        for (ox, word) in line.iter_mut().take(out_w as usize).enumerate() {
            let x = ox as u32 * step * 2;
            let red = u32::from(raw.sample(x + rx, y + ry));
            let blue = u32::from(raw.sample(x + bx, y + by));
            // The greens are the cell's other two positions.
            let green = (u32::from(raw.sample(x + (1 - rx), y + ry))
                + u32::from(raw.sample(x + rx, y + (1 - ry))))
                / 2;
            *word = order.encode(Color::rgb(
                scale(red, gains.red),
                scale(green, gains.green),
                scale(blue, gains.blue),
            ));
        }
    }
    (out_w, out_h)
}
