# VideoCore Property Mailbox
The BCM2837 is not an ARM-only system. The ARM cores run the kernel, but the
VideoCore firmware owns clocks, power domains, the DRAM split and framebuffer
allocation. The ARM asks for those services through the VideoCore property
mailbox. This driver is therefore the single hook for two later milestones:
clocking the CPU up on real boards, and doing GPU or framebuffer work.
The driver is `cogware-bcm2837\src\drivers\mailbox.rs`. It is exported from
`cogware-bcm2837\src\drivers\mod.rs` and instantiated as the global
`MAILBOX: SpinLock<mailbox::Mailbox>` at `cogware-bcm2837\src\drivers\mod.rs`.
Unlike GPIO and PL011, most mailbox operations are not really MMIO
configuration of an ARM peripheral. They are firmware calls. The MMIO registers
are only the doorbell; the real protocol is a shared memory message that the
VideoCore reads, modifies and writes back.

## Why this exists
The module docs at `cogware-bcm2837\src\drivers\mailbox.rs` name the three current
uses:

* `GET_ARM_MEMORY` and `GET_VC_MEMORY` discover the DRAM split for
  `mmu::memory_map`;
* `SET_CLOCK_RATE` on `Clock::Arm` is the path to raising the ARM CPU clock;
* `SET_POWER_STATE` on `Device::V3d` is the remaining GPU hook; the
  framebuffer tags are already used by `drivers::framebuffer`.

The boot path already uses the first of these. `kernel_boot_primary` in
`cogware-bcm2837\src\boot.rs` calls
`unsafe { drivers::mailbox::probe_memory() }` before `mmu::init()`. This is
early enough for the MMU map to reflect the actual firmware `gpu_mem` split.

## Register layout
The mailbox block base is `map::MAILBOX = PERIPHERAL_BASE + 0x0000_B880`
(`cogware-bcm2837\src\bsp\memory.rs`), which is `0x3F00_B880` on the Pi 3.

The nested `bsp::memory::map::mailbox` module begins at
`cogware-bcm2837\src\bsp\memory.rs`. It defines two mailboxes:

```text
MB0  0x3F00_B880  VideoCore to ARM, read-only to the ARM
MB1  0x3F00_B8A0  ARM to VideoCore, write-only to the ARM
```

`MB0` is `MAILBOX` (`cogware-bcm2837\src\bsp\memory.rs`) and `MB1` is
`MAILBOX + 0x20` (`cogware-bcm2837\src\bsp\memory.rs`). Reading MB1 or writing MB0
hangs, so the driver uses raw `read_reg` only on MB0 and raw `write_reg` only
on MB1.

The offsets repeat inside each mailbox:

```text
DATA    0x00
PEEK    0x10
SENDER  0x14
STATUS  0x18
CONFIG  0x1C
```

These constants are at `cogware-bcm2837\src\bsp\memory.rs`.

The status bits are `FULL = 1 << 31` and `EMPTY = 1 << 30`
(`cogware-bcm2837\src\bsp\memory.rs`). Before writing to MB1 the driver waits
until `FULL` is clear. Before reading from MB0 it waits until `EMPTY` is clear.

## Channels
The low four bits of a mailbox word carry the channel. The useful channel
constants are:

```text
0  POWER
1  FRAMEBUFFER       old, superseded interface
2  VIRTUAL_UART
8  PROPERTY_ARM_TO_VC
9  PROPERTY_VC_TO_ARM
```

They are declared at `cogware-bcm2837\src\bsp\memory.rs`. This kernel uses
channel 8, `PROPERTY_ARM_TO_VC`. Because the low four bits carry the channel,
the message buffer address itself must be 16-byte aligned. `MessageBuffer` is
`#[repr(C, align(16))]` at `cogware-bcm2837\src\drivers\mailbox.rs`, and
`transact` debug-asserts the low bits are zero at
`cogware-bcm2837\src\drivers\mailbox.rs`.

## Property message format
A property message is a buffer of `u32` words. `Mailbox::call` sends a single
tag, which looks like this:

```text
word
 0   total size in bytes, including the header and end tag
 1   request/response code: 0 on input, 0x8000_0000 on success
 2   tag id
 3   value buffer size in bytes
 4   request code: 0 on input;
     response code: 0x8000_0000 | response length on output
 5   first value word
 ... more value words, padded to a word boundary
 n   0 end tag
```

The VideoCore writes its reply back into the same buffer. This is why cache
maintenance and the bus address alias are not optional.

`Mailbox::call` builds this layout at `cogware-bcm2837\src\drivers\mailbox.rs`.
The value buffer length is `request.len().max(response.len())`
(`cogware-bcm2837\src\drivers\mailbox.rs`). That detail prevents a classic firmware
protocol bug: some tags send a small request but return a larger response. The
buffer must be large enough for whichever side is larger, not just for the
request words.


## Shared buffer and locking
`BUFFER_WORDS = 64` at `cogware-bcm2837\src\drivers\mailbox.rs`. It is deliberately
larger than the simple board-query calls need, because the framebuffer
mode-set is a multi-tag message.

The message storage is one static:

```rust
#[repr(C, align(16))]
struct MessageBuffer([u32; BUFFER_WORDS]);

static mut MESSAGE: MessageBuffer = MessageBuffer([0; BUFFER_WORDS]);
```

The declarations are at `cogware-bcm2837\src\drivers\mailbox.rs`. This is why
`MAILBOX` must be a `SpinLock`: the lock protects the shared buffer as much as
it protects the mailbox registers. Two cores cannot be mid-transaction in the
same static buffer.

The lock is the global `drivers::MAILBOX` at `cogware-bcm2837\src\drivers\mod.rs`.
The early boot exception is `probe_memory`, which uses `MAILBOX.bypass()` under
a documented single-core, MMU-off contract (`cogware-bcm2837\src\drivers\mailbox.rs`).
The synchronisation rule is explained in [06-synchronisation.md](06-synchronisation.md).

## Coherency rule 1: use a VideoCore bus address
The address written to the mailbox is not an ARM physical address. It is a
VideoCore bus address. The conversion helpers are in `bsp::memory::bus` at
`cogware-bcm2837\src\bsp\memory.rs`.

The VideoCore sees peripherals at `0x7E00_0000`, not at the ARM's
`0x3F00_0000` peripheral base. For SDRAM, it sees four aliases selected by the
top two address bits:

```text
0x0000_0000  L1 + L2 cached
0x4000_0000  L2 coherent
0x8000_0000  L2 cached, non-allocating
0xC000_0000  fully uncached
```

`bus::ALIAS_UNCACHED` is `0xC000_0000` (`cogware-bcm2837\src\bsp\memory.rs`).
`bus::to_bus(physical)` ORs that alias into the address
(`cogware-bcm2837\src\bsp\memory.rs`). `bus::to_physical(bus)` masks it off with
`!ALIAS_MASK` (`cogware-bcm2837\src\bsp\memory.rs`).

Anything shared between ARM and VideoCore must use the uncached alias, because
the GPU is not in the ARM coherency domain. `Mailbox::transact` does exactly
that at `cogware-bcm2837\src\drivers\mailbox.rs`.

## Coherency rule 2: clean and invalidate the ARM cache
After the MMU is enabled, the ARM's own mapping of the mailbox buffer is Normal
cacheable memory. If the ARM writes a request and only keeps it in D-cache, the
VideoCore can read stale memory. If the VideoCore writes the reply and the ARM
keeps its old cache line, the ARM can read its own stale request.

`Mailbox::flush` handles this at `cogware-bcm2837\src\drivers\mailbox.rs`. When
`mmu::is_enabled()` is true, it walks every 64-byte cache line covering the
message and calls `mmu::clean_invalidate_line(line)`. It then uses a compiler
fence and `cpu::dsb_sy()` to order the buffer against the mailbox MMIO access.

`CACHE_LINE = 64` is at `cogware-bcm2837\src\drivers\mailbox.rs`. The cache
operation itself is `dc civac` in `mmu::clean_invalidate_line`
(`cogware-bcm2837\src\arch\mmu.rs`).

The exception the kernel exploits is early boot. Before `mmu::enable()` runs,
all accesses are Device-nGnRnE and therefore uncached. In that window the flush
does no line maintenance, but still performs the ordering barrier. That is
precisely when `probe_memory()` runs, before `mmu::init()`.

## Robustness choices
Mailbox hardware bugs or firmware wedges should not hang the kernel forever.
`POLL_LIMIT = 4_000_000` at `cogware-bcm2837\src\drivers\mailbox.rs` bounds every
status wait. `wait_until` returns `Error::Timeout` if the mailbox never becomes
ready (`cogware-bcm2837\src\drivers\mailbox.rs`).



The error type is a plain enum at `cogware-bcm2837\src\drivers\mailbox.rs`:
`Timeout`, `WrongChannel`, `Rejected`, `TagNotAnswered`, and `MessageTooLong`.
It implements `Display` at `cogware-bcm2837\src\drivers\mailbox.rs`.

## Memory-split integration
The memory split is established before the translation tables are built. The
chain is:

1. `kernel_boot_primary` in `cogware-bcm2837\src\boot.rs` calls
   `drivers::mailbox::probe_memory()` before `mmu::init()`;
2. `probe_memory` issues `GetVcMemory` through `vc_memory()`
   (`cogware-bcm2837\src\drivers\mailbox.rs`);
3. it passes the result to `bsp::memory::set_vc_memory`
   (`cogware-bcm2837\src\drivers\mailbox.rs`);
4. `set_vc_memory` records the split in `VC_MEM_BASE`, `VC_MEM_SIZE` and
   `VC_MEM_PROBED` (`cogware-bcm2837\src\bsp\memory.rs`);
5. `mmu::memory_map()` uses that split to emit `free DRAM (ARM)` and
   `VideoCore DRAM (GPU)` regions (`cogware-bcm2837\src\arch\mmu.rs`).

`VC_MEM_DEFAULT_BYTES = 64 * 1024 * 1024` at
`cogware-bcm2837\src\bsp\memory.rs`, matching the usual `gpu_mem=64` firmware
default. If the mailbox probe fails, the default remains and boot continues.
The boot message says the default was assumed (`cogware-bcm2837\examples\shell\main.rs`).

`set_vc_memory` rounds the reported base down to a 2 MiB boundary
(`cogware-bcm2837\src\bsp\memory.rs`). The level-2 block mappings above the low
16 MiB cannot express a finer split. Rounding down is the safe direction: it may
map a little ARM-owned memory as non-cacheable, which is slower, but it avoids
mapping GPU-owned memory cacheable, which would be incoherent.

`memory_map()` also clamps the VideoCore range to `PERIPHERAL_BASE`
(`cogware-bcm2837\src\arch\mmu.rs`). Firmware reports the reservation as running
to the top of SDRAM, but the top 16 MiB of that range is shadowed by the
peripheral window in the ARM view.

## Normal non-cacheable memory
The new memory kind is `MemoryKind::NormalNonCacheable`
(`cogware-bcm2837\src\arch\mmu.rs`). It uses MAIR attribute index 2:
`ATTR_NORMAL_NC = 2` (`cogware-bcm2837\src\arch\mmu.rs`) with MAIR byte `0x44`.
`MAIR_EL1_VALUE` is built as Device `0x00`, Normal WB/WA `0xFF`, and Normal-NC
`0x44` (`cogware-bcm2837\src\arch\mmu.rs`), giving `0x0044_FF00`.

The leaf descriptor uses the Normal-NC attribute with outer-shareable encoding
(`cogware-bcm2837\src\arch\mmu.rs`). Shareability is architecturally ignored for
non-cacheable Normal memory, but outer-shareable is the conventional choice.

The GPU-shared region is mapped Normal-NC rather than Device. Device memory
would forbid unaligned access and make ordinary `memcpy`-style code illegal. A
framebuffer needs ordinary byte and word accesses, so it must be Normal memory;
it is merely non-cacheable because the VideoCore is outside ARM coherency.

## Board report command
The shell command `b` calls `report_board()` (`cogware-bcm2837\examples\shell\main.rs`).
`report_board` starts at `cogware-bcm2837\examples\shell\main.rs` and uses the mailbox to print
board identity, DRAM split, temperature and selected clocks.

Verified QEMU output is:

```text
Board (via VideoCore property mailbox)
  revision   : 0x00a02082
  serial     : 0x0000000000000000
  ARM DRAM   : 0x0000000000 + 960 MiB
  GPU DRAM   : 0x003c000000 + 64 MiB
  temperature: 25.0 C

  clock       current MHz    max MHz
  ---------------------------------
  ARM               700        700
  core              350        350
  V3D (GPU)         700        700
  SDRAM             700        700
  UART                3          3
```

`0x00a02082` is the genuine Raspberry Pi 3 Model B revision code. QEMU reports
the UART clock as 3 MHz, while real firmware commonly feeds PL011 with 48 MHz
as captured by `PL011_CLOCK_HZ` (`cogware-bcm2837\src\bsp\memory.rs`). QEMU ignores
the baud divisors, so the mismatch is harmless in emulation, but the constant
matters on hardware.

## Clocking the CPU up
`Clock` is declared at `cogware-bcm2837\src\drivers\mailbox.rs`:

```text
Emmc=1  Uart=2  Arm=3  Core=4  V3d=5
H264=6  Isp=7   Sdram=8  Pixel=9  Pwm=10
```

The headline use case is raising the ARM clock on real hardware:

```rust
let max = MAILBOX.lock().max_clock_rate(Clock::Arm)?;
MAILBOX.lock().set_clock_rate(Clock::Arm, max, false)?;
```

This is the same recipe printed by `report_board`
(`cogware-bcm2837\examples\shell\main.rs`). `set_clock_rate` returns the rate actually
granted, and that return value is authoritative. Firmware may clamp the request
according to `config.txt`, voltage and thermal headroom.

The third argument is `skip_turbo`. Passing `false` lets firmware apply its
normal turbo behaviour, which is almost always what is wanted.

`BootInfo::arm_clock` carries the current and maximum ARM rates, read by
`probe_clock()` in the same early window as `probe_memory()`. It is a read, and
only a read.

The kernel deliberately does not clock the CPU up automatically at boot.
Raising `Clock::Arm` without matching `config.txt` settings is how boards
become unstable or corrupt SD cards. The relevant keys are `arm_freq`,
`over_voltage`, `core_freq`, `sdram_freq` and `force_turbo`. Be especially
careful with `force_turbo=1`: it sets the permanent warranty bit.

After changing clocks, monitor thermal headroom through `Tag::GetTemperature`
and `Tag::GetMaxTemperature`, declared in
`cogware-bcm2837\src\drivers\mailbox.rs`.

## GPU and framebuffer hook
`Device` is declared at `cogware-bcm2837\src\drivers\mailbox.rs`:

```text
SdCard=0  Uart0=1  Uart1=2  UsbHcd=3  I2c0=4
I2c1=5    I2c2=6   Spi=7    Ccp2tx=8  V3d=10
```

`map::V3D` is `PERIPHERAL_BASE + 0x00C0_0000`, or `0x3FC0_0000`
(`cogware-bcm2837\src\bsp\memory.rs`). Its registers read back as zero until the V3D
power domain is enabled. The first operation in any 3D driver must be:

```rust
MAILBOX.lock().set_power_state(Device::V3d, true)?;
```

The framebuffer tags already exist in `Tag`:

```text
AllocateFramebuffer 0x00040001
ReleaseFramebuffer  0x00048001
SetPhysicalSize     0x00048003
SetVirtualSize      0x00048004
SetDepth            0x00048005
SetPixelOrder       0x00048006
GetPitch            0x00040008
```

They are at `cogware-bcm2837\src\drivers\mailbox.rs`. `Framebuffer::init`
sets physical size, virtual size, virtual offset, depth and pixel order,
allocates the framebuffer, then reads the pitch — as **one** message, so the
firmware chooses a coherent configuration.

That is what `Mailbox::call_many(&mut [TagIo])` is for; `Mailbox::call` remains
the single-tag convenience wrapper. `BUFFER_WORDS = 64` was sized for this.

### Tags the BSP does not name

`Tag` enumerates the tags the BSP uses itself. The property interface is much
larger than that and only partly documented, so a downstream kernel will
eventually want one that is not there — `GET_THROTTLED` (`0x00030046`) is the
usual first example.

Forking the crate to add a discriminant is a poor trade, so the identifier can
be sent directly:

```rust
let mut reply = [0u32; 1];
drivers::MAILBOX.lock().call_raw(0x0003_0046, &[0], &mut reply)?;
```

`TagIo::raw(id, request, response)` is the multi-tag equivalent, and `TagIo`
stores the identifier as a `u32` for exactly this reason. `Mailbox::call` and
`TagIo::new` are thin wrappers over the raw pair — nothing is lost by using
either. `examples/hello-uart.rs` reads the throttle flags this way.

`AllocateFramebuffer` returns a VideoCore bus address. The ARM must convert it
with `bus::to_physical()` before dereferencing it. The resulting range lies
inside `VideoCore DRAM (GPU)`, which `mmu::memory_map()` already maps
Normal-NC. That is the payoff of probing the memory split before enabling the
MMU: the framebuffer will not need an extra mapping step.

The framebuffer driver now uses these tags for mode setting and double
buffering; see [15-framebuffer.md](15-framebuffer.md).


