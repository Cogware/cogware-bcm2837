# DSI video mode

Video mode streams frames from memory to a panel with no CPU involvement:

```text
image in DRAM -> HVS channel -> pixel valve -> DSI host -> panel
```

`drivers::display` puts it together for DSI1, the port wired to `DISP1` on a
Compute Module and to the display connector on a Pi 3. **None of it has run on
hardware**, and QEMU emulates none of these blocks; every register write was
checked against the Linux 6.12 `vc4` driver. The `dsi-video` example is the
bring-up procedure.

## The order

Linux's order, which `display` and `devices::panel::start` keep:

1. **`display::bring_up`**: power the host, start the PLLD DSI1 channel, point
   the DSI1 pixel clock at the PHY's byte clock, bring the PHY up with the
   high-speed timings, configure — but do not enable — the video stream.
2. **The panel's initialisation**, sent as low-power commands over `DSI1`.
3. **`display::start`**: start the HVS channel, program and enable the pixel
   valve, enable the stream.
4. **Display on**, as a command, with video running.

Each step takes one lock and releases it. `Dsi::init` takes `CLOCKS` while
holding `DSI1`, which is safe because `CLOCKS` never takes another lock.

## The integer PLL

PLLD runs at 2 GHz and carries every peripheral clock, so it cannot move. The
DSI1 bit clock is PLLD divided by a whole number. A panel's mode rarely asks
for exactly that, so `display::fit_timing` takes the next rate up, raises the
pixel clock to match, and lengthens the horizontal front porch so the refresh
rate stays put. The firmware and Linux both do this; a panel that tolerates a
longer blanking interval, which is nearly all of them, never notices. The fit
is checked at compile time.

The pixel clock fed to the valve is the PHY's byte clock, not the pixel rate:
the DSI block's `PIX_CLK_DIV` (bits per pixel over lanes) divides it down.

## Sharing the HVS with the firmware

The firmware keeps showing its HDMI framebuffer through HVS channel 1, and
nothing here stops it. Three things keep the two apart:

* only channel 2 (or 0) is touched; the global `DISPCTRL` is written only if
  the firmware left the HVS disabled;
* our display lists go at the **top** of the 16 KiB list memory, since the
  firmware's sit at the bottom (Linux leaves it the first 32 words);
* the channel's share of the composite output buffer (`DISPBASE`) is set to
  Linux's layout only if the firmware left it empty. `Hvs::output_buffer`
  reports what is there, and the example prints it.

None of that is proven on a board yet. If the panel shows HDMI's picture, or
HDMI glitches when the panel starts, the output-buffer split is the first
suspect.

## Planes and memory

A channel shows one plane: 32-bit XRGB, unscaled, the same word layout as the
HDMI framebuffer (`PixelOrder::Bgr`), so `Canvas` draws into it. With no plane
the channel shows its background colour alone. That is the first thing to try
on new hardware, because it proves the PLL, PHY, valve and stream without any
image memory.

The HVS reads the plane straight from DRAM. A plane in ordinary ARM memory is
cache-cleaned by `Hvs::start` and `Hvs::flip`; one in VideoCore memory, which
is mapped non-cacheable, needs nothing. `display::flip` rewrites the plane's
pointer word in the live display list after the next vertical front porch
begins, which is how Linux does an asynchronous page flip.

## Not here

* **DSI0 video.** Its bit clock is PLLA's DSI0 channel, and PLLA's state is
  the firmware's business; `display` returns `Unsupported`.
* **Scaling, multiple planes, other pixel formats, DPI.** The HVS and pixel
  valve 0 can do all of these; nothing has needed them yet.
* **Interrupt-driven vblank.** `PixelValve::wait_vblank` polls; the valves'
  interrupts are 45, 46 and 42 when it matters.
