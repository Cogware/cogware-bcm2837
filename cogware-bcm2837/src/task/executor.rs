// SPDX-License-Identifier: MIT OR Apache-2.0
//! The per-core executor and its task table.
//!
//! See the [module documentation](super) for the design rationale.

use core::{
    cell::UnsafeCell,
    fmt,
    future::Future,
    mem::{MaybeUninit, align_of, size_of},
    pin::Pin,
    ptr,
    sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering},
    task::{Context, Poll, Waker},
};

use super::{TASK_STORAGE_BYTES, TASKS_PER_CORE, TaskId, waker};
use crate::{arch::cpu, bsp::memory, sync::SpinLock, time};

/// Alignment of a task's inline storage.
///
/// 16 bytes: a future may hold a `u128` or a NEON vector.
const STORAGE_ALIGN: usize = 16;

/// Slot is empty and may be claimed.
const FREE: u8 = 0;
/// Slot is being filled in by [`spawn_on`]; nobody else may touch it.
const SPAWNING: u8 = 1;
/// Slot holds a live future.
const OCCUPIED: u8 = 2;

/// Sentinel for "this core is not inside a poll".
const NO_TASK: u8 = u8::MAX;

/// Type-erased operations on a task's future.
///
/// The hand-rolled half of `Box<dyn Future>`: the data lives in the slot's
/// inline storage, the behaviour here.
struct VTable {
    /// Polls the future in place.
    ///
    /// # Safety
    ///
    /// `ptr` must point at a live, initialised value of the type this vtable
    /// was built for, which has not moved since it was written.
    poll: unsafe fn(*mut u8, &mut Context<'_>) -> Poll<()>,

    /// Drops the future in place.
    ///
    /// # Safety
    ///
    /// As for [`poll`](Self::poll), and the value must not be used afterwards.
    drop_in_place: unsafe fn(*mut u8),
}

/// Type-erased operations on a task's future, one instance per future type.
///
/// Implementing a private trait for every `Future` yields a `&'static VTable`
/// specialised for a concrete type without an allocator: an associated `const`
/// of reference type is promoted to `'static`.
trait Glue {
    /// The vtable for this future type.
    const VTABLE: &'static VTable;
}

impl<F: Future<Output = ()>> Glue for F {
    const VTABLE: &'static VTable = &VTable {
        poll: poll_glue::<F>,
        drop_in_place: drop_glue::<F>,
    };
}

/// # Safety
///
/// See [`VTable::poll`].
unsafe fn poll_glue<F: Future<Output = ()>>(ptr: *mut u8, cx: &mut Context<'_>) -> Poll<()> {
    // SAFETY: the caller guarantees `ptr` addresses a live `F`; the slot state
    // machine guarantees no other reference to it exists.
    let future = unsafe { &mut *ptr.cast::<F>() };

    // SAFETY: the future was written into the slot's storage and is never
    // moved again — the slot only transitions to FREE after the future has
    // been dropped in place — so it is effectively pinned.
    let future = unsafe { Pin::new_unchecked(future) };

    future.poll(cx)
}

/// # Safety
///
/// See [`VTable::drop_in_place`].
unsafe fn drop_glue<F: Future<Output = ()>>(ptr: *mut u8) {
    // SAFETY: the caller guarantees `ptr` addresses a live `F` that is about
    // to be discarded.
    unsafe { ptr::drop_in_place(ptr.cast::<F>()) };
}

/// Inline, over-aligned backing store for one future.
#[repr(align(16))]
struct Storage([MaybeUninit<u8>; TASK_STORAGE_BYTES]);

/// One task slot.
struct Slot {
    /// [`FREE`], [`SPAWNING`] or [`OCCUPIED`]. Guards everything else.
    state: AtomicU8,
    /// The future itself, type-erased.
    storage: UnsafeCell<Storage>,
    /// How to poll and drop [`storage`](Self::storage).
    vtable: UnsafeCell<Option<&'static VTable>>,
    /// Human-readable label, for diagnostics.
    ///
    /// Behind a lock rather than an `UnsafeCell`: the shell reports tasks
    /// belonging to *other* cores, and a `&str` is two words that must not be
    /// read while half-written.
    name: SpinLock<&'static str>,
    /// Number of times this slot's future has been polled.
    polls: AtomicU64,
}

// SAFETY: `storage` and `vtable` are only accessed by the core that owns the
// slot, and only while `state` is SPAWNING (exclusive to the spawner, which
// won the compare-exchange) or OCCUPIED (exclusive to the owning core's poll
// loop). The `Release`/`Acquire` pair on `state` publishes the writes.
unsafe impl Sync for Slot {}

impl Slot {
    /// Creates an empty slot.
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(FREE),
            storage: UnsafeCell::new(Storage([MaybeUninit::uninit(); TASK_STORAGE_BYTES])),
            vtable: UnsafeCell::new(None),
            name: SpinLock::new(""),
            polls: AtomicU64::new(0),
        }
    }
}

/// A snapshot of one occupied task slot, for diagnostics.
#[derive(Clone, Copy, Debug)]
pub struct SlotInfo {
    /// Slot index within its core's table.
    pub slot: usize,
    /// The label passed to [`spawn`].
    pub name: &'static str,
    /// How many times the task has been polled.
    pub polls: u64,
    /// Whether the task will be polled on the next pass.
    pub ready: bool,
    /// Whether the task is waiting for the next idle tick.
    pub polling: bool,
}

/// A single core's task table and run loop.
pub struct Executor {
    /// Index of the core this executor belongs to.
    core: usize,
    /// The task table.
    slots: [Slot; TASKS_PER_CORE],
    /// Bit per slot: poll me now.
    ready: AtomicU32,
    /// Bit per slot: poll me after the next idle tick.
    ///
    /// Stops a hardware-polling future from turning the run loop into a busy
    /// spin. See the [module documentation](super).
    polling: AtomicU32,
    /// Global index of the task being polled, or [`NO_TASK`].
    current: AtomicU8,
    /// Number of idle rounds — trips through [`idle`](Self::idle).
    idles: AtomicU64,
}

impl Executor {
    /// Creates an empty executor for `core`.
    const fn new(core: usize) -> Self {
        Self {
            core,
            slots: [const { Slot::new() }; TASKS_PER_CORE],
            ready: AtomicU32::new(0),
            polling: AtomicU32::new(0),
            current: AtomicU8::new(NO_TASK),
            idles: AtomicU64::new(0),
        }
    }

    /// The task this core is currently polling, if any.
    pub fn current(&self) -> Option<TaskId> {
        TaskId::from_index(self.current.load(Ordering::Relaxed) as usize)
    }

    /// Bit per slot: slots holding a live future.
    pub fn occupied_mask(&self) -> u32 {
        let mut mask = 0;
        for (slot, entry) in self.slots.iter().enumerate() {
            if entry.state.load(Ordering::Acquire) == OCCUPIED {
                mask |= 1 << slot;
            }
        }
        mask
    }

    /// Number of live tasks.
    pub fn task_count(&self) -> u32 {
        self.occupied_mask().count_ones()
    }

    /// Bit per slot: tasks that will be polled on the next pass.
    pub fn ready_mask(&self) -> u32 {
        self.ready.load(Ordering::Relaxed)
    }

    /// Bit per slot: tasks waiting for the next idle tick.
    pub fn polling_mask(&self) -> u32 {
        self.polling.load(Ordering::Relaxed)
    }

    /// How many idle rounds this core's executor has been through.
    pub fn idle_count(&self) -> u64 {
        self.idles.load(Ordering::Relaxed)
    }

    /// The label and poll count of a slot, if it holds a task.
    pub fn slot_info(&self, slot: usize) -> Option<SlotInfo> {
        let entry = self.slots.get(slot)?;
        if entry.state.load(Ordering::Acquire) != OCCUPIED {
            return None;
        }

        Some(SlotInfo {
            slot,
            name: *entry.name.lock(),
            polls: entry.polls.load(Ordering::Relaxed),
            ready: self.ready.load(Ordering::Relaxed) & (1 << slot) != 0,
            polling: self.polling.load(Ordering::Relaxed) & (1 << slot) != 0,
        })
    }

    /// Asks for `slot` to be polled again after the next idle tick.
    ///
    /// The back end of [`poll_again`](super::poll_again). Unlike
    /// [`wake_slot`](Self::wake_slot) it does *not* `sev`: the core is meant
    /// to sleep until the timer's event stream wakes it.
    pub(super) fn mark_polling(&self, slot: usize) {
        self.polling.fetch_or(1 << slot, Ordering::Release);
    }

    /// Marks a slot runnable and kicks the core out of `wfe`.
    fn wake_slot(&self, slot: usize) {
        self.ready.fetch_or(1 << slot, Ordering::Release);

        // `sev` sets every core's event register, so a core about to execute
        // `wfe` finds the register already set and returns immediately, which
        // makes the check-then-sleep in `run` free of lost wake-ups.
        cpu::sev();
    }

    /// Polls one slot once.
    fn poll_slot(&self, slot: usize) {
        let entry = &self.slots[slot];

        if entry.state.load(Ordering::Acquire) != OCCUPIED {
            // The slot was freed after its bit was set: a stale waker, which
            // the `Future` contract permits.
            return;
        }

        // SAFETY: state is OCCUPIED and this is the owning core, so the poll
        // loop has exclusive access to the slot's contents.
        let Some(vtable) = (unsafe { *entry.vtable.get() }) else {
            return;
        };
        let storage = entry.storage.get().cast::<u8>();

        let id = TaskId::new(self.core, slot);

        // SAFETY: `raw_waker` builds a waker whose data pointer is just the
        // task index and whose vtable has no ownership semantics, so it is
        // safe to construct, clone, send and drop.
        let waker = unsafe { Waker::from_raw(waker::raw_waker(id)) };
        let mut cx = Context::from_waker(&waker);

        // Published for `task::current()`, which is how a future discovers
        // which wait queue to put itself in. Only this core touches it, so
        // `Relaxed` suffices.
        self.current.store(id.index() as u8, Ordering::Relaxed);

        // SAFETY: `storage` addresses the live future this vtable was built
        // for; the state machine guarantees no aliasing reference exists.
        let result = unsafe { (vtable.poll)(storage, &mut cx) };

        self.current.store(NO_TASK, Ordering::Relaxed);
        entry.polls.fetch_add(1, Ordering::Relaxed);

        if result.is_ready() {
            // SAFETY: the future has completed and will never be polled again.
            unsafe { (vtable.drop_in_place)(storage) };

            // SAFETY: still OCCUPIED, so this core has exclusive access.
            unsafe { *entry.vtable.get() = None };

            *entry.name.lock() = "";

            // Release, so a later spawner that claims this slot sees the
            // storage as fully torn down.
            entry.state.store(FREE, Ordering::Release);
        }
    }

    /// Polls every task that is ready right now.
    fn poll_ready(&self) {
        let mut bits = self.ready.swap(0, Ordering::AcqRel);

        while bits != 0 {
            let slot = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            self.poll_slot(slot);
        }
    }

    /// Sleeps until there is something to do, or until the next tick.
    ///
    /// `wfe` is only safe when something is guaranteed to wake the core. Two
    /// sources qualify:
    ///
    /// * `sev` from [`wake_slot`](Self::wake_slot) on any core, and
    /// * the generic timer's event stream, which gives the
    ///   [`poll_again`](super::poll_again) tasks their tick.
    ///
    /// Without the event stream a core with nothing but polling tasks would
    /// sleep forever, so the enable bit written by
    /// [`enable_event_stream`](cpu::enable_event_stream) is read back and a
    /// bounded spin is used instead when it did not take effect.
    fn idle(&self) {
        self.idles.fetch_add(1, Ordering::Relaxed);

        if self.polling.load(Ordering::Relaxed) == 0 || cpu::event_stream_enabled() {
            cpu::wfe();
            return;
        }

        let deadline = time::ticks().saturating_add(cpu::EVENT_STREAM_PERIOD_TICKS);
        while time::ticks() < deadline && self.ready.load(Ordering::Acquire) == 0 {
            core::hint::spin_loop();
        }
    }

    /// Runs this core's tasks forever.
    fn run_loop(&self) -> ! {
        loop {
            self.poll_ready();

            if self.ready.load(Ordering::Acquire) != 0 {
                // A wake landed while polling; go round again rather than
                // sleeping on work we already have.
                continue;
            }

            self.idle();

            // Give the pollers their turn.
            let pending = self.polling.swap(0, Ordering::AcqRel);
            if pending != 0 {
                self.ready.fetch_or(pending, Ordering::Release);
            }
        }
    }
}

// One executor per core. The `Executor::new` argument must match the index.
const _: () = assert!(
    memory::CORE_COUNT == 4,
    "the EXECUTORS array below is written out for exactly four cores"
);

/// The per-core executors.
static EXECUTORS: [Executor; memory::CORE_COUNT] = [
    Executor::new(0),
    Executor::new(1),
    Executor::new(2),
    Executor::new(3),
];

/// The executor belonging to the calling core.
pub fn executor() -> &'static Executor {
    &EXECUTORS[cpu::core_id()]
}

/// The executor belonging to `core`, for diagnostics and cross-core spawning.
///
/// Returns `None` if `core` is out of range.
pub fn executor_for(core: usize) -> Option<&'static Executor> {
    EXECUTORS.get(core)
}

/// Marks `task` runnable and wakes the core that owns it.
pub(super) fn wake(task: TaskId) {
    EXECUTORS[task.core()].wake_slot(task.slot());
}

/// Why a task could not be spawned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpawnError {
    /// Every slot on the target core is occupied.
    TableFull,
    /// The core index does not exist.
    NoSuchCore,
    /// The future is bigger than [`TASK_STORAGE_BYTES`].
    TooLarge {
        /// Size of the future, in bytes.
        needed: usize,
    },
    /// The future needs stricter alignment than the slot storage provides.
    Overaligned {
        /// Alignment the future requires, in bytes.
        needed: usize,
    },
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TableFull => write!(f, "task table full ({TASKS_PER_CORE} slots per core)"),
            Self::NoSuchCore => write!(f, "no such core"),
            Self::TooLarge { needed } => write!(
                f,
                "future is {needed} bytes, storage is {TASK_STORAGE_BYTES}"
            ),
            Self::Overaligned { needed } => {
                write!(
                    f,
                    "future needs {needed}-byte alignment, storage gives {STORAGE_ALIGN}"
                )
            }
        }
    }
}

/// Spawns a task on the calling core.
///
/// The future is moved into a static slot and polled until it completes, at
/// which point the slot is freed for reuse.
///
/// # Errors
///
/// See [`SpawnError`]. Nothing is consumed on failure except the future.
pub fn spawn<F>(name: &'static str, future: F) -> Result<TaskId, SpawnError>
where
    F: Future<Output = ()> + Send + 'static,
{
    spawn_on(cpu::core_id(), name, future)
}

/// Spawns a task on a specific core.
///
/// May be called from any core, including before the target core's executor
/// has started, which is how the boot path pre-loads work.
///
/// # Errors
///
/// See [`SpawnError`].
pub fn spawn_on<F>(core: usize, name: &'static str, future: F) -> Result<TaskId, SpawnError>
where
    F: Future<Output = ()> + Send + 'static,
{
    if size_of::<F>() > TASK_STORAGE_BYTES {
        return Err(SpawnError::TooLarge {
            needed: size_of::<F>(),
        });
    }

    if align_of::<F>() > STORAGE_ALIGN {
        return Err(SpawnError::Overaligned {
            needed: align_of::<F>(),
        });
    }

    let executor = EXECUTORS.get(core).ok_or(SpawnError::NoSuchCore)?;

    for (slot, entry) in executor.slots.iter().enumerate() {
        if entry
            .state
            .compare_exchange(FREE, SPAWNING, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            continue;
        }

        // The compare-exchange won, so this thread of control has exclusive
        // access to the slot until `state` is stored again.

        // SAFETY: exclusive by the state machine; the storage is uninitialised
        // and correctly sized and aligned, both checked above.
        unsafe { entry.storage.get().cast::<F>().write(future) };

        // SAFETY: as above.
        unsafe { *entry.vtable.get() = Some(<F as Glue>::VTABLE) };

        *entry.name.lock() = name;

        // Release: publishes the storage and vtable writes to the core that
        // will poll them.
        entry.state.store(OCCUPIED, Ordering::Release);

        let id = TaskId::new(core, slot);
        executor.wake_slot(slot);
        return Ok(id);
    }

    Err(SpawnError::TableFull)
}

/// Hands this core over to its executor. Never returns.
///
/// Every core ends its bring-up here. With no tasks spawned the core sleeps in
/// `wfe`.
pub fn run() -> ! {
    executor().run_loop()
}
