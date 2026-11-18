# Roadmap

This page records what is done, what remains, and which pieces still need real
Raspberry Pi 3 hardware. It should be read with
[00-overview.md](00-overview.md), [01-toolchain-and-build.md](01-toolchain-and-build.md),
[02-boot-and-exception-levels.md](02-boot-and-exception-levels.md),
[03-memory-and-mmu.md](03-memory-and-mmu.md), [04-smp.md](04-smp.md),
[05-drivers-and-mmio.md](05-drivers-and-mmio.md),
[06-synchronisation.md](06-synchronisation.md),
[07-exceptions-and-irq.md](07-exceptions-and-irq.md),
[08-jtag-and-debugging.md](08-jtag-and-debugging.md),
[09-qemu.md](09-qemu.md),
[11-videocore-mailbox.md](11-videocore-mailbox.md), and
[12-async.md](12-async.md).

The project remains deliberately small: stable Rust, no external crates beyond
the two optional trait crates, and a bare-metal BCM2837 kernel
whose behaviour is inspectable from reset to shell.
The one heap it has is its own, and it is 300 lines. The public summary lives
in `README.md`.

## Done and verified in QEMU

The build baseline is stable Rust 1.98, edition 2024, hard-float
`aarch64-unknown-none`, with zero warnings. Debug information remains in every
profile, frame pointers and unwind tables are forced, and the `image` alias
produces `kernel8.img` for the real firmware path while QEMU boots the ELF.

The boot path is in place: `_start`, EL3-or-EL2 descent to EL1, per-core stacks,
`.bss` zeroing, FPU/NEON enablement, and a full 16-slot exception vector table
with an 832-byte NEON-saving trap frame and ESR decoding. `BRK #0`, a deliberate
data abort, and the panic path are all observable from the shell.

The MMU has an identity-mapped, W^X stage-1 map with three memory kinds:

```text
Device
Normal
NormalNonCacheable
```

`MemoryKind::NormalNonCacheable` exists for memory shared with the VideoCore.
The kernel now probes the DRAM split at runtime before building the tables, maps
ARM DRAM cacheable, and maps `VideoCore DRAM (GPU)` non-cacheable. The memory
report is table-based and includes the GPU region.

SMP is working in QEMU. All four cores come online, share GPIO behind a lock,
and rendezvous through `sync::Barrier`. `BOOT_BARRIER` makes the boot-time tally
deterministic: `visits: 4`, one visit per core. The QEMU all-cores-at-entry
quirk is handled by `MMU_GATE`, while the real firmware spin-table path remains
implemented.

The driver set now includes GPIO, PL011, and the VideoCore property mailbox. The
mailbox driver can read board revision, serial, ARM and VideoCore memory, SoC
temperature, current and maximum clocks, and can set power state or clock rate.
It is protected by a `SpinLock` because the property buffer is shared.

The console has `console::with_writer`, which holds the UART once across a
multi-line report or bypasses safely in panic/fault contexts. This matters
because table output and dumps must not be interleaved by another core halfway
through a row. It also has `console::next_byte()`, the async counterpart of
`read_byte()`, which is what the shell task awaits.

The async runtime is in place and is now what every core actually runs. Core 0
spawns `shell` and `heartbeat`; cores 1-3 each spawn a `worker` that drains a
shared `task::Channel<Job, 16>` and accumulates results under a
`task::Mutex<Results>`. The `k` shell command prints the whole task table and
`w` dispatches 24 jobs across the three worker cores; a verified run
distributes them 8/8/8. See [12-async.md](12-async.md).

The two supported QEMU scripts exist:

```text
scripts\run-qemu.ps1            scripts/run-qemu.sh
scripts\qemu-console-test.ps1   scripts/qemu-console-test.sh
```

The first is for live interactive runs, including `-Gdb`, `-Trace`, `-Profile`
and `-NoBuild`. The second drives the shell non-interactively and captures a
transcript, using a Windows named pipe because QEMU stdio cannot accept piped
stdin. See [09-qemu.md](09-qemu.md).

Chain-loading is done, which removes the SD-card write from the hardware
iteration loop. `examples/chainloader.rs` is flashed to the card once; every
kernel after that arrives over the serial line already connected for the
console, in about five seconds. The interesting part is that all four cores
survive the handover: the loader's park loop is a faithful copy of the firmware
spin table rather than a dead-end `wfe`, so the payload's own
`start_secondaries()` reclaims cores 1-3. A plain park would have taken them out
of the spin table permanently and left every subsequently pushed kernel
single-core. Verified in QEMU with matching host/target CRC-32 and `multicore`
reporting all four cores online. `minipush.ps1` / `minipush.py` and
`chainboot-qemu.ps1` / `chainboot-qemu.sh` are the host side, wrapped by
`chainboot.ps1` / `chainboot.sh`. See
[22-chainloading.md](22-chainloading.md).

Repository support files are also in place: `firmware\config.txt` for the Pi
firmware path, `openocd\rpi3.cfg` for hardware debugging, the complete numbered
`docs\` set and `README.md`.

## Phases 1-3: interrupts, the tick and IPIs — done

The SoC interrupt controller, the per-core virtual-timer tick and mailbox IPIs
are implemented and verified under QEMU on all four cores; see
[24-interrupts.md](24-interrupts.md). The tick needed no executor change:
an interrupt wakes the core from `wfe` and the idle loop re-polls hardware
waiters after every wake-up.

Still open from the original plan: interrupt-driven PL011 receive. The console
is polled at the tick rate, which is enough for a shell; the hooks and the rule
it must follow are in [07-exceptions-and-irq.md](07-exceptions-and-irq.md).

## Phase 3b: clocks, DMA, PWM, timer, RNG — done

Not originally on the roadmap. The clock manager, the DMA controller (with SPI0
and EMMC data paths), PWM, the system timer and the hardware RNG are
implemented; see [25-dma.md](25-dma.md) and
[26-clocks-pwm-timer-rng.md](26-clocks-pwm-timer-rng.md). Everything QEMU
emulates is verified there; PWM output and DMA pacing by a peripheral's data
request need a board.

## Phase 3e: an emulator that models them — done

A QEMU fork models DMA pacing, SPI DMA and chip select, I2C repeated START,
GPIO pulls and edges, PWM, both Unicam receivers with an IMX219, the HVS,
pixel valves, both DSI hosts and two panels; `scripts/qemu-scenario.py`
drives it. DSI command and video mode, the camera path and GPIO interrupts
are verified there end to end. See [30-cogware-qemu.md](30-cogware-qemu.md).
What only a board can show — the D-PHY, PLL lock, analogue settings — still
needs one.

## Phase 3d: DSI video mode and device interfaces — written, untested

DSI1 video mode through the HVS and pixel valve 1, the `Canvas` drawing
surface split out of the framebuffer, and the generic device layer (CCI,
sensor and panel traits, Bayer preview) are in; see
[28-dsi-video.md](28-dsi-video.md) and [29-devices.md](29-devices.md). The
Bayer path is verified under QEMU; the rest needs a board. Device tables
belong in a separate crate.

## Phase 3c: camera and display interfaces — written, untested

Both Unicam CSI-2 receivers and both DSI hosts (command mode, low-power) are
implemented following the Linux drivers; see
[27-camera-and-display.md](27-camera-and-display.md). QEMU has neither block.
Next: run both on a Compute Module 3, then DSI high-speed mode (PLLD's DSI
channel), then decide whether video mode through the HVS is worth its size.

## Phase 4: CPU upclocking on hardware

The mailbox mechanism now exists, so CPU upclocking is a concrete near-term
hardware item rather than speculation. The sequence is:

```rust
let rate = MAILBOX.lock().max_clock_rate(Clock::Arm)?;
MAILBOX.lock().set_clock_rate(Clock::Arm, rate, false)?;
```

This must be gated on suitable `arm_freq` and `over_voltage` settings in
`firmware\config.txt`; firmware may clamp the result, and the returned rate is
authoritative. Monitor thermal headroom with `Tag::GetTemperature`.

Do not do this automatically at boot. A board with the wrong voltage or thermal
conditions should still boot at firmware defaults. QEMU cannot validate this
path: its clock rates are not real Pi 3B rates, and changing them there says
little about silicon stability.

## Phase 5: framebuffer — done

This phase is complete and has moved out of the roadmap. The multi-tag mailbox
call, the mode set, double buffering, the clipped 2D primitives, the 8x8 font
and a from-scratch QOI decoder are all implemented and verified at 1280x720
under QEMU. The design notes are in
[15-framebuffer.md](15-framebuffer.md); the protocol notes remain in
[11-videocore-mailbox.md](11-videocore-mailbox.md).

Two predictions here are now confirmed:

* multi-tag property calls were the real prerequisite — a mode set has to be
  one message, because a partially applied mode is very hard to unwind;
* `BUFFER_WORDS = 64` was sized correctly for the seven-tag mode-set message.

What remains unimplemented from the original GPU ambition is **V3D**: QEMU has
no 3D core, so shader compute has to be developed against real silicon.

## Phase 5b: SD card — done

Not originally on the roadmap. The Arasan EMMC controller is now driven far
enough to identify a card, negotiate a 4-bit bus, and do verified 512-byte
block reads and writes over programmed I/O. See [14-emmc.md](14-emmc.md).

There is still no filesystem. Reading a FAT32 partition off block 0 onwards is
the obvious next step and is not started.

## Phase 6: no-alloc async executor — done, and now the main loop

This was the stated stretch goal. It is implemented, and every core's main
loop can run on it. In the shell example, core 0 runs the shell and a heartbeat
as tasks, while cores 1-3 run a worker task that drains a shared `Channel`.
The shell example's secondary entry point ends in `task::run()` rather than
`cpu::halt()`. The full design is in [12-async.md](12-async.md).

What landed:

```text
cogware-bcm2837/src/task/mod.rs        TaskId, TaskSet, current(), the constants
cogware-bcm2837/src/task/executor.rs   slots, vtable glue, Executor, spawn, run
cogware-bcm2837/src/task/waker.rs      RawWaker whose data pointer is the task index
cogware-bcm2837/src/task/yield_now.rs  poll_again(), yield_now(), next_tick()
cogware-bcm2837/src/task/timer.rs      Sleep, sleep_millis, timeout
cogware-bcm2837/src/task/mutex.rs      async Mutex with a TaskSet wait queue
cogware-bcm2837/src/task/channel.rs    bounded MPMC Channel<T, N>
```

Two decisions here shape what comes next:

* **The task table is fixed at 4 cores x 8 slots = 32.** That is why a set of
  tasks is one `u32`, which is why the wait queues need no allocator.
* **Wake-ups are not yet interrupt-driven.** A future waiting on hardware calls
  `poll_again()`, and the core sleeps in `wfe` until the architectural timer's
  event stream (`CNTKCTL_EL1.EVNTEN`, one event per 65 536 ticks) releases it.
  This works with no interrupt controller at all, which is exactly why it was
  chosen first.

What phases 1-3 change once they land, in order of value:

```text
1. UART RX IRQ  -> console::NextByte wakes on a real Waker instead of a tick.
2. Timer IRQ    -> task/timer.rs programs CNTP_CVAL_EL0; sleeps stop overrunning.
3. Local mailbox IPI -> spawn_on() can wake a sleeping core immediately,
                        and `sev` stops being the only cross-core signal.
```

Note that only `timer.rs` and `console.rs` need to change for the first two:
the executor's `ready`/`polling` split was designed so that an interrupt simply
sets a `ready` bit through the existing `Waker`, and the polling path becomes
a fallback rather than the primary mechanism.

Still missing, deliberately:

```text
preemption            a task that loops forever wedges its core
work stealing         a task is bound to the core that spawned it
cancellation          only by dropping a future, e.g. via timeout()
join handles          spawn returns a TaskId, not an awaitable result
```

## Hardware bring-up phase

The kernel runs on real silicon — a Raspberry Pi Zero 2 W. These paths, which
emulation cannot exercise honestly, have been proven there:

```text
firmware spin table at 0xd8            four cores released and online
dc isw set/way data-cache maintenance  caches enabled and stable
19.2 MHz generic timer frequency       read from CNTFRQ_EL0, timings correct
48 MHz PL011 clock and baud divisors   console legible at 115200 8N1
HDMI mode set and framebuffer          captured off the board via capture card
EMMC identification and block reads    16 GB SDHC, CSD v2, 403 ms enumeration
```

These remain unverified on a Raspberry Pi 3:

```text
CPUECTLR_EL1.SMPEN via msr S3_1_C15_C2_1   sits on the EL3-only path, and Pi
                                           firmware enters at EL2, so the
                                           instruction is never reached
physical JTAG via openocd\rpi3.cfg         pins are routed; no probe attached
board_serial returning a real value
EMMC block writes                          reads are proven on a real card;
                                           writing to the boot card has
                                           deliberately not been attempted
```

There is a chicken-and-egg problem for JTAG. The kernel routes its own JTAG pins
as part of boot, so a board that fails before that point may be impossible to
attach to unless firmware routes the pins first. That is why
`enable_jtag_gpio=1` in `firmware\config.txt` matters: it lets a probe attach
to a board that failed before the kernel could initialise GPIO 22-27.

For hardware bring-up, prove one thing at a time: first UART output with the
firmware configuration, then GDB/OpenOCD attach, then the real spin-table
release, then mailbox identity and clocks, then the framebuffer and the SD
card, and only after that upclocking or V3D work.

Flash the chain-loader to the card as soon as UART output is proven, before
anything else. From that point the card never has to come out again, which
turns a minute-long edit-flash-boot cycle into about five seconds and makes
every step above tolerable to iterate on. It also exercises the spin table
early, since the loader's park loop and the payload's `start_secondaries()`
between them cover the real firmware release path.
[21-host-setup.md](21-host-setup.md) covers the card, the cable and the probe;
[22-chainloading.md](22-chainloading.md) covers the loader.

## Still out of scope

The project is still not aiming at a general-purpose OS. Higher-level kernel
features, userspace, filesystems and networking remain out of scope. A heap has
landed — see [13-heap.md](13-heap.md) — but deliberately a minimal one: no
coalescing, no slab layer, no per-core magazines, and no plan to grow it into a
general-purpose allocator.

The framebuffer and the SD card have both landed, and neither implies a stack
above it. There is a display driver, not a graphics stack: no compositor, no
window system, no V3D. There is a block driver, not a filesystem: no FAT, no
partition table parsing beyond checking for the `0x55AA` signature, no VFS.

## Definition of done

A milestone is not done merely because it compiles. For QEMU-verifiable work,
require a clean release build, a boot to the shell, all four cores online, and a
visible proof through boot output, shell output, GDB, or `int.log`. For hardware
work, record the exact board evidence and explicitly say what QEMU cannot prove.

Keep future documentation concrete: real paths, real register addresses, real
script parameters, and the reason each awkward sequence exists.
