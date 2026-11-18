# Clocks, PWM, the system timer and the RNG

## Clock manager

`drivers::clock` drives CPRMAN at `0x3F10_1000`. Each generator is a control
and a divisor register, eight bytes apart, so the block is described as an
array of 64 pairs and a `ClockId` is the control register's offset.

* Every write carries the password `0x5a` in its top byte. A write without it
  is dropped silently, which is the failure this driver exists to make
  impossible.
* The divisor is 12.12 fixed point. `Mash::Integer` discards the fraction, so
  every period is identical; the MASH stages trade jitter for a closer average
  and need integer parts of at least 2, 3 and 5. `divisor` is checked with
  compile-time assertions.
* A generator is stopped and waited for (`BUSY` clear) before its divisor
  changes, since changing it while running can glitch the output. If it never
  stops, `KILL` resets it outright.
* Two sources are offered: the 19.2 MHz oscillator, which the firmware never
  touches, and PLLD's peripheral output at 500 MHz, which it sets once and
  leaves. QEMU emulates CPRMAN but has no firmware to set PLLD up.

The generators do not all have the same divider. The camera and DSI escape
clocks have four integer bits and eight fractional ones, so they divide by at
most 15; the DSI pixel clocks (`Dsi0Pixel` at `0x60`, `Dsi1Pixel` at `0x160`)
have no divider and only select a source, through `Clocks::select`.

The PLLD DSI1 channel, which clocks DSI1's high-speed lanes, sits in the
analogue (A2W) half of the block, 4 KiB above the generators:
`Clocks::enable_dsi1_pll` writes its integer divider of the 2 GHz PLL, pulses
`LOAD` in `CM_PLLD`, and releases `HOLD`.

`CLOCKS` is a leaf lock: `PWM` and the DSI hosts take it inside their own, and
it never takes another.

## PWM

`drivers::pwm` drives the two-channel controller at `0x3F20_C000`.
`Pwm::init(hz)` stops both channels, then clocks the block from the oscillator
up to 9.6 MHz and from PLLD above, always with an integer divisor. The actual
rate comes back: 1 MHz asked of the oscillator is 19.2 MHz / 19 = 1.0105 MHz,
so pick a rate that divides evenly when the period has to be exact.

`route(pin)` switches a pin to its PWM function (GPIO 12/13 ALT0, 18/19 ALT5,
40/41/45 ALT0, 52/53 ALT1) and says which channel it carries. Mark-space mode
is what servos and dimming want; balanced mode spreads the duty through the
period for filtering into a level; serialiser mode shifts FIFO words out, which
is how WS2812 strips and PWM audio are fed by DMA.

QEMU has no PWM model: it accepts the writes and ignores them, so the
`peripherals` check proves only that nothing hangs. On a board, GPIO 12 should
show 1 kHz at 25 %.

## System timer

`drivers::systimer` reads the 1 MHz free-running counter at `0x3F00_3000`. It
is one counter for the whole SoC, unlike the per-core architectural timer, and
its four compare channels are interrupt sources. The VideoCore uses channels 0
and 2, so only 1 and 3 are offered. Nothing takes a lock: the counter is
read-only, the match flags are write-1-to-clear, and a compare register
belongs to whoever owns its channel.

The 64-bit read retries if the high word changed while the low word was read.

## RNG

`drivers::rng` drives the generator at `0x3F10_4000`. `init` discards the
first 0x40000 words, as Linux does, because the source's first output after
power-up is the least random. Its quality is undocumented: use it as a seed,
not as a vetted cryptographic generator.
