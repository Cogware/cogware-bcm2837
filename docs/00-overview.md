# Overview

cogware-bcm2837 is a bare-metal AArch64 BSP for the Raspberry Pi 3 (BCM2837,
four Cortex-A53 cores), written from scratch for current stable Rust. This
document is the map: what the BSP provides, what it deliberately is not, how
the pieces fit together, and where to read next.

## Design goals

The project rebuilds an earlier Raspberry Pi 3 kernel to fix accumulated
problems. The goals, in the order they constrained the design:

1. **Current stable Rust.** No nightly, no `#![feature(...)]`, no `-Z` flags.
   The earlier project was pinned to an old nightly and could not move.
2. **Hard float.** The target is `aarch64-unknown-none`, not
   `aarch64-unknown-none-softfloat`. The Cortex-A53 has a perfectly good FPU
   and NEON unit and the kernel uses them. See
   [The hard-float consequence](#the-hard-float-consequence) below.
3. **MMU and MMIO.** Translation on, with real permissions, not a flat
   identity map with everything writable and executable.
4. **Four-core SMP, with hardware safely shared between cores.** GPIO, the
   UART and the mailbox are all reachable from any core without corruption.
5. **JTAG and full debug symbols**, retained in release, so the kernel can be
   debugged on real hardware as it grows.
6. **A heap that costs nothing to start.** An O(1) allocator over DRAM the MMU
   already maps, so `alloc` works without adding a millisecond to boot. See
   [`13-heap.md`](13-heap.md).
7. **A display and a block device.** The VideoCore framebuffer with alpha
   compositing and text, described in
   [`15-framebuffer.md`](15-framebuffer.md), and SD card block I/O over the
   Arasan EMMC controller, described in [`14-emmc.md`](14-emmc.md). Both are
   off the BSP boot path: the shell example probes them after bring-up.
8. **Nothing else.** No userspace, no filesystem, no networking, no V3D
   shaders in this phase.

Async — an interrupt-driven executor — is an explicit stretch goal and is
covered in [`10-roadmap.md`](10-roadmap.md).

Two self-imposed constraints shape the code more than any feature does: **no
external crates**, and **everything hand-written** — no Docker image, no
Makefile, no code generator. Every line is inspectable.

The first constraint is relaxed only for the `embedded-*` ecosystem, and in two
tiers. `embedded-hal` and `embedded-hal-async` are **on** by default: they are
pure trait crates with no transitive dependencies and no code to audit beyond
their signatures. `embedded-sdmmc` is **off** by default: it is a real
FAT16/FAT32 implementation with transitive dependencies, opt-in because a
correct FAT reader is a large body of fiddly code that is not board-specific.
Every dependency is optional, and `default-features = false` still builds the
entire BSP with an empty dependency list. See
[`20-embedded-hal.md`](20-embedded-hal.md).

## What runs, in order

```
VideoCore firmware
  reads config.txt, loads kernel8.img to 0x80000, releases the ARM
        |
        v
boot.s          all cores: read MPIDR_EL1, set SP from the per-core stack
                core 0 only: zero .bss
                descend EL3 -> EL2 -> EL1 (or EL2 -> EL1)
        |
        +----------------------------+
        |                            |
   core 0                      cores 1-3
        |                            |
  console::init                 wait_for_mmu_gate()   <-- blocks here
  mailbox::probe_memory()            |
  mmu::init()   build tables         |
  arch::init_core()
  smp::mark_online()
  heap::init()
  jtag::init()
  call primary entry
  (shell example)
  banner                        contribute to the shared tally
  self-tests                         |
  smp::start_secondaries() ---->+  released
                                arch::init_core()
                                smp::mark_online()
                                jtag::init()
                                call secondary entry
                                (shell example)
        |                            |
  BOOT_BARRIER.wait()  <------> BOOT_BARRIER.wait()
        |                            |
  report the tally              spawn "worker"
  spawn "shell", "heartbeat"    task::run()   <-- executor, idles in wfe
  task::run()
```

`open_mmu_gate()` → `wait_for_mmu_gate()` handles QEMU releasing all four
cores at reset instead of using the firmware spin table; see
[`04-smp.md`](04-smp.md).

## Module map

| Module | Responsibility |
| --- | --- |
| `arch::boot` (`boot.s`) | reset vector, exception-level descent, stacks, `.bss` |
| `arch::cpu` | system-register access, barriers, `wfe`/`sev` |
| `arch::mmu` | translation tables, memory types, **the region map** |
| `arch::exception` (+ `.s`) | vector table, trap frame, fault decode |
| `arch::cache` (`cache.s`) | cache and TLB maintenance |
| `boot` | BSP bring-up, `BootInfo`, and the `entry!` macro |
| `bsp::memory` | **every hardware address in the project** |
| `drivers::mmio` | volatile register access — the only legal path to hardware |
| `drivers::gpio` | pin function select |
| `drivers::pl011` | UART |
| `drivers::mailbox` | VideoCore property interface |
| `sync` | `SpinLock`, `Once`, `Barrier` |
| `task` | the async executor, wakers, `Mutex`, `Channel`, timers |
| `smp` | secondary release and the MMU publication gate |
| `console` | lock-or-bypass console |
| `debug::jtag` | JTAG pin routing and debug-register unlock |
| `time` | architectural timer |
| `panic` | last-resort fault reporting |

Three modules are load-bearing beyond their size:

- **`bsp::memory`** is the single source of truth for addresses. A hex literal
  for a peripheral anywhere else in the tree is a bug. It also holds the
  VideoCore bus-address translation and the per-core ARM-local registers that
  future interrupt work will need.
- **`arch::mmu::memory_map()`** is the single place region permissions are
  defined. Nothing else decides what is executable or writable.
- **`smp`** holds the gate that makes multi-core boot correct under both QEMU
  and real hardware.

## Layering

```
  examples/shell/main.rs          self-tests, shell
           |
    +------+------+------+------+
    |             |      |      |
 console        sync   task    smp        policy, portable
    |             |      |      |
    +------+------+------+------+
           |
       drivers                             hardware behaviour
     mmio gpio pl011 mailbox
           |
        bsp::memory                        board facts: addresses only
           |
          arch                             CPU facts: mmu, exception, cache, cpu
```

The rule the layering encodes: **board facts go in `bsp`, CPU facts go in
`arch`, and drivers know neither their own address nor which board they are
on.** A driver receives a base address; it does not look one up.

## The hard-float consequence

Choosing the hard-float target is not a one-line change in `.cargo/config.toml`.
It means the compiler may emit NEON and floating-point instructions in **any**
function, including code reached from an exception, and there is no stable way
to disable that per function. Three things follow:

1. `CPACR_EL1.FPEN` must be enabled on **every core** before any Rust runs,
   including before the fault handler could ever be entered.
2. The exception trap frame must save **all 32 NEON registers plus `FPSR` and
   `FPCR`** — 832 bytes, whose field order is dictated by `stp` immediate-offset
   encoding limits.
3. The frame layout is mirrored between `exception.s` and `exception.rs` and
   cross-checked by `const` assertions, because a silent mismatch there
   corrupts registers in a way that is essentially undebuggable.

See [`01-toolchain-and-build.md`](01-toolchain-and-build.md) and
[`07-exceptions-and-irq.md`](07-exceptions-and-irq.md).

## Two rules with no exceptions

**Before the MMU is enabled, all memory is Device memory, and `LDXR`/`STXR`
are architecturally UNPREDICTABLE on Device memory.** That rules out every
read-modify-write atomic, and therefore `SpinLock`, `Once` and `Barrier`, in
the entire pre-MMU window. Plain `AtomicU64` load and store lower to
`LDAR`/`STLR`, which *are* defined on Device memory, and are the only
synchronisation available there. This is what `smp::MMU_GATE` is built from.

**Anything shared with the VideoCore must be mapped Normal non-cacheable *and*
handed over as an uncached bus address.** The GPU is not in the ARM coherency
domain. Getting either half wrong produces corruption that looks exactly like
a race condition.

Both are stated as rules in [`instructions.md`](../instructions.md); both are
explained in full in [`04-smp.md`](04-smp.md),
[`06-synchronisation.md`](06-synchronisation.md) and
[`03-memory-and-mmu.md`](03-memory-and-mmu.md).

## Where reality and emulation diverge

The kernel has been developed entirely against QEMU's `raspi3b` machine. QEMU
is faithful enough to be useful and unfaithful in ways that matter:

| | QEMU | Real BCM2837 |
| --- | --- | --- |
| Secondary core release | all four start at `0x80000` | parked in the spin table at `0xd8` |
| Entry exception level | EL3 | EL2 on current firmware |
| `CNTFRQ_EL0` | 62.5 MHz | 19.2 MHz |
| Set/way maintenance (`dc isw`) | no-op | required |
| PL011 baud divisors | ignored | honoured |
| UART reference clock | reports 3 MHz | 48 MHz via `init_uart_clock` |
| Board serial | returns 0 | real value |

The kernel reads `CNTFRQ_EL0` rather than assuming it, handles both core-release
behaviours, and handles both entry exception levels. The rest is documented in
[`09-qemu.md`](09-qemu.md) and marked in the source.

**The kernel has been validated on real hardware** — a Raspberry Pi Zero 2 W —
covering four-core boot out of the firmware spin table, the MMU and caches, the
hard-float FPU on every core, the interactive shell over UART, chain-loading,
HDMI output captured off the board through a capture card, and SD card
identification and block reads against a real 16 GB SDHC card. Physical JTAG
with a probe and EMMC *writes* remain unverified on silicon, and paths that
cannot be exercised under emulation are still labelled as untested wherever they
appear.

## Reading order

If you are new to the project:

1. [`01-toolchain-and-build.md`](01-toolchain-and-build.md) — how to build it
   and why the flags are what they are.
2. [`02-boot-and-exception-levels.md`](02-boot-and-exception-levels.md) —
   reset to the executor.
3. [`03-memory-and-mmu.md`](03-memory-and-mmu.md) — the address space.
4. [`04-smp.md`](04-smp.md) — four cores and the boot race.
5. [`05-drivers-and-mmio.md`](05-drivers-and-mmio.md) — how to touch hardware.
6. [`06-synchronisation.md`](06-synchronisation.md) — the primitives and their
   ordering arguments.
7. [`07-exceptions-and-irq.md`](07-exceptions-and-irq.md) — the vector table
   and the trap frame.
8. [`08-jtag-and-debugging.md`](08-jtag-and-debugging.md) — GDB and JTAG.
9. [`09-qemu.md`](09-qemu.md) — the emulator.
10. [`10-roadmap.md`](10-roadmap.md) — interrupts, async, and what comes next.
11. [`11-videocore-mailbox.md`](11-videocore-mailbox.md) — GPU hooks and CPU
    clocking.
12. [`12-async.md`](12-async.md) — the executor every core's main loop runs on,
    and how to write `async fn`s against it.
13. [`13-heap.md`](13-heap.md) — the allocator behind `alloc`, why its size
    classes are powers of two, and why starting it costs two stores.
14. [`14-emmc.md`](14-emmc.md) — the SD card: bring-up, the CSD bit-numbering
    trap, and PIO block transfers.
15. [`15-framebuffer.md`](15-framebuffer.md) — mode setting, double buffering,
    the 2D primitives, the font and the QOI decoder.
16. [`16-using-the-bsp.md`](16-using-the-bsp.md) — using the BSP as a
    dependency.
17. [`17-floating-point.md`](17-floating-point.md) — the `math` module: what
    `core` omits, and why there are no transcendentals.
18. [`18-gpio.md`](18-gpio.md) — typed zero-sized pins, why the hot paths take
    no lock, and edge detection.
19. [`19-spi-and-i2c.md`](19-spi-and-i2c.md) — the hardware and bit-banged
    buses, the BCM2837's errata, and sharing one bus across four cores.
20. [`20-embedded-hal.md`](20-embedded-hal.md) — the project's two optional
    trait dependencies, what they implement, and the async side.
21. [`21-host-setup.md`](21-host-setup.md) — installing the toolchain on Linux
    and Windows, for QEMU and for real hardware: SD cards, cables and probes.
22. [`22-chainloading.md`](22-chainloading.md) — pushing a kernel over the
    serial line instead of reflashing the card, without losing three cores.
23. [`23-scripts.md`](23-scripts.md) — every host script, Windows and Linux
    side by side: what each one does, every option, and the everyday workflows.

If you are about to change code, read [`instructions.md`](../instructions.md)
first — particularly rule 5, which is the one most likely to be broken by an
otherwise reasonable-looking change.
