# The cogware QEMU and scenario tests

Stock QEMU's `raspi3b` stops at the parts of the BCM2837 this BSP now drives:
no DMA pacing, no repeated START on I2C, no pulls or edge detection on GPIO,
no PWM, and no camera or display blocks at all. A fork of QEMU 11.1.3 at
`~/qemu-cogware` fills those in, and `scripts/qemu-scenario.py` drives it.
`~/qemu-cogware/COGWARE.md` lists every model and property.

The fork is a separate program under QEMU's licence (GPL-2.0-or-later), not
part of this repository; its patch is `~/qemu-cogware/cogware.patch`.

## Scenarios

A scenario is a text file: boot an example with extra devices, then interleave
waiting for console output with acting on the emulated hardware over QMP.

```text
boot gpio-events
expect ready: GPIO 17=0 GPIO 22=1
drive 22=0
drive 17=1
expect edge 1: GPIO 17=1 GPIO 22=0
check /machine/soc/peripherals/pwm.ch1-period-ns ~ 1000000
set /machine/peripheral/panel.snapshot out/panel.ppm
```

```sh
scripts/qemu-scenario.py scripts/scenarios/*.txt
```

| Scenario | Proves |
| --- | --- |
| `gpio-events` | pulls, external drive, edge interrupts on bank 0 |
| `pwm` | 1 kHz at 25 % measured on channel 1 |
| `spi-dma` | SPI0 by DMA, paced by the SPI's data requests |
| `dsi-panel` | DSI1 command mode: DCS bring-up, a read, frame-memory writes, every register write by DMA |
| `dsi-video` | DSI1 video: PLL, PHY, HVS channel 2, pixel valve 1, background, plane, page flip |

The QEMU is started paused and resumed only once QMP is ready: a QMP
handshake that arrives while a guest is busy at boot can go unanswered
indefinitely, even on stock QEMU, and nothing should run before the console
is being read.

## Bugs it found

In the BSP, before any board ran the code:

* `Clocks::enable_dsi1_pll` took "the fastest rate not above" the request.
  `display::fit_timing` hands it PLLD / 3 truncated to whole hertz, a fraction
  under the true rate, so it chose PLLD / 4: the link would have run 25 %
  slow while the pixel valve assumed the fitted clock. It now takes the
  nearest divider, checked at compile time.
* `dsi-video` held `HVS` twice in one expression; the lock's re-entrancy check
  caught it.

In upstream QEMU, fixed in the fork: the clock manager's divisor writes were
ignored (a word/byte index mix-up), the I2C controller could not do repeated
START and started a transfer on any write that left it enabled, the SPI
controller never drove chip select, and the DMA controller ignored data
requests. The `buses` example's strict repeated-START check now passes, and
its JEDEC-by-DMA read with it.

## Stock QEMU still works

Every example still runs on stock QEMU, and the fork prints the same except
where it models more: pulls read as pulled, edges latch, repeated START and
SPI DMA work, and the camera and display examples get past power-on.
