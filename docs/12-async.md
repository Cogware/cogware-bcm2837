# Async runtime

The kernel now has a cooperative async runtime. It is deliberately not a
general-purpose executor: it is the smallest design that lets a no-heap,
hard-float `aarch64-unknown-none` kernel park Cortex-A53 cores in `wfe` while
tasks wait for UART bytes, timer ticks, locks or cross-core work.
It uses stable Rust 1.98 and edition 2024, with no nightly features.

The surrounding rules still matter. The synchronisation rule in
[`instructions.md`](../instructions.md) says no read-modify-write atomic may be
touched before the calling core has enabled its MMU. The async task table uses
RMW atomics and `SpinLock`, so executors only start at the end of bring-up,
after `arch::init_core()`.

## Why async at all

Without interrupts, the obvious way to wait for hardware is a polling loop:
read a status register, spin for a while, and try again. That works, but it
throws away the whole point of four cores. A task waiting for a byte from the
PL011 or for a timer deadline should not keep its core hot.

The async runtime gives those waits a place to park. A future returns
`Poll::Pending`, the executor moves on to another ready task, and if there is no
ready work the core executes `wfe`. This is still cooperative: there is no
preemption. A task that never awaits wedges the core that owns it.

The rejected alternative was a heap-backed executor with `Box<dyn Future>`.
When this executor was written there was no allocator at all, so boxing was not
an option. There is one now — see [`13-heap.md`](13-heap.md) — but the fixed
slot was kept deliberately: it makes the task count a compile-time constant,
which is what lets the whole wait-queue machinery be a single `AtomicU32`, and
it means spawning cannot fail for reasons that vary with heap fragmentation.
The design stores each future in a fixed slot:

```rust
pub const TASK_STORAGE_BYTES: usize = 1024;
```

Each slot owns 1 KiB of `#[repr(align(16))]` inline storage and a hand-rolled
vtable:

```rust
struct VTable {
    poll: unsafe fn(*mut u8, &mut Context<'_>) -> Poll<()>,
    drop_in_place: unsafe fn(*mut u8),
}
```

That is a `Box<dyn Future>` in a static arena: data in the slot, behaviour in
the vtable. It is the stable-Rust choice. The embassy-style "put the future in
a static whose exact type is sized by the compiler" approach wants
type-alias-impl-trait; TAIT is not stable, so that route is not available here.

Oversize and over-alignment are runtime errors, never undefined behaviour.
`spawn_on` checks `size_of::<F>()` and `align_of::<F>()` before writing to the
slot, and reports:

```rust
pub enum SpawnError {
    TableFull,
    NoSuchCore,
    TooLarge { needed: usize },
    Overaligned { needed: usize },
}
```

## The fixed numbers

The table is small on purpose:

| Constant | Value | Why it matters |
| --- | ---: | --- |
| `TASKS_PER_CORE` | 8 | Each executor has eight slots. |
| `CORE_COUNT` | 4 | The BCM2837 has four Cortex-A53 cores. |
| `MAX_TASKS` | 32 | Four cores times eight tasks. |
| `TASK_STORAGE_BYTES` | 1024 | Inline storage reserved per future. |

The punchline is `MAX_TASKS = 32`: a set of tasks is a single `u32`.
`TaskSet` is therefore just an `AtomicU32`, one bit per `TaskId`. That is what
makes allocation-free wait queues possible for `task::Mutex` and
`task::Channel`.

The invariant is compile-time checked:

```rust
const _: () = assert!(
    MAX_TASKS <= 32,
    "TaskSet is a u32; reduce TASKS_PER_CORE or widen it"
);
```

The alternative would be a linked wait list or heap allocation. Both are larger
and introduce failure modes the kernel does not need at this stage.

## Task identity and the waker

`TaskId` is a single byte wrapper around the system-wide slot index. It exposes
the conversions the executor needs:

```rust
pub const fn new(core: usize, slot: usize) -> Self
pub const fn core(self) -> usize
pub const fn slot(self) -> usize
pub const fn index(self) -> usize
pub const fn from_index(index: usize) -> Option<Self>
```

The `RawWaker` data pointer is that task index. It is built with
`core::ptr::without_provenance(task.index())` and decoded with `.addr()`, both
stable since Rust 1.84:

```rust
RawWaker::new(ptr::without_provenance(task.index()), &VTABLE)
```

The waker carries no ownership. `clone` copies the pointer, `drop` does
nothing, and there is no refcount or lifetime to maintain. That is the whole
reason waking never allocates — which matters even now that a heap exists,
because a waker may be cloned from a context where taking the heap lock would
be the wrong thing to do.

The consequence is explicit and acceptable. A stale waker whose slot has been
freed and reused wakes the new task in that slot. That costs one spurious poll.
The `Future` contract permits spurious polls, and every future in this kernel
re-checks its condition on entry before making progress.

## `task::current()`

Stable `Waker` does not expose its raw parts, so a future cannot ask the
`Context`, "which task am I?" The executor publishes that fact separately.

Around each poll, `Executor::poll_slot` stores the task's global index in a
per-core `AtomicU8`:

```rust
const NO_TASK: u8 = u8::MAX;
```

`task::current()` reads that value through the current core's executor:

```rust
pub fn current() -> Option<TaskId>
```

It returns `None` outside a poll: from the boot path, an exception handler, or a
core whose executor has not started. `TaskSet::insert_current()` is built on
this. A future does not need an allocation or a raw waker escape hatch to put
itself in a wait set.

## Slot state machine

Each slot moves through one simple state machine:

```text
FREE(0) -> SPAWNING(1) -> OCCUPIED(2) -> FREE(0)
```

| State | Owner | Meaning |
| --- | --- | --- |
| `FREE` | Nobody | The slot may be claimed. |
| `SPAWNING` | The spawner that won `compare_exchange` | Storage, vtable and name are being written. |
| `OCCUPIED` | The owning core's poll loop | The future is live and may be polled. |

The `compare_exchange(FREE, SPAWNING, Ordering::Acquire, Ordering::Relaxed)` is
the exclusive claim. The final `state.store(OCCUPIED, Ordering::Release)`
publishes the storage and vtable writes. The poll loop observes
`OCCUPIED` with acquire ordering before reading them. That release/acquire pair
is what justifies:

```rust
unsafe impl Sync for Slot {}
```

The rejected design was "use `UnsafeCell` everywhere and trust core ownership".
That would not cover cross-core spawning. The state byte is the publication
edge that makes the slot safe to share.

`Slot::name` is a `SpinLock<&'static str>`, not an `UnsafeCell`, because the
`k` shell command reports tasks belonging to other cores. A `&str` is two words;
without the lock, a reader could observe a half-written pointer/length pair.

## The two wake masks

This is the important part of the design. It is what stops the idle loop from
being a busy spin.

| Mask | Meaning | Who sets it | Does it `sev`? |
| --- | --- | --- | --- |
| `ready` | Poll now. | `wake()` / `wake_slot()` | Yes. |
| `polling` | Poll after the next idle tick. | `poll_again()` / `mark_polling()` | No. |

`poll_again()` is for polled hardware, deadlines and "try again later" futures.
It inserts the current slot in `polling` and deliberately does not signal an
event. If it did `sev`, the executor would wake immediately, poll the same
future again, and recreate the busy spin async was meant to remove.

A real `Waker::wake`, by contrast, means some other state changed. It sets the
`ready` bit and executes `sev`, because sleeping would add avoidable latency.

The run loop is:

```text
loop
  drain ready and poll each set bit
  if ready became non-empty while polling:
      continue        # do not sleep on work already known to exist
  idle()              # wfe, or bounded fallback
  move polling -> ready
end
```

One edge case is accepted and documented: a task in `polling` can starve if some
other task on the same core is always `ready`. That is not a hidden bug. It is
the consequence of a cooperative executor with no preemption; code that wants
fairness must await, for example with `task::yield_now()` or `task::next_tick()`.

## How a core sleeps without an interrupt controller

`wfe` needs a guaranteed wake-up source. This kernel has two:

1. `sev` from a real wake-up on any core;
2. the generic timer event stream.

`arch::cpu::enable_event_stream()` configures `CNTKCTL_EL1`:

| Field | Value | Meaning |
| --- | --- | --- |
| `EVNTEN` bit 2 | 1 | Enable the stream. |
| `EVNTDIR` bit 3 | 0 | Fire on 0→1 transitions. |
| `EVNTI` bits 7:4 | 15 | Watch counter bit 15. |

Bit 15 gives one event per `2^(15 + 1) = 65536` counter ticks. That is about
1.05 ms under QEMU's 62.5 MHz counter, and about 3.41 ms on hardware's
19.2 MHz counter.

The rejected route was programming `CNTP_CVAL_EL0` and using `wfi`. That wake-up
depends on BCM2836 local interrupt routing, which cannot be validated in QEMU.
The event stream needs no interrupt controller at all. It is enabled from
`arch::init_core()`, so every core that reaches the executor has its own bounded
`wfe` source.

## No lost wake-ups

The check-then-sleep idiom is correct because `sev` sets every core's event
register.

Consider the race: the executor drains `ready`, checks that it is zero, and is
about to execute `wfe`. Another core wakes a task just before the instruction.
`wake_slot` sets the `ready` bit and executes `sev`. The sleeping core's event
register is now set. When it finally executes `wfe`, the instruction consumes
that event and returns immediately. It then loops and observes the ready work.

Without that event-register behaviour, the core could miss the wake and sleep
until the next timer event. With it, the check-then-sleep pattern loses no real
wake-up.

## `idle()` and the event-stream fallback

`Executor::idle` does not trust that `enable_event_stream()` worked. It reads
`CNTKCTL_EL1.EVNTEN` back through:

```rust
pub fn event_stream_enabled() -> bool
```

If there are no polling tasks, or if the event stream is enabled, `idle()` can
execute `cpu::wfe()`. If tasks are waiting in `polling` and the event stream is
not running, a plain `wfe` might sleep forever. The fallback is a bounded spin
on `time::ticks()` for `cpu::EVENT_STREAM_PERIOD_TICKS`, also watching for a
new `ready` bit.

That costs a little power on an implementation that ignores the `CNTKCTL_EL1`
write. It is still the right failure mode: polling tasks are delayed by one
period, not lost forever.

## Measured QEMU behaviour

QEMU's TCG implements `wfe` as a vCPU yield rather than a real sleep. It returns
almost immediately regardless of the event stream. The observed result is a
very high idle-round count: roughly 12 000-20 000 idle rounds per second per
core under QEMU.

On real hardware, `wfe` genuinely parks the core. The idle rate should then be
bounded by the event stream at roughly:

```text
19 200 000 / 65 536 ~= 293 Hz
```

The idle counts printed by `k` are therefore not a power measurement under
QEMU. This specific behaviour has not been verified on hardware.

## Synchronisation primitives

The async runtime adds waiting primitives above the low-level locks described
in [06-synchronisation.md](06-synchronisation.md).

| Situation | Use | Why |
| --- | --- | --- |
| A few instructions, no `.await` | `sync::SpinLock` | Small critical sections are cheaper than parking. |
| Held across an `.await` | `task::Mutex` | Waiters park instead of spinning. |
| Callable from an exception handler | `SpinLock` with `try_lock` | Exception context must not block on interrupted code. |
| Cross-core work hand-off | `task::Channel<T, N>` | Bounded queue plus parked senders and receivers. |

The console keeps its `SpinLock`. It is taken and released inside a single
`write_fmt`, and a fault handler must be able to reach it through `try_lock` or
the documented bypass path.

Both `Mutex::lock` and `Channel::send` / `Channel::recv` use the same
register-before-recheck idiom:

```text
try the operation
if it fails:
    insert current task in the wait set
    try the operation again
    if it now succeeds, remove the task from the wait set
    otherwise return Pending
```

The other order loses a wake-up. If a mutex holder releases the lock between a
failed `try_lock` and insertion into the wait set, it finds the set empty and
wakes nobody. The waiting task then returns `Pending` and can remain parked
forever even though the lock is free. Channels have the same race: a receiver
could make room, or a sender could add an item, in the gap before registration.

`Channel<T, N>` is:

```rust
pub struct Channel<T, const N: usize>
```

Internally it is a `SpinLock<Ring<T, N>>` plus two `TaskSet`s named `senders`
and `receivers`. The ring uses `[Option<T>; N]`, a head index and a length. The
queue state outside the elements costs 8 bytes regardless of `N`, because the
two wait sets are two `u32` masks.

The non-blocking operations are safe outside a task:

```rust
pub fn try_send(&self, value: T) -> Result<(), T>
pub fn try_recv(&self) -> Option<T>
```

`try_send` returns the value back on failure. `try_recv` returns `None` if the
ring is empty. Both wake the opposite wait set when they make progress.
Code that publishes state outside a specific waker can use:

```rust
pub fn notify_all()
```

It executes `sev`, waking every core parked in `wfe`.

## Timers

`Sleep` is poll-driven:

```rust
pub struct Sleep {
    deadline: u64,
}
```

It compares `time::ticks()` with an absolute deadline. If the deadline has not
expired, it calls `poll_again()` and returns `Poll::Pending`. A sleep can
therefore overrun by up to one event-stream period: about 1 ms in QEMU and
about 3.4 ms on hardware. These timers pace work; they do not meet hard
deadlines.

The public constructors are:

```rust
pub fn sleep(duration: Duration) -> Sleep
pub fn sleep_millis(millis: u64) -> Sleep
pub fn sleep_micros(micros: u64) -> Sleep
```

`timeout()` races a future against the same counter:

```rust
pub fn timeout<F: Future>(duration: Duration, future: F) -> Timeout<F>
```

`Timeout::poll` deliberately calls `poll_again()` even when the inner future
registered a real waker. Otherwise the timeout could never fire if the inner
future stayed parked forever. The inner future is dropped on timeout; that is
the only cancellation mechanism today.

When an IRQ controller arrives, the executor design does not need to change.
Only `timer.rs` should need to move from poll-driven deadlines to
interrupt-driven wake-ups.

## Writing async code

Spawn with a static label:

```rust
pub fn spawn<F>(name: &'static str, future: F) -> Result<TaskId, SpawnError>
where
    F: Future<Output = ()> + Send + 'static

pub fn spawn_on<F>(core: usize, name: &'static str, future: F) -> Result<TaskId, SpawnError>
where
    F: Future<Output = ()> + Send + 'static
```

The label is not optional. It is what the `k` command prints.

### A polled register future

`console::NextByte` is the pattern for wrapping hardware that has no interrupt
yet:

```rust
impl Future for NextByte {
    type Output = u8;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<u8> {
        if let Some(byte) = read_byte() {
            return Poll::Ready(byte);
        }

        task::poll_again();
        Poll::Pending
    }
}
```

The order matters: check the register, return `Ready` if possible, otherwise
ask for one more idle tick and return `Pending`.

### Cross-core work

`cogware-bcm2837\examples\shell\main.rs` has the worked example:

```rust
#[derive(Clone, Copy, Debug)]
struct Job {
    seq: u32,
}

static JOBS: task::Channel<Job, 16> = task::Channel::new();

struct Results {
    completed: u32,
    total: f64,
    by_core: [u32; memory::CORE_COUNT],
}

static RESULTS: task::Mutex<Results> = task::Mutex::new(Results {
    completed: 0,
    total: 0.0,
    by_core: [0; memory::CORE_COUNT],
});
```

Each secondary core runs:

```rust
async fn worker(core_id: usize) {
    loop {
        let job = JOBS.recv().await;
        let term = 1.0 / f64::from(job.seq + 1);

        let completed = {
            let mut results = RESULTS.lock().await;
            results.completed += 1;
            results.total += term;
            results.by_core[core_id] += 1;
            results.completed
        };

        cprintln!("job {:>2} -> {term:.8} (completed {completed})", job.seq);
    }
}
```

Core 0's `dispatch_jobs()` sends 24 jobs through the same channel and waits
with `task::next_tick().await` until the shared results reach the target.

### Rules

1. Never block inside a task: no `time::spin_for`, and no long `SpinLock`
   contention.
2. Never hold a `SpinLock` guard across an `.await`.
3. Keep futures under 1 KiB, or `spawn` returns `SpawnError::TooLarge`.
4. Do not require alignment stricter than 16 bytes, or `spawn` returns
   `SpawnError::Overaligned`.
5. Every task needs a `&'static str` name because that is what `k` prints.

## Boot integration

The primary boot path ends by spawning the interactive work and handing core 0
to its executor:

```rust
spawn_or_report("shell", shell());
spawn_or_report("heartbeat", heartbeat());
task::run()
```

Each secondary ends by spawning one worker on itself and then running its own
executor:

```rust
spawn_or_report("worker", worker(core_id));
task::run()
```

That replaces the old `cpu::halt()` end state. An idle secondary still sleeps
in `wfe`, but now a `sev` from a channel send can put it back to work.

The ordering constraint comes from rule 5 in
[`instructions.md`](../instructions.md): `LDXR`/`STXR` are UNPREDICTABLE on
Device memory. Nothing in `task` may be touched before the calling core has
enabled its MMU. This is automatic in the current boot order because
executors start only after `arch::init_core()` has enabled the MMU and event
stream on that core.

## Shell diagnostics

The `k` command prints every occupied slot, then per-core executor counters,
then the heartbeat count and job queue depth. This is a captured QEMU sample:

```text
Async tasks
  +------+------+----------------------+------------+---------+
  | core | slot | task                 |      polls |  state  |
  +------+------+----------------------+------------+---------+
  |  0   |  0   | shell                |      64248 | parked  |
  |  0   |  1   | heartbeat            |      64440 | parked  |
  |  1   |  0   | worker               |          3 | parked  |
  |  2   |  0   | worker               |          2 | parked  |
  |  3   |  0   | worker               |          3 | parked  |
  +------+------+----------------------+------------+---------+
  core 0: 2 task(s), 64440 idle wakeups
  core 1: 1 task(s), 101700 idle wakeups
  core 2: 1 task(s), 99986 idle wakeups
  core 3: 1 task(s), 102517 idle wakeups
  heartbeats: 20 (250 ms each)
  queue     : 0/16 job(s)
```

The `w` command dispatches jobs to the workers. A faithful abridged excerpt:

```text
Dispatching 24 jobs to 3 worker cores...
[core 2] job  2 -> 0.33333333 (completed 1)
[core 3] job  0 -> 1.00000000 (completed 2)
[core 1] job  1 -> 0.50000000 (completed 3)
...
  completed : 24/24
  harmonic  : 3.7759581777535067
  core 1    : 8 jobs
  core 2    : 8 jobs
  core 3    : 8 jobs
```

That proves several things at once: 24 jobs distributed evenly, 8/8/8, across
three cores through one lock-free-to-the-caller channel; each result
accumulated under the async mutex; and floating-point arithmetic ran on every
worker core under the hard-float target.

## Limitations and next steps

| Limitation | Consequence |
| --- | --- |
| No preemption | A task that loops forever wedges its core. |
| No work stealing | A task stays on the core that spawned it. |
| No general cancellation | `timeout` cancels only by dropping the inner future. |
| No interrupt-driven wake-ups | Hardware waits use `poll_again()` and event ticks. |
| Coarse sleeps | Deadlines overrun by up to one event-stream period. |
| 1 KiB per future | Large async state machines fail to spawn. |
| 8 tasks per core | `SpawnError::TableFull` is a normal capacity error. |

The natural next step is the interrupt work described in
[`10-roadmap.md`](10-roadmap.md): local timer interrupts for deadlines and
mailbox/IPI interrupts for directed cross-core wake-ups. Until then, the event
stream is the deliberate bridge between busy polling and a real IRQ-driven
runtime.
