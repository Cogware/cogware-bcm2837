# Framebuffer and 2D drawing

The VideoCore owns the display. The ARM requests a framebuffer through the
property mailbox described in
[11-videocore-mailbox.md](11-videocore-mailbox.md), receives a bus address and
pitch, then draws by writing `u32`s to mapped memory.

**There is no display hardware to drive from the ARM:** no registers, command
ring or interrupt. Once the mode is set, the framebuffer is memory.

## Getting a mode

`Framebuffer::init(width, height, pages)` sends one property message with the
full mode-set sequence, because the firmware applies the tags in a single pass
and partial mode changes are difficult to recover:

```text
SetPhysicalSize   width x height          what the display shows
SetVirtualSize    width x (height*pages)  the drawing surface
SetVirtualOffset  0, 0                    which page is on screen
SetDepth          32                      one pixel is one u32
SetPixelOrder     RGB                     a request, not a guarantee
AllocateFramebuffer align 16              returns bus address + size
GetPitch                                  returns bytes per row
```

`init` releases any previous allocation first. This is not defensive coding —
calling `init` twice without it leaks the old buffer, because
`AllocateFramebuffer` hands out a fresh region and the firmware has no idea the
caller has forgotten the old one.

It then clears every page. A fresh VideoCore allocation contains whatever the
GPU last had there, which on a warm reboot is the previous session's screen.

### The pixel order is an answer, not a request

`SetPixelOrder` is advisory. The firmware replies with what it actually did,
and the driver believes the reply:

```rust
pub enum PixelOrder {
    Bgr,  // tag value 0 -> the word is 0xAARRGGBB
    Rgb,  // tag value 1 -> R and B are swapped
}
```

The naming is confusing in the firmware: tag value `0`,
which the firmware calls BGR, is the one where a little-endian `u32` reads as
`0xAARRGGBB` — the canonical order this driver uses internally. So `Bgr` needs
**no** channel swap, and `Rgb` does.

`PixelOrder::encode` is an involution: identity for `Bgr`, an R↔B swap for
`Rgb`. Applying it twice returns the original. `blit_blend` relies on that to
read a surface word back into canonical form without a separate decode.

QEMU reports `Bgr`, so the swap path is exercised by inspection only.

### Bus addresses

`AllocateFramebuffer` returns a **bus** address. The alias bits in the top two bits
say how the GPU intends to cache it, and the ARM must strip them —
`bus::to_physical` masks them off. QEMU happens to return `0x3C100000` with no
alias bits set, so both the masked and unmasked values work there; on real
firmware they do not.

The result lands inside the VideoCore reservation, which the MMU maps
`NormalNonCacheable`. That is why drawing is a plain store with no cache
maintenance: the memory is already coherent with the GPU. It also means writes
are slow, so the drawing primitives are written to touch each pixel once.

## Double buffering

The virtual surface is twice the visible height, and `SetVirtualOffset` chooses
which half is scanned out:

```text
        virtual surface (width x height*2)
      +-----------------------------+
      |  page 0                     |  <- offset 0
      +-----------------------------+
      |  page 1                     |  <- offset height
      +-----------------------------+
```

`flip()` sends one `SetVirtualOffset` and swaps the driver's idea of which page
is being drawn into. It does not copy anything and does not wait for vblank, so
tearing is possible; for a boot splash and a test card that is not worth a
frame of latency.

`MAX_PAGES` is 4, but the driver currently allocates 2.

## Surfaces and `unsafe`

`surface()` is the only `unsafe` in the module. It turns the mapped address and
the pitch into a `&mut [u32]` for the page currently being drawn:

```text
offset  = draw_page * height * pitch
length  = height * pitch / 4
```

Everything else — `clear`, `fill_rect`, `draw_rect`, `line`, `blit`,
`blit_blend`, the text calls — is safe code over that slice. That is the point:
one audited function converts hardware into a Rust slice, and no drawing
primitive needs to reason about addresses again.

Every primitive clips to the surface first, so drawing partly or entirely
off-screen is defined behaviour rather than a fault. The test card deliberately
draws past the right edge on every run to keep that path exercised.

### Direct access: `scanline` and `read`

The primitives above cover shapes. They do not cover the case where the colour
is a function of the pixel — compositing, anti-aliasing, a plasma — and doing
that through `pixel` is a trap: it rebuilds the surface slice on *every call*.
Measured on a Pi 3 at 2400x900, a per-pixel loop through `pixel` runs at about
14 Mpixel/s against 350 for `fill_rect`, a factor of twenty-five for identical
writes. The difference is the call, not the memory.

`scanline(y)` borrows one row of the drawing page as `&mut [u32]`, so that loop
becomes a slice write:

```rust
let word = fb.encode(Color::rgb(255, 128, 0));   // before the row is borrowed
if let Some(row) = fb.scanline(y) {
    for slot in row.iter_mut() { *slot = word; }
}
```

Two things follow from the signature. The borrow is exclusive, so anything else
the loop needs from the framebuffer — `encode` above — has to be taken first.
And the slice is the *visible* width, not the stride: the surplus is the pan
margin the display never shows, and handing it out would invite writing into it.

`read(x, y)` is the counterpart to `pixel`, and it is what makes a caller's own
alpha compositing possible. `blit_blend` is the only blend in the BSP, and on
non-cacheable framebuffer memory its destination read is most of its cost: 5.6
Mpixel/s against 101 for the opaque `blit` on the same board. A caller that
knows what it drew can compute the background instead and skip the read
entirely; one that cannot now has the means to blend for itself.

`encode` and `decode` convert between `Color` and the surface's word order,
which the firmware picks at mode set and a caller must not assume.

`examples/display.rs` draws a per-pixel band through `scanline` and reads a
pixel back to prove the round trip.

## `Color` and blending

`Color` is `0xAARRGGBB` in canonical order, with the usual constructors.

`Color::mix(other, amount)` is a per-channel lerp:

```text
result = self * (255 - amount) + other * amount
```

which is already exactly source-over compositing when `amount` is the source
alpha. There is no separate blend function because there does not need to be
one: `under.mix(top, top.alpha())` *is* the blend.

## Images and `Image`

`Image` is a borrowed view over a `&[u32]`, not an owner:

```rust
pub struct Image<'a> {
    pixels: &'a [u32],
    width:  u32,
    height: u32,
    stride: u32,   // words per row in `pixels`
}
```

`stride` is what makes `sub()` free. A sub-rectangle is the same slice with a
different origin, width, height and an unchanged stride — no copy, no
allocation. `Image::new` sets `stride == width`; `sub` is the only thing that
makes them differ.

Two helpers build on that:

* `opaque_bounds()` scans for the tightest rectangle containing any pixel with
  a non-zero alpha;
* `trimmed()` returns `sub(opaque_bounds())`.

These exist because the CogWare logo is a 480×480 canvas that is **88 % fully
transparent** — the artwork occupies a band across the middle. Blitting it
untrimmed wastes most of the work and makes it impossible to centre, because
the visual centre is not the canvas centre.

### `blit` versus `blit_blend`

`blit` copies. It ignores alpha entirely, which is right for opaque source
data and wrong for anything with transparency.

`blit_blend` composites. It has two fast paths that matter more than the
blending arithmetic does:

```text
alpha == 0    -> skip the pixel entirely (no read, no write)
alpha == 255  -> store directly (no read, no blend)
otherwise     -> read the destination, mix, store
```

For the logo, the `alpha == 0` path skips the great majority of pixels. Without
it every fully transparent pixel would cost a read-modify-write of
non-cacheable memory for no visible effect.

## Text

`font.rs` is an 8×8 bitmap font covering printable ASCII, one byte per
scanline, with a replacement box drawn for anything outside the range — so a
bad byte is visible rather than invisible.

`draw_char_scaled` and `draw_text_scaled` take an integer scale factor and
replicate pixels, which keeps the font crisp at 2× and 3× instead of blurring
it. `draw_text` is the 1× case. `text_width(text, scale)` gives the pixel width
so text can be centred without guessing.

The background colour is an `Option<Color>`:

* `Some(colour)` fills the whole character cell — legible over anything;
* `None` draws only the glyph strokes and leaves the pixels between them
  untouched.

`None` is what puts text *on top of* an image rather than in a box over it.

`draw_fmt` takes `format_args!`, so anything `Display` can go on screen without
an intermediate `String`.

`TextBuffer` wraps the above into a scrolling console region for later use.

## QOI decoding

`qoi.rs` decodes the [Quite OK Image
format](https://qoiformat.org/qoi-specification.pdf), written from the
specification rather than ported from any implementation.

QOI is a good fit here for three reasons: the whole decoder is about 270 lines,
it needs no tables and no allocation beyond the output, and it is genuinely
lossless. PNG would mean a DEFLATE implementation; BMP would mean shipping a
1 MiB asset.

The format is a 14-byte header followed by a stream of chunks, each of which
encodes a pixel relative to earlier ones:

```text
QOI_OP_RGB    0xFE + 3 bytes      literal colour
QOI_OP_RGBA   0xFF + 4 bytes      literal colour with alpha
QOI_OP_INDEX  00xxxxxx            a slot in the 64-entry seen-pixel cache
QOI_OP_DIFF   01rrggbb            -2..1 per channel from the previous pixel
QOI_OP_LUMA   10gggggg + 4+4      -32..31 green, -8..7 red/blue relative to it
QOI_OP_RUN    11xxxxxx            2..62 repeats of the previous pixel
```

The 64-entry cache is indexed by `(r*3 + g*5 + b*7 + a*11) % 64`, which is
fixed by the format — it is not a hash the decoder gets to choose.

`decode()` returns `(Header, Vec<u32>)` directly in canonical `0xAARRGGBB`, so
there is no second pass to convert.

Robustness, because this parses data the kernel did not produce:

* every read is bounds-checked against the input;
* the pixel loop is bounded by the header's pixel count, so a truncated or
  malicious stream cannot spin;
* `MAX_PIXELS` is 4096², so a corrupt header cannot ask the heap for gigabytes;
* seven distinct `Error` variants, each with a `Display` message, so a failure
  says which check tripped.

The logo is embedded with `include_bytes!` as `drivers::LOGO_QOI` — 35 KiB of
`.rodata`, decoded to 900 KiB of heap in about 2.4 ms.

## Console

| key | what it does |
| --- | --- |
| `f` | requests a mode if needed, then prints the display table |
| `g` | draws the test card and flips it on screen |

The test card is not decoration; each element exists to exercise something:

```text
colour bars      + grey ramp     channel order and the depth
CogWare logo (alpha-composited)  QOI decode, trimming, blit_blend
"COGWARE BCM2837" over the logo      draw_text_scaled with no background
procedural image                 heap, FPU, and the opaque blit
"on image" over that image       text over opaque source data
nested rectangles                draw_rect and the clip path
line fan (drawn off the edge)    line() and clipping
mode readout                     draw_fmt
```

Typical timings under QEMU at 1280×720: logo decode 2.4 ms, whole card
9.8 ms for 921 600 pixels.

The procedural image is computed, not embedded, and uses squared distance with
explicit comparisons — `core` has no `sqrt`, no float `abs`, and no
transcendentals, so any float maths here has to be arithmetic only.

## What QEMU gives you

Verified against `-display default,show-cursor=on`:

* the requested 1280×720 is granted exactly, `pitch` 5120, so no row padding;
* `PixelOrder::Bgr`, i.e. no channel swap;
* bus address `0x3C100000`, allocation 7 372 800 bytes — exactly two pages;
* `flip()` works and is visible.

Note that `-show-cursor` was removed in QEMU 11; it is a display sub-option now
(`-display default,show-cursor=on`), and passing the old form is a hard parse
error rather than a warning.

What QEMU does not give you: **V3D**. There is no 3D core and no shader
execution, so the compute path this project eventually wants has to be
developed against real silicon.

The 2D path, though, is not emulator-only. The mode set, the 1280x720 surface
and a QOI image alpha-composited over it have all been captured off a real
Pi Zero 2 W's HDMI output through a capture card, so what this document
describes is confirmed behaviour on silicon rather than an emulator artefact.

## Related

* [11-videocore-mailbox.md](11-videocore-mailbox.md) — the property channel and
  the tag encoding used to set the mode
* [03-memory-and-mmu.md](03-memory-and-mmu.md) — why the framebuffer window is
  `NormalNonCacheable` and the peripheral window is not
* [13-heap.md](13-heap.md) — where decoded images live
