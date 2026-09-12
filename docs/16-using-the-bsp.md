# 16 — Using the BSP as a dependency

Everything up to this point describes how `cogware-bcm2837` works. This
document is about consuming it: what a downstream kernel has to provide, why it
has to provide those particular things, and what to do when the link fails.

The examples in `cogware-bcm2837/examples/` do all of this for you, because
Cargo builds them as part of the same package. A separate crate does not get
that for free, and the differences are the subject here.

---

## 1. The shortest working kernel

Four files.

**`Cargo.toml`**

```toml
[package]
name = "my-kernel"
version = "0.1.0"
edition = "2024"

[dependencies]
cogware-bcm2837 = { path = "../cogware-bcm2837" }

# Cargo ignores [profile] in anything but the workspace root, so these belong
# here — or in your workspace root if you have one.
[profile.release]
panic = "abort"       # required: there is no unwinder
opt-level = 2
debug = "full"        # keep DWARF for JTAG; it is not loaded onto the target
codegen-units = 1

[profile.dev]
panic = "abort"
opt-level = 1         # opt-level 0 produces a multi-megabyte image
```

**`.cargo/config.toml`**

```toml
[build]
target = "aarch64-unknown-none"

[target.aarch64-unknown-none]
rustflags = [
  "-C", "target-cpu=cortex-a53",
  "-C", "relocation-model=static",
  "-C", "link-arg=-Tlink.ld",
]

runner = "qemu-system-aarch64 -M raspi3b -serial mon:stdio -serial null -display none -kernel"
```

**`src/main.rs`**

```rust
#![no_std]
#![no_main]

use cogware_bcm2837::{BootInfo, cpu, entry, println};

entry!(main);

fn main(boot: BootInfo) -> ! {
    println!("core {}, EL{}", boot.core_id, boot.exception_level);
    println!("healthy: {}", boot.healthy());
    cpu::halt()
}
```

Then `cargo run --release`. That is a complete, bootable kernel; the transcript
at the end of this document is from exactly these files.

---

## 2. The four rustflags, and why each is needed

| Flag | Why |
| --- | --- |
| `target-cpu=cortex-a53` | Lets LLVM schedule for the real pipeline, and enables the erratum-843419 workaround in the linker. |
| `relocation-model=static` | The kernel loads at a fixed physical address with no dynamic loader. A GOT would be dead weight, and `link.ld` asserts that it stays empty. |
| `link-arg=-Tlink.ld` | Selects the BSP's linker script. **This is the one you cannot omit**; see below. |
| `target = "aarch64-unknown-none"` | The **hard-float** target. Not `-softfloat`. |

For JTAG, add `force-frame-pointers=yes` and `force-unwind-tables=yes`. They
use some space but enable GDB backtraces through optimised code. This
repository sets both.

### Why `-Tlink.ld` has to come from you

This is the one piece of downstream configuration that cannot be automated.

`cogware-bcm2837/build.rs` cannot pass the linker script on your behalf.
Cargo's `cargo::rustc-link-arg` applies to *the emitting package's own* targets
— its binaries, examples, tests and benches — and does **not** propagate across
a dependency edge. When Cargo links `my-kernel`, the BSP's link arguments are
simply not in play.

What a build script *can* do is influence the linker's search path, and that is
the mechanism used here. `build.rs`:

1. copies `link.ld` into `OUT_DIR`, and
2. emits `cargo::rustc-link-search=$OUT_DIR`.

`rustc-link-search` **does** propagate. So by the time your crate links, the
directory containing `link.ld` is on the search path, and the bare name in
`-Tlink.ld` resolves without you knowing or caring where the BSP is checked out.

This is the same arrangement `cortex-m-rt` uses, for the same reason.

The BSP additionally passes `-T<absolute path>` for its *own* examples, which is
why `cargo run --example hello-uart` needs no configuration at all. The two
mechanisms do not conflict: only one of them is ever active for a given link.

---

## 3. `entry!`

The BSP owns `_start` and the two symbols the boot assembly branches to. Your
code is called from the end of that path, and `entry!` is how you name it.

```rust
// Core 0 only. Cores 1-3 stay parked, and print a line if released.
entry!(main);

// Core 0 runs `main`; cores 1-3 run `worker` once released.
entry!(main, worker);

// The same, spelled out.
entry!(primary = main, secondary = worker);
```

Both functions have the same shape:

```rust
fn main(boot: BootInfo) -> ! { .. }
```

It is a `macro_rules!` macro, not an attribute like `#[entry]`, and that is a
consequence of the empty dependency table: an attribute macro would require a
proc-macro crate,
and a proc-macro crate would require `syn` and `quote`. So `entry!` expands to
two `#[unsafe(no_mangle)]` functions named `__cogware_main_primary` and
`__cogware_main_secondary`, which the BSP declares in an `unsafe extern "Rust"`
block and calls.

The practical consequences:

* **Invoke it exactly once**, at your crate root. Twice is a duplicate-symbol
  error at link time.
* **Omitting it is a link error**, not a compile error, and the message names
  `__cogware_main_primary` rather than anything you wrote.
* The macro binds your function to a `fn(BootInfo) -> !` before calling it, so a
  signature mismatch is reported against *your* function rather than somewhere
  inside the macro.

---

## 4. What has already happened when `main` runs

The BSP's `kernel_boot_primary` (`cogware-bcm2837/src/boot.rs`) performs a fixed
sequence before handing over. The order is not arbitrary:

| Step | Why here |
| --- | --- |
| 1. Console | So everything after it can report its own failure. |
| 2. VideoCore memory probe | Must run with the MMU **off**: the mailbox buffer is then already uncached, which is what the GPU requires, and no cache maintenance is needed. Its answer decides where the non-cacheable window goes. |
| 3. `mmu::init` | Needs the probe's answer. |
| 4. `arch::init_core` | Enables the MMU and caches, installs the vector table. **Locks are illegal before this point**, because the exclusive monitor needs Normal memory. |
| 5. `smp::mark_online` | The first thing that takes a lock. |
| 6. `heap::init` | Two stores. The arena is DRAM the map already covers. |
| 7. `jtag::init` | Claims GPIO 22-27, so it needs the GPIO lock. |

So by the time your `main` is entered, `println!` works, `Box`/`Vec`/`String`
work, the MMU and both caches are on, `f64` arithmetic is real hardware
floating point, and an exception will land in a handler rather than a reset
loop.

`BootInfo` reports what each of those steps actually achieved — every field is
read back from the hardware afterwards, not a value the BSP intended to write.
`boot.healthy()` folds the machine-state fields into one check. The VideoCore
probe is deliberately excluded from it: it fails on a board with no GPU
reservation, and the kernel is still perfectly healthy.

`boot.arm_clock` is the one field that reports something you may want to act
on. The firmware boots the cores at their **minimum** rate — 600 MHz of a
possible 1200 on a Pi 3 — and leaves them there. On Linux the cpufreq governor
asks for more within a second of boot and nobody notices; a bare-metal kernel
that never asks runs at half speed forever, and every other field still says
the machine is healthy, because it is.

The BSP reports both numbers and changes neither:

```rust
match boot.arm_clock {
    Ok(clock) if clock.at_maximum() => { /* nothing to do */ }
    Ok(clock) => println!("running at {} of {} MHz", clock.hz / 1_000_000, clock.max_hz / 1_000_000),
    Err(error) => println!("clock unknown ({error})"),
}
```

Raising it is `Mailbox::set_clock_rate`, and it stays your decision: a rate
change costs boot time, has thermal and power consequences the BSP cannot
judge, and needs matching `config.txt` settings. See
[11-videocore-mailbox.md](11-videocore-mailbox.md) before you do it.

### Secondary cores

All four cores leave reset together, but cores 1-3 stop at the MMU gate inside
`kernel_boot_secondary` and stay there. Nothing releases them implicitly:

```rust
// SAFETY: core 0, once, after the console and MMU are up.
unsafe { smp::start_secondaries() };
let online = smp::wait_for_cores(memory::CORE_COUNT, 1_000_000);
```

That is why the single-argument `entry!(main)` is safe on a four-core machine:
the other three are parked at a `wfe` and cost nothing. See
`examples/multicore.rs`.

#### `wait_for_cores` does not wait for *your* code

`smp::wait_for_cores` returns when the BSP has finished bringing the cores up,
which is strictly *before* it calls your secondary entry point. Reading state
that a secondary produces, immediately after this call, is a race — and one
that usually reads the initial value rather than crashing, so it looks like the
secondary never ran:

```rust
unsafe { smp::start_secondaries() };
smp::wait_for_cores(4, 1_000_000);
let total = TALLY.lock().sum;   // WRONG: probably still 0
```

Synchronise on the thing you actually care about. A [`Barrier`] sized to the
number of participating cores is the direct way:

```rust
static START: Barrier = Barrier::new(4);

// in secondary(): ...write to TALLY..., then
START.wait();

// in main(), after wait_for_cores:
START.wait();                   // now every secondary has written
let total = TALLY.lock().sum;
```

[`Barrier`]: ../cogware-bcm2837/src/sync/barrier.rs

---

## 4b. Floating point

The hard-float target gives you `f32`/`f64` arithmetic with no soft-float
library, but `core` only exposes part of the API: `abs`, `min`, `max`,
`copysign` and `recip` are there; `sqrt`, `floor`, `ceil`, `trunc`, `round`,
`round_ties_even` and `mul_add` are `std`-only, because on a general target
they are libm calls.

On the A53 each of those is a single instruction, so the BSP issues them
directly:

```rust
use cogware_bcm2837::math::Float;

let d = (dx * dx + dy * dy).sqrt();
let cells = (width / cell_size).ceil();
let acc = x.mul_add(y, acc);      // one rounding, not two
```

There are deliberately no transcendentals — no `sin`, `ln` or `powf`. The
hardware has no instruction for them, so any implementation would be a software
approximation whose accuracy and size trade-offs belong to the application.
See [`docs/17-floating-point.md`](17-floating-point.md).

---

## 4c. Pins and buses

`Pins::take()` hands out one typed, zero-sized handle per GPIO, once per boot
across all four cores:

```rust
use cogware_bcm2837::hal::gpio::Pins;

let pins = Pins::take().expect("already taken");
let mut led = pins.p16.into_output_low();
led.set_high();                  // one store, no lock
```

The buses are `SpinLock` singletons and claim their own pins in `init`:

```rust
use cogware_bcm2837::{drivers, hal::shared::I2cDevice, drivers::i2c};

drivers::I2C1.lock().init(100_000);

// A Copy handle with the address baked in. Put it in a static and
// every core can use it; one whole transaction per lock acquisition,
// so a register select and its read can never be torn apart.
static RTC: I2cDevice<i2c::I2c> = I2cDevice::new(&drivers::I2C1, 0x68);

// `I2cDevice` is `Copy`, so each core takes its own handle out of the static.
let mut rtc = RTC;
let mut time = [0u8; 7];
rtc.write_read(&[0x00], &mut time)?;
```

`hal::bitbang` gives you the same two buses in software on any pins, which is
the answer when you need a second SPI (QEMU has no SPI1) or when a device
stretches the I²C clock — the BCM2837's hardware master mishandles that, and no
amount of driver care fixes it.

With the `embedded-hal` feature on, which it is by default, all of the above
also implement the `embedded-hal` 1.0 traits, so ecosystem driver crates work
unmodified. See [`docs/18-gpio.md`](18-gpio.md),
[`docs/19-spi-and-i2c.md`](19-spi-and-i2c.md) and
[`docs/20-embedded-hal.md`](20-embedded-hal.md).

---

## 5. Features

```toml
[dependencies]
cogware-bcm2837 = { path = "..", default-features = false, features = ["jtag"] }
```

| Feature | Default | Gates |
| --- | --- | --- |
| `panic-handler` | on | The `#[panic_handler]` in `src/panic.rs`. |
| `allocator` | on | The `#[global_allocator]` static in `src/heap.rs`. |
| `jtag` | on | JTAG pin routing and debug-register unlock during boot. |
| `embedded-hal` | on | `embedded-hal` 1.0 impls for the pins, delays and buses. |
| `embedded-hal-async` | on | The async counterparts. Implies `embedded-hal`. |
| `embedded-sdmmc` | **off** | `BlockDevice` and `TimeSource` over the SD slot, so the `embedded-sdmmc` crate can mount a FAT16/FAT32 volume. See `hal::sdmmc`. |

The last three are the project's only external dependencies, and they are
optional for a reason: turn them off and the dependency list is empty, and the
BSP is exactly what it claims to be.

`embedded-sdmmc` is off by default because, unlike the `embedded-hal` pair, it
is a real implementation rather than a set of signatures. Enable it and add the
same version to your own manifest — the two must unify, or the BSP's
`BlockDevice` impl is for a different trait than the one your `VolumeManager`
wants:

```toml
cogware-bcm2837 = { version = "0.1", features = ["embedded-sdmmc"] }
embedded-sdmmc = { version = "0.10", default-features = false }
```

```rust
use cogware_bcm2837::{drivers, hal::sdmmc::{NoClock, SdCard}};
use embedded_sdmmc::{Mode, VolumeIdx, VolumeManager};

drivers::EMMC.lock().init()?;

// `read_only` fails writes at the block layer, which is what you want on the
// card the board booted from.
let manager = VolumeManager::new(SdCard::read_only(), NoClock);
let volume = manager.open_volume(VolumeIdx(0))?;
let root = volume.open_root_dir()?;
let file = root.open_file_in_dir("CONFIG.TXT", Mode::ReadOnly)?;
```

Leave the `embedded-hal` pair on — they are on by default, because a BSP that
ecosystem drivers cannot talk to is only half a BSP — and you gain two pure
trait crates with no transitive dependencies of their own, after which every
driver crate written against `embedded-hal` 1.0 works against `Spi0`, `I2c`,
the bit-banged buses and the typed pins. See
[`docs/20-embedded-hal.md`](20-embedded-hal.md).

Turning `embedded-hal` off removes the trait impls, not the drivers: `Spi0`,
`I2c`, `bitbang` and the typed pins keep their own inherent methods and the
BSP's own infallible traits in `hal::digital`.

The first two exist because a binary may contain **exactly one** panic handler
and **exactly one** global allocator. If you need your own, turn the feature off
and supply it; nothing else changes. `heap::Kernel` stays public either way, so
you can still install the BSP's allocator yourself, or wrap it:

```rust
#[global_allocator]
static ALLOCATOR: cogware_bcm2837::heap::Kernel = cogware_bcm2837::heap::Kernel;
```

Turning `allocator` off is not merely a matter of dropping the attribute: the
BSP gates `extern crate alloc` on the same feature, so nothing in the linked BSP
can allocate and your image needs **no global allocator at all**. That is the
point — a feature called `allocator` that still forced you to supply one would
be a lie. The one casualty is `drivers::qoi`, whose `decode` returns a `Vec`;
it disappears with the feature, and with it the ability to draw the logo. Every
other driver, the executor, the sync primitives and `math` are allocation-free
and unaffected.

`heap::init()` runs during boot regardless of the feature — it is two stores,
and the `heap::alloc` / `heap::stats` API is useful without the attribute.

`jtag` is the odd one out: it is not a correctness constraint, it just claims
six GPIO pins. Turn it off if you need GPIO 22-27 for something else.

---

## 6. Failure modes

**`undefined symbol: __cogware_main_primary`**
You did not invoke `entry!`, or you invoked it inside a function or module that
was optimised out. Put it at the crate root.

**`undefined symbol: _start`, or an image with no entry point**
`-Tlink.ld` did not reach the linker. Check `.cargo/config.toml` is in the
directory you are running Cargo from — Cargo reads it relative to the current
directory, not the manifest.

**`cannot find linker script link.ld`**
The `rustc-link-search` from `build.rs` did not apply. This usually means the
BSP was not actually rebuilt; try `cargo clean -p cogware-bcm2837`.

**`GOT is not empty: the kernel must be linked position-dependent`**
`relocation-model=static` is missing.

**Duplicate `#[panic_handler]` / `#[global_allocator]`**
You defined one and left the corresponding feature on. Set
`default-features = false` and re-enable only what you want.

**Boots, then hangs silently before any output**
Almost always the wrong target. `aarch64-unknown-none-softfloat` will link, and
then trap on the first `f64` in the BSP because FP/SIMD is enabled for the
hard-float ABI only.

**QEMU exits immediately with no output at all**
An argument QEMU did not accept — it fails at parse time, before the machine
starts, so it looks exactly like a kernel that died on the first instruction.
`-show-cursor` was removed in QEMU 11 and is the usual culprit; see
`docs/09-qemu.md`.

---

## 7. Verified transcript

The files in section 1, in a directory entirely outside this workspace:

```text
$ cargo build --release
   Compiling cogware-bcm2837 v0.1.0 (C:\Users\...\cogware-bcm2837)
   Compiling downstream-test v0.1.0 (C:\Users\...\Temp\bsp-downstream-test)
    Finished `release` profile [optimized + debuginfo] target(s) in 4.53s

$ aarch64-none-elf-readelf -h target/aarch64-unknown-none/release/downstream-test
  Entry point address:               0x80000

$ cargo run --release
downstream crate on core 0, EL1
healthy: true
```

---

## Related

* `docs/01-toolchain-and-build.md` — the workspace, the profiles, and the
  build-script mechanism in more depth.
* `docs/02-boot-and-exception-levels.md` — what `_start` does before Rust runs.
* `docs/04-smp.md` — the spin table and the MMU gate.
* `docs/08-jtag-and-debugging.md` — why the profiles keep full DWARF.
* `docs/09-qemu.md` — running, and where QEMU lies about the hardware.
