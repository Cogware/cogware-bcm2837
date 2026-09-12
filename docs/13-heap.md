# Heap allocator

The kernel now has a heap, but it is not a licence to turn boot into a desktop
runtime. It is a deliberately small allocator for the few places where owned
buffers make the kernel clearer: `Box`, `Vec`, `String`, and raw blocks for
drivers that need an address.

The design is a segregated power-of-two free-list allocator over the ARM-owned
DRAM described in [03-memory-and-mmu.md](03-memory-and-mmu.md). It uses stable
Rust 1.98 and edition 2024. There is no nightly `#[alloc_error_handler]`: the
default allocation-error handler has been stable since Rust 1.68, so
`no_std` + `extern crate alloc` + `#[global_allocator]` is enough.

The surrounding rule still matters. The synchronisation rule in
`LDXR`/`STXR` are not used before the MMU is on, and therefore no
`SpinLock`, before the calling core has enabled its MMU. The heap is built on a
`SpinLock<Inner>`, so it only opens after `arch::init_core()`.

## Why this shape

The cold-boot target is more important than allocator cleverness. Heap
initialisation must not scale with heap size.

Two obvious designs fail that rule:

* A heap reserved as a `.bss` or `NOLOAD` arena still costs a memset of the
  whole arena before Rust code can run.
* A classic free-list heap that threads the whole arena onto a list at init
  costs a walk over every block before the shell appears.

This allocator does neither. Initialisation records the arena bounds and an
optional chain-load reservation, then publishes an `AtomicBool`. No arena
memory is zeroed or walked, and no free list is built. The arena already
exists as mapped DRAM, so there is no page-table work either.

```text
kernel image            heap arena                            VideoCore
and stacks              ARM-owned DRAM                         reservation
     |                       |                                      |
     v                       v                                      v
+----------+-----------------+----------------------+---------------+
| image    | allocated/cached| never written        | GPU-owned RAM |
+----------+-----------------+----------------------+---------------+
           ^                 ^                      ^
           |                 |                      |
        start             watermark               end
        kernel_end()      bump pointer            vc_mem_base()
```

That is why the heap can exist without changing the measured boot time: the
real work is deferred until an allocation actually happens.

## The arena

The usable range is:

```text
align_up(layout::kernel_end(), 16) .. memory::vc_mem_base()
```

On the current QEMU test machine that is:

```text
0x12b000 .. 0x3c000000     958 MiB
```

Those numbers are observations, not constants in the allocator. Both bounds
come from the same sources as `arch::mmu::memory_map()`: the linker symbol read
through `bsp::memory::layout::kernel_end()` and the firmware-probed VideoCore
split read through `bsp::memory::vc_mem_base()`. That keeps the heap and the MMU
map in lockstep.

No new mapping is created for the heap. `memory_map()` already covers the same
span as `free DRAM (ARM)`, mapped Normal, read-write, and execute-never. The
heap just starts using that region after the MMU has made it ordinary cacheable
memory.

### Chain-loaded kernels

The [chain-loader](22-chainloading.md) hands off one 4 KiB page containing the
secondary park code through x4-x6. The primary boot path validates that page
against firmware-confirmed ARM RAM before calling the crate-private reserved
heap initialiser. The page must be aligned, entirely inside the arena and at
or above the linker's 16 MiB kernel window.

That page is permanently excluded for this kernel's lifetime, even if the
application allocates without ever starting its secondaries. Both bump carving
and padding free-list donation skip it. The remaining RAM, including the
loader's former staging area, stays available. `Stats::reserved_range` and
`reserved_bytes()` report the hole; `capacity()` and `untouched()` subtract
withheld bytes, preserving `capacity == live + cached + untouched`.

Ordinary firmware boots have no reserved page. Public unsafe `heap::init()`
remains usable for a caller with no handover reservation; the BSP's standard
boot path selects the appropriate initialiser automatically. Do not reinitialise
a running heap to discard a reservation.

## Size classes

Every request is rounded to one power-of-two class:

| Constant | Value | Meaning |
| --- | ---: | --- |
| `MIN_SHIFT` | 4 | Smallest class is 16 bytes. |
| `MAX_SHIFT` | 30 | Largest class is 1 GiB. |
| `CLASS_COUNT` | 27 | Classes 16 B through 1 GiB inclusive. |

Allocation is:

```text
class = class_of(layout)
pop class free list
if empty:
    carve from bump pointer
```

Freeing is:

```text
class = class_of(layout)
push block onto class free list
```

There is no search, no best-fit scan, and no coalescing walk. `alloc` is O(1):
one LIFO pop or one bump carve. `dealloc` is O(1): one LIFO push.

```text
heads[0]  16 B  -> [next] -> [next] -> null
heads[1]  32 B  -> [next] -> null
heads[2]  64 B  -> null
...
heads[26] 1 GiB -> null
```

The price is paid in fragmentation rather than time. That is intentional for a
kernel whose normal allocation pattern is a handful of long-lived driver and
diagnostic structures.

## No per-allocation header

Allocated blocks have no header. `GlobalAlloc::dealloc` receives the same
`Layout` that `GlobalAlloc::alloc` received, so `heap::dealloc` can recompute
the size class instead of storing it next to the object.

That is a real saving for tiny allocations. A `Box<u32>` costs four bytes of
payload rounded to a 16-byte class, and zero bytes of allocator metadata. There
is no hidden 16-byte header turning it into a 32-byte allocation.

The same choice makes the free lists cheap. A freed block's body becomes:

```rust
struct Free {
    next: *mut Free,
}
```

That is why `MIN_SIZE` is 16 rather than 8. The block must be large enough for
the pointer, and 16-byte alignment is also the natural AArch64 baseline for the
stack and NEON `q` registers.

## Alignment

`class_of` chooses the class from:

```text
max(layout.size(), layout.align(), MIN_SIZE)
```

rounded up to a power of two. `Inner::carve` then aligns the bump pointer to
that class size before handing out the block.

The result is stronger than "large enough": a class-N block is N bytes long and
N-byte aligned. A 16-byte value that asks for 64-byte alignment becomes a
64-byte block, so the alignment falls out of the same rule as the size. There
is no over-alignment side table and no special slow path.

The padding skipped to reach the aligned address is not leaked. `Inner::carve`
passes it to `Inner::release_range`, which donates the gap to the ordinary free
lists.

If a chain-load park page lies in the gap, `release_range` splits the donation
into the spans before and after that page. Merely moving the bump pointer
would be insufficient: donating the whole gap could hand the executing park
code back out on a later free-list allocation.

```text
next before carve
     |
     v
+----+----------------+----------------+
|gap | aligned block  | still untouched|
+----+----------------+----------------+
  ^                   ^
  |                   |
 donated by           next after carve
 release_range
```

`release_range(lo, hi)` repeatedly emits the largest power-of-two block that is
both correctly aligned and fits in the remaining span:

* `by_align` comes from `lo.trailing_zeros()`. A block may only have size `S`
  if `lo` is aligned to `S`, so the existing low-bit zero count bounds the
  class that can start there.
* `by_span` comes from `span.leading_zeros()`. It is the largest power of two
  not larger than `hi - lo`, so the block cannot overrun the gap.

The chosen size is `min(by_align, by_span, MAX_SIZE)`. The loop terminates
because each iteration advances `lo` by at least `MIN_SIZE`. In practice it
runs at most once per size class: each step either improves the alignment of
`lo` or consumes enough of the span that the largest fitting block shrinks.

## Reallocation

`Kernel::realloc` has one important fast path. If the old and new layouts map
to the same size class, the pointer is returned unchanged:

```text
old layout -> class 16
new layout -> class 16
return same block, copy nothing
```

That is counted as `grown_in_place`. It is why a `Vec` doubling from 8 to
16 bytes is free: both sizes are still in the 16-byte class. Crossing a class
boundary allocates a new block, copies `min(old_size, new_size)` bytes, and
frees the old block.

## MMU ordering constraint

This deserves its own section because getting it wrong is not a performance
bug; it is architecturally undefined territory.

`SpinLock` is described in [06-synchronisation.md](06-synchronisation.md). It
uses `LDXR`/`STXR`. On AArch64 those exclusives are architecturally
UNPREDICTABLE when aimed at Device memory, and before the MMU is enabled all
memory behaves as Device-nGnRnE, which the project does not do.

The heap therefore must not be used before `arch::init_core()` has enabled the
MMU on the calling core. The primary boot path enforces the order:

```text
drivers::mailbox::probe_memory()
validate optional chain-load page against firmware ARM/VideoCore bounds
arch::mmu::init()
arch::init_core()
heap::init_with_reservation(validated_page_or_none)  // crate-private boot path
release later work
```

The allocator also defends the failure case. `READY` is a plain `AtomicBool`.
Its acquire load lowers to `LDARB`, which is defined on Device memory. `alloc`
checks `READY` before it touches the `SpinLock`; if the heap is not ready, it
returns null instead of executing an exclusive. For Rust collection allocation,
that null becomes the normal `handle_alloc_error` panic path rather than an
unpredictable pre-MMU instruction.

`heap::init()` is marked `unsafe` because its caller must uphold the order:
primary core only, MMU already on, mailbox memory split already probed, and no
secondary core released far enough to allocate concurrently.

## Concurrency

There is one heap:

```rust
static HEAP: SpinLock<Inner>
```

All four cores share it. The critical sections are deliberately tiny: pop or
push one free-list pointer, or advance the bump pointer. That is cheaper than
splitting the design prematurely.

`Inner` contains raw pointers into the arena, so it has:

```rust
unsafe impl Send for Inner {}
```

The justification is narrow. The pointers name plain DRAM blocks, and they are
only dereferenced while `HEAP` is held. The lock supplies the exclusion the raw
pointers do not encode.

If contention ever shows up in a real profile, per-core magazines belong at the
edge of the existing design: refill from `Inner::take`, drain through
`Inner::give`, and leave the arena accounting in one place. Until then,
magazines would be more code to debug for no measured benefit.

## Using it

Most code should use the ordinary `alloc` types:

```rust
extern crate alloc;

use alloc::{boxed::Box, string::String, vec::Vec};

let value = Box::new(0x00C0_FFEE_u64);

let mut numbers = Vec::new();
numbers.push(1_u32);

let mut text = String::new();
let _ = core::fmt::write(&mut text, format_args!("heap works"));
```

Callers that need a raw address can use the heap API directly:

```rust
use core::alloc::Layout;

let layout = Layout::from_size_align(4096, 4096).unwrap();
let ptr = heap::alloc(layout);
if !ptr.is_null() {
    // use the block
    unsafe { heap::dealloc(ptr, layout) };
}
```

The safety rule is the same as `GlobalAlloc`: `heap::dealloc(ptr, layout)` must
receive the exact pointer and layout pair returned by `heap::alloc`, and the
pointer must not be used afterwards.

Diagnostics use:

```rust
pub fn stats() -> Stats
```

`Stats` reports `start`, `end`, `watermark`, `live`, `peak`, allocation counts,
free-list counts, and derived helpers:

```rust
Stats::capacity()
Stats::untouched()
Stats::cached()
Stats::class_size(class)
```

## The `a` console command

The shell command `a` prints heap occupancy and then runs a small self-test.
The self-test is intentionally repeatable: it costs microseconds and lets a
human check the allocator after other work has fragmented the free lists.

This is a captured QEMU run:

```text
Heap
  +------------------+--------------+---------+------------------------------+
  | field            | value        |    size | note                         |
  +------------------+--------------+---------+------------------------------+
  | arena start      | 0x000012b000 |         | first byte above image       |
  | arena end        | 0x003c000000 |         | VideoCore reservation base   |
  | capacity         |   1005408256 | 958 MiB | mapped Normal RW at boot     |
  | watermark        | 0x000012e000 |         | highest byte ever touched    |
  | live             |          256 | 256 B   | handed out right now         |
  | peak             |         6272 |   6 KiB | high-water mark              |
  | cached           |        12032 |  11 KiB | on free lists, reusable      |
  | untouched        |   1005395968 | 958 MiB | never written DRAM           |
  +------------------+--------------+---------+------------------------------+
  allocs 17 | frees 16 | reused 9 | resize-in-place 1 | failed 0
  free lists:  16Bx2  32Bx1  64Bx1  128Bx3  256Bx1  512Bx2  1KiBx2  2KiBx2  4KiBx1
  self-test: 5/5 passed
    [ok] Box<u64> round trip
    [ok] Vec<u32> grown to 1024
    [ok] String formatting
    [ok] 64-byte aligned Box
    [ok] free list reuses the block
  self-test cost 17 alloc(s), all returned
```

The important line is not the capacity. It is the watermark:

```text
0x12e000 - 0x12b000 = 12 KiB
```

Only 12 KiB of a 958 MiB arena has ever been touched. Everything above the
watermark is DRAM that has never been written by the allocator, which is the
whole point of the bump design.

`live` is 256 bytes rather than zero because the `Vec` holding the self-test
results is still alive when the snapshot is taken. It is constant across
repeated runs, so it is not a leak. On the second invocation of `a`, `allocs`
went from 17 to 34 while `reused` went from 9 to 26: all 17 allocations in the
second run came from free lists instead of moving the bump pointer.

Boot time stays in its pre-heap range of roughly 30-34 ms, because `heap::init()`
does the same amount of work regardless of whether the arena is 12 KiB or
958 MiB.

## Proving it is SMP-safe

A self-test that only ever runs on core 0 proves nothing about a lock. So the
`worker` tasks on cores 1-3 build their result line with `alloc::format!`
rather than printing it directly:

```rust
let line = alloc::format!("job {:>2} -> {term:.8} (completed {completed})", job.seq);
cprintln!("{line}");
```

Three secondary cores therefore allocate and free concurrently while core 0's
shell does the same. Running `w` three times (72 jobs) and then `a` gives:

```text
  allocs 89 | frees 88 | reused 78 | resize-in-place 1 | failed 0
```

with the watermark still at `0x12e000` and `live` still exactly 256 bytes. Every
one of those 72 cross-core allocations was served from a free list without
touching a new byte of DRAM, nothing leaked, and no count is inconsistent —
which is what a correctly locked allocator looks like from the outside.

## Trade-offs

| Cost | Consequence | Why it is acceptable here |
| --- | --- | --- |
| Power-of-two rounding | A 17-byte allocation consumes 32 bytes. | The QEMU arena is 958 MiB, and kernel allocations are few. |
| No coalescing | Adjacent free 16-byte blocks never become a 32-byte block. | Coalescing is exactly the work that makes general-purpose allocators slower. |
| Class-boundary `realloc` copies | Growing past a class size allocates and copies. | The common `Vec` doublings still hit the in-class fast path at small sizes. |
| One global lock | Simultaneous allocation on four cores serialises. | The critical section is a load and a store, not a list walk. |

The pathological case is real: a workload can free many small blocks and then
fail a large allocation while memory is nominally free. That is the chosen
failure mode. This kernel does not have the allocation pattern of a
general-purpose OS, and the cost of avoiding that pathology is paid on every
ordinary allocation.

## What this is not

This is not a general-purpose userspace allocator. It has no coalescing, no
page-return path, no guard pages, no quarantine, and no per-object debugging
header.

The likely future layers are deliberately separate:

```text
coalescing allocator   only if real fragmentation appears
per-core magazines     only if the single lock shows up in a profile
slab caches            for hot fixed-size kernel objects
```

That matches the roadmap's tone in [10-roadmap.md](10-roadmap.md): add the
piece that removes a measured limit, not the piece a larger OS would eventually
want.
