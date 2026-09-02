# SMP

The BCM2837 has four Cortex-A53 cores, and this kernel brings all four into EL1
with the MMU, caches, and FP/SIMD enabled. Core 0 builds the machine,
publishes the MMU tables, releases the secondaries, and then all cores use the
same `SpinLock`, `Once`, and `Barrier` primitives.

QEMU and real hardware start the secondary cores differently, so the boot path
must handle both.

The synchronisation rules are in [06-synchronisation.md](06-synchronisation.md).
This page shows how they apply during boot and when sharing GPIO, the UART
console, and the VideoCore mailbox across cores.

## The four cores

`MPIDR_EL1` identifies the calling core. On the BCM2837, affinity level 0 is the
core number, so the boot assembly masks the register with `0xFF` and carries
that value in `x19` until it tail-calls Rust.

The project treats the core count as a board fact:

```rust
pub const CORE_COUNT: usize = 4;
pub const KERNEL_STACK_SIZE: usize = 128 * 1024;
```

Those constants in `bsp::memory` must stay in sync with
`cogware-bcm2837\link.ld`. Each
core owns a 128 KiB stack carved from the `.stacks` `NOLOAD` section. Stacks
grow downwards, and `layout::core_stack(core)` computes:

```text
top    = __stacks_end - core * KERNEL_STACK_SIZE
bottom = top - KERNEL_STACK_SIZE
```

The verified layout for the current image is:

```text
core 0 stack   0x000fe000 .. 0x0011e000
core 1 stack   0x000de000 .. 0x000fe000
core 2 stack   0x000be000 .. 0x000de000
core 3 stack   0x0009e000 .. 0x000be000
```

Core 0 gets the highest stack because the formula starts from `__stacks_end`
and subtracts `core_id * stack_size`.

`.stacks` stays after `.bss` because the reset path installs `sp` before core 0
zeroes `.bss`. The live stack must not be in the zeroed range. The section is
`NOLOAD`, or the flat image would be padded with the stack reservation.

Every core runs the same reset symbol, `_start`. The assembly first masks
asynchronous exceptions, records the core id, installs that core's stack,
normalises EL3/EL2 to EL1h, ungates FP/SIMD, enables Cortex-A53 SMP coherency,
invalidates firmware cache state, and then branches by core id. Core 0 zeroes
`.bss` and enters `kernel_boot_primary`; non-zero cores enter
`kernel_boot_secondary`.

The FP/SIMD step is not optional. The target is the hard-float
`aarch64-unknown-none` target, so LLVM may use `q` and `d` registers in ordinary
Rust code. Each core must set `CPACR_EL1.FPEN` before compiler-generated code
runs. The exception path saves the full NEON register file for the same reason.

## The boot problem

Real Raspberry Pi firmware parks cores 1-3 in a spin table at physical `0xd8`.
Core `n` polls `0xd8 + n * 8` and branches to the 64-bit address written there.
Core 0 releases a secondary by writing `_start` into that slot, cleaning the
cache line, and sending `sev` because the firmware loop waits with `wfe`.

That was the expected model. QEMU's `raspi3b` model instead releases all four
cores at `0x80000` at reset and never uses the firmware spin table. Confirmed
with `-d int`, which showed four EL3-to-EL2 and four EL2-to-EL1 `eret`s at
reset, and GDB, which showed four threads immediately on attach.

Cores 1-3 reached `kernel_boot_secondary` and called `mmu::enable()` before
core 0 populated `TABLES`. Those tables live in `.bss`, so they were still
zero. A secondary enabled translation against a zero L1 table, faulted on the
next instruction fetch, and could also fault while fetching the exception
vector.

The spin table alone cannot fix that: it only controls whether a core starts
from firmware, and under QEMU the core has already started. The kernel needs a
second condition: the MMU tables must exist before a secondary enables
translation.

That condition is `smp::MMU_GATE`.

## The gate

`MMU_GATE` is an `AtomicU64` publication barrier. The closed value is zero. The
open value is a recognisable pattern:

```text
0x5747_4154_454F_4B21   "WGATEOK!"
```

A secondary does this as its first Rust action:

```rust
pub fn wait_for_mmu_gate() {
    while MMU_GATE.load(Ordering::Acquire) != GATE_OPEN {
        core::hint::spin_loop();
    }
}
```

Core 0 opens the gate only after it has probed the VideoCore split, built the
translation tables, enabled its own MMU, switched the console into lock-based
operation, and marked itself online. Opening is a release store followed by a
cache-line clean/invalidate, a full `dsb`, and `sev`:

```rust
MMU_GATE.store(GATE_OPEN, Ordering::Release);
mmu::clean_invalidate_line(addr_of!(MMU_GATE) as usize);
cpu::dsb_sy();
cpu::sev();
```

The acquire/release pairing is the publication contract. Every descriptor store
and every initialisation a secondary depends on must happen before the release
store. A secondary that observes the open value with acquire ordering can then
enable its own MMU.

Core 0 has its data cache on by the time it opens the gate, while a waiting
secondary still has caches off. The gate value must therefore be pushed to the
point of coherency. Without `dc civac` and the following barrier, real hardware
could keep the new value in core 0's L1 while a secondary repeatedly reads zero
from memory.

Under GDB, cores 1, 2 and 3 were observed waiting in `wait_for_mmu_gate()`
until core 0 opened the gate.

Anything a secondary reads must be published before the gate opens. Adding
core-0 initialisation after `open_mmu_gate()` that secondaries depend on is a
race.

## The pre-MMU warning

The gate uses plain load and store, and nothing else, for a reason that must not
be weakened.

**No read-modify-write atomic, no `SpinLock`, no `Once`, and no `Barrier` may be
used by a core before that core has enabled its MMU.**

On AArch64, Rust RMW atomics lower to exclusive-monitor instructions such as
`LDXR`/`STXR`, `LDAXR`/`STLXR`, or close variants. Those instructions are
architecturally UNPREDICTABLE on Device memory. With the MMU off, all memory is
Device-nGnRnE. Therefore every primitive built from RMW atomics is illegal in
the pre-MMU window.

Plain atomic load and store are the exception. `AtomicU64::load(Ordering::Acquire)`
and `AtomicU64::store(Ordering::Release)` lower to `LDAR` and `STLR`, not to an
exclusive sequence. Those are ordinary single-copy atomic accesses and are
defined before the MMU is on.

That is why `MMU_GATE` is an `AtomicU64` and not a `Barrier`, `Once`, or
`SpinLock<bool>`. It is also why secondary boot code must stay short enough to
audit. If a future change wants to coordinate cores before `arch::init_core()`,
it must be built from plain load/store plus explicit cache maintenance, or it
must move after the MMU switch. See [06-synchronisation.md](06-synchronisation.md).

## Starting secondaries

`start_secondaries()` handles both possible worlds.

First it writes `_start` to the real firmware spin-table slots for cores 1-3:

```rust
for core_id in 1..CORE_COUNT {
    let slot = SPIN_TABLE_BASE + core_id * 8;
    slot.write_volatile(_start as u64);
    mmu::clean_invalidate_line(slot);
}

dsb sy
sev
```

The volatile store is aimed at low memory reserved for the firmware stub. The
cache maintenance is required because the parked core reads with caches off. The
`sev` is required because the firmware loop executes `wfe` before reloading its
slot.

On real hardware, that write is what makes a secondary execute kernel code.
The released core re-enters `_start`, computes its own stack and core id,
skips `.bss` zeroing, and reaches `kernel_boot_secondary`. By then the MMU gate
is already open, so it passes straight through.

Under QEMU, the spin-table slots are not what released the cores. The
secondaries are already in `kernel_boot_secondary`, spinning on `MMU_GATE`. The
same `start_secondaries()` call still opens the gate, so they continue.

The code therefore performs both steps every time: the real hardware release
and the QEMU-safe publication barrier.

After opening the gate, core 0 waits up to one second for `ONLINE` to reach
`CORE_COUNT`. Each core calls `mark_online()` only after its own MMU is enabled,
because `ONLINE.fetch_add` is a read-modify-write atomic.

## The primitives

The public synchronisation module exports `Barrier`, `Once`, `SpinLock` and
`SpinLockGuard`. They are small enough to read, but each one encodes an ordering
argument.

`SpinLock<T>` is the ordinary sharing primitive. It contains an atomic `state`,
a diagnostic `holder`, and the protected `UnsafeCell<T>`. Lock acquisition is
test-and-test-and-set: first try to change `state` from 0 to 1 with acquire
ordering; if that fails, spin on relaxed loads until the line looks free again.

The second test matters. A naive loop that repeatedly performs
`compare_exchange` would bounce the cache line into exclusive ownership on every
waiting core. The relaxed read loop lets waiters mostly share the line until the
holder releases it.

`SpinLockGuard` gives RAII release. Dropping the guard clears `holder` and stores
`state = 0` with release ordering. The acquire on the next successful locker
pairs with that release, so updates inside the critical section become visible
to the next holder.

`try_lock()` performs one acquisition attempt and returns `None` if the lock is
held. That is for paths that must not block, especially panic and exception
reporting. A fault handler can interrupt code that already owns the console
lock; spinning there would wait for code that cannot run until the handler
returns.

`Once<T>` is for one-time initialisation after the MMU is on. Its states are
`UNINITIALISED`, `INITIALISING`, and `READY`. The winner changes
`UNINITIALISED` to `INITIALISING`, writes the value, and publishes `READY` with
a release store. Losers spin on acquire loads until `READY` is visible.

The acquire on `get()` and `get_or_init()` is the reason a returned `&T` can see
the initialised contents. Without the release/acquire edge, another core could
observe the state change without observing the data write that preceded it.

`Barrier` is a reusable, sense-reversing barrier. It has `arrived`,
`generation`, and `total`. A waiter first reads the current generation, then
increments `arrived`. The last arriver resets `arrived`, bumps `generation`,
issues `sev`, and returns. Other cores wait until the generation changes.

The generation is the sense. It is what makes the barrier reusable. A naive
counting barrier breaks when a fast core leaves round N and enters round N+1
before a slow core has finished observing round N. The fast core can be counted
into the old round or leave the counter in a state nobody can interpret.
Reading the generation before incrementing ties each arrival to the round it
intended to join.

`waiting()` and `rounds()` are diagnostics. They are not permission to mutate the
protected state of the barrier; they are snapshots for reporting.

## The boot barrier

`BOOT_BARRIER` is the worked example. Every core updates the shared tally and
then waits at the barrier. Core 0 prints the tally only after all four cores have
arrived.

Before the barrier existed, core 0 sometimes printed:

```text
visits: 1
```

That was not a lock failure. It was a snapshot of a race. With `BOOT_BARRIER`,
`visits: 4` is deterministic when all cores are online.

This is the distinction the SMP design keeps making. A lock protects one
critical section. It does not say that all other cores have reached the same
point in the programme. A barrier does.

The barrier is skipped if not all cores came online, because waiting for a
missing core would be a deliberate hang.

## Sharing hardware

The project requirement is not just to light up four cores; it is to share fixed
hardware resources safely across those cores. GPIO, the PL011 console, and the
VideoCore mailbox are singleton resources.

The driver module exposes them as locked statics:

```rust
pub static GPIO: SpinLock<gpio::Gpio> = SpinLock::new(unsafe { gpio::Gpio::new() });
pub static UART: SpinLock<pl011::Pl011Uart> = SpinLock::new(unsafe { pl011::Pl011Uart::new() });
pub static MAILBOX: SpinLock<mailbox::Mailbox> = SpinLock::new(unsafe { mailbox::Mailbox::new() });
```

GPIO needs the lock because many register updates are packed read-modify-write
operations. Two cores changing different pins in the same `GPFSELn` word can
lose an update without serialisation. The boot demo has every core read GPIO14
under the lock and confirm it is routed to ALT0/TXD0.

The console needs the lock because a UART byte stream is a shared output device.
Without a lock, lines from different cores interleave at character granularity
and fault dumps become unreadable. `console::with_writer` takes the UART lock
once for a whole multi-line report so memory maps, fault dumps, and tables
cannot be split by another core's `println!`.

The mailbox lock protects more than registers. The property interface uses one
static 16-byte-aligned message buffer. The VideoCore writes its reply back into
that same buffer. Two concurrent calls would overwrite each other's request and
reply even if the mailbox registers themselves were magically independent.

The mailbox has one early-boot bypass: `probe_memory()` runs before the MMU is
enabled, before secondary cores are allowed to proceed, and before locks are
legal. That is sound because the system is still single-core and uncached.
Normal mailbox calls after boot go through `MAILBOX.lock()`.

The console has a similar documented exception. `console.rs` checks
`mmu::is_enabled()`. If the MMU is off, it bypasses the UART lock; if the MMU is
on, it locks. This is sound only because the boot invariant says "MMU off"
implies "core 0 alone". If a future boot path lets any secondary print before
its MMU is on, the console design must change at the same time.

The lock discipline is intentionally narrow. Do not hold more than one driver
lock at a time unless a future document introduces a global lock ordering. Do
not hold a lock across a spin-wait, a `wfe`, a mailbox poll that can wedge
forever, or anything that can fault. Deadlocking core 0 takes the console with
it and the board goes silent.

## The shared tally

`cogware-bcm2837\examples\shell\main.rs` contains a small shared-state demo because SMP bugs should be visible
at boot, not after a driver grows complicated.

The shared value is:

```rust
struct Tally {
    visits: u64,
    per_core: [u64; CORE_COUNT],
    sum: f64,
}

static TALLY: SpinLock<Tally> = SpinLock::new(...);
```

Each core takes the lock, increments `visits`, increments its own `per_core`
slot, and adds `(core_id + 1).recip()` to the `f64` sum. The lock proves shared
mutable data survives contention. The per-core array proves every core actually
visited. The floating-point field proves something more specific to this
project.

Because the target is hard-float, the kernel has promised that FP/SIMD is live
on every core and that the exception path preserves it. The tally makes `f64`
travel through a lock-protected shared structure, while `self_test_fpu()` forces
real `fsqrt` arithmetic on each core. If FP/SIMD were still trapped on a
secondary, or if a context path corrupted `d` registers, the demo would fail in
a place that is easy to notice.

The demo also reads shared GPIO under lock. That ties the abstract `SpinLock` to
the real user requirement: resources such as GPIO can be shared safely across
all four cores.

## What comes next

Each core now ends its bring-up in `task::run()` rather than `cpu::halt()`, so
"parked" means "idle in an executor" and a core can be given work at any time.
Cross-core work is handed over through `task::Channel`, which is a `SpinLock`ed
ring buffer plus two `TaskSet` wait queues; the waking side issues `sev` and
the sleeping side is in `wfe`. That is the whole IPI mechanism today, and it
is enough because `sev` is broadcast. See [12-async.md](12-async.md).

Inter-processor interrupts are not implemented yet, but the address hooks are
already in `bsp::memory::local`:

```rust
mailbox_write_set(core, mbox)
mailbox_read_clear(core, mbox)
mailbox_irq_control(core)
irq_source(core)
fiq_source(core)
timer_irq_control(core)
```

The ARM-local mailbox write-set register is the IPI mechanism. A future sender
will write a bit to `mailbox_write_set(target_core, mailbox)`, the target core
will take a mailbox interrupt, and the handler will clear it through the
read-clear register. That belongs with the IRQ work in [10-roadmap.md](10-roadmap.md).
The pay-off for the executor is that `spawn_on` could then wake a sleeping core
directly rather than relying on a broadcast `sev`.

Per-core generic timer interrupts are the next scheduling primitive. The boot
code already lets EL1 read the counters, and `time.rs` reads `CNTFRQ_EL0`
rather than assuming a frequency.

A future async executor would sit above those two mechanisms. IPIs distribute
work or wake remote cores; per-core timers drive deadlines; locks remain the
low-level protection around singleton devices. None of that exists yet, and it
should not be documented as if it does.

## What silicon changed

The gaps above were closed on a Pi Zero 2 W, and two of the three answers were
the opposite of what the code assumed. Both defects were invisible under
`-M raspi3b`.

**`CPUECTLR_EL1.SMPEN` must not be written at EL1.** Access to the
IMPLEMENTATION DEFINED registers is gated per exception level: `ACTLR_EL3`
grants EL2, `ACTLR_EL2` grants EL1. `armstub8.S` sets the former and never the
latter, so `msr S3_1_C15_C2_1` at EL1 traps to EL2, where `VBAR_EL2` is still
the stub's -- the core disappears into firmware with no output at all. The
write is also redundant, because the stub already sets `SMPEN` on every core
before it drops to EL2. It now lives only on the EL3 path, for the custom-stub
case. Full write-up in
[`02-boot-and-exception-levels.md`](02-boot-and-exception-levels.md).

**Set/way maintenance has to be split by role.** `dc isw` invalidates *without*
cleaning, and the walk to the Level of Coherency includes L2 -- which all four
cores share. A secondary running the full walk would discard core 0's dirty L2
lines, and secondaries are released long after core 0 is live. `boot.s` now
branches on core id *before* any cache maintenance: core 0 calls
`__dcache_invalidate_all`, secondaries call `__dcache_invalidate_l1`, which
stops at level 0 because the L1 data cache is private per core. QEMU models
set/way as a no-op, so it could never have caught this.

**The firmware spin table works as documented.** All four cores come up through
the `armstub8.S` mailboxes, reaching the barrier 24.7 ms after reset -- against
4.4 ms in QEMU, where the cores are already running at reset and the release is
a formality.

## Nondeterminism is the point

On hardware the secondaries announce themselves out of order, and the order
changes between runs:

```text
[core 1] online at EL1, MMU on, FPU on
[core 3] online at EL1, MMU on, FPU on
[core 2] online at EL1, MMU on, FPU on
```

QEMU releases them deterministically and prints 1, 2, 3 every time. Do not
treat a green emulator run as evidence that locking is correct: under
`-M raspi3b` a broken lock has far fewer chances to lose.

The `multicore` example makes the ordering observable in a second way. Each core
adds `1/(id + 1)` to a shared `f64` under the lock, and `f64` addition is not
associative, so the accumulation order shows up in the last digit:

| Lock order | `H(4)` |
| --- | --- |
| `0,1,2,3` | `2.083333333333333` |
| `0,1,3,2` | `2.0833333333333335` |

Both are the correctly rounded result for their own order, 1 ULP apart. The
hardware run printed the second, matching the order the cores actually reported.
A drifting last digit here is evidence that the lock is serialising a genuine
race; a *stable* value across many hardware runs would be the suspicious
outcome.

Emulator behaviour is useful evidence, but it is not a substitute for the Pi 3.
