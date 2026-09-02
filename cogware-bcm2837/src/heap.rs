//! Kernel heap: a segregated power-of-two free-list allocator over free DRAM.
//!
//! # Shape
//!
//! Initialisation must be O(1), which rules out a `.bss`/`NOLOAD` arena (a
//! memset proportional to heap size) and a free-list built up at init (a walk
//! of the whole arena).
//!
//! The arena is instead the free DRAM above the image, and [`init`] only
//! records two addresses: nothing is written, zeroed or walked. The region is
//! already mapped Normal / read-write / execute-never by
//! [`arch::mmu::memory_map`](crate::arch::mmu::memory_map) (the "free DRAM
//! (ARM)" entry), so there is no page-table work either.
//!
//! # The allocator
//!
//! Every request is rounded up to a power of two between [`MIN_SIZE`] and
//! [`MAX_SIZE`]; that rounded size is its *size class*. Each class has a LIFO
//! free list, and memory that has never been handed out is taken from a bump
//! pointer that only moves forward.
//!
//! ```text
//! alloc:  pop the class's free list, else bump          -> O(1), no search
//! dealloc: push onto the class's free list              -> O(1), no search
//! ```
//!
//! # No per-allocation header
//!
//! Blocks carry no metadata. [`GlobalAlloc::dealloc`] is handed the same
//! [`Layout`] that [`GlobalAlloc::alloc`] received, so the size class is
//! recomputed on free rather than stored. Overhead is zero bytes per block, and
//! a free block's body holds the free list's `next` pointer.
//!
//! # Alignment
//!
//! A class-`n` block is `n`-byte aligned as well as `n` bytes long, because the
//! bump pointer is aligned to the class size before carving. The class is
//! `max(size, align)` rounded up, so the caller's alignment is satisfied by
//! construction. Padding skipped to reach that alignment is not leaked:
//! `Inner::release_range` splits it into power-of-two blocks and pushes them
//! onto the appropriate free lists.
//!
//! # Trade-offs
//!
//! * **Up to 2x internal fragmentation.** A 17-byte allocation occupies 32
//!   bytes.
//! * **No coalescing.** Two adjacent free 16-byte blocks never become a 32-byte
//!   block, so a workload that frees many small blocks and then asks for one
//!   large one can fail with memory nominally free.
//! * **No `realloc` in place except within a class.** Growing a `Vec` from 40
//!   to 48 bytes is free (both are class 64); crossing a class boundary copies.
//!
//! # Concurrency
//!
//! All four cores share one [`SpinLock`]. The critical section is a pointer
//! load and a store; per-core magazines, if ever needed, belong in
//! `Inner::take` and `Inner::give`.
//!
//! # Ordering constraints
//!
//! [`SpinLock`] uses `LDXR`/`STXR`, which are UNPREDICTABLE while the MMU is
//! off (rule 5 in `instructions.md`). The heap must not be touched before
//! [`arch::init_core`](crate::arch::init_core) has enabled the MMU on the
//! calling core. This is enforced: `READY` is a plain [`AtomicBool`] (`LDARB`,
//! which *is* defined on Device memory), checked before the lock is taken, so
//! an allocation attempted before [`init`] returns null rather than executing
//! an exclusive. The null return fails `alloc`, which panics through
//! `handle_alloc_error`.

use core::{
    alloc::{GlobalAlloc, Layout},
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{
    bsp::memory::{self, layout},
    sync::SpinLock,
};

/// Base-2 logarithm of the smallest block the heap will hand out.
///
/// 16 bytes, not 8: a block body must hold a `next` pointer, and 16-byte
/// alignment satisfies the AAPCS64 stack, the NEON `q` registers and every
/// primitive type.
const MIN_SHIFT: usize = 4;

/// Base-2 logarithm of the largest block the heap will hand out.
///
/// 1 GiB exceeds the whole ARM-visible DRAM of a Raspberry Pi 3, so no
/// satisfiable request is rejected for being too large; the cap only bounds the
/// size of [`Inner::heads`].
const MAX_SHIFT: usize = 30;

/// Smallest block the heap will hand out, in bytes.
pub const MIN_SIZE: usize = 1 << MIN_SHIFT;

/// Largest block the heap will hand out, in bytes.
pub const MAX_SIZE: usize = 1 << MAX_SHIFT;

/// Number of size classes.
pub const CLASS_COUNT: usize = MAX_SHIFT - MIN_SHIFT + 1;

/// Set once [`init`] has published the region bounds.
///
/// A plain [`AtomicBool`]: it is read on the allocation path *before* the
/// [`SpinLock`] is taken, which keeps a pre-MMU allocation from executing an
/// exclusive. See the module docs.
static READY: AtomicBool = AtomicBool::new(false);

/// A block sitting on a free list.
///
/// Stored in the block's own body, so free blocks cost no extra memory.
struct Free {
    next: *mut Free,
}

/// Rounds `value` up to the next multiple of `align`, which must be a power of
/// two.
#[inline(always)]
const fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

/// Returns the size class that satisfies `layout`, or `None` if the request is
/// larger than [`MAX_SIZE`].
///
/// The class must satisfy both size and alignment, hence the `max`: a 16-byte
/// allocation wanting 64-byte alignment becomes a class-64 block, which the
/// bump allocator guarantees is 64-byte aligned.
#[inline(always)]
fn class_of(layout: Layout) -> Option<usize> {
    let need = layout.size().max(layout.align()).max(MIN_SIZE);
    if need > MAX_SIZE {
        return None;
    }

    // `need >= MIN_SIZE >= 2`, so `need - 1` cannot underflow and the shift is
    // `ceil(log2(need))`.
    let shift = (usize::BITS - (need - 1).leading_zeros()) as usize;
    Some(shift - MIN_SHIFT)
}

/// Size in bytes of blocks in `class`.
#[inline(always)]
const fn class_size(class: usize) -> usize {
    1 << (class + MIN_SHIFT)
}

/// A snapshot of heap usage, as reported by [`stats`].
#[derive(Clone, Copy, Default)]
pub struct Stats {
    /// First address of the arena.
    pub start: usize,
    /// One past the last address of the arena.
    pub end: usize,
    /// One past the highest address the bump pointer has ever reached. Memory
    /// below this has been touched; memory above it is untouched DRAM.
    pub watermark: usize,
    /// Bytes currently handed out, counted in whole blocks.
    pub live: usize,
    /// Largest [`Stats::live`] ever observed.
    pub peak: usize,
    /// Successful calls to `alloc`.
    pub allocs: u64,
    /// Calls to `dealloc`.
    pub deallocs: u64,
    /// Allocations served from a free list rather than the bump pointer.
    pub reused: u64,
    /// Allocations that failed because the arena was exhausted or the request
    /// exceeded [`MAX_SIZE`].
    pub failed: u64,
    /// Reallocations satisfied without copying, because the old and new sizes
    /// share a size class.
    pub grown_in_place: u64,
    /// Number of blocks on each class's free list.
    pub free_counts: [u32; CLASS_COUNT],
}

/// The allocator state, guarded by `HEAP`.
struct Inner {
    /// Next never-yet-allocated address.
    next: usize,
    /// One past the last usable address.
    limit: usize,
    /// LIFO free list head per size class.
    heads: [*mut Free; CLASS_COUNT],
    /// Length of each free list, so [`stats`] need not walk the lists.
    free_counts: [u32; CLASS_COUNT],
    start: usize,
    live: usize,
    peak: usize,
    allocs: u64,
    deallocs: u64,
    reused: u64,
    failed: u64,
    grown_in_place: u64,
}

// SAFETY: `Inner` holds raw pointers into the heap arena, plain DRAM shared by
// every core. They are only dereferenced while `HEAP` is held, so the lock
// provides the exclusion the raw pointers do not.
unsafe impl Send for Inner {}

impl Inner {
    const fn new() -> Self {
        Self {
            next: 0,
            limit: 0,
            heads: [ptr::null_mut(); CLASS_COUNT],
            free_counts: [0; CLASS_COUNT],
            start: 0,
            live: 0,
            peak: 0,
            allocs: 0,
            deallocs: 0,
            reused: 0,
            failed: 0,
            grown_in_place: 0,
        }
    }

    /// Pushes a block onto `class`'s free list.
    ///
    /// # Safety
    ///
    /// `addr` must be the base of an unused block of at least
    /// `class_size(class)` bytes that lies inside the arena and is aligned to
    /// its class size.
    #[inline(always)]
    unsafe fn give(&mut self, class: usize, addr: usize) {
        let block = addr as *mut Free;
        // SAFETY: the caller guarantees `addr` is an unused block of at least
        // `MIN_SIZE` bytes, which is large enough for a `Free`.
        unsafe { (*block).next = self.heads[class] };
        self.heads[class] = block;
        self.free_counts[class] += 1;
    }

    /// Pops a block from `class`'s free list, or null if it is empty.
    #[inline(always)]
    fn take(&mut self, class: usize) -> *mut u8 {
        let head = self.heads[class];
        if head.is_null() {
            return ptr::null_mut();
        }

        // SAFETY: a non-null head is a block this allocator handed out and had
        // returned to it, so its body holds a valid `next`.
        self.heads[class] = unsafe { (*head).next };
        self.free_counts[class] -= 1;
        self.reused += 1;
        head.cast()
    }

    /// Donates `[lo, hi)` to the free lists, splitting it into the largest
    /// correctly aligned power-of-two blocks that fit.
    ///
    /// Stops the padding skipped by [`Inner::carve`] from being leaked. The
    /// loop runs at most once per size class: each step either doubles the
    /// alignment of `lo` or halves the remaining span.
    fn release_range(&mut self, mut lo: usize, hi: usize) {
        while hi - lo >= MIN_SIZE {
            let span = hi - lo;

            // The block must be aligned to its own size, so it is bounded by
            // the alignment `lo` already has...
            let tz = lo.trailing_zeros() as usize;
            let by_align = if tz >= MAX_SHIFT { MAX_SIZE } else { 1 << tz };

            // ...and by the largest power of two that fits in what is left.
            let by_span = 1 << (usize::BITS - 1 - span.leading_zeros());

            let size = by_align.min(by_span).min(MAX_SIZE);

            // SAFETY: `size` is a power of two in `[MIN_SIZE, MAX_SIZE]`, `lo`
            // is aligned to it by construction, and `lo + size <= hi` lies
            // inside the arena because the caller's range does.
            unsafe { self.give(size.trailing_zeros() as usize - MIN_SHIFT, lo) };
            lo += size;
        }
    }

    /// Takes a fresh block of `class` from the bump pointer.
    fn carve(&mut self, class: usize) -> *mut u8 {
        let size = class_size(class);
        let base = align_up(self.next, size);

        // `align_up` can wrap only for absurd sizes; check anyway so a
        // corrupted `next` cannot become a wild pointer.
        if base < self.next || base > self.limit || size > self.limit - base {
            return ptr::null_mut();
        }

        if base > self.next {
            self.release_range(self.next, base);
        }

        self.next = base + size;
        base as *mut u8
    }
}

/// The one heap.
static HEAP: SpinLock<Inner> = SpinLock::new(Inner::new());

/// The `#[global_allocator]` shim.
///
/// A unit struct: all state lives in `HEAP`, and [`GlobalAlloc`] only provides
/// a shared `&self`.
///
/// The type is always available so a downstream crate that turns the
/// `allocator` feature off can install or wrap it under its own
/// `#[global_allocator]`. Only the attribute below is gated, since a binary may
/// contain exactly one.
pub struct Kernel;

#[cfg(feature = "allocator")]
#[global_allocator]
static ALLOCATOR: Kernel = Kernel;

// SAFETY: `alloc` returns either null or a pointer to a block of at least
// `layout.size()` bytes, aligned to at least `layout.align()`, that no other
// live allocation overlaps. `dealloc` is only called with a pointer/layout pair
// that `alloc` returned, and returns the block to the class its layout selects,
// which is the class it was taken from.
unsafe impl GlobalAlloc for Kernel {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded from the caller, who guarantees the pair came from
        // `alloc`.
        unsafe { dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Growing or shrinking within one size class is a no-op: the block is
        // already big enough and correctly aligned.
        let new_layout = match Layout::from_size_align(new_size, layout.align()) {
            Ok(new_layout) => new_layout,
            Err(_) => return ptr::null_mut(),
        };

        if class_of(layout) == class_of(new_layout) && class_of(layout).is_some() {
            HEAP.with(|heap| heap.grown_in_place += 1);
            return ptr;
        }

        let new_ptr = alloc(new_layout);
        if !new_ptr.is_null() {
            // SAFETY: both blocks are live and at least `copied` bytes long,
            // and they cannot overlap because `new_ptr` is a fresh block.
            unsafe {
                ptr::copy_nonoverlapping(ptr, new_ptr, layout.size().min(new_size));
                dealloc(ptr, layout);
            }
        }
        new_ptr
    }
}

/// Publishes the heap region and opens the allocator for business.
///
/// The arena is every byte of ARM-visible DRAM above the kernel image and below
/// the VideoCore's reservation. Both bounds come from the same sources as the
/// memory map -- [`layout::kernel_end`] and [`memory::vc_mem_base`] -- rather
/// than literals, so the heap cannot drift out of step with the region the MMU
/// maps.
///
/// Costs two stores. No memory is read, written or zeroed.
///
/// # Safety
///
/// Call once, on the primary core, after `mmu::enable` (the heap takes a
/// [`SpinLock`], which needs the MMU) and after
/// [`drivers::mailbox::probe_memory`](crate::drivers::mailbox::probe_memory)
/// (which is what moves [`memory::vc_mem_base`] off its default), and before
/// any secondary core is released.
pub unsafe fn init() {
    let start = align_up(layout::kernel_end(), MIN_SIZE);
    let end = memory::vc_mem_base();

    assert!(
        end > start,
        "heap: VideoCore base {end:#x} is not above the kernel image end {start:#x}"
    );

    HEAP.with(|heap| {
        heap.start = start;
        heap.next = start;
        heap.limit = end;
    });

    // Release: the bounds above must be visible to any core that observes
    // `READY`, the only thing an allocating core checks.
    READY.store(true, Ordering::Release);
}

/// Whether [`init`] has run.
pub fn is_ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Allocates a block satisfying `layout`, or null on failure.
///
/// Prefer `Box`/`Vec` and friends; this is for callers that need to place
/// something at a raw address.
pub fn alloc(layout: Layout) -> *mut u8 {
    // Checked before the lock: taking a `SpinLock` with the MMU off would
    // execute an exclusive on Device memory, which is UNPREDICTABLE.
    if !is_ready() {
        return ptr::null_mut();
    }

    let Some(class) = class_of(layout) else {
        HEAP.with(|heap| heap.failed += 1);
        return ptr::null_mut();
    };

    HEAP.with(|heap| {
        let mut block = heap.take(class);
        if block.is_null() {
            block = heap.carve(class);
        }

        if block.is_null() {
            heap.failed += 1;
            return block;
        }

        heap.live += class_size(class);
        heap.peak = heap.peak.max(heap.live);
        heap.allocs += 1;
        block
    })
}

/// Returns a block to its free list.
///
/// # Safety
///
/// `ptr` must have come from [`alloc()`] with an identical `layout`, and must not
/// be used again.
pub unsafe fn dealloc(ptr: *mut u8, layout: Layout) {
    if ptr.is_null() {
        return;
    }

    let Some(class) = class_of(layout) else {
        return;
    };

    HEAP.with(|heap| {
        // SAFETY: the caller guarantees this block came from `alloc` with this
        // layout, so it is `class_size(class)` bytes of memory that nothing
        // else refers to.
        unsafe { heap.give(class, ptr as usize) };
        heap.live -= class_size(class);
        heap.deallocs += 1;
    });
}

/// Takes a snapshot of heap usage.
pub fn stats() -> Stats {
    if !is_ready() {
        return Stats::default();
    }

    HEAP.with(|heap| Stats {
        start: heap.start,
        end: heap.limit,
        watermark: heap.next,
        live: heap.live,
        peak: heap.peak,
        allocs: heap.allocs,
        deallocs: heap.deallocs,
        reused: heap.reused,
        failed: heap.failed,
        grown_in_place: heap.grown_in_place,
        free_counts: heap.free_counts,
    })
}

impl Stats {
    /// Total size of the arena in bytes.
    pub const fn capacity(&self) -> usize {
        self.end - self.start
    }

    /// Bytes never yet handed to anyone: the span above the bump pointer.
    ///
    /// Not the same as "free": blocks on the free lists are also available, but
    /// are counted below the watermark.
    pub const fn untouched(&self) -> usize {
        self.end - self.watermark
    }

    /// Bytes sitting on the free lists, ready to be reused.
    pub fn cached(&self) -> usize {
        let mut total = 0;
        let mut class = 0;
        while class < CLASS_COUNT {
            total += self.free_counts[class] as usize * class_size(class);
            class += 1;
        }
        total
    }

    /// Size in bytes of blocks in `class`.
    pub const fn class_size(class: usize) -> usize {
        class_size(class)
    }
}
