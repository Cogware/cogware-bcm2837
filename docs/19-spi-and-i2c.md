# 19. SPI and I²C

The BSP drives both buses two ways: the hardware controllers in
`drivers::spi` / `drivers::i2c`, and software bit-banging in `hal::bitbang` that
works on any pins. Both are wrapped for safe multi-core sharing by
`hal::shared`, and both are exposed through `embedded-hal` — see
[`docs/20-embedded-hal.md`](20-embedded-hal.md).

This chapter is mostly about the parts that are *not* obvious from the
datasheet, because those are the parts that cost debugging time.

---

## 19.1 The core clock is 250 MHz, not 150

Both dividers are documented against "the core clock". The BCM2835 datasheet's
I²C section calls that "nominally 150 MHz". It is wrong — it is a known erratum,
and the real figure on every Pi in the default firmware configuration is
**250 MHz**. Linux's `i2c-bcm2835` and `spi-bcm2835` both read it from the clock
tree and both get 250 MHz.

So:

```
SPI:  SCLK = 250 MHz / CDIV
I²C:  SCL  = 250 MHz / DIV
```

`CORE_CLOCK_HZ` is a public constant in both modules. If you change `core_freq`
in `config.txt`, change it there too — nothing can detect it at runtime, because
the register that would tell you is behind the mailbox and the buses come up
before that is convenient.

## 19.2 Hardware SPI: `drivers::spi`

SPI0 lives at `MMIO_BASE + 0x0020_4000` and owns GPIO 7–11 in `ALT0`:

| GPIO | Function |
| --- | --- |
| 7 | CE1 |
| 8 | CE0 |
| 9 | MISO |
| 10 | MOSI |
| 11 | SCLK |

```rust
let mut spi = drivers::SPI0.lock();
spi.init(spi::Config {
    speed_hz: 1_000_000,
    mode: spi::MODE_0,
    chip_select: spi::ChipSelect::Ce0,
    cs_active_high: false,
});

let mut id = [0u8; 3];
spi.transaction(|bus| {
    bus.write(&[0x9F])?;
    bus.read(&mut id)
})?;
```

`init` claims the five pins itself, so you do not call `into_alt` for them.
`transaction` holds `TA`, and therefore hardware CE, across every transfer and
delay in its closure. Nested calls join the outer transaction. An RAII guard
deasserts CE and clears the FIFOs on return, including error returns. Each
transfer waits for `DONE` before returning, so a following delay is measured
from the end of the preceding transfer, not from its FIFO submission.

CE0 and CE1 have separate polarity bits (`CSPOL0` at bit 21 and `CSPOL1` at
bit 22). Selecting CE1 updates its polarity without changing CE0's idle level.

### Three things the driver has to get right

**1. `CDIV` is not restricted to powers of two.** The datasheet says it must be,
and the hardware does not care; Linux rounds up to the nearest even number and
so do we. A power-of-two-only implementation gives you 976 kHz when you asked
for 1 MHz, which is the sort of thing that makes a marginal device look flaky.
`CDIV = 0` means 65536, so `clock_from_divider` substitutes that rather than
dividing by zero.

**2. SPI is full duplex, and a "write" still fills the RX FIFO.** Every clock
edge shifts a bit out *and* a bit in. If you write 100 bytes and never read, the
64-entry RX FIFO fills at byte 64 and the controller stalls forever with `TXD`
clear. Every routine in the driver — including `write` — reads back exactly as
many bytes as it writes and discards them. The `buses` example transfers 96
bytes specifically to cross that boundary.

**3. Clearing `TA` requires writing `DONE`.** The datasheet documents `DONE` as
read-only. It is not: on real silicon the transfer-active bit does not clear
unless you write `DONE = 1` in the same operation. This is an erratum, it is
what Linux does, and omitting it hangs the *next* transfer, not the current one.

### What QEMU models

`hw/ssi/bcm2835_spi.c` arrived in QEMU 9.0 and is a real implementation — the
`buses` example reads a genuine JEDEC ID (`BF 25 41`) out of an emulated
`sst25vf016b`. Attach devices with:

```powershell
-device sst25vf016b,bus=spi
```

The SSI bus is called `spi`. **SPI1 and SPI2 (the AUX controllers) do not exist
in QEMU at all** — not stubbed, absent. If you need a second hardware SPI under
emulation you cannot have one; bit-bang it.

QEMU's BCM2835 SPI model does **not** wire chip-select changes to the attached
SSI device. Successful JEDEC reads therefore do not prove CE timing or that an
interrupted command is discarded on deselection. The SPI driver's host unit
tests inspect `TA` across nested transfers and error cleanup, and check the CE1
polarity mask; physical CE timing remains untested on hardware.

## 19.3 Hardware I²C: `drivers::i2c`

The BCM2837 calls its I²C controllers BSC (Broadcom Serial Controller). Three
exist; two are usable:

| Controller | Offset | Pins | Notes |
| --- | --- | --- | --- |
| BSC0 | `0x0020_5000` | GPIO 0, 1 | ID EEPROM on a HAT; usually leave alone |
| BSC1 | `0x0080_4000` | GPIO 2, 3 | the header's I²C, `I2C1` |
| BSC2 | `0x0080_5000` | — | HDMI, do not touch |

```rust
let mut i2c = drivers::I2C1.lock();
i2c.init(100_000);

let mut time = [0u8; 7];
i2c.write_read(0x68, &[0x00], &mut time)?;
```

Timing: `DIV` must be even and is clamped to `2..=0xFFFE`, so 100 kHz is 2500
and 400 kHz is 625. `DEL` carries the sampling and driving delays either side of
the `SCL` edge, set to Linux's fractions — `(div/16) << 16 | (div/4)`. `CLKT`,
the clock-stretch timeout, is set to **0**, i.e. disabled, for the reason in the
next section.

### The clock-stretching bug

**The BCM2835/2837 I²C master mishandles clock stretching.** It only tolerates a
slave holding `SCL` down immediately after the ACK phase of a read. Stretch
anywhere else and the master carries on regardless, sampling `SDA` while the
slave is still setting it up, and you get silently corrupted data rather than an
error. Linux flags the adapter `I2C_AQ_NO_CLK_STRETCH` and the raspberrypi
forums are full of people rediscovering it.

There is no software fix. This is why the BSP also ships a
bit-banged I²C: if your device stretches — many EEPROMs and most ADCs do, and
anything running firmware on an MCU certainly does — use `hal::bitbang::I2c`,
which polls `SCL` and waits.

`examples/bitbang.rs` exercises exactly this: a simulated slave holds `SCL` down
after the address ACK, the software master polls it 40 times, and the read
comes back intact.

### Repeated START

`write_read` has to emit `Sr` rather than `P`+`S`, or a device with an
auto-incrementing pointer gives you the wrong register. The BSC does not have an
explicit "repeated start" bit; the trick — again, Linux's — is to program `DLEN`
and re-write `C` with `ST` **while the write phase is still draining and `TA` is
still set**. The controller then chains the two. The driver deliberately does
*not* prefill the TX FIFO for this reason: prefilling lets the write complete
before the read is armed, and you get a STOP.

The driver waits for `TA` before feeding a combined write: `TXD` is only FIFO
space and is also set while the controller is inactive. Before arming the read
it checks that `TA` remains set and `DONE` remains clear. It does not clear the
sticky `DONE` bit between phases, and checks it again after writing `ST`, so a
STOP in that window cannot become a success-shaped STOP/START fallback.
The check is conservative: an already-completed read also returns
`RepeatedStartLost`. Polling and error cleanup are bounded.

Losing the race returns **`RepeatedStartLost`**, never success. Some write bytes
may already have reached the slave, so automatically retrying a non-idempotent
transaction is unsafe. QEMU completes each write synchronously and does not
model BSC repeated START; strict combined reads consequently return this error
there. The hardware chaining path still requires validation on a real board.

The operation-list API merges consecutive write buffers or consecutive read
buffers into one hardware phase without copying. The total for each phase is
limited only by the real 16-bit `DLEN` field, not by an adapter buffer size.
Single-direction lists and a write phase followed by a final read are supported.
A non-final read or a zero-length chained phase is rejected with
`UnsupportedTransaction` **before START**. Use bit-banged I²C for unrestricted
direction changes; it implements the complete operation-list contract.

The `buses` example distinguishes strict transfers from a device-specific
alternative: DS1338 and TMP105 retain their pointer across STOP, so that demo
explicitly performs register selection and reading as separate transfers under
one lock. This is an application choice, not a silent HAL fallback.

### What QEMU models

All three BSCs, since QEMU 9.0. Buses are `i2c-bus.0`, `i2c-bus.1`,
`i2c-bus.2`:

```powershell
-device ds1338,address=0x68,bus=i2c-bus.1
-device tmp105,address=0x49,bus=i2c-bus.1
```

`at24c-eeprom` and `i2c-echo` are available too. `scripts/run-qemu.ps1` and
`scripts/qemu-console-test.ps1`, and their `.sh` counterparts, add all three
devices automatically for the `buses` example, or on `-BusDevices` /
`--bus-devices`.

One emulation quirk shaped the driver. QEMU begins a transfer on **any** write
to `C` that has `I2CEN` set, not just one with `ST`:

```c
if (writeval & (BCM2835_I2C_C_ST | BCM2835_I2C_C_I2CEN)) {
    bcm2835_i2c_begin_transfer(s);
```

and its I²C core only ever *appends* to `current_devs`, clearing the list in
`i2c_end_transfer`. So a separate "clear the FIFO" write to `C` opened a stray
transfer to the *previous* address that nothing ever ended, and the next address
probed then answered as well — a bus scan reported every device twice
(`0x49 0x4a 0x68 0x69` for two devices).

The fix is also the more datasheet-correct code: fold `CLEAR` into the same
write as `ST`, which the datasheet explicitly sanctions — *"If CLEAR and ST are
both set in the same operation, the FIFO is cleared before the new frame is
started"* — and which saves a bus cycle. The scan now reports `0x49 0x68`.

## 19.4 Bit-banging: `hal::bitbang`

Generic over `hal::digital` traits, so it works on any pins, any number of them,
and — importantly — on *fake* pins:

```rust
let mut spi = bitbang::Spi::new(sck, mosi, miso, MODE_0, 1_000_000);
let mut i2c = bitbang::I2c::new(sda, scl, 100_000);
```

`Spi` supports all four modes and takes `NoPin` for MOSI or MISO when the device
is one-directional. `I2c` requires `OpenDrainPin` for both lines, because I²C is
wired-AND: a participant may only pull down, never drive high.

`speed_hz` of **0** means "as fast as the pins will go" — no delay loop at all.
That is the right setting for a simulated bus and roughly 780 kHz on real pins.

The software I²C returns a much more useful error type than the hardware one:

```rust
pub enum Error {
    EmptyRead,         // a complete read phase had no bytes; rejected before START
    AddressNack,        // nobody at that address
    DataNack(usize),    // the device stopped ACKing after N bytes
    ClockTimeout,       // stretched past the spin limit
    BusStuck,           // SDA was already low before START
}
```

`set_stretch_limit` controls how long `ClockTimeout` takes.

`I2c::transaction` accepts `hal::shared::I2cOperation` lists (or a reborrowable
`I2cOperations` implementation). Adjacent same-direction buffers share one
address; every direction change uses repeated START. A read buffer boundary
does not send NACK: only the final byte of the complete read phase does.
Empty buffers do not split phases, an empty list does not touch the pins, and
errors attempt STOP while preserving the first error and releasing both master
outputs. An entirely empty read phase is rejected before START (`EmptyRead`):
after acknowledging a read address a slave may already hold SDA low for its
first data bit, so a STOP cannot be guaranteed without clocking an extra byte.
Empty buffers inside a non-empty read phase remain supported.

### Why the simulated-pin harness earns its keep

`examples/bitbang.rs` builds `Cell`/`RefCell`-based wired-AND nets and two
simulated devices — an SPI slave written *from the SPI specification* rather
than from our master, and a seven-phase I²C slave state machine. Because the
slave samples on the edge the mode says it should, a phase error fails instead
of cancelling out.

It immediately found a real bug. `I2c::restart` released `SDA` before driving
`SCL` low. But the previous byte ended on the slave's ACK, where the *slave* is
holding `SDA` down and `SCL` is still high — with no falling edge on `SCL` the
slave never releases, `SDA` never rises, and the START condition's falling edge
never happens. The repeated START was silently lost. On real hardware that is a
scope-and-guess afternoon; here it was a failing assertion.

The regression checks now also run a `W/W/R/R/W/R` sequence with empty buffers
through the native, embedded-hal, shared, and async shared adapters. The slave
checks the data, four STARTs, one STOP, two read ACKs and two read NACKs. Further
checks cover NACK recovery, stretch-timeout cleanup and GPIO-CS error cleanup.

## 19.5 Sharing a bus across four cores: `hal::shared`

A bus is a `SpinLock` singleton — `drivers::SPI0`, `drivers::I2C0`,
`drivers::I2C1`. The wrappers in `hal::shared` turn one into a handle you can
copy to every core:

```rust
static RTC: I2cDevice<i2c::I2c> = I2cDevice::new(&drivers::I2C1, 0x68);

// on any core:
let mut rtc = RTC;
rtc.write_read(&[0x00], &mut time)?;
```

| Type | Purpose |
| --- | --- |
| `SharedI2c<B>` | a `Copy` handle to a bus; `transaction` runs a closure under the lock |
| `I2cDevice<B>` | the same with a device address baked in |
| `SpiDevice<B, CS>` | a bus plus a `ChipSelect` — `GpioCs<P>` or `NoCs` |
| `SpiBus` / `I2cBus` | the traits both the hardware and bit-banged buses implement |
| `I2cTransaction` | an `I2cBus` that implements operation lists without splitting transactions |
| `I2cOperation` / `I2cOperations` | allocation-free native buffers and reborrowable adapter lists |

**The unit of atomicity is one transaction, not one byte.** Every method takes
the lock, runs the complete write/read/write_read, and releases it. Nothing can
interleave between a register-select write and the read that follows it — which
is the failure mode that makes shared I²C buses corrupt data in
naïvely written firmware.

`transaction` exists for the cases where one transfer is not enough: a
write-then-verify, or a device that needs a delay mid-sequence. Everything
inside the closure runs under one acquisition.

The SPI wrapper also enters `SpiBus::transaction`: `NoCs` with hardware CE now
holds selection for the entire closure, not separately for each transfer.
`GpioCs` still asserts before bus activity and deasserts after controller
cleanup. Configure hardware `ChipSelect::None` when a GPIO owns selection.

For I²C, the existing closure API is only a software lock boundary. Use
`I2cTransaction::transaction` for one electrical transaction. Existing custom
`I2cBus` implementations keep their simple API; to support the embedded-hal
shared adapters they must implement `I2cTransaction` too. There is deliberately
no default implementation that turns an unsupported list into separate calls.

The `buses` example exercises it: all four cores run 16 write/read-back rounds
each against different NVRAM addresses on the same DS1338, 64 transactions
total, and a torn transaction would show up as a mismatch. It reports 0.

## 19.6 Source

| File | Contents |
| --- | --- |
| `src/drivers/spi.rs` | SPI0: `Config`, `Mode`, `ChipSelect`, transfers |
| `src/drivers/i2c.rs` | BSC0/BSC1: timing, repeated START, `scan` |
| `src/drivers/mod.rs` | the `SPI0`, `I2C0`, `I2C1` singletons |
| `src/hal/bitbang.rs` | software SPI and I²C |
| `src/hal/shared.rs` | `SharedI2c`, `I2cDevice`, `SpiDevice`, the bus traits |
| `examples/buses.rs` | hardware buses against emulated devices, four cores |
| `examples/bitbang.rs` | software buses against simulated devices |
