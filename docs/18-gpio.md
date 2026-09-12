# 18. GPIO

The BCM2837 has 54 general-purpose pins, 32 of which are wired to the Pi 3's
40-pin header. Each one can be an input, an output, or one of six alternate
functions that hand it to a peripheral. This chapter is about the typed API in
`hal::gpio` that sits on top of the raw register block in `drivers::gpio`.

Two questions explain most of the design:

- **Why a typestate API at all**, when the registers are three stores?
- **Why does `set_high` take no lock**, when every other driver in the BSP is
  wrapped in a `SpinLock`?

---

## 18.1 The register block, briefly

`drivers::gpio` maps the block at `MMIO_BASE + 0x0020_0000` and exposes it as a
`SpinLock<Gpio>`. The registers that matter:

| Register | Width | Access | What it does |
| --- | --- | --- | --- |
| `GPFSEL0..5` | 6 × 32 | read-modify-write | three bits per pin: input, output, or `ALT0..5` |
| `GPSET0..1` | 2 × 32 | write-1-to-set | writing a 1 drives that pin high; writing 0 does nothing |
| `GPCLR0..1` | 2 × 32 | write-1-to-clear | writing a 1 drives that pin low |
| `GPLEV0..1` | 2 × 32 | read-only | the actual level on the pad |
| `GPEDS0..1` | 2 × 32 | write-1-to-clear | sticky "an edge happened here" |
| `GPREN/GPFEN/GPHEN/GPLEN` | 8 × 32 | read-modify-write | which events arm `GPEDS` |
| `GPPUD` + `GPPUDCLK0..1` | 3 × 32 | sequenced | pull-up/pull-down selection |

Ten of those thirteen are *not* read-modify-write. `GPSET`/`GPCLR`/`GPEDS` are
write-1-to-act, so a write only ever affects the bits you set — two cores
writing different bits of `GPSET0` cannot lose each other's work, because there
is no read step to interleave with. `GPLEV` is a pure load.

## 18.2 Lock-free where it is safe, locked where it is not

`hal::gpio` splits the API in two:

**No lock:** `set_high`, `set_low`, `toggle`, `set_state`, `is_high`, `is_low`,
`take_event`. These compile to a single `str` or `ldr` to Device memory. There
is no critical section because there is no read-modify-write to protect.

```rust
// Two cores, two pins, simultaneously. Both are correct.
// core 1:                       // core 2:
led.set_high();                  motor.set_low();
// -> str w0, [GPSET0]           // -> str w0, [GPCLR0]
```

**Takes the `GPIO` lock:** `into_input`, `into_output`, `into_alt`,
`into_open_drain`, the pull configuration, and `set_edge_detect`. All of these
are read-modify-write of `GPFSEL`, `GPREN`/`GPFEN`/`GPHEN`/`GPLEN`, or the
`GPPUD` sequence, which is stateful across three writes and a delay.

The practical consequence is that the hot path — the one a bit-banged bus runs
a million times a second — has no lock in it, and the cold path — configuring a
pin, which happens once at startup — does.

The one place this costs something is `OpenDrain`. The pad has no open-drain
driver, so "release the line" means "become an input and let the external
pull-up win", which is a `GPFSEL` write and therefore a lock. At 100 kHz a half
bit-period is about 6,000 core cycles and an uncontended `SpinLock` is a
handful, so the delay dominates by three orders of magnitude. It would matter
at 10 MHz; you would not bit-bang at 10 MHz.

> Lock ordering: the bus drivers take `SPI0`/`I2C0`/`I2C1` and *then* `GPIO`
> inside `init`, because claiming the pins is a `GPFSEL` write. Never take
> `GPIO` and then a bus lock.

## 18.3 Typestate: `Pin<N, MODE>`

```rust
pub struct Pin<const N: u8, MODE> { /* zero-sized */ }
```

Both the pin number and the mode are compile-time. `size_of::<Pin<16, Output>>()`
is **0** — the struct holds a `PhantomData` and nothing else, and the pin number
is baked into the instruction that touches it. `set_high` on a `Pin<16, Output>`
is one immediate store with a constant-folded bit mask.

The mode parameter is what makes wrong code fail to compile:

```rust
let pin = pins.p16.into_output_low();
pin.set_high();          // fine

let pin = pins.p17.into_pull_up_input();
pin.set_high();          // error: no method `set_high` on Pin<17, Input<PullUp>>

let pin = pins.p18.into_alt::<0>();
pin.is_high();           // error: the peripheral drives this pin, not you
```

Mode changes consume the pin by value, so there is never a stale handle in the
wrong mode:

```rust
let pin = pins.p16.into_output_low();   // Pin<16, Output>
let pin = pin.into_input();                // Pin<16, Input<Floating>>
// the Output handle no longer exists
```

Readability is controlled by a sealed `Readable` marker rather than by listing
modes, so `Output`, `Input<P>` and `OpenDrain` can all be read (the level
register works regardless of direction — reading back an output is genuinely
useful) while `Alt<A>` and `Unknown` cannot.

## 18.4 `Pins::take()`

```rust
let pins = Pins::take().expect("someone already took them");
let mut led = pins.p16.into_output_low();
```

`Pins` is a struct of all 54 `Pin<N, Unknown>` handles, named `p0` through
`p53`. `take()` is backed by an
`AtomicBool` compare-exchange, so it returns `Some` exactly once per boot across
all four cores and `None` after that. Because the fields are moved out
individually, the borrow checker enforces single ownership of each pin for free:
two drivers cannot both hold `gpio16`.

`unsafe fn steal()` exists for the cases the ownership model cannot express —
recovering after a panic, or a second handle in an exception vector. It is
`unsafe` precisely because it defeats the guarantee above.

Some pins are already claimed by the time your code runs:

| Pins | Owner |
| --- | --- |
| 14, 15 | PL011 UART — the console |
| 22–27 | JTAG, when the `jtag` feature is on |
| 48–53 | EMMC / SD card |

`Pins::take()` still hands them to you. Reconfiguring them is allowed and will
break the corresponding driver, which is the correct trade: the BSP does not
know what you are building.

## 18.5 Runtime pin numbers: `AnyPin`

Typestate is the right default and the wrong tool for a pin number that comes
from a config file or a device tree. `degrade()` converts:

```rust
let pin: Pin<17, Output> = pins.p17.into_output_high();
let pin: AnyPin<Output>  = pin.degrade();
assert_eq!(pin.number(), 17);
```

`AnyPin<MODE>` keeps the mode in the type and moves the number into a `u8`
field, so `size_of::<AnyPin<Output>>() == 1`. The mode guarantees survive; only
the constant folding is lost. Both types implement the same `hal::digital`
traits, so a driver generic over `OutputPin` takes either.

## 18.6 Alternate functions

```rust
let pin = pins.p16.into_alt::<4>();   // Pin<16, Alt<4>>, SPI1 CE2
```

`Alt<A>` has no level methods at all. The peripheral owns the pad; reading or
writing it from the CPU is meaningless, so the type refuses.

You rarely need this for the buses the BSP drives — `Spi0::init` and
`I2c::init` claim and configure their own pins. `into_alt` is for the functions
the BSP has no driver for: PWM, PCM, SPI1/SPI2, secondary UARTs.

## 18.7 Edge detection and `async`

`GPEDS` is a sticky per-pin latch armed by `GPREN` (rising), `GPFEN` (falling),
`GPHEN` (high level) and `GPLEN` (low level). The BSP exposes it two ways.

Polled:

```rust
let mut button = pins.p20.into_pull_up_input();
button.set_edge_detect(false, true);   // falling only
if button.take_event() {               // reads and clears GPEDS
    // ...
}
```

Awaited, via `hal::asynch`:

```rust
button.wait_for(Edge::Falling).await;
```

`WaitForEdge` arms the detector on its first poll and disarms it on drop, so a
cancelled wait leaves no configuration behind. It currently re-polls through the
executor's `poll_again()` rather than sleeping on an interrupt; wiring `GPEDS`
to the GIC is on the roadmap.

**QEMU does not model any of this.** `GPEDS` and the four enable registers are
absent from `hw/gpio/bcm2835_gpio.c`, so an awaited edge never fires under
emulation. `hal::asynch::edge_detection_works()` probes GPIO 4 at runtime — it
arms both edges, toggles the pin, and reports whether `GPEDS` latched — so an
example can print "not modelled here" instead of hanging. On silicon it returns
`true`.

## 18.8 What QEMU does and does not model

Before trusting a green test run:

| Feature | QEMU `raspi3b` |
| --- | --- |
| `GPFSEL` | stored, readable, not acted on |
| `GPSET`/`GPCLR` → `GPLEV` | **works** — output loopback reads back correctly |
| `GPEDS`, `GPREN`/`GPFEN`/`GPHEN`/`GPLEN` | **absent** — no edge ever latches |
| `GPPUD`/`GPPUDCLK` | **absent** — pulls do nothing, a floating input reads the stale latch |
| External drive | nothing outside the machine can move a pin |

So `examples/gpio.rs` can honestly verify ownership, typestate, mode changes and
output loopback (20,000 edges in ~13 ms, about 780 kHz), and has to label the
pull-up and edge sections as unverifiable. For anything that needs a *real*
counterparty, `examples/bitbang.rs` builds simulated wired-AND nets in ordinary
Rust and drives the same generic code against them — see
[`docs/19-spi-and-i2c.md`](19-spi-and-i2c.md).

## 18.9 Source

| File | Contents |
| --- | --- |
| `src/drivers/gpio.rs` | the register block, the `GPIO` lock, lock-free free functions |
| `src/hal/gpio.rs` | `Pin`, `AnyPin`, `Pins`, the mode types |
| `src/hal/digital.rs` | `InputPin`, `OutputPin`, `StatefulOutputPin`, `OpenDrainPin` |
| `src/hal/asynch.rs` | `Edge`, `WaitForEdge`, `edge_detection_works` |
| `examples/gpio.rs` | everything above, running |
