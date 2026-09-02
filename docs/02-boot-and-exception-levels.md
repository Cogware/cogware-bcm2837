# Boot and exception levels
This note follows the BSP from reset to the application entry point, then the
shell example to `shell()`. It complements
[01-toolchain-and-build.md](01-toolchain-and-build.md),
[03-memory-and-mmu.md](03-memory-and-mmu.md), [04-smp.md](04-smp.md),
[06-synchronisation.md](06-synchronisation.md),
[07-exceptions-and-irq.md](07-exceptions-and-irq.md), and
[09-qemu.md](09-qemu.md).
The high-value lesson is that boot is per-core. Real firmware and QEMU release
secondary cores differently, so the path must be correct under both behaviours.

## Starting conditions
The VideoCore boots the Raspberry Pi 3, not the ARM cores. Firmware reads
`config.txt`, loads `kernel8.img` to physical address:
```text
0x0008_0000
```
and releases the ARM side. The `kernel8.img` filename selects the 64-bit image
path; `arm_64bit=1` is the matching explicit `config.txt` statement. The kernel
is linked at the same address and initially identity maps memory, so physical
and virtual addresses match during bring-up.
The low memory below the image is firmware territory. The secondary-core spin
table used by real hardware is:
```text
0x0000_00d8 + core_id * 8
```
The kernel maps that window but does not allocate from it.

## Exception levels
AArch64 has four exception levels:
```text
EL0  user mode
EL1  kernel mode
EL2  hypervisor mode
EL3  secure monitor / firmware mode
```
Higher numbers are more privileged. This kernel runs at EL1. It has no EL0 user
processes, does not act as an EL2 hypervisor, and does not retain EL3 secure
monitor control.
The reset path accepts EL3, EL2, or EL1 and normalises the core to EL1h: EL1
using the EL1 stack pointer.
QEMU's `raspi3b` has been verified to enter at EL3. Under GDB at reset, `cpsr`
was:
```text
0x400003cd
```
That means the `.L_el3` path is exercised in QEMU. Real hardware may enter at
EL2 depending on firmware and armstub state. Both paths must work.

## Why reset is assembly
`cogware-bcm2837\src\arch\boot.s` owns reset. Rust cannot safely express running before
a stack exists, changing exception level with `eret`, or clearing `.bss` before
statics are valid.
Assembly is also required because the kernel uses hard-float
`aarch64-unknown-none`. LLVM may use FP/SIMD registers in any Rust function, so
`CPACR_EL1.FPEN` must be set before compiler-generated Rust executes. See
[01-toolchain-and-build.md](01-toolchain-and-build.md).
`cogware-bcm2837\src\arch\mod.rs` includes the file with `global_asm!` and substitutes
Rust constants for the core-id mask and stack size, avoiding silent drift.

## Core identity and stacks
The reset path reads `MPIDR_EL1`:
```asm
mrs     x19, MPIDR_EL1
and     x19, x19, #0b11
```
On the BCM2837, affinity level 0 identifies one of the four Cortex-A53 cores.
Masking with `0b11` gives core id 0 through 3. Core 0 is primary; cores 1-3 take
the secondary Rust path.
The linker script reserves four 128 KiB stacks in `.stacks`, after `.bss` and
marked `NOLOAD`. Boot assembly computes each top from `__stacks_end`:
```asm
ADR_ABS x0, __stacks_end
mov     x1, #KERNEL_STACK_SIZE
mul     x1, x1, x19
sub     x20, x0, x1
mov     sp, x20
```
Stacks grow down. Installing `SP` before zeroing `.bss` is safe only because
the stack region is outside the zeroed range.

## EL3 descent
If `CurrentEL` reports EL3, `.L_el3` configures `SCR_EL3`:
```asm
mov     x0, #SCR_EL3_VALUE
msr     SCR_EL3, x0
```
The value sets the lower-level contract:
```text
NS   lower levels are Non-secure
RW   lower levels run AArch64
HCE  hypervisor calls are enabled for the descent path
SMD  secure monitor calls are disabled
RES1 architecturally required one bits are preserved
```
The code also clears `CPTR_EL3` so FP/SIMD does not trap upward to EL3. It then
prepares `SPSR_EL3` for EL2h with debug, SError, IRQ and FIQ masked, sets
`ELR_EL3` to `.L_el2`, and executes `eret`.
QEMU proves this path today because `raspi3b` starts the kernel at EL3.

## EL2 descent
The EL2 path sets `HCR_EL2.RW`:
```asm
mov     x0, #(1 << 31)
msr     HCR_EL2, x0
```
That makes EL1 AArch64 rather than AArch32. It writes `CPTR_EL2` with required
RES1 bits set and trap bits clear, so EL1 FP/SIMD access is not trapped to EL2.
It also prepares the generic counter for EL1:
```asm
mrs     x0, CNTHCTL_EL2
orr     x0, x0, #0b11
msr     CNTHCTL_EL2, x0
msr     CNTVOFF_EL2, xzr
```
`EL1PCTEN` and `EL1PCEN` let EL1 read the counter and timer. Clearing
`CNTVOFF_EL2` makes the virtual counter track the physical counter.
Finally, EL2 sets `SPSR_EL2` for EL1h, points `ELR_EL2` at `.L_el1`, and
returns with `eret`.

## EL1 common path
At `.L_el1`, the code reinstalls the EL1 stack pointer and ungates FP/SIMD:
```asm
mrs     x0, CPACR_EL1
orr     x0, x0, #(0b11 << 20)
msr     CPACR_EL1, x0
isb
```
`FPEN = 0b11` permits FP/SIMD at EL0 and EL1. The `isb` makes later instructions
observe the new permission. The code then clears `FPCR` and `FPSR`.
Next it enables Cortex-A53 coherency participation:
```asm
mrs     x0, S3_1_C15_C2_1
orr     x0, x0, #(1 << 6)
msr     S3_1_C15_C2_1, x0
isb
```
`CPUECTLR_EL1.SMPEN` must be set before the data cache is enabled, or the core
may miss broadcast cache and TLB maintenance.
The boot path invalidates firmware cache state before writing translation
tables:
```asm
bl      __dcache_invalidate_all
ic      iallu
dsb     nsh
isb
```
QEMU is forgiving here; hardware is not.

## `.bss` zeroing
Only core 0 clears `.bss`:
```asm
cbnz    x19, .L_secondary
ADR_ABS x0, __bss_start
ADR_ABS x1, __bss_end
```
The loop stores zero pairs until `__bss_end`. This must happen before Rust
because Rust assumes zero-initialised statics are actually zero. In this kernel
that includes translation tables, locks, counters, mailbox buffers and the SMP
gate.
Only core 0 may do it. If a secondary cleared `.bss` while core 0 was building
state, it would erase live tables and locks.

## Branches into Rust
Core 0 tail-calls:
```asm
mov     x0, x19
b       kernel_boot_primary
```
Secondaries tail-call:
```asm
mov     x0, x19
b       kernel_boot_secondary
```
The core id is the first Rust argument. There is no caller to return to.
Both symbols are defined in `cogware-bcm2837\src\boot.rs`; the application
entry points are named by the `entry!` macro in each bootable example or
downstream kernel.

## Per-core architectural initialisation
Every core must establish its own EL1 runtime state. The reset assembly has
already enabled FP/SIMD and A53 coherency before Rust. The Rust-side per-core
initialisation point installs `VBAR_EL1`, enables the stage-1 MMU through
`SCTLR_EL1`, enables the caches, and only then may unmask DAIF-controlled
exceptions when the vector table is ready.
In the current source, `arch::init_core()` calls the vector-table install path
and `mmu::enable()`. The resulting boot report checks the visible state:
```text
MMU on, D-cache on, I-cache on
VBAR_EL1 = ...
FP/SIMD ungated: true
```
Locks and read-modify-write atomics become legal only after the current core has
Normal memory enabled by the MMU.

## The SMP boot race

Real Raspberry Pi firmware parks cores 1-3 in the spin table at `0xd8`. Core 0
later writes an entry point, cleans the cache line, and sends `sev`.
QEMU's `raspi3b` does not behave that way. It releases all four cores at
`0x80000` at reset and never uses the firmware spin table as the initial release
mechanism. Running QEMU with:
```text
-d int
```
showed four EL3-to-EL2 and four EL2-to-EL1 `eret` transitions at reset. GDB
showed four threads immediately on attach.
The resulting bug was that secondaries reached `mmu::enable()` while core 0 had
not populated `TABLES`. Those tables live in `.bss`, so they were still zero.
The secondary enabled translation against an empty table and immediately
faulted.

## `MMU_GATE`
`smp::MMU_GATE` fixes the race independently of how the core started:
```rust
static MMU_GATE: AtomicU64 = AtomicU64::new(0);
const GATE_OPEN: u64 = 0x5747_4154_454F_4B21; // "WGATEOK!"
```
Secondaries call this as their first Rust action:
```rust
smp::wait_for_mmu_gate();
```
Core 0 opens the gate only after the translation tables are built. The open path
stores `WGATEOK!`, cleans the cache line containing the gate, issues `dsb sy`,
and sends `sev`. The value is recognisable on purpose; a stray non-zero word
should not release a secondary by accident.

## Why the gate is plain load/store
Before the MMU is on, all memory is Device memory.
AArch64 says `LDXR` and `STXR` are architecturally UNPREDICTABLE on Device
memory. Every read-modify-write atomic uses that exclusive pair. Therefore
`SpinLock`, `Once`, `Barrier`, `fetch_add`, `compare_exchange` and similar
primitives are illegal before `mmu::enable()` on the current core.
Plain `AtomicU64` load and store are different. Acquire loads and release stores
lower to `LDAR` and `STLR`, which are defined on Device memory. That is the only
legal pre-MMU synchronisation primitive available here.
This is why `MMU_GATE` is not a lock. See
[06-synchronisation.md](06-synchronisation.md) and [04-smp.md](04-smp.md).
Under GDB, cores 2, 3 and 4 were observed waiting inside `__isb` in
`wait_for_mmu_gate`, as expected.

## Primary boot sequence
`kernel_boot_primary()` in `cogware-bcm2837\src\boot.rs` initialises the console first. This bypasses locks
because the MMU is off and core 0 is the only core allowed to use the drivers.
It then probes the VideoCore DRAM split:
```rust
drivers::mailbox::probe_memory()
```
This is before the MMU because pre-MMU memory is already uncached from the
ARM's point of view, so the mailbox buffer needs no cache maintenance.
Then core 0 builds tables and enables its own runtime state:
```rust
mmu::init()
arch::init_core()
```
After that, it marks itself online, initialises the heap, routes JTAG pins and
unlocks debug if the `jtag` feature is enabled, builds a `BootInfo`, and calls
the primary application entry point that `entry!` named. In the shell example,
that application code prints the banner, memory map and self-test output.
The application releases secondaries with:
```rust
smp::start_secondaries()
```
That writes the real spin-table slots and opens `MMU_GATE` for QEMU-started
secondaries. In the shell example, core 0 then waits for all cores, contributes
to the shared tally, waits at `BOOT_BARRIER`, prints the deterministic tally,
spawns the `shell` and `heartbeat` tasks, and hands itself to its executor with
`task::run()`. The shell is an `async fn` from that point on.

## Secondary boot sequence
`kernel_boot_secondary()` in `cogware-bcm2837\src\boot.rs` starts with:
```rust
smp::wait_for_mmu_gate();
```
Nothing that needs locks, Rust global initialisation, or the MMU may move before
that line.
After the gate opens, the secondary calls `arch::init_core()`, marks itself
online, unlocks its own debug registers if the `jtag` feature is enabled,
builds a `BootInfo`, and calls the secondary application entry point. In the
shell example, that entry point reports through the locked console, runs the
FPU self-test, contributes to the shared tally, waits at `BOOT_BARRIER`,
spawns its `worker` task, and hands the core to its executor with `task::run()`.
That call never returns; the core idles in `wfe` until a job arrives on the
shared channel. It replaced a plain `cpu::halt()`, and the sleeping behaviour
is identical — the difference is that the core can now be woken and put to
work. See [`12-async.md`](12-async.md).
Secondaries do not enter the shell. The interactive console belongs to core 0.

## `BOOT_BARRIER`
`BOOT_BARRIER` is a normal lock-based synchronisation primitive, so it is used
only after all cores have enabled the MMU.
It exists because the shared tally report used to race. Without the barrier,
core 0 could print while secondaries were still incrementing:
```text
visits: 1
```
With the barrier, the report is deterministic:
```text
visits: 4
```
If not all cores come online, core 0 skips the barrier to avoid hanging forever.

## Real spin-table release
For hardware, `smp::start_secondaries()` writes `_start` to:
```text
0xd8 + core_id * 8
```
for cores 1-3. It cleans each cache line because the waiting core is running
with caches off and might otherwise keep seeing a stale zero.
Then it executes a full-system barrier and `sev`. The `sev` is required because
the firmware loop waits with `wfe` before re-reading its slot.
Under QEMU this release is mostly ceremonial; those cores are already inside the
kernel and waiting on `MMU_GATE`.

## Annotated boot transcript
The verified QEMU exception-level path is:
```text
reset @ 0x00080000
  cpsr = 0x400003cd
  CurrentEL = EL3
.L_el3
  SCR_EL3 = NS | RW | HCE | SMD | RES1
  SPSR_EL3 selects EL2h with DAIF masked
  ELR_EL3 = .L_el2
  eret
.L_el2
  HCR_EL2.RW = 1
  CNTHCTL_EL2 allows EL1 counter access
  CNTVOFF_EL2 = 0
  SPSR_EL2 selects EL1h with DAIF masked
  ELR_EL2 = .L_el1
  eret
.L_el1
  CPACR_EL1.FPEN = 0b11
  FPCR = 0, FPSR = 0
  CPUECTLR_EL1.SMPEN = 1
  caches invalidated
  core 0 clears .bss
  branch to Rust
```
Exact addresses change as the image changes. The ordering must not.

## Rules for changing boot
Do not touch locks, `Once`, barriers or read-modify-write atomics before the
current core has enabled the MMU.
Do not move secondary code before `wait_for_mmu_gate()` unless it is legal on
Device memory and independent of core 0 initialisation.
Do not add secondary dependencies after `open_mmu_gate()` on core 0; publish all
required state before opening the gate.
Do not move Rust before `CPACR_EL1.FPEN`. The compiler is allowed to use NEON in
any function.
If the descent path changes, re-verify EL3-to-EL1 under QEMU and EL2-to-EL1 on
real hardware when silicon testing is available.

## Porting note: `CPUECTLR_EL1` must not be written at EL1

This is recorded because the failure has no symptom at all.

`CPUECTLR_EL1` is IMPLEMENTATION DEFINED, addressed as `S3_1_C15_C2_1`. Setting
its `SMPEN` bit is required for Cortex-A53 cache coherency, so writing
it early looks correct. It is not, because access to the IMPLEMENTATION DEFINED
registers is gated per exception level:

* `ACTLR_EL3` grants EL2 access.
* `ACTLR_EL2` grants EL1 access.

The Raspberry Pi's `armstub8.S` sets `ACTLR_EL3` and never sets `ACTLR_EL2`. A
read or write from EL1 therefore traps to EL2 -- where `VBAR_EL2` still points
at the stub's vector table. The core vanishes into firmware. No output, no
panic, no fault report: an apparently dead board.

QEMU does not model the gating, so `-M raspi3b` executes the same instruction
happily. The kernel booted perfectly in the emulator and hung on two Pi
Zero 2 W boards.

The write is also redundant. `armstub8.S` sets `SMPEN` on every core before the
`in_el2` split, so by the time a Pi kernel runs, coherency is already enabled.
The kernel only needs to do it when entered at EL3 by a custom stub, which is
where the write now lives:

```
.L_el3:
    msr     SCR_EL3, x0
    msr     CPTR_EL3, xzr
    mrs     x0, S3_1_C15_C2_1       /* legal here, gated at EL1 */
    orr     x0, x0, #(1 << 6)       /* SMPEN */
    msr     S3_1_C15_C2_1, x0
```

The general rule: treat every `S3_<op1>_C15_*` encoding as EL2-or-above unless
the stub is known to have opened it, and do not trust QEMU to reject one.

## `SCTLR_EL1` is set explicitly, not inherited

The descent to EL1 writes a known `SCTLR_EL1` before `eret`:

```
SCTLR_EL1_RES1 = 0x30D00800
```

That is the ARMv8.0 RES1 bits (11, 20, 22, 23, 28, 29) and nothing else: MMU
off, caches off, little-endian, alignment checking off.

Outside the RES1 bits the architectural reset value is UNKNOWN, and the value
the firmware leaves behind is not contractual. A stray `EE` (big-endian data),
`A` (strict alignment) or `M` (MMU enabled against a garbage `TTBR`) would fault
on the first instruction at EL1, with `VBAR_EL1` not yet installed -- the same
symptomless hang as above. Inheriting the field and masking bits into it, which
is what `mmu::init()` later does, is only safe once the base is known.
