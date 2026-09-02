# cogware-bcm2837

A bare-metal AArch64 **board support package** for the Broadcom BCM2837 — the
quad-core Cortex-A53 SoC in the Raspberry Pi 3 — written from scratch for
**current stable Rust** and the **hard-float** `aarch64-unknown-none` target.

No external crates, save two optional trait-only dependencies. No Docker, no
Makefile. `core`, `alloc` backed by our own allocator, and a linker script.

It takes a kernel image from the firmware's first instruction to a point where
safe Rust runs on four cores with the MMU, caches, FPU and `alloc` all live, and
then gets out of the way.

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

```
Hello from the BCM2837.

  core            : 0
  exception level : EL1
  MPIDR_EL1       : 0x0000000080000000
  FP/SIMD         : yes
  MMU             : yes
  D-cache         : yes
  I-cache         : yes
  VBAR_EL1        : 0x0000000000080800
  VideoCore       : 64 MiB at 0x003c000000

  FPU check       : H(4) = 2.083333333333333

Boot complete in 17.242192ms. All checks passed.
```

## What it does

- Boots on all **four cores**, descending from EL3 or EL2 to EL1 as the reset
  state requires.
- Enables the **MMU** with an identity-mapped, W^X stage-1 translation table
  and three memory types (Device, Normal, Normal non-cacheable).
- Uses the **hard-float target**, so the FPU and NEON are live on every core —
  and the exception path saves the entire NEON register file to make that safe.
- Shares hardware **across cores** behind spin locks: GPIO, the PL011 UART, the
  VideoCore mailbox, the framebuffer and the SD controller are all lock-guarded
  statics.
- Installs a full **exception vector table** with a decoded fault dump.
- Talks to the **VideoCore property mailbox** — board identity, the DRAM split,
  temperature, every clock, and the hooks for GPU power and CPU upclocking.
- Routes **JTAG** to GPIO 22-27 and unlocks the debug registers; keeps full
  DWARF in every profile.
- Provides an allocation-free **async executor**, one per core, so each core's
  main loop can be `async`.
- Backs `alloc` with a **300-line O(1) heap** over DRAM the MMU already maps,
  so `Box`, `Vec` and `String` work and boot time does not move.
- Fills in the **floating-point API `core` omits** — `sqrt`, the rounding
  family and a fused `mul_add` — as single FPU instructions, not libm calls.
- Drives the **VideoCore framebuffer** at 1280x720x32 with double buffering, a
  clipped 2D drawing set, an 8x8 font and a from-scratch **QOI decoder**, so
  images composite with alpha and text draws on top of them.
- Reads and writes the **SD card** over the Arasan EMMC controller: full
  identification, a 4-bit bus and verified 512-byte block I/O.
- Gives you **typed, zero-sized GPIO pins** — the pin number and the mode live
  in the type, `Pins::take()` hands them out once, and the hot paths
  (`set_high`, `is_high`) take no lock at all because the registers are
  write-1-to-act.
- Drives **SPI and I²C** in hardware (SPI0, BSC0/BSC1) and in software on any
  pins, with the BCM2837's clock-stretch and full-duplex-FIFO errata handled
  rather than discovered later.
- **Shares a bus safely across all four cores**: `I2cDevice` and `SpiDevice` are
  `Copy` handles whose unit of atomicity is a whole transaction, so a
  register-select and its read can never be torn apart.
- Implements **`embedded-hal` 1.0 and `embedded-hal-async`** behind an optional
  feature, so off-the-shelf sensor and display drivers work unmodified.
- **Mounts FAT16/FAT32 on the SD card** behind an optional, off-by-default
  `embedded-sdmmc` feature, so a kernel can read files by name.

Deliberately out of scope: userspace, networking, V3D shaders.
See [`docs/10-roadmap.md`](docs/10-roadmap.md).

## Using it

Add the dependency, add three rustflags, name your entry point. The full recipe
is in [`docs/16-using-the-bsp.md`](docs/16-using-the-bsp.md).

```toml
[dependencies]
cogware-bcm2837 = { path = "../cogware-bcm2837" }
```

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

### Features

| Feature | Default | Gates |
| --- | --- | --- |
| `panic-handler` | on | the `#[panic_handler]` |
| `allocator` | on | the `#[global_allocator]` |
| `jtag` | on | JTAG pin routing during boot |
| `embedded-hal` | on | the `embedded-hal` 1.0 trait implementations |
| `embedded-hal-async` | on | the `embedded-hal-async` impls; implies `embedded-hal` |
| `embedded-sdmmc` | **off** | `BlockDevice` over the SD slot, for FAT16/FAT32 |

The first two exist because a binary may contain exactly one of each. Turn one
off to supply your own. The `embedded-hal` pair are trait definitions with no
code — turn them off and the drivers remain, only the trait impls go.

`embedded-sdmmc` is the one feature off by default. It is a real FAT
implementation rather than a trait crate, so a kernel that never touches the
card should not pay for it; turn it on and `hal::sdmmc::SdCard` lets that crate
mount the boot partition. Whatever you enable, `default-features = false`
still builds the whole BSP with an empty dependency list.

## Examples

Every example is a complete, bootable kernel.

| Example | Shows |
| --- | --- |
| [`hello-uart`](cogware-bcm2837/examples/hello-uart.rs) | the smallest possible kernel, and a boot self-check |
| [`multicore`](cogware-bcm2837/examples/multicore.rs) | releasing cores 1-3, a shared `SpinLock`, a `Barrier` |
| [`async-tasks`](cogware-bcm2837/examples/async-tasks.rs) | the executor, channels, timers, cross-core spawning |
| [`heap`](cogware-bcm2837/examples/heap.rs) | `Box`/`Vec`/`String`, allocator statistics, size classes |
| [`display`](cogware-bcm2837/examples/display.rs) | framebuffer mode set, the QOI logo, text over an image |
| [`sdcard`](cogware-bcm2837/examples/sdcard.rs) | EMMC bring-up, block read, verified write |
| [`gpio`](cogware-bcm2837/examples/gpio.rs) | typestate pins, ownership, alt functions, `embedded-hal` |
| [`bitbang`](cogware-bcm2837/examples/bitbang.rs) | software SPI and I²C, verified against simulated devices |
| [`buses`](cogware-bcm2837/examples/buses.rs) | hardware SPI0 and BSC1 against emulated chips, on four cores |
| [`async-bus`](cogware-bcm2837/examples/async-bus.rs) | `embedded-hal-async` buses, delays and pin waits on the executors |
| [`shell`](cogware-bcm2837/examples/shell/main.rs) | all of the above, interactively — the reference kernel |
| [`chainloader`](cogware-bcm2837/examples/chainloader.rs) | receives a kernel over serial and boots it, so the card stays put |

```powershell
cargo run --release --example hello-uart
cargo run --release --example shell

# Push a kernel over the wire instead of reflashing — see docs/22.
.\scripts\chainboot-qemu.ps1 -Payload multicore    # into QEMU
.\scripts\chainboot.ps1 multicore -Serial COM3     # onto the board
```
```bash
cargo run --release --example hello-uart
cargo run --release --example shell

./scripts/chainboot-qemu.sh --payload multicore    # into QEMU
cargo chain --example multicore                    # onto the board
```

## Requirements

| Tool | Version used | Notes |
| --- | --- | --- |
| Rust | current stable, edition 2024 | channel pinned by `rust-toolchain.toml` |
| Target | `aarch64-unknown-none` | hard float — **not** `-softfloat` |
| QEMU | 11.1.0 | `qemu-system-aarch64`, machine `raspi3b` |
| ARM GNU Toolchain | 15.2.Rel1 | `aarch64-none-elf-objcopy`, `-gdb` |

[`docs/21-host-setup.md`](docs/21-host-setup.md) has step-by-step instructions
for Linux and Windows, for emulation and for real hardware.

On Windows with [Scoop](https://scoop.sh) — no WSL, no administrator rights:

```powershell
rustup target add aarch64-unknown-none
scoop install qemu gcc-aarch64-none-elf
```

On Linux:

```bash
rustup target add aarch64-unknown-none
sudo pacman -S qemu-system-aarch64 aarch64-linux-gnu-binutils   # Arch
sudo apt install qemu-system-arm binutils-aarch64-linux-gnu     # Debian, Ubuntu
```

> A separate `gdb` from Scoop is x86-only and **cannot** debug AArch64. Use
> `aarch64-none-elf-gdb`.

## Quick start

```powershell
# build every example (debug symbols retained even in release)
cargo build --release --examples

# produce the raw image the Pi firmware actually loads
cargo image --example shell -- -O binary kernel8.img

# boot the reference kernel in QEMU, UART attached to this console
.\scripts\run-qemu.ps1

# boot a specific example
.\scripts\run-qemu.ps1 -Example hello-uart

# boot with the display window and an SD card, drawing the splash on its own
.\scripts\run-qemu.ps1 -AutoDraw -SdImage .\sd.img

# boot halted with a GDB stub on :1234
.\scripts\run-qemu.ps1 -Gdb

# drive the shell from a script and capture the transcript
.\scripts\qemu-console-test.ps1 -Commands 'm','s','b' -LogPath out.log

# same, but also screenshot the framebuffer to a PPM
.\scripts\qemu-console-test.ps1 -Commands 'f','g' -Screenshot card.ppm
```

The same, on Linux:

```bash
cargo build --release --examples
cargo image --example shell -- -O binary kernel8.img

./scripts/run-qemu.sh
./scripts/run-qemu.sh hello-uart
./scripts/run-qemu.sh --auto-draw --sd-image ./sd.img
./scripts/run-qemu.sh --gdb

./scripts/qemu-console-test.sh --commands "m s b" --log-path out.log
./scripts/qemu-console-test.sh --commands "f g" --screenshot card.ppm

# build and chain-load onto a real board in one step
cargo chain --example shell
cargo chain --example multicore -- --port /dev/ttyUSB0
```

`cargo chain` is an alias that swaps the target's QEMU runner for
`scripts/chainboot-runner.sh`, so `cargo run` pushes to the board instead of
booting the emulator. It is Linux-only and must be run from the workspace root;
on Windows use `.\scripts\chainboot.ps1`. See
[`docs/23-scripts.md`](docs/23-scripts.md) §3.1.

Every script is documented option by option, for both platforms, in
[`docs/23-scripts.md`](docs/23-scripts.md).

`cargo run --release --example <name>` also works — the runner in
`.cargo/config.toml` launches QEMU with the PL011 on stdio.

## The `shell` example

| Key | Action |
| --- | --- |
| `h` | this help |
| `m` | physical memory map and kernel image layout |
| `s` | SMP status — per-core MPIDR, EL, MMU and FPU state |
| `j` | JTAG pin routing |
| `t` | uptime from the architectural timer |
| `c` | CPU, cache and system-register state |
| `b` | board identity, DRAM split, temperature and clocks |
| `k` | async task table — every core's executor, slots, poll and idle counts |
| `w` | dispatch 24 jobs to the worker tasks on cores 1-3 and collect results |
| `a` | heap statistics, free-list occupancy and an allocator self-test |
| `f` | display mode — request a framebuffer and print what the GPU granted |
| `g` | draw the test card: logo, alpha compositing, text, geometry |
| `i` | SD card identification — type, capacity, CID and CSD fields |
| `r` | hexdump block 0 of the SD card and check for an MBR signature |
| `x` | SD write self-test — write block 1, read it back, verify |
| `e` | raise `BRK #0` to exercise the synchronous fault path |
| `d` | raise a data abort to exercise the fault decoder |
| `p` | panic |

Anything else is echoed, which is how the RX path is tested.

## Debugging

```powershell
.\scripts\run-qemu.ps1 -Gdb
aarch64-none-elf-gdb .\target\aarch64-unknown-none\release\examples\shell
```
```bash
./scripts/run-qemu.sh --gdb
gdb-multiarch target/aarch64-unknown-none/release/examples/shell
```
```
(gdb) target remote :1234
(gdb) break kernel_boot_primary
(gdb) continue
(gdb) info threads        # all four cores appear as GDB threads
(gdb) bt                  # backtraces resolve through inlined frames
```

Debug symbols are on in **every** profile; `kernel8.img` is the stripped
binary image while the ELF beside it keeps full DWARF for GDB and OpenOCD.
See [`docs/08-jtag-and-debugging.md`](docs/08-jtag-and-debugging.md).

## Layout

```
cogware-bcm2837/
  Cargo.toml       the library, its features, and the example manifest
  build.rs         stages link.ld for downstream crates; asm rebuild triggers
  link.ld          image layout and the symbols the kernel reads back
  src/
    lib.rs         the crate root
    boot.rs        the entry symbols boot.s branches to, BootInfo, entry!
    smp.rs         secondary-core release and the MMU publication gate
    console.rs     lock-or-bypass console, chosen on MMU state
    heap.rs        the allocator behind `alloc`: size classes over free DRAM
    math.rs        sqrt, the frint rounding family and fmadd, as instructions
    panic.rs       the panic handler
    print.rs       print!, println!, cprintln!
    time.rs        the architectural counter
    arch/
      boot.s       reset vector, EL descent, stack setup, .bss zeroing
      exception.s  vector table and the NEON-saving trap frame
      exception.rs fault decode and dump
      mmu.rs       translation tables, memory types, the region map
      cache.s      cache and TLB maintenance
      cpu.rs       system-register access and barriers
    bsp/memory.rs  every hardware address in the project
    drivers/
      mmio.rs      volatile register access
      gpio.rs      pin function select, lock-free level access
      pl011.rs     UART
      spi.rs       SPI0: all four modes, both chip selects
      i2c.rs       BSC0/BSC1: timing, repeated START, bus scan
      mailbox.rs   VideoCore property mailbox
      framebuffer.rs  display: mode set, double buffering, 2D drawing, text
      font.rs      8x8 bitmap glyphs for printable ASCII
      qoi.rs       QOI image decoder, written from the specification
      emmc.rs      SD card: bring-up, identification, block read and write
    hal/
      gpio.rs      Pin<N, MODE> and AnyPin: typestate pins
      digital.rs   infallible pin traits the rest of the HAL is built on
      delay.rs     counter-backed blocking delays
      bitbang.rs   software SPI and I2C on any pins
      shared.rs    SharedI2c, I2cDevice, SpiDevice: safe cross-core sharing
      eh.rs        embedded-hal 1.0 adapter (optional)
      asynch.rs    edge-wait futures and the async bus traits
    sync/          SpinLock, Once, Barrier
    task/          async runtime: executor, waker, Mutex, Channel, timers
    debug/jtag.rs  JTAG pin routing and debug-register unlock
  assets/          data linked into the image (the CogWare logo)
  examples/        twelve bootable kernels; see the table above
scripts/           QEMU launchers, console harness and chain-boot tools,
                   each with a PowerShell and a Bash version
docs/              design notes
instructions.md    the rules the code is written to
```

## Documentation

| Document | Subject |
| --- | --- |
| [`docs/00-overview.md`](docs/00-overview.md) | what the BSP is and how it fits together |
| [`docs/01-toolchain-and-build.md`](docs/01-toolchain-and-build.md) | target, flags, linker script, artefacts |
| [`docs/02-boot-and-exception-levels.md`](docs/02-boot-and-exception-levels.md) | reset to entry point, EL3/EL2 descent |
| [`docs/03-memory-and-mmu.md`](docs/03-memory-and-mmu.md) | translation tables, memory types, the map |
| [`docs/04-smp.md`](docs/04-smp.md) | four cores, the boot race, shared hardware |
| [`docs/05-drivers-and-mmio.md`](docs/05-drivers-and-mmio.md) | volatile MMIO, GPIO, PL011, the lock order |
| [`docs/06-synchronisation.md`](docs/06-synchronisation.md) | SpinLock, Once, Barrier and their ordering |
| [`docs/07-exceptions-and-irq.md`](docs/07-exceptions-and-irq.md) | vector table, trap frame, fault decode |
| [`docs/08-jtag-and-debugging.md`](docs/08-jtag-and-debugging.md) | GDB, OpenOCD, JTAG on hardware |
| [`docs/09-qemu.md`](docs/09-qemu.md) | running under emulation and where it lies |
| [`docs/10-roadmap.md`](docs/10-roadmap.md) | interrupts, and what comes next |
| [`docs/11-videocore-mailbox.md`](docs/11-videocore-mailbox.md) | GPU hooks and CPU clocking |
| [`docs/12-async.md`](docs/12-async.md) | the executor, wakers, async locks and channels |
| [`docs/13-heap.md`](docs/13-heap.md) | the allocator: size classes, the bump pointer, why init is free |
| [`docs/14-emmc.md`](docs/14-emmc.md) | the SD card driver: bring-up, the CSD, block I/O |
| [`docs/15-framebuffer.md`](docs/15-framebuffer.md) | framebuffer, 2D drawing, the font and the QOI decoder |
| [`docs/16-using-the-bsp.md`](docs/16-using-the-bsp.md) | consuming the crate from another project |
| [`docs/17-floating-point.md`](docs/17-floating-point.md) | the `math` module: what `core` lacks and why there are no transcendentals |
| [`docs/18-gpio.md`](docs/18-gpio.md) | typed pins, why the hot paths take no lock, edge detection |
| [`docs/19-spi-and-i2c.md`](docs/19-spi-and-i2c.md) | hardware and bit-banged buses, the errata, sharing across cores |
| [`docs/20-embedded-hal.md`](docs/20-embedded-hal.md) | the trait dependencies, what it implements, and the async side |
| [`docs/21-host-setup.md`](docs/21-host-setup.md) | installing the toolchain on Linux and Windows, SD cards, cables, probes |
| [`docs/22-chainloading.md`](docs/22-chainloading.md) | pushing kernels over serial instead of reflashing the card |
| [`docs/23-scripts.md`](docs/23-scripts.md) | every host script, Windows and Linux, option by option |

[`instructions.md`](instructions.md) holds the coding rules. Read at least
rule 5 before touching anything that runs before the MMU is enabled.

## Status

**Runs on real silicon.** Validated on a Raspberry Pi Zero 2 W: four-core boot
out of the firmware spin table, EL2 to EL1 descent, MMU and caches, the
hard-float FPU on every core, the exception self-tests, JTAG pin routing, the
async executor, the heap, the interactive shell over UART, and chain-loading a
kernel over serial with all four cores surviving the handover.

HDMI output is confirmed on hardware too: the VideoCore mode set, the 1280x720
framebuffer and the QOI logo composited over it were captured off the Pi's HDMI
through a capture card.

SD card identification and block reads are confirmed on a real 16 GB SDHC card:
CSD v2 capacity decode, a 403 ms enumeration that emulation completes in 10 ms,
and a file read out of the FAT32 boot partition.

Also verified under QEMU `raspi3b`: SD block writes, source-level debugging over
the GDB stub, and the crate consumed as a dependency from an unrelated
workspace.

Still unverified on silicon: physical JTAG with a probe attached, EMMC writes —
the boot card is not expendable — and `CPUECTLR_EL1.SMPEN`, which sits on the
EL3-only path that Pi firmware never takes. Each is marked as such in the source
and in the docs.

## Licence

MIT OR Apache-2.0.
