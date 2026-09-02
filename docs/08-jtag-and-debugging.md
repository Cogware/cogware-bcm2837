# JTAG and debugging
This note has two halves. GDB against QEMU is verified working. Real JTAG on
Raspberry Pi 3 hardware is designed in the kernel but untested on silicon.
Keep that boundary clear when reporting results.
This page complements [01-toolchain-and-build.md](01-toolchain-and-build.md),
[02-boot-and-exception-levels.md](02-boot-and-exception-levels.md),
[05-drivers-and-mmio.md](05-drivers-and-mmio.md),
[06-synchronisation.md](06-synchronisation.md),
[07-exceptions-and-irq.md](07-exceptions-and-irq.md), and
[09-qemu.md](09-qemu.md).

## Debug artefacts
Debug the ELF:
```text
target\aarch64-unknown-none\release\examples\shell
```
Do not debug the raw image:
```text
kernel8.img
```
`kernel8.img` has no ELF headers, symbols or DWARF. The release ELF
deliberately keeps all of those while the firmware image is stripped to raw
bytes.
Confirmed ELF facts:
```text
format: AArch64 EXEC
entry:  0x80000
DWARF:  .debug_info .debug_line .debug_loc .debug_ranges
        .debug_abbrev .debug_str .debug_aranges
```
The two-artefact model is covered in
[01-toolchain-and-build.md](01-toolchain-and-build.md): boot or flash the raw
image when firmware requires it, but point GDB and OpenOCD at the ELF.

## Use the cross GDB
The debugger must be:
```text
C:\Users\ninja\scoop\apps\gcc-aarch64-none-elf\current\bin\aarch64-none-elf-gdb.exe
```
Scoop's separate `gdb` package is x86-only. It cannot debug this AArch64
target.
The same ARM GNU Toolchain provides the matching `objcopy`, `objdump` and
`readelf`, which avoids host/target confusion when inspecting the ELF.

## QEMU GDB stub
This section is verified working.
Start QEMU halted at reset:
```powershell
.\scripts\run-qemu.ps1 -Gdb
```
```bash
./scripts/run-qemu.sh --gdb
```
The script adds:
```text
-s -S
```
`-s` opens QEMU's GDB stub on TCP port 1234. `-S` keeps the CPUs stopped before
the first guest instruction.
The equivalent Cargo form is:
```powershell
cargo run --release --example shell -- -s -S
```
Attach from another PowerShell window:
```powershell
aarch64-none-elf-gdb target\aarch64-unknown-none\release\examples\shell
(gdb) target remote :1234
```
or:
```powershell
aarch64-none-elf-gdb target\aarch64-unknown-none\release\examples\shell -ex "target remote :1234"
```
QEMU supplies the remote CPU state. The ELF supplies symbols, source lines and
unwind data.

## Verified QEMU behaviour
The QEMU stub has verified:
```text
symbol resolution
source-level breakpoints
full backtraces through inlined frames
all four CPUs visible as GDB threads
```
`info threads` shows all four emulated cores, typically `Thread 1.1` through
`Thread 1.4`.
Backtraces through release code work because the build keeps DWARF, frame
pointers and unwind tables. A representative path is:
```text
read_volatile -> ReadWrite::read -> ReadWrite::modify
-> Gpio::set_function -> Gpio::set_functions -> Gpio::route_pl011
-> console::init -> kernel_boot_primary -> _start
```

## Worked QEMU session
Start the emulator:
```powershell
.\scripts\run-qemu.ps1 -Gdb
```
```bash
./scripts/run-qemu.sh --gdb
```
Attach:
```powershell
aarch64-none-elf-gdb target\aarch64-unknown-none\release\examples\shell
```
Then run:
```text
(gdb) target remote :1234
(gdb) p/x $cpsr
(gdb) break kernel_boot_primary
(gdb) continue
(gdb) bt
(gdb) info registers
(gdb) info threads
(gdb) thread 3
```
At reset, `cpsr` has been observed as:
```text
0x400003cd
```
That proves QEMU enters at EL3, so the `.L_el3` descent path described in
[02-boot-and-exception-levels.md](02-boot-and-exception-levels.md) is exercised.

## Harmless noise
On Windows, GDB may print:
```text
warning: could not convert 'main' from the host encoding (CP1252) to UTF-32
```
That has been harmless host-side debug-info noise.
A backtrace at reset may end with:
```text
Backtrace stopped: previous frame identical to this frame (corrupt stack?)
```
At `_start`, this just means there is no caller below the reset symbol.

## Inspecting boot state
Inspect `MMU_GATE` when debugging SMP bring-up:
```text
(gdb) info variables MMU_GATE
(gdb) p/x smp::MMU_GATE
```
Rust symbol spelling can be awkward, so tab-completion is useful. The open
value is:
```text
0x57474154454f4b21
```
which is the ASCII marker `WGATEOK!`.
Inspect the translation tables with the `TABLES` symbol from
`cogware-bcm2837\src\arch\mmu.rs`:
```text
(gdb) info variables TABLES
(gdb) p/x &TABLES
```
After `mmu::init()`, descriptors should no longer be all zero. Before
`MMU_GATE` opens, secondaries must not enable translation from them.
Inspect stacks through linker symbols:
```text
(gdb) p/x &__stacks_start
(gdb) p/x &__stacks_end
(gdb) info registers sp
```
Each core owns 128 KiB. Core 0 is at the top of the range; core 3 is at the
bottom.

## Static inspection
The `disasm` and `syms` aliases stop at the Cargo options, so name the example
and then pass the tool arguments after `--`. Annotated disassembly:
```powershell
cargo disasm --example shell -- --disassemble --demangle --section-headers
```
Symbols sorted by address:
```powershell
cargo syms --example shell -- --numeric-sort
```
Use the ARM readelf for headers and DWARF sections:
```powershell
aarch64-none-elf-readelf -h target\aarch64-unknown-none\release\examples\shell
aarch64-none-elf-readelf -S target\aarch64-unknown-none\release\examples\shell
```
These commands inspect the ELF and do not require QEMU or hardware.

## QEMU is not hardware JTAG
QEMU can verify CPU debugging through its GDB stub, source breakpoints,
exception-level descent, backtraces and four visible CPU threads.
QEMU cannot verify GPIO electrical routing, VideoCore `DBGEN`, OpenOCD adapter
wiring, CoreSight debug-base correctness on a physical board, or halt requests
through an external probe.
That distinction matters: the first half of this page is proven; the second
half is a hardware plan.

## Kernel JTAG code
The hardware-facing code is in:
```text
cogware-bcm2837\src\debug\jtag.rs
```
It routes GPIO22-27 to ALT4 and unlocks architectural debug state.
Core 0 routes the pins through the GPIO driver:
```rust
GPIO.lock().set_functions(&JTAG_PINS, Function::Alt4);
```
Every core unlocks its own debug registers:
```text
OSLAR_EL1 = 0
OSDLR_EL1 = 0
MDSCR_EL1.MDE = 1
```
`OSLAR_EL1` clears the OS Lock, which otherwise blocks external access to debug
registers. `OSDLR_EL1` clears the OS Double Lock. `MDSCR_EL1.MDE` enables
monitor debug events. `KDE` is deliberately left clear so events are not routed
back to the current EL instead of the external debugger.
The debug registers are per-core, so secondaries run the unlock path for
themselves after their MMU and vector state are live.

## JTAG pinout
The six-wire ARM JTAG port uses ALT4:
```text
GPIO  Signal  40-pin header
22    TRST    15
23    RTCK    16
24    TDO     18
25    TCK     22
26    TDI     37
27    TMS     13
```
These pins do not clash with GPIO14 and GPIO15, so the PL011 UART console can
remain active while a probe is attached.
Use a common ground and an adapter suitable for the Pi's signal voltage. The
exact adapter and wiring remain unverified for this project.

## Hardware `config.txt`
Real hardware should include:
```text
enable_jtag_gpio=1
arm_64bit=1
init_uart_clock=48000000
```
`enable_jtag_gpio=1` is critical because it asserts `DBGEN` inside the SoC. The
kernel can route pins and unlock debug registers, but it cannot assert that
VideoCore-controlled debug-enable signal.
`arm_64bit=1` documents the intended AArch64 path beside `kernel8.img`.
`init_uart_clock=48000000` keeps firmware PL011 clocking aligned with the
kernel's `PL011_CLOCK_HZ`. Serial is the fallback when JTAG is not yet working.

## Chicken and egg
The current kernel routes JTAG pins itself. That means a probe cannot reliably
halt the board before the kernel has run far enough to execute `debug::jtag`.
The reference project solves this with a tiny `X1_JTAG_boot` stub that runs
first, configures the pins, and then hands off to the real kernel. The same
approach is available here if early halt-at-reset debugging becomes necessary.
For now, the in-kernel setup keeps the boot flow simple and self-contained.

## OpenOCD configuration shape
The reference model is:
```text
.reference\cogware-copilot\docker\rustembedded-osdev-utils\rpi3.cfg
```
It should be treated as a model, not copied blindly. Its shape is:
```text
transport select jtag
reset_config trst_and_srst
adapter_khz 1000
jtag_ntrst_delay 500
jtag newtap rpi3 tap -irlen 4 ... -expected-id 0x4ba00477
dap create rpi3.dap -chain-position rpi3.tap
```
Then it creates four AArch64 targets, one per core, with CTI objects and
per-core debug bases. The reference debug bases are:
```text
0x80010000 0x80012000 0x80014000 0x80016000
```
and CTI bases are:
```text
0x80018000 0x80019000 0x8001a000 0x8001b000
```
A complete real setup also needs SMP grouping so GDB can reason about the four
cores together.
After OpenOCD is running, GDB connects to OpenOCD's default GDB port:
```powershell
aarch64-none-elf-gdb target\aarch64-unknown-none\release\examples\shell
(gdb) target remote :3333
```
This hardware path is still untested on silicon.

## Hardware status
The design in place is:
```text
GPIO22-27 routed to ALT4
OS lock released
OS double-lock released
MDSCR_EL1.MDE set
firmware expected to assert DBGEN
OpenOCD expected to describe four Cortex-A53 targets
```
What has not yet been proven is physical halt, single-step, hardware
breakpoints, reset behaviour, or all-core SMP debug through a real adapter.
When hardware testing happens, record the adapter, wiring, OpenOCD version,
`config.txt`, and a transcript here. Those notes are more valuable than a vague
"JTAG works" claim.

## Shell JTAG report
The shell command:
```text
j
```
prints JTAG state. Verified output shows the six pins configured as ALT4:
```text
JTAG
  GPIO22 TRST FSEL=0b011  ALT4 ok
  GPIO23 RTCK FSEL=0b011  ALT4 ok
  GPIO24 TDO  FSEL=0b011  ALT4 ok
  GPIO25 TCK  FSEL=0b011  ALT4 ok
  GPIO26 TDI  FSEL=0b011  ALT4 ok
  GPIO27 TMS  FSEL=0b011  ALT4 ok
```
`0b011` is the BCM2837 function-select encoding for ALT4.
Under QEMU, this verifies the kernel's MMIO route and readback path only. It
says nothing about electrical JTAG transport.

## Debugging SMP boot
Under QEMU, all four CPUs appear immediately:
```text
(gdb) info threads
```
That is expected and is the reason `MMU_GATE` exists. If a secondary faults
early, check whether it passed the gate before core 0 populated `TABLES`.
A correct secondary can be found spinning in `wait_for_mmu_gate()` until the
gate contains `WGATEOK!`. Do not replace this with a `SpinLock`; exclusive
atomics are not legal on Device memory before the MMU is enabled. See
[06-synchronisation.md](06-synchronisation.md).

## Debugging exceptions
The shell exposes intentional fault paths:
```text
e  raise BRK #0
d  raise a data abort
p  panic
```
`e` proves the synchronous exception vector can dispatch and return. `d` reads
from `0xC000_0000`, above the populated L1 map, and should produce a decoded
data abort. `p` exercises the panic path, which must not assume the console
lock is available.
These paths are verified in QEMU. Real JTAG should make the same paths easier
to inspect on hardware, but that remains to be proven.

## Boot tracing without a debugger

JTAG cannot help with the earliest part of `_start`. The first Rust call is
`console::init()`, so anything that goes wrong before it is completely silent:
no console, no panic, no vector table. The `boot-trace` feature exists for
exactly that window.

```bash
cargo build --release --example hello-uart --features boot-trace
```

It is deliberately **not** in `default`. Enabling it makes `_start` emit a
single ASCII byte at each milestone, straight to the PL011.

The trace does not rely on any other kernel code. `TRACE_INIT` programs
GPIO 14/15 to ALT0 and configures the PL011 from scratch -- the firmware does
*not* leave UART0 usable, so the trace cannot assume otherwise. The FIFO wait
is bounded at ~2M polls so a misconfigured UART cannot masquerade as a hang.

| Marker | Emitted at | Core |
| --- | --- | --- |
| `1` | reset entry, interrupts masked | all |
| `A` | past the LED blink point | all |
| `B` | stack pointer installed | all |
| `1`/`2`/`3` | `CurrentEL`, as a digit | all |
| `C` | took the EL3 descent path | all |
| `2` | took the EL2 descent path | all |
| `3` | running at EL1 | all |
| `4` | `CPACR_EL1.FPEN` set, no FP executed yet | all |
| `5` | first FP register access survived | all |
| `6` | past the `CPUECTLR_EL1` decision | all |
| `7` | caches invalidated | core 0 |
| `8` | branching into Rust | core 0 |
| `9` | `console::init()` returned | core 0 |
| `a` | `mailbox::probe_memory()` returned | core 0 |
| `b` | `mmu::init()` returned | core 0 |
| `c` | `arch::init_core()` returned | core 0 |
| `d` | `heap::init()` returned | core 0 |
| `e` | about to enter `main_primary` | core 0 |
| `P` | parked: `CurrentEL` was EL0 | all |

Markers `7` and `8` are core 0 only; the role split happens immediately after
`6`, and secondaries branch to `kernel_boot_secondary` without zeroing `.bss`
or walking the shared L2.

A healthy boot on a Pi Zero 2 W, which enters at EL2:

```text
1AB223456789abcde
Hello from the BCM2837.
```

Under `-M raspi3b` QEMU enters at **EL3** and starts all four cores at reset,
so the markers interleave and a `C` appears. Both are expected, and the
difference is itself useful: it is why an EL1-only fault can pass in QEMU and
hang on silicon.

Read the last marker to localise a hang. Stopping after `5` means the fault is
in the next instruction sequence, not somewhere diffuse -- that is how the
`CPUECTLR_EL1` trap described in
[`02-boot-and-exception-levels.md`](02-boot-and-exception-levels.md) was found.

`TRACE_REG` emits a computed character rather than a literal, and `LED_BLINK`
drives the ACT LED on GPIO 29 for a liveness signal when no serial adapter is
attached. Both are kept as documented bring-up aids. `LED_BLINK` is not
invoked by default: its delay loops are slow enough to be mistaken for a hang.
