# 20. embedded-hal

`embedded-hal` is the Rust embedded ecosystem's set of bus and pin traits. A
driver crate written against them — for an OLED panel, an IMU, a CAN
controller — works on any chip whose BSP implements them. The BSP implements
them, so those drivers work here.

This is the **only** external dependency in the project, and
[`instructions.md`](../instructions.md) rule 1.1 explains the terms.

---

## 20.1 Why the exception was granted

The rule is "no external crates, ever", and it exists so that every line in the
image is inspectable. `embedded-hal` is admitted because it does not weaken
that:

- It is a **pure trait crate**. There is no code — no register access, no
  allocation, no `unsafe`, no runtime. What you audit is a set of signatures.
- It has **zero transitive dependencies**. Adding it adds exactly one crate, not
  a tree. Same for `embedded-hal-async`.
- It is **optional**, behind the `embedded-hal` / `embedded-hal-async` features.
  They are on by default, because a BSP ecosystem drivers cannot talk to is
  only half a BSP — but `default-features = false` gives you the entire BSP
  with an empty dependency list, and nothing else changes.
- Nothing in `src/` outside `src/hal/eh.rs` and `src/hal/asynch.rs` names an
  `embedded_hal` type. The BSP is written against its own traits; the eh layer
  is an adapter bolted on top and can be removed without touching anything else.

One further `embedded-*` crate is admitted on different terms.
**`embedded-sdmmc`** is *not* a trait crate: it is a FAT16/FAT32
implementation, and it brings transitive dependencies. It is therefore **off by
default**, and it is in the tree only because a correct FAT reader is a large
amount of fiddly code with nothing board-specific about it — exactly the kind
of wheel worth not reinventing. The same confinement rule applies: nothing
outside `src/hal/sdmmc.rs` names an `embedded_sdmmc` type, and that file does
no more than implement `BlockDevice` over
[`drivers::emmc`](14-emmc.md). See `hal::sdmmc`.

The exception does not generalise. Any other crate needs a fresh decision.

## 20.2 Our traits underneath, theirs on top

`hal::digital` defines four **infallible** traits:

```rust
pub trait InputPin        { fn is_high(&self) -> bool;  fn is_low(&self) -> bool; }
pub trait OutputPin       { fn set_high(&mut self);     fn set_low(&mut self); ... }
pub trait StatefulOutputPin: OutputPin { fn is_set_high(&self) -> bool; ... }
pub trait OpenDrainPin    { fn drive_low(&mut self);    fn release(&mut self); ... }
```

No `Result`, because a GPIO write on this chip is a store to Device memory that
cannot fail. That matters in the bit-bang inner loops, where the embedded-hal
shape would put a `Result<(), Infallible>` on every single edge — free at
runtime, noisy in the source, and it would make `?` appear in code that has
nothing to fail.

`embedded-hal` is then implemented in terms of those. You pay for the `Result`
only where you asked for the compatibility.

## 20.3 What is implemented

| embedded-hal trait | Implemented for |
| --- | --- |
| `digital::InputPin` | `Pin<N, MODE>` where readable, `AnyPin<MODE>` |
| `digital::OutputPin` | `Pin<N, Output>`, `Pin<N, OpenDrain>`, `AnyPin` |
| `digital::StatefulOutputPin` | the same |
| `delay::DelayNs` | `hal::delay::Delay` |
| `spi::SpiBus` | `drivers::spi::Spi0`, `hal::bitbang::Spi` |
| `spi::SpiDevice` | `hal::shared::SpiDevice<B, CS>` |
| `i2c::I2c` | `drivers::i2c::I2c`, `hal::bitbang::I2c`, `SharedI2c` |

`I2cDevice` deliberately does *not* implement `i2c::I2c`: that trait takes the
address as an argument on every call, and an `I2cDevice` has one baked in.
Hand a driver crate a `SharedI2c` and let it address its own chip; use
`I2cDevice` in your own code, where the point is that you cannot get the
address wrong.

and from `embedded-hal-async`:

| Trait | Implemented for |
| --- | --- |
| `digital::Wait` | `Pin<N, MODE>`, `AnyPin<MODE>` |
| `delay::DelayNs` | `Delay` |
| `spi::SpiDevice` | `hal::shared::SpiDevice` |
| `i2c::I2c` | `SharedI2c` |

Usage is exactly what you would expect:

```rust
use embedded_hal::i2c::I2c;

fn read_seconds<I: I2c>(i2c: &mut I) -> Result<u8, I::Error> {
    let mut byte = [0u8; 1];
    i2c.write_read(0x68, &[0x00], &mut byte)?;
    Ok(byte[0])
}

// hardware BSC1, bit-banged, or a shared handle — all three compile
read_seconds(&mut RTC_BUS)?;
```

## 20.4 Adapter mismatches

**`is_high` takes `&mut self` in embedded-hal 1.0.** Both
`digital::InputPin::is_high` and `StatefulOutputPin::is_set_high` do. Ours take
`&self`, the correct signature for a load. The adapter reborrows.

**Error kinds are narrow.** `digital::ErrorKind` has exactly one variant,
`Other`. `spi::ErrorKind` has no timeout variant, so both of our SPI errors map
to `Other` too. The concrete error type is preserved in `Error::kind`'s
`Debug` output; only the classification is lossy.

**Operation sets differ.** `i2c::Operation` is just `Read`/`Write`;
`spi::Operation` adds `Transfer`, `TransferInPlace` and `DelayNs(u32)`.
The I²C adapter reborrows each buffer through our `I2cOperations` interface:
there is no allocation, copy, or fixed maximum number of operations. The bus,
not the adapter, controls the whole electrical transaction. Consecutive writes
or reads are contiguous, direction changes use repeated START, and only the
final byte of a read phase is NACKed.

Bit-banged I²C supports arbitrary direction changes. An entirely empty read
phase is rejected before START, rather than clocking an unrequested byte to
release SDA; empty buffers within a non-empty phase are supported.
The BSC supports a single
direction or a write phase followed by a final read, with up to 65535 bytes per
phase. Unsupported sequences (including a non-final read or an empty chained
phase) return `UnsupportedTransaction` before START. If the write drains before
the repeated START is armed, the result is `RepeatedStartLost`, not a silent
STOP/START substitution. The write may already have changed the device.
QEMU currently cannot emulate BSC chaining, so this error is expected there.

`SharedI2c<B>` requires `B: I2cTransaction` for its embedded-hal implementations.
Existing custom `I2cBus` implementations retain their simple API but must opt in
to operation-list support; an incorrect generic fallback is not provided.

**Unlike `SpiBus`, `SpiDevice` asserts and deasserts CS for each call.** A
command-then-response exchange **must be one call**. Reading the JEDEC ID from
a flash chip as

```rust
dev.write(&[0x9F])?;      // CE0 drops here
dev.read(&mut id)?;       // CE0 rises again: the chip has forgotten the command
```

returns zeros, because the SST25VF016B — like most SPI devices — abandons the
command when chip select deasserts. Issue it as a single transaction instead:

```rust
let mut buf = [0x9F, 0, 0, 0];
dev.transfer_in_place(&mut buf)?;   // buf[1..4] == [0xBF, 0x25, 0x41]
```

or use `dev.transaction(&mut [Operation::Write(&cmd), Operation::Read(&mut id)])`,
which holds CS across both, as the device protocol requires.
**QEMU does not wire SPI chip-select changes to its
SSI slave**, so it cannot validate interrupted-command reset or CE timing.
Native `Spi0::transaction` and the shared wrapper keep `TA` set across the
entire operation list, including delays, and clean it up on errors. Host
register tests check that state machine; physical CE timing remains untested.

**Blanket impls are impossible.** `impl<T: our::OutputPin> eh::OutputPin for T`
violates the orphan rule, so `src/hal/eh.rs` implements concretely on `Pin` and
`AnyPin`. Readability is gated with `where Pin<N, MODE>: crate::hal::digital::InputPin`,
which transitively requires the private `Readable` marker without naming it —
so `Alt<A>` pins still cannot be read, through the eh traits or otherwise.

## 20.5 The async side

`embedded_hal_async::digital::Wait` maps onto `WaitForEdge` from
[`docs/18-gpio.md`](18-gpio.md). The future arms `GPREN`/`GPFEN` on first poll
and disarms on drop, so cancelling a `select!` branch leaves no state behind.

The async bus impls yield once and then run the blocking transaction. That is
deliberate and it is not a placeholder for something better: an I²C byte at
100 kHz is 90 µs, and a DMA-driven or interrupt-driven transfer would need the
GIC plumbing plus a completion waker for what is, in this BSP's target use, a
handful of transfers at startup. Yielding lets other tasks run at transaction
granularity, which is the granularity the lock works at anyway (§19.5). If you
are streaming to a display over SPI at 30 fps, that is the point to revisit it.

`examples/async-bus.rs` runs an ecosystem-shaped driver generic over
`embedded_hal_async::i2c::I2c`, four cores contending for one bus entirely from
tasks (48 async scatter writes with locked readback), and `DelayNs` sleeping on the timer while
the cores idle in `wfe` several hundred times rather than spinning.
Strict repeated START is reported as unavailable in QEMU. The DS1338-specific
readback explicitly allows STOP between pointer selection and reading, keeping
both transfers under one lock without awaiting. The bitbang example separately
checks a complete multi-direction operation list through the async shared
adapter against a simulated slave, including its ACK/NACK and START/STOP edges.

One trap it documents by avoiding: **never call `Barrier::wait` from an async
task.** It is a blocking spin, and a blocking spin inside a cooperative
executor starves every other task on that core — including the timers those
tasks are waiting on. Watch an atomic from a task that yields instead.

> Under QEMU, `Wait` never completes — `GPEDS` is not modelled. Call
> `hal::asynch::edge_detection_works()` to find out at runtime rather than
> hanging. See §18.7.

## 20.6 Turning it on and off

Both features are **on by default** — a BSP that ecosystem drivers cannot talk
to is only half a BSP:

```toml
[dependencies]
cogware-bcm2837 = "0.1"
```

To take the dependency list back to empty:

```toml
cogware-bcm2837 = { version = "0.1", default-features = false, features = ["panic-handler", "allocator", "jtag"] }
```

| Feature | Effect |
| --- | --- |
| `embedded-hal` | the blocking trait impls |
| `embedded-hal-async` | the async ones; implies `embedded-hal` |
| `embedded-sdmmc` | `BlockDevice` over the SD slot; **off** by default |

Turning them off removes the trait impls, not the drivers. `Spi0`, `I2c`,
`bitbang::Spi`, `bitbang::I2c`, `Pin` and `AnyPin` keep every inherent method
and every one of the BSP's own traits. Only the ecosystem compatibility goes.

## 20.7 Source

| File | Contents |
| --- | --- |
| `src/hal/digital.rs` | our infallible pin traits |
| `src/hal/eh.rs` | the whole embedded-hal 1.0 adapter (private module) |
| `src/hal/asynch.rs` | `WaitForEdge`, `Wait`, async delay and bus impls |
| `examples/gpio.rs` | a generic `OutputPin` driver |
| `examples/buses.rs` | a generic `I2c` driver over a shared bus |
| `examples/async-bus.rs` | a generic *async* `I2c` driver, async delays, and the `Wait` probe |
