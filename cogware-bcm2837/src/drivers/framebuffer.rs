// SPDX-License-Identifier: MIT OR Apache-2.0
//! VideoCore framebuffer.
//!
//! The ARM cannot program the display; the VideoCore owns it. Setting a mode is
//! a property-mailbox transaction, not a register write, and the mode tags must
//! travel in the *same* message as the allocation tag
//! ([`mailbox::Mailbox::call_many`]) so the allocation observes the mode that
//! precedes it. Splitting them across messages allocates a stale-sized buffer
//! on some firmware revisions.
//!
//! What comes back is a *bus* address inside the VideoCore's private DRAM
//! reservation. Two consequences:
//!
//! * it must go through [`memory::bus::to_physical`] before the ARM
//!   dereferences it;
//! * it lands in the range [`crate::arch::mmu::memory_map`] already maps
//!   `NormalNonCacheable`, so pixels are written with ordinary stores and need
//!   no cache maintenance. Non-cacheable rather than Device, because a
//!   framebuffer wants unaligned access and bulk copies; non-cacheable rather
//!   than Normal, because the GPU is not in the ARM's coherency domain.
//!
//! [`Framebuffer::init`] refuses a framebuffer that falls outside that mapped
//! window instead of letting the first store take a translation fault.
//!
//! # Double buffering
//!
//! The buffer is allocated several screens tall while only one screen is
//! displayed. Drawing goes to a hidden page and [`Framebuffer::flip`] moves the
//! display with `SET_VIRTUAL_OFFSET`, so presenting a frame changes one number
//! instead of copying a megabyte.
//!
//! # Emulation
//!
//! QEMU's `raspi3b` implements the mailbox property interface and a linear
//! framebuffer, so everything here runs under emulation. It implements no part
//! of the VideoCore IV 3D pipeline (no V3D, no QPU), so graphics go no further
//! without real hardware.

use core::fmt::Write;

use super::{
    font,
    mailbox::{self, Tag, TagIo},
};
use crate::bsp::memory;

/// Bits per pixel this driver requests and understands.
///
/// Only 32 is supported: the one depth where a pixel is a single aligned
/// `u32`.
pub const DEPTH_BITS: u32 = 32;

/// Alignment requested of the firmware for the framebuffer allocation.
const ALLOC_ALIGN: u32 = 4096;

/// Largest number of pages [`Framebuffer::init`] will accept.
const MAX_PAGES: u32 = 4;

/// A colour, held canonically as `0xAARRGGBB` regardless of what the display
/// is actually configured for.
///
/// The firmware decides the byte order of the framebuffer and reports it back;
/// [`Framebuffer`] converts on the way out. Keeping one canonical form means
/// callers never have to know which order was negotiated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct Color(u32);

impl Color {
    /// An opaque colour from its components.
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self::argb(0xFF, red, green, blue)
    }

    /// A colour with an explicit alpha byte.
    ///
    /// The Pi's primary plane ignores alpha, but the byte occupies the word
    /// either way, so it is written rather than left undefined.
    pub const fn argb(alpha: u8, red: u8, green: u8, blue: u8) -> Self {
        Self((alpha as u32) << 24 | (red as u32) << 16 | (green as u32) << 8 | blue as u32)
    }

    /// The red component.
    pub const fn red(self) -> u8 {
        (self.0 >> 16) as u8
    }

    /// The green component.
    pub const fn green(self) -> u8 {
        (self.0 >> 8) as u8
    }

    /// The blue component.
    pub const fn blue(self) -> u8 {
        self.0 as u8
    }

    /// The alpha component.
    pub const fn alpha(self) -> u8 {
        (self.0 >> 24) as u8
    }

    /// The canonical `0xAARRGGBB` word.
    pub const fn to_argb(self) -> u32 {
        self.0
    }

    /// Blends towards `other` by `amount` in 0..=255, component-wise.
    ///
    /// Integer-only, to avoid a float round trip in a per-pixel loop.
    pub const fn mix(self, other: Self, amount: u8) -> Self {
        const fn lerp(a: u8, b: u8, t: u8) -> u8 {
            ((a as u32 * (255 - t as u32) + b as u32 * t as u32) / 255) as u8
        }
        Self::argb(
            lerp(self.alpha(), other.alpha(), amount),
            lerp(self.red(), other.red(), amount),
            lerp(self.green(), other.green(), amount),
            lerp(self.blue(), other.blue(), amount),
        )
    }
}

/// Black.
pub const BLACK: Color = Color::rgb(0, 0, 0);
/// White.
pub const WHITE: Color = Color::rgb(255, 255, 255);
/// Red.
pub const RED: Color = Color::rgb(255, 0, 0);
/// Green.
pub const GREEN: Color = Color::rgb(0, 255, 0);
/// Blue.
pub const BLUE: Color = Color::rgb(0, 0, 255);

/// Order in which the firmware stores the colour components of a pixel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PixelOrder {
    /// Blue in the least significant byte: the word is `0xAARRGGBB`, which is
    /// [`Color`]'s canonical form, so no conversion is needed.
    Bgr,
    /// Red in the least significant byte: the word is `0xAABBGGRR`, so red and
    /// blue are exchanged on the way to memory.
    Rgb,
}

impl PixelOrder {
    /// The tag value the firmware uses for this order.
    const fn as_tag(self) -> u32 {
        match self {
            Self::Bgr => 0,
            Self::Rgb => 1,
        }
    }

    /// Interprets a value reported by the firmware.
    const fn from_tag(value: u32) -> Self {
        if value == 0 { Self::Bgr } else { Self::Rgb }
    }

    /// Converts a word read back out of the surface into a canonical colour.
    #[inline(always)]
    const fn decode(self, word: u32) -> Color {
        match self {
            Self::Bgr => Color::argb(
                (word >> 24) as u8,
                (word >> 16) as u8,
                (word >> 8) as u8,
                word as u8,
            ),
            Self::Rgb => Color::argb(
                (word >> 24) as u8,
                word as u8,
                (word >> 8) as u8,
                (word >> 16) as u8,
            ),
        }
    }

    /// Converts a canonical colour into the word the hardware expects.
    #[inline(always)]
    const fn encode(self, color: Color) -> u32 {
        match self {
            Self::Bgr => color.to_argb(),
            Self::Rgb => {
                (color.alpha() as u32) << 24
                    | (color.blue() as u32) << 16
                    | (color.green() as u32) << 8
                    | color.red() as u32
            }
        }
    }
}

/// Why a framebuffer operation failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The property mailbox transaction itself failed.
    Mailbox(mailbox::Error),
    /// The firmware returned a null pointer or a zero-sized allocation.
    AllocationFailed,
    /// The firmware granted a mode other than the one asked for, and the
    /// difference matters — a depth other than [`DEPTH_BITS`], or a pitch that
    /// is not a whole number of pixels.
    ModeRejected,
    /// The allocation lies outside the DRAM the kernel maps for the VideoCore,
    /// so writing to it would fault. See [`crate::arch::mmu::memory_map`].
    OutsideMappedMemory,
    /// A geometry argument was zero, or too large to be plausible.
    BadGeometry,
    /// The framebuffer has not been initialised.
    NotReady,
}

impl From<mailbox::Error> for Error {
    fn from(error: mailbox::Error) -> Self {
        Self::Mailbox(error)
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Mailbox(inner) => write!(f, "{inner}"),
            Self::AllocationFailed => f.write_str("firmware allocated no framebuffer"),
            Self::ModeRejected => f.write_str("firmware granted an unusable mode"),
            Self::OutsideMappedMemory => {
                f.write_str("framebuffer is outside mapped VideoCore DRAM")
            }
            Self::BadGeometry => f.write_str("implausible framebuffer geometry"),
            Self::NotReady => f.write_str("framebuffer not initialised"),
        }
    }
}

/// A borrowed 32-bit image in [`Color`]'s canonical `0xAARRGGBB` order.
///
/// Borrowed rather than owned so an image can live in `.rodata`, on the stack,
/// or on the heap.
///
/// A sub-rectangle keeps the parent's `stride` while narrowing `width` and
/// `height`, so cropping copies nothing.
#[derive(Clone, Copy)]
pub struct Image<'a> {
    /// Pixels, row-major. The first pixel of row `n` is at `n * stride`.
    pub pixels: &'a [u32],
    /// Visible width in pixels.
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
    /// Distance in pixels between the starts of consecutive rows. Equal to
    /// `width` for a whole image, larger for a crop.
    pub stride: u32,
}

impl<'a> Image<'a> {
    /// Wraps a pixel buffer, checking that it is the size the geometry implies.
    pub fn new(pixels: &'a [u32], width: u32, height: u32) -> Option<Self> {
        let expected = (width as usize).checked_mul(height as usize)?;
        if expected != pixels.len() || width == 0 || height == 0 {
            return None;
        }
        Some(Self {
            pixels,
            width,
            height,
            stride: width,
        })
    }

    /// Narrows this image to a sub-rectangle, without copying.
    ///
    /// Returns `None` if the rectangle is empty or leaves the parent.
    pub fn sub(&self, x: u32, y: u32, width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        if x.checked_add(width)? > self.width || y.checked_add(height)? > self.height {
            return None;
        }
        let offset = (y as usize).checked_mul(self.stride as usize)? + x as usize;
        Some(Self {
            pixels: self.pixels.get(offset..)?,
            width,
            height,
            stride: self.stride,
        })
    }

    /// The tightest rectangle containing every pixel that is not fully
    /// transparent, as `(x, y, width, height)`.
    ///
    /// Returns `None` if every pixel has an alpha of zero.
    pub fn opaque_bounds(&self) -> Option<(u32, u32, u32, u32)> {
        let (mut min_x, mut min_y) = (self.width, self.height);
        let (mut max_x, mut max_y) = (0u32, 0u32);
        let mut found = false;

        for y in 0..self.height {
            let row = (y as usize) * (self.stride as usize);
            for x in 0..self.width {
                // The alpha byte is the top byte of the canonical word.
                if self.pixels[row + x as usize] >> 24 == 0 {
                    continue;
                }
                found = true;
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }

        if !found {
            return None;
        }
        Some((min_x, min_y, max_x - min_x + 1, max_y - min_y + 1))
    }

    /// Trims the fully transparent margin, returning the image unchanged if it
    /// has none and `None` if it is entirely transparent.
    pub fn trimmed(&self) -> Option<Self> {
        let (x, y, width, height) = self.opaque_bounds()?;
        self.sub(x, y, width, height)
    }
}

/// The mode a live framebuffer is running in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Info {
    /// ARM physical address of the whole allocation.
    pub base: usize,
    /// Size of the whole allocation in bytes.
    pub size: usize,
    /// VideoCore bus address the firmware reported.
    pub bus_address: u32,
    /// Visible width in pixels.
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
    /// Bytes per row, which the firmware may pad beyond `width * 4`.
    pub pitch: u32,
    /// Bits per pixel; always [`DEPTH_BITS`].
    pub depth: u32,
    /// Byte order the firmware chose.
    pub order: PixelOrder,
    /// How many screens tall the allocation is.
    pub pages: u32,
    /// Which page is currently being drawn into.
    pub draw_page: u32,
    /// Which page is currently on the display.
    pub visible_page: u32,
}

/// The VideoCore framebuffer.
///
/// Held inside a [`crate::sync::SpinLock`] in [`crate::drivers::FRAMEBUFFER`].
/// Starts out unconfigured; [`Framebuffer::init`] brings it up, and every
/// drawing method is a no-op until it has.
pub struct Framebuffer {
    base: usize,
    size: usize,
    bus_address: u32,
    width: u32,
    height: u32,
    pitch: u32,
    stride_words: usize,
    order: PixelOrder,
    pages: u32,
    draw_page: u32,
    visible_page: u32,
    ready: bool,
}

impl Framebuffer {
    /// Creates the unconfigured framebuffer.
    ///
    /// # Safety
    ///
    /// Only one may exist, because it takes exclusive ownership of the
    /// firmware's single display allocation.
    pub const unsafe fn new() -> Self {
        Self {
            base: 0,
            size: 0,
            bus_address: 0,
            width: 0,
            height: 0,
            pitch: 0,
            stride_words: 0,
            order: PixelOrder::Bgr,
            pages: 1,
            draw_page: 0,
            visible_page: 0,
            ready: false,
        }
    }

    /// Sets the mode and allocates the framebuffer.
    ///
    /// `pages` is how many screens tall to allocate: 1 for a single buffer, 2
    /// for the double buffering [`Framebuffer::flip`] drives.
    ///
    /// Re-initialising a live framebuffer releases the old allocation first:
    /// the firmware tracks exactly one, so allocating over it fails or leaks.
    ///
    /// Failure before the release leaves the previous mode untouched; failure
    /// after it leaves no display, which is non-fatal — the kernel carries on
    /// over the UART.
    pub fn init(&mut self, width: u32, height: u32, pages: u32) -> Result<(), Error> {
        if width == 0 || height == 0 || pages == 0 || pages > MAX_PAGES {
            return Err(Error::BadGeometry);
        }
        // Virtual height `height * pages` must fit a u32 and stay within what
        // the firmware will plausibly grant.
        let virtual_height = height.checked_mul(pages).ok_or(Error::BadGeometry)?;
        if width > 4096 || virtual_height > 8192 {
            return Err(Error::BadGeometry);
        }

        self.release()?;

        let mut physical = [0u32; 2];
        let mut virt = [0u32; 2];
        let mut offset = [0u32; 2];
        let mut depth = [0u32; 1];
        let mut order = [0u32; 1];
        let mut allocation = [0u32; 2];
        let mut pitch = [0u32; 1];

        // Hoisted: a `TagIo` borrows its request, which must outlive the list.
        let want_physical = [width, height];
        let want_virtual = [width, virtual_height];
        let want_offset = [0u32, 0];
        let want_depth = [DEPTH_BITS];
        let want_order = [PixelOrder::Bgr.as_tag()];
        let want_align = [ALLOC_ALIGN];

        {
            let mut tags = [
                TagIo::new(Tag::SetPhysicalSize, &want_physical, &mut physical),
                TagIo::new(Tag::SetVirtualSize, &want_virtual, &mut virt),
                TagIo::new(Tag::SetVirtualOffset, &want_offset, &mut offset),
                TagIo::new(Tag::SetDepth, &want_depth, &mut depth),
                TagIo::new(Tag::SetPixelOrder, &want_order, &mut order),
                // Request word is the alignment; the reply is the address and
                // size, so the buffer is two words wide.
                TagIo::new(Tag::AllocateFramebuffer, &want_align, &mut allocation),
                TagIo::new(Tag::GetPitch, &[], &mut pitch),
            ];
            super::MAILBOX.lock().call_many(&mut tags)?;
        }

        let bus_address = allocation[0];
        let size = allocation[1] as usize;
        if bus_address == 0 || size == 0 {
            return Err(Error::AllocationFailed);
        }

        if depth[0] != DEPTH_BITS {
            return Err(Error::ModeRejected);
        }
        let granted_width = physical[0];
        let granted_height = physical[1];
        let pitch = pitch[0];
        if granted_width == 0 || granted_height == 0 || pitch < granted_width * 4 || pitch % 4 != 0
        {
            return Err(Error::ModeRejected);
        }

        // The firmware may grant a different page count, so trust the virtual
        // height it reports, not the request.
        let granted_pages = (virt[1] / granted_height).clamp(1, MAX_PAGES);

        let base = memory::bus::to_physical(bus_address);
        let needed = pitch as usize * (granted_height as usize) * (granted_pages as usize);
        if size < needed {
            return Err(Error::AllocationFailed);
        }
        if !mapped_for_videocore(base, size) {
            return Err(Error::OutsideMappedMemory);
        }

        self.base = base;
        self.size = size;
        self.bus_address = bus_address;
        self.width = granted_width;
        self.height = granted_height;
        self.pitch = pitch;
        self.stride_words = pitch as usize / 4;
        self.order = PixelOrder::from_tag(order[0]);
        self.pages = granted_pages;
        self.draw_page = 0;
        self.visible_page = 0;
        self.ready = true;

        // A fresh allocation holds whatever the VideoCore last left there.
        // Blank every page, not just the first, so the first `flip` does not
        // reveal stale memory.
        for page in 0..self.pages {
            self.draw_page = page;
            self.clear(BLACK);
        }
        self.draw_page = 0;
        Ok(())
    }

    /// Releases the allocation and returns to the unconfigured state.
    pub fn release(&mut self) -> Result<(), Error> {
        if !self.ready {
            return Ok(());
        }
        self.ready = false;
        let mut out = [0u32; 1];
        super::MAILBOX
            .lock()
            .call(Tag::ReleaseFramebuffer, &[], &mut out)?;
        Ok(())
    }

    /// Whether a mode has been set and the surface can be drawn to.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// The current mode, or `None` if there is not one.
    pub fn info(&self) -> Option<Info> {
        if !self.ready {
            return None;
        }
        Some(Info {
            base: self.base,
            size: self.size,
            bus_address: self.bus_address,
            width: self.width,
            height: self.height,
            pitch: self.pitch,
            depth: DEPTH_BITS,
            order: self.order,
            pages: self.pages,
            draw_page: self.draw_page,
            visible_page: self.visible_page,
        })
    }

    /// Visible width in pixels, or zero when not initialised.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Visible height in pixels, or zero when not initialised.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Presents the page just drawn and moves drawing to the next one.
    ///
    /// With a single page this still costs a mailbox round trip but is
    /// harmless, so callers do not have to special-case it.
    pub fn flip(&mut self) -> Result<(), Error> {
        if !self.ready {
            return Err(Error::NotReady);
        }
        let showing = self.draw_page;
        let mut out = [0u32; 2];
        super::MAILBOX
            .lock()
            .call(Tag::SetVirtualOffset, &[0, showing * self.height], &mut out)?;
        self.visible_page = showing;
        self.draw_page = (showing + 1) % self.pages;
        Ok(())
    }

    /// The page currently being drawn into, as a mutable slice of pixels.
    ///
    /// Every drawing primitive funnels through here, so the module's one
    /// `unsafe` block turns the mapped allocation into a slice.
    fn surface(&mut self) -> &mut [u32] {
        let words = self.stride_words * self.height as usize;
        let start = self.base + self.draw_page as usize * words * 4;
        // SAFETY: `init` proved `base..base + size` is inside the DRAM the MMU
        // maps `NormalNonCacheable` / RW, and that `size` covers every page, so
        // this range is mapped, writable and correctly aligned for `u32`. The
        // `Framebuffer` is a singleton behind a lock, so no other slice over
        // the same memory can be live.
        unsafe { core::slice::from_raw_parts_mut(start as *mut u32, words) }
    }

    /// The page currently being drawn into, as a shared slice of pixels.
    ///
    /// The read-only half of [`Framebuffer::surface`], for the one caller that
    /// only needs to look: taking `&mut self` to read a pixel would forbid
    /// reading one while any other borrow is alive, for no reason.
    fn surface_ref(&self) -> &[u32] {
        let words = self.stride_words * self.height as usize;
        let start = self.base + self.draw_page as usize * words * 4;
        // SAFETY: identical to `surface`, minus the mutation: the range was
        // proved mapped and correctly aligned by `init`, and a shared borrow of
        // it cannot outlive the `&self` it was derived from.
        unsafe { core::slice::from_raw_parts(start as *const u32, words) }
    }

    /// Converts a colour into the word this surface stores.
    ///
    /// Needed by anything writing through [`Framebuffer::scanline`], which
    /// hands out raw words: the firmware picks the channel order at mode set,
    /// so a caller cannot assume it.
    pub fn encode(&self, color: Color) -> u32 {
        self.order.encode(color)
    }

    /// Converts a word from this surface back into a colour.
    pub fn decode(&self, word: u32) -> Color {
        self.order.decode(word)
    }

    /// Borrows one row of the drawing page as raw pixel words.
    ///
    /// Returns the *visible* pixels of row `y` — `width` of them. The
    /// allocation's stride is usually wider, and the surplus is the pan margin
    /// the display never shows, so it is deliberately not handed out.
    ///
    /// # Why this exists
    ///
    /// Every primitive here funnels through one private accessor, and
    /// [`Framebuffer::pixel`] rebuilds that slice on every call. That makes a
    /// per-pixel loop through the public API roughly twenty-five times more
    /// expensive per pixel than [`Framebuffer::fill_rect`], measured on a
    /// Pi 3 — and the difference is the call, not the write. Anything doing its
    /// own per-pixel work, such as compositing or anti-aliasing, should take a
    /// row and write it directly rather than paying that cost per pixel.
    ///
    /// The borrow is exclusive for as long as the slice lives, so nothing else
    /// can be drawing while a row is held.
    pub fn scanline(&mut self, y: u32) -> Option<&mut [u32]> {
        if !self.ready || y >= self.height {
            return None;
        }
        let (start, width) = (y as usize * self.stride_words, self.width as usize);
        Some(&mut self.surface()[start..start + width])
    }

    /// Reads one pixel back from the drawing page.
    ///
    /// The counterpart to [`Framebuffer::pixel`], and the thing that makes a
    /// caller's own alpha compositing possible: [`Framebuffer::blit_blend`] is
    /// the only blend in the BSP, and on non-cacheable framebuffer memory its
    /// destination read is most of its cost. A caller that can avoid the read —
    /// because it knows what it drew — should; one that cannot now has a way to
    /// do the blend itself.
    ///
    /// Returns `None` outside the surface rather than a black pixel, so a bug
    /// in the caller's geometry does not read as valid data.
    pub fn read(&self, x: u32, y: u32) -> Option<Color> {
        if !self.ready || x >= self.width || y >= self.height {
            return None;
        }
        let index = y as usize * self.stride_words + x as usize;
        Some(self.order.decode(self.surface_ref()[index]))
    }

    /// Fills the whole drawing page with one colour.
    pub fn clear(&mut self, color: Color) {
        if !self.ready {
            return;
        }
        let word = self.order.encode(color);
        self.surface().fill(word);
    }

    /// Sets one pixel. Coordinates outside the surface are ignored.
    ///
    /// Convenient for a handful of pixels and the wrong tool for a loop: it
    /// clips and rebuilds the surface slice on every call, which costs far more
    /// than the write. Use [`Framebuffer::scanline`] for per-pixel work, and
    /// [`Framebuffer::fill_rect`] for anything that is a span.
    #[inline]
    pub fn pixel(&mut self, x: u32, y: u32, color: Color) {
        if !self.ready || x >= self.width || y >= self.height {
            return;
        }
        let word = self.order.encode(color);
        let index = y as usize * self.stride_words + x as usize;
        self.surface()[index] = word;
    }

    /// Fills an axis-aligned rectangle, clipped to the surface.
    pub fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        if !self.ready {
            return;
        }
        let Some((x, y, width, height)) = self.clip(x, y, width, height) else {
            return;
        };
        let word = self.order.encode(color);
        let stride = self.stride_words;
        let surface = self.surface();
        for row in y..y + height {
            let start = row as usize * stride + x as usize;
            surface[start..start + width as usize].fill(word);
        }
    }

    /// Draws the one-pixel outline of a rectangle, clipped to the surface.
    pub fn draw_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        if width == 0 || height == 0 {
            return;
        }
        self.fill_rect(x, y, width, 1, color);
        self.fill_rect(x, y + height - 1, width, 1, color);
        self.fill_rect(x, y, 1, height, color);
        self.fill_rect(x + width - 1, y, 1, height, color);
    }

    /// Draws a line with Bresenham's algorithm, clipped per pixel.
    pub fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: Color) {
        if !self.ready {
            return;
        }
        let (mut x, mut y) = (x0, y0);
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut error = dx + dy;

        loop {
            if x >= 0 && y >= 0 {
                self.pixel(x as u32, y as u32, color);
            }
            if x == x1 && y == y1 {
                break;
            }
            let doubled = error * 2;
            if doubled >= dy {
                error += dy;
                x += sx;
            }
            if doubled <= dx {
                error += dx;
                y += sy;
            }
        }
    }

    /// Copies an image onto the surface, ignoring its alpha channel.
    ///
    /// The image is in [`Color`]'s canonical order, so this also converts to
    /// whatever the firmware negotiated. Use [`Framebuffer::blit_blend`] for
    /// images with transparency.
    pub fn blit(&mut self, x: u32, y: u32, image: &Image<'_>) {
        if !self.ready {
            return;
        }
        let Some((x, y, width, height)) = self.clip(x, y, image.width, image.height) else {
            return;
        };
        let order = self.order;
        let stride = self.stride_words;
        let source_stride = image.stride as usize;
        let pixels = image.pixels;
        let surface = self.surface();

        for row in 0..height as usize {
            let destination = (y as usize + row) * stride + x as usize;
            let source = row * source_stride;
            for column in 0..width as usize {
                surface[destination + column] = order.encode(Color(pixels[source + column]));
            }
        }
    }

    /// Composites an image onto the surface honouring its alpha channel.
    ///
    /// Straight source-over blending: `out = src*a + dst*(1-a)`. Fully
    /// transparent and fully opaque pixels take shortcuts.
    ///
    /// The destination is read back to blend, so this is slower than
    /// [`Framebuffer::blit`]: on non-cacheable framebuffer memory a read is a
    /// full trip to DRAM. Use the opaque path when there is no alpha.
    pub fn blit_blend(&mut self, x: u32, y: u32, image: &Image<'_>) {
        if !self.ready {
            return;
        }
        let Some((x, y, width, height)) = self.clip(x, y, image.width, image.height) else {
            return;
        };
        let order = self.order;
        let stride = self.stride_words;
        let source_stride = image.stride as usize;
        let pixels = image.pixels;
        let surface = self.surface();

        for row in 0..height as usize {
            let destination = (y as usize + row) * stride + x as usize;
            let source = row * source_stride;
            for column in 0..width as usize {
                let top = Color(pixels[source + column]);
                let alpha = top.alpha();
                if alpha == 0 {
                    continue;
                }
                let index = destination + column;
                let word = if alpha == 255 {
                    order.encode(top)
                } else {
                    // The surface word is in hardware order, so decode it back
                    // to canonical before blending. `encode` is its own
                    // inverse for both orders it supports.
                    let under = Color(order.encode(Color(surface[index])));
                    order.encode(under.mix(top, alpha))
                };
                surface[index] = word;
            }
        }
    }

    /// Draws one character enlarged by an integer factor.
    ///
    /// `background` of `None` leaves the unlit pixels of the cell alone, which
    /// is what makes text over an image look right.
    ///
    /// Nearest-neighbour, so a glyph stays crisp rather than going soft. A
    /// `scale` of 0 draws nothing.
    pub fn draw_char_scaled(
        &mut self,
        x: u32,
        y: u32,
        character: char,
        foreground: Color,
        background: Option<Color>,
        scale: u32,
    ) {
        if !self.ready || scale == 0 {
            return;
        }
        let rows = font::glyph(character);
        let lit = self.order.encode(foreground);
        let unlit = background.map(|color| self.order.encode(color));
        let stride = self.stride_words;
        let (width, height) = (self.width, self.height);
        let surface = self.surface();

        for (row, bits) in rows.iter().enumerate() {
            for row_repeat in 0..scale {
                let py = y as usize + row * scale as usize + row_repeat as usize;
                if py >= height as usize {
                    break;
                }
                for column in 0..font::GLYPH_WIDTH {
                    let set = bits & (0x80 >> column) != 0;
                    let Some(word) = (if set { Some(lit) } else { unlit }) else {
                        continue;
                    };
                    for column_repeat in 0..scale {
                        let px = x as usize + column * scale as usize + column_repeat as usize;
                        if px >= width as usize {
                            break;
                        }
                        surface[py * stride + px] = word;
                    }
                }
            }
        }
    }

    /// Draws a string, advancing one cell per character.
    ///
    /// `\n` returns to the starting column on the next line. No wrapping and no
    /// scrolling: this is for labels and readouts, not a terminal.
    pub fn draw_text(
        &mut self,
        x: u32,
        y: u32,
        text: &str,
        foreground: Color,
        background: Option<Color>,
    ) {
        self.draw_text_scaled(x, y, text, foreground, background, 1);
    }

    /// Draws a string enlarged by an integer factor.
    pub fn draw_text_scaled(
        &mut self,
        x: u32,
        y: u32,
        text: &str,
        foreground: Color,
        background: Option<Color>,
        scale: u32,
    ) {
        if scale == 0 {
            return;
        }
        let advance = font::GLYPH_WIDTH as u32 * scale;
        let line_height = font::GLYPH_HEIGHT as u32 * scale;
        let (mut cx, mut cy) = (x, y);
        for character in text.chars() {
            if character == '\n' {
                cx = x;
                cy += line_height;
                continue;
            }
            self.draw_char_scaled(cx, cy, character, foreground, background, scale);
            cx += advance;
        }
    }

    /// The width in pixels a string will occupy at a given scale.
    ///
    /// Fixed-width font, so this is arithmetic rather than measurement.
    pub fn text_width(text: &str, scale: u32) -> u32 {
        let longest = text
            .split('\n')
            .map(|line| line.chars().count() as u32)
            .max()
            .unwrap_or(0);
        longest * font::GLYPH_WIDTH as u32 * scale
    }

    /// Draws formatted text, for callers that would otherwise need a `String`.
    ///
    /// Renders through a fixed stack buffer, so output longer than 128 bytes is
    /// truncated rather than allocated.
    pub fn draw_fmt(
        &mut self,
        x: u32,
        y: u32,
        arguments: core::fmt::Arguments<'_>,
        foreground: Color,
        background: Option<Color>,
    ) {
        let mut buffer = TextBuffer::new();
        let _ = buffer.write_fmt(arguments);
        self.draw_text(x, y, buffer.as_str(), foreground, background);
    }

    /// Clips a rectangle to the surface, returning `None` if nothing survives.
    fn clip(&self, x: u32, y: u32, width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
        if x >= self.width || y >= self.height || width == 0 || height == 0 {
            return None;
        }
        Some((x, y, width.min(self.width - x), height.min(self.height - y)))
    }
}

/// Whether `base..base + size` lies in the DRAM the kernel maps for the
/// VideoCore.
///
/// The upper bound mirrors the clamp in [`crate::arch::mmu::memory_map`]: the
/// firmware reports its reservation running to the top of SDRAM, but that top
/// is shadowed by the peripheral window in the ARM's view and is not mapped.
fn mapped_for_videocore(base: usize, size: usize) -> bool {
    let start = memory::vc_mem_base();
    let end = start
        .saturating_add(memory::vc_mem_size())
        .min(memory::PERIPHERAL_BASE);
    let Some(last) = base.checked_add(size) else {
        return false;
    };
    base >= start && last <= end
}

/// A small stack-allocated sink for [`Framebuffer::draw_fmt`].
struct TextBuffer {
    bytes: [u8; 128],
    used: usize,
}

impl TextBuffer {
    const fn new() -> Self {
        Self {
            bytes: [0; 128],
            used: 0,
        }
    }

    fn as_str(&self) -> &str {
        // SAFETY: only whole `&str` fragments are appended, and a fragment is
        // rejected outright rather than truncated, so the buffer never splits a
        // UTF-8 sequence.
        unsafe { core::str::from_utf8_unchecked(&self.bytes[..self.used]) }
    }
}

impl Write for TextBuffer {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let bytes = text.as_bytes();
        if self.used + bytes.len() > self.bytes.len() {
            // Dropping the whole fragment keeps `as_str` sound; a partial copy
            // could cut a multi-byte character in half.
            return Err(core::fmt::Error);
        }
        self.bytes[self.used..self.used + bytes.len()].copy_from_slice(bytes);
        self.used += bytes.len();
        Ok(())
    }
}
