# Camera and display interfaces

The BCM2837 has two CSI-2 camera receivers (Unicam) and two DSI display hosts.
A Pi 3 brings out one of each; a Compute Module 3 brings out all four:

| Interface | Lanes | Base | Compute Module connector |
| --- | --- | --- | --- |
| Unicam `CAM0` | 2 | `0x3F80_0000` | `CAM0` |
| Unicam `CAM1` | 4 | `0x3F80_1000` | `CAM1` |
| DSI0 | 2 | `0x3F20_9000` | `DISP0` |
| DSI1 | 4 | `0x3F70_0000` | `DISP1` |

The lanes are dedicated pins, so neither driver touches the GPIO block. The
sensor's or panel's I2C, reset and power-enable lines are ordinary GPIO and
the application's business.

**Neither driver has run on hardware.** QEMU emulates neither block. Both
sequences were checked line by line against the Linux 6.12 sources
(`bcm2835-unicam`, `vc4_dsi`, `clk-bcm2835`), and the examples stop cleanly at
the power step under QEMU. Video mode is in
[28-dsi-video.md](28-dsi-video.md); the device interfaces in
[29-devices.md](29-devices.md).

## Power and clocks

Both blocks read back nothing until the firmware powers them, through the
fine-grained domain tag (`SET_DOMAIN_STATE`, `0x0003_8030`; Unicam 12 and 13,
DSI 17 and 18). `Mailbox::set_domain_state` insists on the firmware echoing the
domain and its new state. QEMU's emulated firmware returns the tag with no
value, and without the check the camera example went on to "capture" a frame:
in QEMU the Unicam window behaves like RAM, so every status bit read back as
whatever had just been written to it.

`unicam::power_on` and `dsi::power_on` take `MAILBOX` and then `CLOCKS`, one
after the other, never nested. Unicam wants its low-power clock at 100 MHz;
DSI wants its escape clock at 100 MHz. Both come from PLLD, divided by 5.
These generators have a four-bit integer divider, not twelve, so
`ClockId::divi_max` refuses anything slower than PLLD / 15.

Linux also holds the VPU clock at 250 MHz or more while a receiver runs. The
Pi 3 firmware's default core clock meets that, so nothing here raises it; a
`config.txt` that lowers `core_freq` would need it raised through the
mailbox.

## Unicam

`Unicam::start` brings the D-PHY up for the configured lane count and writes
frames into a buffer: the data type and virtual channel to keep, the line
stride (a multiple of 16), optional unpacking to 16 bits per pixel. Output is
the sensor's raw data — usually Bayer — because the image processor belongs to
the VideoCore firmware.

Capture starts in trigger mode, as Linux does, so the first frame begins at a
real frame start. With `Config::continuous`, `service` leaves trigger mode
after the first event; otherwise each `trigger()` captures one frame.

`service(port)` reads and acknowledges the frame-start, frame-end and error
flags, counts frames and wakes waiters. It takes no lock, so it is both the
polled path (`wait_frame`, the `NextFrame` future) and the body of the
interrupt handlers `handle_cam0`/`handle_cam1` (IRQ 38 and 39). `set_buffer`
between a frame end and the next frame start swaps buffers without tearing.

The receiver writes behind the cores' backs, so `start` is `unsafe`: the buffer
belongs to the hardware until `stop` returns.

Configuring the sensor (mode, lane count, streaming on) is the caller's job.
The `camera` example leaves `start_sensor` empty for exactly that.

## DSI, command mode

`Dsi` sends DCS and generic packets: short and long writes, reads with a bus
turnaround, `set_window` and `write_memory` for a panel with its own frame
memory. Long payloads use the pixel FIFO for the word-aligned part, as Linux
does, which caps one packet at 1023 bytes; `write_memory` chunks pixels into
packets of a multiple of six bytes so no 16- or 24-bit pixel is split.

Commands are always sent in low-power escape mode, which needs only the
escape clock: about 10 Mbit/s on lane 0, enough for initialisation and slow
full-frame updates. With `Config::hs_clock_hz` set, `Dsi::init` also starts the
PLLD DSI1 channel and the pixel clock, and the host can stream video; see
[28-dsi-video.md](28-dsi-video.md).

### DSI1 ignores writes from the ARM

DSI1's AXI slave does not respond to register writes from the ARM cores; it
does respond to the DMA engine. Linux works around this the same way: every
DSI1 register write is a one-word DMA transfer (`dma::Channel::write_word`), on
a lite channel `Dsi::init` claims and holds. Reads work normally. DSI0 has no
such problem.

### Two register maps

DSI0 is an older revision with a different layout and different bit positions
in `CTRL`, `PHYC`, `INT_STAT` and the analogue front end. The driver reaches
registers by offset through a per-port `Layout` rather than one `#[repr(C)]`
map; each port's `ID` register must read `0x0064_7369` ("dsi") before `init`
goes any further.

### Bugs the audit against Linux found

The first version was written from the register descriptions alone. Checking
it against the Linux sources found five errors, all fixed, none of which QEMU
could have shown:

* `DISP1_CTRL`'s 32-bit little-endian format is 2, not 3; 3 is big-endian,
  which would have byte-swapped every long payload.
* DSI0's escape-mode divider field in `PHYC` is bits 17:12, not 15:8.
* DSI1's escape clock generator is at `CM + 0x158`. `0x60` is DSI0's *pixel*
  clock, so `dsi::power_on(Dsi1)` was programming the wrong generator.
* The unit interval is `500 MHz / PLL rate` — the PHY clock is double data
  rate — not `1 GHz / rate`; and `T_INIT` is the firmware's 5 ms.
* The host needs a soft reset and end-of-transmission configuration before
  the PHY is set up, and DSI0's analogue front end takes no lane-drive fields.

## Compute Module notes

The CM3's onboard eMMC, on non-Lite modules, sits on the controller
`drivers::emmc` drives, but that driver initialises SD cards only (`ACMD41`);
an eMMC needs the MMC `CMD1` sequence, which is not implemented. A CM3 Lite
with an SD slot on the carrier is unaffected.

Carrier boards usually wire the camera and display I2C to GPIO 28/29 or 44/45
rather than 0/1. `I2c::use_pins` moves BSC0 (28/29 ALT0, 44/45 ALT1) or BSC1
(44/45 ALT2) there before `init`.
