# Synchronisation

The synchronisation module exports the primitives the rest of the kernel is
expected to build on: `Barrier`, `Once`, `SpinLock` and `SpinLockGuard`
(`cogware-bcm2837\src\sync\mod.rs`). They are small, but they sit on top of a very
important AArch64 rule.

**No lock, no `Once`, no `Barrier`, and no read-modify-write atomic may be used
by a core before that core has enabled its own MMU.**

The reason is architectural. Rust read-modify-write atomics such as
`fetch_add`, `compare_exchange`, and the atomics inside `SpinLock`, `Once` and
`Barrier`, are implemented with the AArch64 exclusive monitor: `LDXR`/`STXR`
and acquire/release variants such as `LDAXR`/`STLXR`. On Cortex-A53 these
instructions are defined for Normal cacheable memory. With the MMU off, memory
behaves as Device-nGnRnE, and exclusive-monitor behaviour is UNPREDICTABLE.

Plain atomic load/store are exempt. `AtomicU64::load` and `AtomicU64::store`
lower to `LDAR` and `STLR`, not to a read-modify-write exclusive sequence. This
is why the SMP `MMU_GATE` can hand a core from MMU-off boot code into MMU-on
kernel code safely. See [03-memory-and-mmu.md](03-memory-and-mmu.md) and
[04-smp.md](04-smp.md) for the surrounding boot order.

## Where the rule is documented in source

`cogware-bcm2837\src\sync\mod.rs` states that all synchronisation primitives rely on
`LDXR`/`STXR` and may only be used once the calling core's MMU is enabled.

`cogware-bcm2837\src\sync\spinlock.rs` gives the detailed version for the lock:
`lock()` compiles to `LDAXR`/`STLXR`; with the MMU off every access is
Device-nGnRnE; therefore no `SpinLock` may be taken before
`crate::arch::mmu::enable` has run on that core.

That comment also records the load/store exemption. The current wording mentions
an `AtomicBool`; the underlying point is broader and is what the SMP gate uses:
plain atomic load/store are ordinary single-copy atomic accesses, while RMW
atomics are exclusive-monitor operations.

## Console locking follows the MMU

The console is the clearest example of this rule shaping real code.
`cogware-bcm2837\src\console.rs` decides whether locking is available by reading the
per-core MMU enable bit:

```rust
fn locking_available() -> bool {
    mmu::is_enabled()
}
```

All ordinary console entry points delegate to `with_writer`: `_print`,
`flush`, `read_byte` and `statistics`. If the MMU is on, it holds `UART.lock()`
for the whole closure. Contention is not permission to bypass the lock.
Before the MMU is enabled it uses `UART.bypass()` under an `EarlyBorrow`
guard made only from plain atomic loads and stores. A nested console call
panics before constructing a second mutable reference; being single-core
does not by itself exclude reentrant callbacks.

This is sound only because of a boot-order invariant: the only code that prints
with the MMU off is core 0 before `start_secondaries` releases the other cores.
Every secondary enables its own MMU before its first `println!`. Therefore
“MMU off” implies “single-threaded” in this kernel.

## `SpinLock<T>`

`SpinLock<T>` is declared at `cogware-bcm2837\src\sync\spinlock.rs`. It contains:

```rust
state: AtomicU32,
holder: AtomicU32,
data: UnsafeCell<T>,
```

`state` is `0` for free and `1` for held. `holder` records the current core id
for diagnostics, or `NO_HOLDER`, which is `u32::MAX`
(`cogware-bcm2837\src\sync\spinlock.rs`). The payload sits in
`UnsafeCell<T>`, because the lock is what makes mutable access sound.

The unsafe trait implementations at `cogware-bcm2837\src\sync\spinlock.rs` require
`T: Send`. The lock serialises access, but it cannot make a non-sendable value
safe to move between cores.

## Lock acquisition

`SpinLock::lock` starts at `cogware-bcm2837\src\sync\spinlock.rs`. It first checks
for self-deadlock:

```rust
assert!(
    self.holder.load(Ordering::Relaxed) != me,
    "SpinLock is not reentrant: core {me} tried to re-acquire a lock it already holds"
);
```

This lock is not reentrant. Panicking on a recursive acquisition is preferable to
spinning forever.

The acquisition loop uses test-and-test-and-set. The first test is
`compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)` at
`cogware-bcm2837\src\sync\spinlock.rs`. If it succeeds, the lock records the holder
and returns `SpinLockGuard`. If it fails, the code spins on a plain relaxed load
until `state` becomes zero (`cogware-bcm2837\src\sync\spinlock.rs`).

That second test matters on real SMP hardware. Waiting cores mostly read a
shared cache line instead of repeatedly forcing exclusive ownership of the line.

`SpinLockGuard` is declared at `cogware-bcm2837\src\sync\spinlock.rs`. Its `Deref`
and `DerefMut` implementations at `cogware-bcm2837\src\sync\spinlock.rs` expose
ordinary references while the guard exists. `Drop` clears `holder` and stores
`state = 0` with release ordering (`cogware-bcm2837\src\sync\spinlock.rs`).

Both `SpinLockGuard<T>` and `task::MutexGuard<T>` carry
`PhantomData<&mut T>`. A lock may protect a `Send` value such as `Cell<u32>`,
but sharing references to its guard also requires `T: Sync`: those references
expose `&T` without another lock acquisition. The marker preserves this
distinction instead of inheriting the lock's weaker auto-trait bound.

## Non-blocking and unsafe paths

`try_lock` at `cogware-bcm2837\src\sync\spinlock.rs` performs one
`compare_exchange`. It returns `None` instead of spinning if the lock is already
held. This is essential for exception and panic paths, where the interrupted
code might be the lock holder.

`with` at `cogware-bcm2837\src\sync\spinlock.rs` is just a closure helper around
`lock()`.

`is_locked` at `cogware-bcm2837\src\sync\spinlock.rs` is diagnostic. It uses a
relaxed load and must not be treated as permission to access the payload without
a guard.

`bypass` at `cogware-bcm2837\src\sync\spinlock.rs` returns `&mut T` without taking
the lock. The caller must exclude all other accesses and outstanding references,
including on the same core. Single-core boot before the MMU is enabled meets
that contract. A panic while a guard remains outstanding does not.

`force_unlock` at `cogware-bcm2837\src\sync\spinlock.rs` unconditionally releases the
lock. It is unsafe because no outstanding guard may subsequently be used or
dropped, and no reference to the payload may remain live. The BSP's panic path
does not use it.

## `Once<T>`

`Once<T>` is the one-shot initialisation cell in `cogware-bcm2837\src\sync\once.rs`. Its
state machine is:

```rust
const UNINITIALISED: u32 = 0;
const INITIALISING: u32 = 1;
const READY: u32 = 2;
```

These constants are at `cogware-bcm2837\src\sync\once.rs`. The struct itself is at
`cogware-bcm2837\src\sync\once.rs` and contains an `AtomicU32` state plus
`UnsafeCell<MaybeUninit<T>>`.

`get_or_init` starts at `cogware-bcm2837\src\sync\once.rs`. It first uses an acquire
load to fast-path `READY`. Otherwise it tries:

```rust
compare_exchange(UNINITIALISED, INITIALISING, Ordering::Acquire, Ordering::Acquire)
```

That compare-exchange is at `cogware-bcm2837\src\sync\once.rs`. The winner writes
the value and publishes `READY` with a release store
(`cogware-bcm2837\src\sync\once.rs`). Losers spin on acquire loads until the value
is ready (`cogware-bcm2837\src\sync\once.rs`).

`get` at `cogware-bcm2837\src\sync\once.rs` is non-initialising. It returns `Some(&T)`
only after the same acquire observation of `READY`.

Because `get_or_init` uses `compare_exchange`, `Once` is subject to the MMU
rule. It must not be raced before the caller's MMU is enabled.

## `Barrier`

`Barrier` is a sense-reversing barrier declared at
`cogware-bcm2837\src\sync\barrier.rs`. It has three fields:

```rust
arrived: AtomicUsize,
generation: AtomicUsize,
total: usize,
```

`Barrier::new` at `cogware-bcm2837\src\sync\barrier.rs` asserts `total > 0`, because a
zero-participant barrier can never release.

`Barrier::wait` starts at `cogware-bcm2837\src\sync\barrier.rs`. The order is the key
detail:

1. read `generation` with acquire ordering;
2. increment `arrived` with `fetch_add(1, Ordering::AcqRel)`;
3. if this core is the last arriver, reset `arrived`, bump `generation`, issue
   `cpu::sev()`, and return;
4. otherwise spin until `generation` changes.

Reading `generation` before incrementing `arrived` is what makes the barrier
reusable. A core that immediately enters the next round cannot be counted into
the round it just left, because waiters are comparing against the generation
they observed before arriving.

The last arriver calls `cpu::sev()` at `cogware-bcm2837\src\sync\barrier.rs`. The
current wait loop uses `core::hint::spin_loop()`, but the event is still the
right wake-up primitive for a future `wfe`-based waiter.

`waiting` and `rounds` are diagnostic helpers at
`cogware-bcm2837\src\sync\barrier.rs`.

`Barrier` is used in `cogware-bcm2837\examples\shell\main.rs` as
`BOOT_BARRIER` so core 0 reports the shared
tally only after all four cores have finished their work. Before the barrier
existed, the tally could print `visits: 1`, which was only a snapshot of a race.

## Driver lock discipline

The current driver globals are `GPIO` and `UART` in `cogware-bcm2837\src\drivers\mod.rs`
(`cogware-bcm2837\src\drivers\mod.rs`). GPIO protects packed
read-modify-write register fields; UART protects a shared byte stream. The
register-level details are in [05-drivers-and-mmio.md](05-drivers-and-mmio.md).

The rule is intentionally simple: take only one of these locks at a time, and
never nest them. Today that is enough to prove there is no driver-lock cycle.

Future code that needs two peripherals should either split the operation:

```rust
{
    let mut gpio = drivers::GPIO.lock();
    // GPIO work only.
}
{
    let mut uart = drivers::UART.lock();
    // UART work only.
}
```

or introduce a documented global lock ordering before nesting any locks.

## Exception context is special

Exception and IRQ handlers cannot use ordinary blocking rules. An IRQ can
interrupt code that already owns a lock on the same core. If the handler then
spins on that lock, it waits for code that cannot run until the handler returns.

This is why `IRQ_HANDLER` in `cogware-bcm2837\src\arch\exception.rs` is an
`AtomicUsize` raw function pointer rather than a locked structure. The handler
path uses an acquire load and calls the function directly.

Faults and panics use `console::with_emergency_writer`
(`cogware-bcm2837\src\console.rs`), not ordinary `with_writer`. It tries the
UART lock once to keep a report together when possible, but its writer never
borrows the driver or changes its counters. It accesses only the external
PL011 data and status registers through stateless `RegisterBlock` helpers.
If the UART is already held, output can interleave without creating overlapping
Rust references or a data race in driver RAM. UART waits are bounded and a
failure is returned as `fmt::Error`.

The exception details are in [07-exceptions-and-irq.md](07-exceptions-and-irq.md).

## Which lock to use

Since the [async runtime](12-async.md) landed there are two families of lock in
the kernel, and picking the wrong one is a deadlock rather than a slowdown.

| Situation | Primitive |
| --- | --- |
| A few instructions, no `.await` | `sync::SpinLock` |
| A critical section that spans an `.await` | `task::Mutex` |
| Callable from an exception handler | `sync::SpinLock` with `try_lock` |
| Handing values between cores | `task::Channel<T, N>` |
| One-time initialisation | `sync::Once` |
| Rendezvous of all four cores | `sync::Barrier` |

The reason the distinction matters: a `SpinLockGuard` is not released while its
task is parked. Another task on the same core that takes the same lock will
spin until the timer forces the parked task back — and on a single core with
nothing else runnable, forever. `task::Mutex` parks the waiter in a `TaskSet`
instead, which is why it is the only correct choice across a suspension point.

The console deliberately keeps its `SpinLock`. It is taken and released inside
a single `write_fmt`, never across an `.await`, and a fault handler must be
able to try it once without blocking. Emergency output is deliberately excluded
from the ordinary UART byte statistics.

## Regression checks

`cargo run --release --example regressions` checks the guards' positive and
negative auto-trait bounds at compile time, then checks cross-core console
exclusion and emergency output while UART is held. It also takes a returning
breakpoint inside that critical section and verifies timer progress alongside a
continuously yielding task. Successful completion prints `regressions: PASS`.

## Checklist for new synchronisation

Before adding a primitive or using an atomic operation in boot code:

1. Ask whether it is a read-modify-write atomic. If yes, it requires the MMU.
2. Use plain atomic load/store only for MMU-off handshakes, and keep the path
   short enough to audit.
3. Do not put locks inside `mmio.rs`; ownership belongs at the driver level.
4. Do not block in exception context on a lock that interrupted code might hold.
5. Keep GPIO and UART locks non-nested unless a global ordering is documented.
6. If the critical section contains an `.await`, it must be a `task::Mutex`.
   Holding a `SpinLock` across a suspension point is a bug, not a trade-off.
