# cogware-bcm2837

**A bare-metal board support package for the Broadcom BCM2837 — the quad-core
Cortex-A53 in the Raspberry Pi 3.**

[![rust](https://img.shields.io/badge/rust-stable-b7410e)](https://www.rust-lang.org)
[![edition](https://img.shields.io/badge/edition-2024-b7410e)](https://doc.rust-lang.org/edition-guide/rust-2024/)
[![target](https://img.shields.io/badge/target-aarch64--unknown--none-informational)](https://doc.rust-lang.org/rustc/platform-support/aarch64-unknown-none.html)
[![float](https://img.shields.io/badge/float-hard-success)](../docs/17-floating-point.md)
[![no_std](https://img.shields.io/badge/no__std-yes-success)](https://docs.rust-embedded.org/book/intro/no-std.html)
[![licence](https://img.shields.io/badge/licence-MIT%20OR%20Apache--2.0-blue)](#licence)

Takes a kernel image from the firmware's first instruction to a point where
safe Rust runs on four cores with the MMU, caches, FPU and `alloc` all live —
then gets out of the way.

Written from scratch for **current stable Rust**, against the datasheet. No
Docker, no Makefile, no build system to learn. The only dependencies are two
optional trait-definition crates that contain no code.

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

```text
core 0, EL1
healthy: true
```

By the time `main` runs, the console, MMU, caches, exception vectors, FPU and
heap are all up. Cores 1–3 wait in a parking loop until you release them.

## Why this crate exists

Most AArch64 Pi projects use `aarch64-unknown-none-softfloat` and route every
`f64` through a software library, because a hard-float kernel has to ungate
FP/SIMD before the first Rust instruction and save 32 NEON registers on every
trap. This crate does both of those things, so the FPU the silicon already has
is available.

The other half is sharing. Four cores reach the same GPIO block, the same UART
and the same buses. Here every peripheral is a lock-guarded static, the unit of
atomicity on a bus is a whole transaction rather than a register write, and the
lock ordering is written down rather than remembered.

## Highlights

| | |
| --- | --- |
| **Boot** | EL3/EL2 → EL1 descent, per-core stacks, `.bss` zeroing, all four cores |
| **MMU** | identity-mapped stage-1 tables, W^X, three memory types |
| **Hard float** | FP/SIMD ungated per core; the trap frame saves all 32 NEON registers |
| **Exceptions** | full vector table with a decoded fault dump |
| **SMP** | secondary release, an MMU publication gate, spin locks and barriers |
| **Async** | an allocation-free executor per core, with timers, channels and an async `Mutex` |
| **Heap** | ~300-line O(1) size-class allocator over DRAM the MMU already maps |
| **GPIO** | `Pin<N, MODE>` typestate; `set_high`/`is_high` take **no lock at all** |
| **Buses** | hardware SPI0 and BSC0/BSC1, plus bit-banged SPI and I²C on any pins |
| **`embedded-hal`** | 1.0 blocking *and* async, so ecosystem drivers work unmodified |
| **Display** | 1280×720×32 framebuffer, double buffered, 2D drawing, 8×8 font, QOI decoder |
| **Storage** | Arasan EMMC: identification, 4-bit bus, verified 512-byte block I/O |
| **Mailbox** | VideoCore property channel — board identity, DRAM split, clocks, temperature |
| **Debug** | JTAG on GPIO 22–27, debug registers unlocked, full DWARF in *every* profile |
| **Maths** | `sqrt`, the rounding family and fused `mul_add` as single instructions, not libm |

Deliberately out of scope: userspace, a filesystem, networking, V3D shaders.

## Getting started

```toml
[dependencies]
cogware-bcm2837 = "0.1"
```

A kernel is not an ordinary binary, so a downstream crate needs a little Cargo
configuration of its own — the target, the linker script, and three flags:

```toml
# .cargo/config.toml
[build]
target = "aarch64-unknown-none"

[target.aarch64-unknown-none]
rustflags = [
  "-C", "target-cpu=cortex-a53",
  "-C", "relocation-model=static",
  "-C", "link-arg=-Tlink.ld",
]
```

```toml
# Cargo.toml — Cargo ignores [profile] outside the workspace root, so these
# belong in yours.
[profile.release]
panic = "abort"       # required: there is no unwinder
opt-level = 2
debug = "full"        # keep DWARF for JTAG; it is not loaded onto the target
codegen-units = 1
```

`link.ld` resolves because this crate's build script stages it in `OUT_DIR` and
publishes that directory as a link search path — but link arguments do not
cross a dependency edge, so the `-Tlink.ld` above has to be yours. See
[`docs/16-using-the-bsp.md`](../docs/16-using-the-bsp.md) for details,
including how to override the script.

Bringing the other three cores up is one `unsafe` call, because it must happen
exactly once and only after the MMU is live:

```rust
entry!(primary = main, secondary = worker);

fn main(boot: BootInfo) -> ! {
    // SAFETY: called once, from the primary core, after the MMU is enabled.
    unsafe { smp::start_secondaries() };
    let online = smp::wait_for_cores(4, 1_000_000);
    println!("{online} cores online");
    task::run()          // this core's async main loop
}

fn worker(_boot: BootInfo) -> ! {
    task::run()
}
```

## Features

| Feature | Default | Gates |
| --- | --- | --- |
| `panic-handler` | on | the `#[panic_handler]` |
| `allocator` | on | the `#[global_allocator]`, and `extern crate alloc` with it |
| `jtag` | on | JTAG pin routing during boot (claims GPIO 22–27) |
| `embedded-hal` | on | the `embedded-hal` 1.0 trait implementations |
| `embedded-hal-async` | on | the `embedded-hal-async` 1.0 implementations (implies `embedded-hal`) |
| `embedded-sdmmc` | **off** | `BlockDevice`/`TimeSource` over the SD slot, so `embedded-sdmmc` can mount FAT16/FAT32 |

The first two exist because a binary may contain exactly one of each; turn one
off to supply your own. Turning off `allocator` unlinks `alloc` entirely, so
the resulting image needs **no** global allocator rather than a different one —
verified by an external consumer that boots four cores with none at all. The
only casualty is `qoi`, whose decoder returns a `Vec`. The [`heap`] module and
its API stay available either way, for a manually driven arena.

`--no-default-features` builds the entire BSP with an **empty** dependency list
and nothing claimed. Every combination is built and booted locally.

## Examples

Every example is a complete, bootable kernel. Run one with
`cargo run --release --example hello-uart`.

| Example | Shows |
| --- | --- |
| [`hello-uart`](examples/hello-uart.rs) | the smallest possible kernel, and a boot self-check |
| [`multicore`](examples/multicore.rs) | releasing cores 1–3, a shared `SpinLock`, a `Barrier` |
| [`regressions`](examples/regressions.rs) | guard auto-traits, console exclusion, emergency output and timer fairness |
| [`async-tasks`](examples/async-tasks.rs) | the executor, channels, timers, cross-core spawning |
| [`heap`](examples/heap.rs) | `Box`/`Vec`/`String`, allocator statistics, size classes |
| [`gpio`](examples/gpio.rs) | typestate pins, ownership, alternate functions |
| [`bitbang`](examples/bitbang.rs) | software SPI and I²C, verified against simulated devices |
| [`buses`](examples/buses.rs) | hardware SPI0 and BSC1 against emulated chips, on four cores |
| [`async-bus`](examples/async-bus.rs) | `embedded-hal-async` buses, delays and pin waits |
| [`display`](examples/display.rs) | framebuffer mode set, a QOI logo, text over an image |
| [`sdcard`](examples/sdcard.rs) | EMMC bring-up, block read, verified write |
| [`shell`](examples/shell/main.rs) | all of the above, interactively — the reference kernel |
| [`chainloader`](examples/chainloader.rs) | receives a kernel over the UART and boots it, so the SD card is written once |

## Module map

```text
cogware_bcm2837
├── boot          BootInfo, the entry! macro, the parking loop
├── smp           secondary release, core counting, the MMU gate
├── arch          cpu, mmu, exception decode; boot.s, exception.s, cache.s
├── bsp::memory   every hardware address in the project, in one place
├── console       lock-or-bypass console, chosen on MMU state
├── print         print!, println!, cprintln!
├── heap          the O(1) allocator behind `alloc`, and its statistics
├── math          Float: sqrt, floor/ceil/trunc/round/round_ties_even, mul_add
├── time          the architectural counter; uptime, Instant, Duration
├── sync          SpinLock, Once, Barrier
├── task          executor, waker, Mutex, Channel, timers, sleep
├── drivers       mmio, gpio, pl011, spi, i2c, mailbox, framebuffer, font, qoi, emmc
├── hal           Pin<N, MODE>, digital traits, delays, bitbang, SharedI2c/SpiDevice
└── debug::jtag   pin routing and the debug-register unlock
```

## Documentation

The design notes live in [`docs/`](../docs). Start with
[the overview](../docs/00-overview.md), or jump to what you need:

| | |
| --- | --- |
| [Host setup](../docs/21-host-setup.md) | installing the toolchain on Linux and Windows, for QEMU and for hardware |
| [Toolchain and build](../docs/01-toolchain-and-build.md) | target, flags, linker script, artefacts |
| [Using the BSP](../docs/16-using-the-bsp.md) | consuming this crate from another project |
| [Boot](../docs/02-boot-and-exception-levels.md) · [MMU](../docs/03-memory-and-mmu.md) · [SMP](../docs/04-smp.md) | bring-up |
| [Drivers](../docs/05-drivers-and-mmio.md) · [GPIO](../docs/18-gpio.md) · [SPI/I²C](../docs/19-spi-and-i2c.md) · [embedded-hal](../docs/20-embedded-hal.md) | peripherals |
| [Async](../docs/12-async.md) · [Sync](../docs/06-synchronisation.md) · [Heap](../docs/13-heap.md) | runtime |
| [QEMU](../docs/09-qemu.md) · [JTAG](../docs/08-jtag-and-debugging.md) · [Chain-loading](../docs/22-chainloading.md) | running and debugging |
| [Host scripts](../docs/23-scripts.md) | every helper script, Windows and Linux, option by option |

## Safety model

Boot is inherently unsafe: it runs with the MMU off, mutates system registers
and reaches hardware through raw pointers. All of that is confined to this
crate. Every `unsafe` block carries a `SAFETY:` comment naming the invariant it
relies on, and the public API is safe wherever it can be. Where it cannot —
`smp::start_secondaries` and the `init` functions — the contract is documented
under a `# Safety` heading.

## Status

**Runs on real silicon.** Validated on a Raspberry Pi Zero 2 W: four-core boot
out of the firmware spin table, EL2 to EL1 descent, MMU and caches, the
hard-float FPU on every core, the exception self-tests, JTAG pin routing, the
async executor, the heap, the interactive shell over UART, and chain-loading a
kernel over serial with all four cores surviving the handover.

HDMI output is confirmed on hardware too: the VideoCore mode set, the 1280x720
framebuffer and a QOI image composited over it were captured off the Pi's HDMI
through a capture card.

SD card identification and block reads are confirmed on a real 16 GB SDHC card:
CSD v2 capacity decode, a 403 ms enumeration that emulation completes in 10 ms,
and a file read out of the FAT32 boot partition.

Also verified under QEMU `raspi3b`: hardware and bit-banged SPI/I²C against
emulated chips, SD block writes, source-level debugging over the GDB stub, and
the crate consumed as a dependency from an unrelated workspace under several
feature sets.

Still unverified on silicon: physical JTAG with a probe attached, EMMC writes —
the boot card is not expendable — and `CPUECTLR_EL1.SMPEN`, which sits on the
EL3-only path that Pi firmware never takes. Each is marked as such in the source
and in the docs.

## Licence

Licensed under either of

* Apache License, Version 2.0
* MIT License

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in this crate shall be dual licensed as
above, without any additional terms or conditions.
