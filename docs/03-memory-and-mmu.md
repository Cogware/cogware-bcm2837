# Memory and the MMU

The kernel runs with a stage-1 EL1 MMU, an identity map, and one translation
base: `TTBR0_EL1`. There is no high half, no userspace split, and no relocation
step. The point is not virtual-memory sophistication; the point is to make the
hardware memory types match what the code is doing.

That matters immediately for SMP. With the MMU off, every access behaves as
Device-nGnRnE. `LDXR`/`STXR` are architecturally UNPREDICTABLE on Device memory,
which means Rust read-modify-write atomics and `SpinLock` are illegal until a
core has enabled its own MMU. The Normal, Inner-Shareable mapping is what makes
ordinary synchronisation possible. See [06-synchronisation.md](06-synchronisation.md)
and [04-smp.md](04-smp.md) for the cross-core side of the rule.

`arch::mmu::memory_map()` is the single place where region permissions are
defined. The linker script supplies section boundaries, `bsp::memory` supplies
board addresses, and the MMU code turns those facts into descriptors.

## The address space

The map is deliberately simple: virtual address equals physical address.

That is the right choice for this kernel because there is no dynamic loader, no
runtime relocation, and no process address space to isolate. The Raspberry Pi
firmware and QEMU load `kernel8.img` at physical `0x80000`; the reset code runs
there; the linker script is written for that address; and the build uses a
static relocation model. Keeping VA == PA means the program counter, stack
pointer, linker symbols, and MMIO constants all remain valid across the MMU
switch.

It also keeps driver code honest. A BCM2837 register at `0x3F20_0000` is still
written as `0x3F20_0000` after translation is enabled. There is no hidden
`ioremap`, no early-boot alias, and no second address a future driver author has
to remember.

The useful ARM view is:

```text
0x0000_0000 .. 0x0008_0000   firmware stub + SMP spin table
0x0008_0000 .. kernel_end     loaded kernel image and per-core stacks
kernel_end  .. vc_mem_base    ARM-owned DRAM
vc_mem_base .. 0x3F00_0000    VideoCore-owned DRAM, Normal-NC
0x3F00_0000 .. 0x4000_0000    BCM2837 peripheral window, Device
0x4000_0000 .. 0x4002_0000    BCM2836/7 ARM-local block, Device
```

The peripheral base is the Pi 2/3 base, `0x3F00_0000`. The older Pi 1 used
`0x2000_0000`; this project targets BCM2837 and does not runtime-detect other
SoCs. The ARM-local interrupt, timer and mailbox block starts at `0x4000_0000`.

The boot-time table currently prints this map:

```text
  | 0x0000080000 | 0x000008c000 |  48 KiB | Normal    | RO X | kernel .text                   |
  | 0x000008c000 | 0x0000091000 |  20 KiB | Normal    | RO   | kernel .rodata                 |
  | 0x0000091000 | 0x000011e000 | 564 KiB | Normal    | RW   | kernel .data/.bss + stacks     |
  | 0x0000000000 | 0x0000080000 | 512 KiB | Normal    | RW   | firmware stub + SMP spin table |
  | 0x000011e000 | 0x003c000000 | 958 MiB | Normal    | RW   | free DRAM (ARM)                |
  | 0x003c000000 | 0x003f000000 |  48 MiB | Normal-NC | RW   | VideoCore DRAM (GPU)           |
  | 0x003f000000 | 0x0040000000 |  16 MiB | Device    | RW   | BCM2837 peripherals (MMIO)     |
  | 0x0040000000 | 0x0040020000 | 128 KiB | Device    | RW   | ARM local peripherals (MMIO)   |
```

The order is most-specific first. That is why `.text`, `.rodata`, and
`.data/.bss + stacks` appear before the broader DRAM ranges.

The policy is W^X. `.text` is read-only and executable. `.rodata` is read-only
and no-execute. Every writable region is no-execute. Nothing in this kernel runs
at EL0, so `UXN` is set everywhere as well.

If an address has no matching region, the descriptor is zero. A zero descriptor
is invalid, so a stray access faults rather than silently aliasing some broader
range. This is intentional. It also means an inverted or empty region is
harmless: it simply leaves a hole.

## Translation-table design

`TCR_EL1` is configured for a 4 GiB input address space through `TTBR0_EL1`.
With a 4 KiB granule and `T0SZ = 32`, the walk starts at level 1. The kernel
allocates one L1 table, one L2 table, and eight L3 tables in `.bss`.

The shape is:

```text
TTBR0_EL1
  |
  v
L1 table
  |
  +-- L1[0] table -> L2 table                         0x0000_0000..0x4000_0000
  |                  |
  |                  +-- L2[0] table -> L3[0] pages    0x0000_0000..0x0020_0000
  |                  +-- L2[1] table -> L3[1] pages    0x0020_0000..0x0040_0000
  |                  +-- ...
  |                  +-- L2[7] table -> L3[7] pages    0x00E0_0000..0x0100_0000
  |                  +-- L2[8..504] 2 MiB blocks       0x0100_0000..0x3F00_0000
  |                  +-- L2[504..512] 2 MiB blocks     0x3F00_0000..0x4000_0000
  |
  +-- L1[1] block, Device                              0x4000_0000..0x8000_0000
  +-- L1[2] invalid                                    0x8000_0000..0xC000_0000
  +-- L1[3] invalid                                    0xC000_0000..0x1_0000_0000
```

The low 16 MiB gets 4 KiB pages. That is the fine window. It exists because the
loaded image lives low in DRAM and the MMU must put different permissions on
`.text`, `.rodata`, `.data`, `.bss`, and `.stacks` without wasting a 2 MiB
block per section.

Above 16 MiB, 2 MiB L2 blocks are enough. The free DRAM and VideoCore split are
coarse-grained by design. `bsp::memory::set_vc_memory()` rounds the GPU base
down to 2 MiB for exactly this reason.

`L1[1]` is a single 1 GiB Device block. Only the first 128 KiB of it is used by
the ARM-local block, but a level-1 block is the natural descriptor at that
address and the rest is left as Device, no-execute space. `L1[2]` and `L1[3]`
are invalid; the demo data-abort command deliberately reads `0xC000_0000` to
prove the fault path.

The descriptors are built from `memory_map()` by asking which region contains
the descriptor's base address. If no region contains it, the entry stays zero.
That rule is easy to audit and avoids a dangerous fallback mapping.

The tables live in `.bss` as an all-zero `static`. Core 0 is the only writer.
It fills the L3 pages, then the L2 entries, then the L1 entries, and only then
publishes the table root through `TTBR0_EL1`. Secondary cores are held at the
SMP gate until this has happened.

## Register configuration

The kernel uses stage-1 translation at EL1. There is no EL2 translation and no
second-stage translation in this design.

`TCR_EL1` is `0x2_B5A0_3520`:

```text
T0SZ  = 32        TTBR0 covers 2^(64 - 32) = 4 GiB
IRGN0 = 0b01      TTBR0 walks are inner write-back, write-allocate
ORGN0 = 0b01      TTBR0 walks are outer write-back, write-allocate
SH0   = 0b11      TTBR0 walks are Inner Shareable
TG0   = 0b00      4 KiB granule for TTBR0
T1SZ  = 32        programmed but unused
EPD1  = 1         disable TTBR1 walks entirely
IRGN1 = 0b01      harmless because EPD1 disables TTBR1
ORGN1 = 0b01      harmless because EPD1 disables TTBR1
SH1   = 0b11      harmless because EPD1 disables TTBR1
TG1   = 0b10      4 KiB granule encoding for TTBR1
IPS   = 0b010     40-bit intermediate physical address size
```

`EPD1 = 1` is important. There is no high half. Any accidental address that
would normally select `TTBR1_EL1` should fault, not walk an uninitialised table.
`TTBR1_EL1` is also written as zero for clarity.

`MAIR_EL1` is `0x0044_FF00`. The three live attributes are:

```text
attr 0 = 0x00   Device-nGnRnE
attr 1 = 0xFF   Normal, inner/outer write-back write-allocate
attr 2 = 0x44   Normal, inner/outer non-cacheable
```

`TTBR0_EL1` receives the physical address of the L1 table. The mapping is an
identity map, so the address is also the virtual address used by Rust to name
the same static.

The enable sequence is deliberately ordered:

1. build the tables in memory;
2. `dsb sy` so the descriptor writes are ordered before the table-base write;
3. write `MAIR_EL1`, `TCR_EL1`, `TTBR0_EL1`, and zero `TTBR1_EL1`;
4. `dsb ishst` to publish the table configuration;
5. `tlbi vmalle1` to discard stale EL1 translations;
6. `dsb ish` so the invalidation is complete;
7. `isb` so following instructions see the new translation regime;
8. set `SCTLR_EL1.M`, `SCTLR_EL1.C`, `SCTLR_EL1.I`, and `SCTLR_EL1.SA`;
9. `ic iallu`, `dsb nsh`, `isb` so instruction fetch restarts cleanly.

The barriers are not decoration. System registers are not ordinary variables;
without the `isb`, later instructions can execute under the old regime. Table
writes are not useful until the hardware walker is guaranteed to see them.
Translation invalidation is not complete until the following `dsb` says so.

Every core runs the enable sequence for itself. `SCTLR_EL1`, `TCR_EL1`,
`MAIR_EL1`, and the translation base registers are per-core architectural state.
All cores point at the same already-built tables.

## Memory types

`MemoryKind::Device` is for MMIO only. It uses Device-nGnRnE: no gathering, no
reordering, no early write acknowledgement. That is the conservative mapping for
registers, and it is why ordinary memory operations must never be aimed at a
peripheral window.

`MemoryKind::Normal` is ordinary kernel RAM. It is write-back, write-allocate,
and Inner Shareable. Inner shareability is what lets the Cortex-A53 cores use
the exclusive monitor coherently for locks and other RMW atomics.

`MemoryKind::NormalNonCacheable` is for memory shared with the VideoCore or a
future DMA engine outside the ARM coherency domain. It is deliberately Normal,
not Device.

The distinction is easy to get wrong. Device memory forbids unaligned access and
makes ordinary `memcpy`-style code illegal. A framebuffer needs ordinary byte and
word stores, and firmware-owned buffers may be copied as normal data. Mapping
that memory Device would avoid caching but would also change the rules of the
instructions that can touch it.

Normal-NC gives ordinary data semantics without retaining dirty lines in the ARM
cache. Shareability is architecturally ignored for non-cacheable Normal memory;
the descriptor uses `SH_OUTER` because it is the conventional encoding and does
not carry a deeper synchronisation promise.

The descriptor permission bits are similarly narrow:

```text
kernel .text         Normal, RO, executable at EL1
kernel .rodata       Normal, RO, PXN
kernel data/stacks   Normal, RW, PXN
firmware low memory  Normal, RW, PXN
ARM free DRAM        Normal, RW, PXN
VideoCore DRAM       Normal-NC, RW, PXN
MMIO windows         Device, RW, PXN
```

A new permission belongs in `memory_map()`, not in driver code and not in a
second table-building path.

## The VideoCore split

The VideoCore owns the firmware, the property mailbox service, framebuffer
allocation, clocks, and the V3D/QPU power domain. It also owns a reservation at
the top of SDRAM. The ARM must not allocate ordinary cached memory from that
reservation.

The split is discovered before the MMU is enabled:

```text
kernel_boot_primary
  -> drivers::mailbox::probe_memory()
       -> MAILBOX.bypass().vc_memory()
       -> bsp::memory::set_vc_memory(base, size)
  -> arch::mmu::init()
       -> arch::mmu::memory_map()
```

The ordering is important. Before `mmu::enable()`, all memory is uncached
Device-nGnRnE, so the early mailbox transaction does not need cache maintenance.
It is also still single-core, so bypassing the mailbox lock is sound.

`set_vc_memory()` records the firmware answer in atomics and rounds the base
down to a 2 MiB boundary. The rounding direction is a safety decision. If the
true split falls inside a block, rounding down may map a little ARM-owned memory
Normal-NC. That costs performance. Rounding up would map a little GPU-owned
memory cacheable. That risks stale data and corruption. The safe error is the
slow one.

`memory_map()` then emits:

```text
free DRAM (ARM)      kernel_end .. vc_mem_base()
VideoCore DRAM       vc_mem_base() .. min(vc_mem_base() + vc_mem_size(), 0x3F00_0000)
```

The clamp to `PERIPHERAL_BASE` is essential. Firmware reports the VideoCore
reservation as running to the top of SDRAM at `0x4000_0000`, but in the ARM's
view the top 16 MiB of that range is shadowed by the peripheral window. The MMU
must not turn `0x3F00_0000..0x4000_0000` into Normal-NC RAM; it is Device MMIO.

QEMU reports a 960 MiB ARM / 64 MiB VideoCore split with the GPU base at
`0x3C00_0000`. If the mailbox probe fails, boot continues with
`VC_MEM_DEFAULT_BYTES = 64 MiB`, matching the common `gpu_mem=64` firmware
setting.

The bus address rules live in `bsp::memory::bus`. The VideoCore sees
peripherals at `0x7E00_0000`, not at the ARM's `0x3F00_0000`. It sees SDRAM
through four aliases selected by the top two bits:

```text
0x0000_0000   L1 + L2 cached
0x4000_0000   L2 coherent, L1 bypassed
0x8000_0000   L2 cached, non-allocating
0xC000_0000   fully uncached
```

`to_bus()` ORs in `0xC000_0000`; `to_physical()` masks the alias bits off.
Anything shared with the ARM uses the uncached alias because the GPU is not in
the ARM coherency domain. The mailbox details are in
[11-videocore-mailbox.md](11-videocore-mailbox.md).

## Cache maintenance

`cogware-bcm2837\src\arch\cache.s` contains the whole-cache data maintenance routine
because set/way maintenance is awkward to express correctly in Rust. It walks
`CLIDR_EL1` and `CCSIDR_EL1`, selects each data cache level through
`CSSELR_EL1`, and issues `dc isw` for every set and way.

That path is one of the emulator differences: set/way operations such as
`dc isw` are no-ops in QEMU, but they are required on real Cortex-A53
silicon. The code is present because hardware needs it; the exact effect is not
verified by QEMU.

The instruction names used in the MMU and mailbox paths are:

```asm
dc civac, xN     // clean and invalidate one data-cache line by VA to PoC
ic iallu         // invalidate all instruction caches to PoU
dsb sy           // complete all explicit memory accesses before continuing
dsb ish          // same idea, inner-shareable domain
dsb ishst        // order stores in the inner-shareable domain
isb              // discard prefetched instructions and refetch under new state
```

`dc civac` is used for cache lines that another agent will read or has written:
the firmware spin-table slots, the SMP gate, and the mailbox message buffer once
the ARM cache is enabled. Cleaning pushes dirty ARM data to the point of
coherency; invalidating makes a later ARM load fetch the other agent's reply.

`ic iallu` is used around the MMU switch because instruction fetch before and
after translation enable is not the same architectural regime. The following
`dsb` and `isb` make the invalidation complete and visible to the executing
stream.

A useful rule of thumb: `dc civac` makes data visible; `ic iallu` makes code
fetch honest; `dsb` waits for the maintenance to complete; `isb` makes the core
stop executing with stale assumptions.

## The linker/MMU contract

`cogware-bcm2837\link.ld` is part of the MMU design. Section starts are page-aligned so
the L3 descriptors can grant exact 4 KiB permissions to each section.

The linker exports:

```text
__text_start      __text_end
__rodata_start    __rodata_end
__data_start      __data_end
__bss_start       __bss_end
__stacks_start    __stacks_end
__kernel_start    __kernel_end
```

Rust reads those addresses through `bsp::memory::layout`; code must not
hard-code image boundaries. `__data_end` is the page-aligned end of `.data` and
`.bss`; `__kernel_end` is after the per-core stacks and is the first byte the
heap allocates from — see [`13-heap.md`](13-heap.md), which takes its arena as
`__kernel_end .. vc_mem_base()` precisely so that it tracks this symbol.

`.stacks` is `NOLOAD` and must stay after `.bss`. The reset code installs a
stack before it zeroes `.bss`; putting stacks inside the zeroed region would
erase live stack frames. Including stacks in the load image would also turn
`kernel8.img` into half a megabyte of zeroes.

The script asserts `__kernel_end <= 16M`. If the image outgrows the fine window,
the MMU could no longer express section permissions at 4 KiB granularity without
more L3 tables.

## How to add a region to the map

1. Add the physical address constant to `bsp::memory` if it is board layout, or
   use an existing linker symbol from `bsp::memory::layout` if it is kernel
   image layout.
2. Decide the memory type first: `Device` for MMIO, `Normal` for ARM RAM,
   `NormalNonCacheable` for memory shared with the VideoCore or non-coherent
   DMA.
3. Add one `Region` to `arch::mmu::memory_map()`. Keep the order
   most-specific first, before any broader range that would otherwise catch the
   same start address.
4. Use the narrowest permissions that work. Writable code and executable data
   are bugs until there is a design document explaining why they are not.
5. Check the descriptor granularity. Below 16 MiB, 4 KiB pages can describe the
   exact bounds. Above 16 MiB, the current table shape gives 2 MiB blocks except
   for the `L1[1]` ARM-local block.
6. Leave holes invalid. Do not add a catch-all descriptor to make a fault go
   away; the fault is telling you the map and the access disagree.
7. If the region is shared with the VideoCore, also hand the firmware an
   uncached bus address with `bsp::memory::bus::to_bus()`. The MMU attribute and
   the bus alias are two halves of the same coherency rule.
8. Update the boot-time table documentation and any affected sibling design
   note. Hardware quirks belong in `docs/`, not in memory.
