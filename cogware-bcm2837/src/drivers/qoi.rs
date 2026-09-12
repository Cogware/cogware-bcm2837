// SPDX-License-Identifier: GPL-3.0-only
//! QOI ("Quite OK Image") decoder.
//!
//! QOI is a lossless byte-oriented format that decodes in a single pass with
//! no entropy coding, no tables and no allocation beyond the output buffer.
//!
//! Implemented from the published specification (Dominic Szablewski, 2021).
//!
//! # Format
//!
//! A 14-byte header, then a stream of chunks, then an 8-byte end marker:
//!
//! | chunk          | tag        | payload                                  |
//! |----------------|------------|------------------------------------------|
//! | `QOI_OP_RGB`   | `11111110` | 3 bytes, alpha carries over              |
//! | `QOI_OP_RGBA`  | `11111111` | 4 bytes                                  |
//! | `QOI_OP_INDEX` | `00......` | 6-bit slot in a 64-entry recent cache    |
//! | `QOI_OP_DIFF`  | `01......` | 2 bits per channel, bias 2, wrapping     |
//! | `QOI_OP_LUMA`  | `10......` | 6-bit green delta + 1 byte of red/blue   |
//! | `QOI_OP_RUN`   | `11......` | run of 1..=62 of the previous pixel      |
//!
//! The two 8-bit tags are checked before the 2-bit tags: `11111110` and
//! `11111111` would otherwise read as runs of 63 and 64, lengths the encoder
//! never emits.
//!
//! # Output
//!
//! Decoding produces `Vec<u32>` in [`framebuffer::Color`]'s canonical
//! `0xAARRGGBB`, so the result drops straight into
//! [`framebuffer::Image`] with no second pass.
//!
//! [`framebuffer::Color`]: super::framebuffer::Color
//! [`framebuffer::Image`]: super::framebuffer::Image

use alloc::{vec, vec::Vec};
use core::fmt;

/// Bytes in the QOI header.
const HEADER_LEN: usize = 14;

/// Magic bytes at the start of every QOI file.
const MAGIC: [u8; 4] = *b"qoif";

/// Entries in the running colour cache.
const INDEX_LEN: usize = 64;

/// Largest image this decoder will allocate for, in pixels.
///
/// Caps allocation at 64 MiB of ARGB so a corrupt header cannot request a
/// 16 GiB buffer.
const MAX_PIXELS: u64 = 4096 * 4096;

/// Why decoding failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// Fewer than 14 bytes were supplied, so there is not even a header.
    TooShort,
    /// The first four bytes were not `qoif`.
    BadMagic,
    /// The channel count was not 3 (RGB) or 4 (RGBA).
    BadChannels,
    /// Width or height was zero, or the pixel count exceeded `MAX_PIXELS`.
    BadDimensions,
    /// A chunk ran off the end of the input.
    Truncated,
    /// The chunk stream described more pixels than the header declared.
    Overrun,
    /// The chunk stream ended before the declared pixel count was reached.
    Incomplete,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::TooShort => "input is shorter than a QOI header",
            Self::BadMagic => "not a QOI image (bad magic)",
            Self::BadChannels => "channel count is neither 3 nor 4",
            Self::BadDimensions => "zero or implausibly large dimensions",
            Self::Truncated => "chunk stream ended mid-chunk",
            Self::Overrun => "more pixels than the header declared",
            Self::Incomplete => "fewer pixels than the header declared",
        };
        f.write_str(text)
    }
}

/// How the encoder says the RGB channels relate to light.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Colorspace {
    /// sRGB colour with a linear alpha channel.
    Srgb,
    /// All four channels linear.
    Linear,
}

/// The fixed-size part of a QOI file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// 3 for RGB, 4 for RGBA. Purely advisory: the chunk stream can carry
    /// alpha regardless, and this decoder always produces ARGB.
    pub channels: u8,
    /// Advisory colour space tag.
    pub colorspace: Colorspace,
}

impl Header {
    /// Total pixels in the image.
    pub const fn pixel_count(&self) -> usize {
        (self.width as usize) * (self.height as usize)
    }
}

/// Parses just the header, without decoding any pixels.
///
/// Useful for sizing a layout before paying for the decode.
pub fn header(bytes: &[u8]) -> Result<Header, Error> {
    if bytes.len() < HEADER_LEN {
        return Err(Error::TooShort);
    }
    if bytes[0..4] != MAGIC {
        return Err(Error::BadMagic);
    }

    let width = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let height = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    let channels = bytes[12];
    let colorspace = if bytes[13] == 0 {
        Colorspace::Srgb
    } else {
        Colorspace::Linear
    };

    if channels != 3 && channels != 4 {
        return Err(Error::BadChannels);
    }
    if width == 0 || height == 0 {
        return Err(Error::BadDimensions);
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(Error::BadDimensions);
    }

    Ok(Header {
        width,
        height,
        channels,
        colorspace,
    })
}

/// Decodes a QOI image into canonical `0xAARRGGBB` pixels.
///
/// The returned `Vec` is exactly `width * height` long, so it can be handed
/// straight to [`framebuffer::Image::new`].
///
/// Every read from `bytes` is bounds-checked and the pixel loop is bounded by
/// the header's pixel count, so a corrupt or hostile image returns an error
/// rather than looping or reading out of bounds.
///
/// [`framebuffer::Image::new`]: super::framebuffer::Image::new
pub fn decode(bytes: &[u8]) -> Result<(Header, Vec<u32>), Error> {
    let header = header(bytes)?;
    let total = header.pixel_count();

    let mut pixels: Vec<u32> = vec![0; total];

    // Running cache, seeded with all-zero entries. That is not the seed
    // pixel: a slot holds opaque black only once such a pixel is emitted.
    let mut cache = [[0u8; 4]; INDEX_LEN];

    // Channels kept split rather than packed: DIFF and LUMA operate per
    // channel with wrapping arithmetic.
    let (mut r, mut g, mut b, mut a) = (0u8, 0u8, 0u8, 255u8);

    let mut input = HEADER_LEN;
    let mut written = 0usize;

    while written < total {
        let tag = *bytes.get(input).ok_or(Error::Truncated)?;
        input += 1;

        // Run is the only chunk that emits more than one pixel.
        let mut run = 1usize;

        if tag == 0xFE {
            // QOI_OP_RGB: alpha is inherited from the previous pixel.
            let chunk = bytes.get(input..input + 3).ok_or(Error::Truncated)?;
            input += 3;
            r = chunk[0];
            g = chunk[1];
            b = chunk[2];
        } else if tag == 0xFF {
            // QOI_OP_RGBA.
            let chunk = bytes.get(input..input + 4).ok_or(Error::Truncated)?;
            input += 4;
            r = chunk[0];
            g = chunk[1];
            b = chunk[2];
            a = chunk[3];
        } else {
            match tag >> 6 {
                // QOI_OP_INDEX.
                0b00 => {
                    let entry = cache[(tag & 0x3F) as usize];
                    r = entry[0];
                    g = entry[1];
                    b = entry[2];
                    a = entry[3];
                }
                // QOI_OP_DIFF: three 2-bit deltas, bias 2, wrapping.
                0b01 => {
                    r = r.wrapping_add((tag >> 4) & 0x03).wrapping_sub(2);
                    g = g.wrapping_add((tag >> 2) & 0x03).wrapping_sub(2);
                    b = b.wrapping_add(tag & 0x03).wrapping_sub(2);
                }
                // QOI_OP_LUMA: green moves by a 6-bit delta biased 32; red and
                // blue move relative to green by 4-bit deltas biased 8.
                0b10 => {
                    let second = *bytes.get(input).ok_or(Error::Truncated)?;
                    input += 1;
                    let dg = (tag & 0x3F).wrapping_sub(32);
                    let dr_dg = ((second >> 4) & 0x0F).wrapping_sub(8);
                    let db_dg = (second & 0x0F).wrapping_sub(8);
                    r = r.wrapping_add(dg).wrapping_add(dr_dg);
                    g = g.wrapping_add(dg);
                    b = b.wrapping_add(dg).wrapping_add(db_dg);
                }
                // QOI_OP_RUN: 1..=62 repeats. 63 and 64 are unreachable here
                // because 0xFE and 0xFF were taken above.
                _ => run = ((tag & 0x3F) as usize) + 1,
            }
        }

        if written + run > total {
            return Err(Error::Overrun);
        }

        let packed = u32::from_be_bytes([a, r, g, b]);
        pixels[written..written + run].fill(packed);
        written += run;

        // A run repeats the previous pixel, already cached at this slot, so
        // refreshing it unconditionally is harmless.
        cache[cache_slot(r, g, b, a)] = [r, g, b, a];
    }

    if written != total {
        return Err(Error::Incomplete);
    }

    Ok((header, pixels))
}

/// The cache slot a colour hashes to.
///
/// The multipliers are fixed by the specification and must match exactly for
/// streams to interoperate.
#[inline(always)]
fn cache_slot(r: u8, g: u8, b: u8, a: u8) -> usize {
    let hash = (r as usize)
        .wrapping_mul(3)
        .wrapping_add((g as usize).wrapping_mul(5))
        .wrapping_add((b as usize).wrapping_mul(7))
        .wrapping_add((a as usize).wrapping_mul(11));
    hash % INDEX_LEN
}
