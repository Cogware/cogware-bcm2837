# Drivers and MMIO

The driver layer has eight live drivers: GPIO, PL011 UART0, the VideoCore
property mailbox, the framebuffer, the Arasan EMMC (SD card) controller, SPI0,
and the two I2C masters BSC0 and BSC1.
The module list in `cogware-bcm2837\src\drivers\mod.rs` is `emmc`, `font`,
`framebuffer`, `gpio`, `i2c`, `mailbox`, `mmio`, `pl011`, `qoi` and `spi`.

The common rule is singleton ownership. A peripheral or firmware interface is a
fixed hardware resource, not a normal heap value. The kernel creates one global
instance of each live driver and puts it behind a `SpinLock`:

```rust
pub static GPIO: SpinLock<gpio::Gpio> = SpinLock::new(unsafe { gpio::Gpio::new() });
pub static UART: SpinLock<pl011::Pl011Uart> = SpinLock::new(unsafe { pl011::Pl011Uart::new() });
pub static MAILBOX: SpinLock<mailbox::Mailbox> = SpinLock::new(unsafe { mailbox::Mailbox::new() });
pub static FRAMEBUFFER: SpinLock<framebuffer::Framebuffer> = /* ... */;
pub static EMMC: SpinLock<emmc::Emmc> = /* ... */;
```

`font` and `qoi` have no singleton because they touch no hardware: `font` is a
table of glyphs and `qoi` is a pure decoder. They live under `drivers` because
they exist only to serve the framebuffer.

The locking rule and the MMU prerequisite for locks are covered in
[06-synchronisation.md](06-synchronisation.md). Three of these drivers have
their own page, because they are more than ordinary MMIO:
[11-videocore-mailbox.md](11-videocore-mailbox.md),
[14-emmc.md](14-emmc.md) and [15-framebuffer.md](15-framebuffer.md).

Note one exception to the MMIO rules below. The framebuffer *control* path is
the mailbox, but the framebuffer *pixels* are not MMIO at all: they are ordinary
`Normal-NC` DRAM shared with the GPU. Rule 4 in
the MMIO rule does not apply to them, which is why
`framebuffer.rs` writes pixels through a plain `&mut [u32]` rather than
`RegisterBlock`.

## Adding a peripheral

`cogware-bcm2837\src\drivers\mod.rs` now carries the project recipe for adding a
new peripheral:

1. Add its base address to `crate::bsp::memory::map`. It will already be mapped
   Device-nGnRnE because the MMU maps the whole peripheral window.
2. Describe its registers as a `#[repr(C)]` struct of `mmio::ReadWrite`,
   `mmio::ReadOnly` and `mmio::WriteOnly` cells, then wrap it in an
   `mmio::RegisterBlock` so offsets are checked by the compiler.
3. Add a `pub static` in `drivers\mod.rs` wrapping the singleton in a
   `SpinLock`.

That recipe applies to ARM-visible MMIO blocks. Anything the VideoCore firmware
owns, including clocks, power domains and framebuffer allocation, goes through
`drivers::mailbox` instead.

## Physical address windows

The BCM2837 address constants are in `cogware-bcm2837\src\bsp\memory.rs`. The ARM
peripheral window is `PERIPHERAL_BASE = 0x3F00_0000`
(`cogware-bcm2837\src\bsp\memory.rs`) through `PERIPHERAL_END = 0x4000_0000`
(`cogware-bcm2837\src\bsp\memory.rs`). The ARM-local block begins at
`ARM_LOCAL_BASE = 0x4000_0000` and ends at `ARM_LOCAL_END = 0x4002_0000`
(`cogware-bcm2837\src\bsp\memory.rs`).

Everything in `bsp::memory::map` sits inside the Device-mapped peripheral
window. A new driver needs no new MMU mapping; it needs a register layout and a
`RegisterBlock`.

The map is deliberately broad, so later driver work starts from named constants
rather than rediscovered addresses:

```text
SYSTEM_TIMER   0x3F003000   DMA        0x3F007000
IRQ_CONTROLLER 0x3F00B200   ARM_TIMER  0x3F00B400
MAILBOX        0x3F00B880   PM         0x3F100000
CM             0x3F101000   RNG        0x3F104000
GPIO           0x3F200000   PL011_UART 0x3F201000
SPI0           0x3F204000   I2C0       0x3F205000
PWM            0x3F20C000   AUX        0x3F215000
EMMC           0x3F300000   I2C1       0x3F804000
USB            0x3F980000   V3D        0x3FC00000
SDRAM          0x3FE00000   DMA15      0x3FE05000
```

The source locations are `cogware-bcm2837\src\bsp\memory.rs`. The mini UART
lives in `AUX` but is deliberately unused; PL011 is the console UART because the
mini UART is tied to the drifting VideoCore clock.

`PL011_CLOCK_HZ = 48_000_000` and `CONSOLE_BAUD = 115_200` are at
`cogware-bcm2837\src\bsp\memory.rs`. QEMU may report the UART clock as 3 MHz via
the mailbox, but it ignores baud divisors; the 48 MHz constant is the real Pi 3
firmware default that matters on hardware.

## ARM-local hooks

`bsp::memory::local` begins at `cogware-bcm2837\src\bsp\memory.rs` and describes the
BCM2836/7 ARM-local block at `0x4000_0000`. The fixed registers are
`CONTROL`, `TIMER_PRESCALER` and `GPU_IRQ_ROUTING`
(`cogware-bcm2837\src\bsp\memory.rs`).

The per-core accessors are:

```rust
timer_irq_control(core)
mailbox_irq_control(core)
irq_source(core)
fiq_source(core)
mailbox_write_set(core, mbox)
mailbox_read_clear(core, mbox)
```

They are at `cogware-bcm2837\src\bsp\memory.rs`. `mailbox_write_set` is the IPI
mechanism: writing the appropriate bit raises a mailbox interrupt on another
core. These constants are hooks for the IRQ work described in
[07-exceptions-and-irq.md](07-exceptions-and-irq.md) and
[10-roadmap.md](10-roadmap.md).

## The MMIO layer

`cogware-bcm2837\src\drivers\mmio.rs` is the hand-rolled stand-in for `tock-registers`.
The kernel writes its own register abstraction, so it keeps it explicit:

* `RegisterBlock<T>` binds a physical base address to a `#[repr(C)]` register
  layout (`cogware-bcm2837\src\drivers\mmio.rs`);
* `ReadWrite<T>`, `ReadOnly<T>` and `WriteOnly<T>` model register access
  permissions (`cogware-bcm2837\src\drivers\mmio.rs`);
* every access is volatile through `ptr::read_volatile` or
  `ptr::write_volatile` (`cogware-bcm2837\src\drivers\mmio.rs`).

This gives two useful properties. The `#[repr(C)]` struct lets the compiler
check offsets, and volatile access prevents LLVM from merging, reordering or
eliding the actual MMIO transaction.

`ReadWrite::modify` at `cogware-bcm2837\src\drivers\mmio.rs` is still a plain
read-modify-write:

```rust
pub fn modify(&self, f: impl FnOnce(T) -> T) {
    self.write(f(self.read()));
}
```

It is volatile, not atomic. Callers must hold the driver lock when the register
can be touched by more than one core.

There is no interior locking in `mmio.rs`. `RegisterBlock<T>` is `Send` but not
`Sync`; its docs at `cogware-bcm2837\src\drivers\mmio.rs` say a peripheral must be
owned by one `SpinLock` before it is shared. `RegisterBlock::new` is `unsafe`
(`cogware-bcm2837\src\drivers\mmio.rs`) because the caller must prove the address and
aliasing contract. `regs()` casts the base integer to `&T`
(`cogware-bcm2837\src\drivers\mmio.rs`).

`mmio::barrier()` at `cogware-bcm2837\src\drivers\mmio.rs` emits `dsb sy`. Device
memory already constrains CPU ordering, but the compiler still needs an explicit
barrier when normal memory and MMIO must be observed in a particular order.

## GPIO layout and sharing

The GPIO register block starts at `cogware-bcm2837\src\drivers\gpio.rs`. The fields
used by the driver include:

```text
gpfsel   0x00  six 32-bit function-select registers
gpset    0x1C  two write-only set registers
gpclr    0x28  two write-only clear registers
gplev    0x34  two read-only level registers
gppud    0x94  pull-up/down control
gppudclk 0x98  pull-up/down clock registers
```

The file asserts `gppud == 0x94` and `gplev == 0x34` at
`cogware-bcm2837\src\drivers\gpio.rs`. `MAX_PIN = 53` is at
`cogware-bcm2837\src\drivers\gpio.rs`.

`Function` is declared at `cogware-bcm2837\src\drivers\gpio.rs`. The encodings are
straight from the datasheet:

```text
Input=0b000  Output=0b001  Alt0=0b100  Alt1=0b101
Alt2=0b110   Alt3=0b111    Alt4=0b011  Alt5=0b010
```

GPIO is the showcase for cross-core sharing. `Gpio::set_function` computes
`register = pin / 10` and `shift = (pin % 10) * 3`
(`cogware-bcm2837\src\drivers\gpio.rs`), then performs a read-modify-write of a
packed `GPFSELn` word. Two cores updating different pins in the same word can
silently lose one update unless the operation is serialised.

The singleton lock is `drivers::GPIO` at `cogware-bcm2837\src\drivers\mod.rs`.
`Gpio::new` is `unsafe` (`cogware-bcm2837\src\drivers\gpio.rs`) because constructing
a second instance would create a second owner of the same register block.

The verified SMP demonstration has all four cores read GPIO14 under the lock and
observe `FSEL=0b100`, meaning `Alt0` / `TXD0`. The readback method is
`Gpio::function` at `cogware-bcm2837\src\drivers\gpio.rs`.

## GPIO operations

`set_function` masks and writes one pin's three-bit function field
(`cogware-bcm2837\src\drivers\gpio.rs`). `set_functions` applies that to a slice
(`cogware-bcm2837\src\drivers\gpio.rs`).

`set_pull` implements the Pi 3 legacy `GPPUD` plus `GPPUDCLK` protocol
(`cogware-bcm2837\src\drivers\gpio.rs`). It writes the pull mode, waits
`SETTLE_CYCLES = 150` (`cogware-bcm2837\src\drivers\gpio.rs`), clocks the chosen
pins, waits again, and clears the control and clock registers. The waits are
`cpu::spin_cycles` calls at `cogware-bcm2837\src\drivers\gpio.rs`.

`set_high`, `set_low` and `level` use `GPSETn`, `GPCLRn` and `GPLEVn`
(`cogware-bcm2837\src\drivers\gpio.rs`). `route_pl011` selects ALT0
for pins 14 and 15 and disables their pulls (`cogware-bcm2837\src\drivers\gpio.rs`).

## PL011 layout

The PL011 driver is `cogware-bcm2837\src\drivers\pl011.rs`. Its register layout begins
at `cogware-bcm2837\src\drivers\pl011.rs`:

```text
DR    0x00  data
FR    0x18  flags
IBRD  0x24  integer baud divisor
FBRD  0x28  fractional baud divisor
LCR_H 0x2C  line control
CR    0x30  control
IMSC  0x38  interrupt mask set/clear
ICR   0x44  interrupt clear
```

Offset checks for `FR`, `IBRD`, `CR` and `ICR` are at
`cogware-bcm2837\src\drivers\pl011.rs`.

The main bit constants are `FR.BUSY = 1 << 3`, `FR.RXFE = 1 << 4`,
`FR.TXFF = 1 << 5`, `LCR_H.FEN = 1 << 4`, `LCR_H.WLEN_8 = 0b11 << 5`,
`CR.UARTEN = 1 << 0`, `CR.TXE = 1 << 8`, `CR.RXE = 1 << 9`,
`IMSC.RXIM = 1 << 4`, `IMSC.RTIM = 1 << 6`, and `ICR_ALL = 0x7FF`
(`cogware-bcm2837\src\drivers\pl011.rs`).

## PL011 initialisation and I/O

`BaudDivisors::new` at `cogware-bcm2837\src\drivers\pl011.rs` computes `IBRD` and
`FBRD` in integer arithmetic. The compile-time checks at
`cogware-bcm2837\src\drivers\pl011.rs` include the real console case: 48 MHz at
115200 baud gives `IBRD = 26` and `FBRD = 3`.

`Pl011Uart::init` starts at `cogware-bcm2837\src\drivers\pl011.rs`. The order is
mandatory:

1. write `CR = 0`;
2. spin while `FR.BUSY`;
3. write `LCR_H = 0`, clearing `FEN` and flushing the FIFOs;
4. write `ICR = ICR_ALL`;
5. write `IBRD`;
6. write `FBRD`;
7. write `LCR_H = WLEN_8 | FEN`;
8. write `CR = UARTEN | TXE | RXE`.

The `LCR_H` write latches the `IBRD`/`FBRD` divisor pair, so it must come after
both divisor writes. `console::init` also does the hardware-level order: route
GPIO14/15 first, then initialise the UART (`cogware-bcm2837\src\console.rs`).

`write_byte` spins while `FR.TXFF` and writes `DR`
(`cogware-bcm2837\src\drivers\pl011.rs`). `write_str_crlf` expands `\n` to `\r\n`
(`cogware-bcm2837\src\drivers\pl011.rs`). `read_byte` returns `None` when `FR.RXFE`
is set (`cogware-bcm2837\src\drivers\pl011.rs`). `flush` waits for `FR.BUSY` to
clear (`cogware-bcm2837\src\drivers\pl011.rs`).

The console is polled today. `console::next_byte()` wraps `read_byte` in a
future that calls `task::poll_again()` and returns `Pending` when the FIFO is
empty, so the shell task sleeps between keystrokes instead of spinning — see
[`12-async.md`](12-async.md). `enable_rx_interrupts` sets `RXIM | RTIM` in
`IMSC` (`cogware-bcm2837\src\drivers\pl011.rs`), and `clear_interrupts` writes
`ICR_ALL` (`cogware-bcm2837\src\drivers\pl011.rs`). Those are the hooks for the IRQ
work in [07-exceptions-and-irq.md](07-exceptions-and-irq.md).

## VideoCore property mailbox

The mailbox block is at `0x3F00_B880`, `map::MAILBOX`
(`cogware-bcm2837\src\bsp\memory.rs`). It is the ARM's doorbell to VideoCore
firmware, not a normal peripheral configuration block. Clocks, power domains,
the DRAM split and framebuffer allocation all go through it.

`drivers::MAILBOX` is at `cogware-bcm2837\src\drivers\mod.rs`. Its lock protects two
things: the mailbox registers and the single static 16-byte-aligned message
buffer, `MESSAGE`, declared at `cogware-bcm2837\src\drivers\mailbox.rs`. The buffer
is `BUFFER_WORDS = 64` words (`cogware-bcm2837\src\drivers\mailbox.rs`), sized with
future multi-tag framebuffer setup in mind.

The register constants live in `bsp::memory::map::mailbox`. `MB0` is
VideoCore-to-ARM at `0x3F00_B880`, and `MB1` is ARM-to-VideoCore at
`0x3F00_B8A0` (`cogware-bcm2837\src\bsp\memory.rs`). Reading MB1 or writing MB0
hangs. Offsets repeat inside each mailbox: `DATA = 0x00`, `PEEK = 0x10`,
`SENDER = 0x14`, `STATUS = 0x18`, `CONFIG = 0x1C`
(`cogware-bcm2837\src\bsp\memory.rs`). Status bits are `FULL = 1 << 31` and
`EMPTY = 1 << 30` (`cogware-bcm2837\src\bsp\memory.rs`).

The channel constants are `POWER = 0`, `FRAMEBUFFER = 1`, `VIRTUAL_UART = 2`,
`PROPERTY_ARM_TO_VC = 8` and `PROPERTY_VC_TO_ARM = 9`
(`cogware-bcm2837\src\bsp\memory.rs`). The low four bits of the mailbox word are
the channel, which is why the shared message buffer is 16-byte aligned.

## Mailbox message and coherency rules

`Mailbox::call` at `cogware-bcm2837\src\drivers\mailbox.rs` builds one property tag
per message. The message layout is:

```text
[0] total size in bytes including the header
[1] request/response code: 0 in, 0x8000_0000 on success out
[2] tag id
[3] value buffer size in bytes
[4] request code 0 in; 0x8000_0000 | response length out
... value words
[n] 0 end tag
```

The VideoCore writes the reply back into the same buffer. `Mailbox::call` sizes
the value area to `request.len().max(response.len())`
(`cogware-bcm2837\src\drivers\mailbox.rs`) because some tags return more than they
receive. Sizing only for the request is a classic mailbox bug.

There are two coherency rules. First, the address written to MB1 must be a
VideoCore bus address, specifically the uncached alias. `bsp::memory::bus` says
that SDRAM aliases are `0x0000_0000` L1+L2 cached, `0x4000_0000` L2 coherent,
`0x8000_0000` L2 cached non-allocating and `0xC000_0000` fully uncached
(`cogware-bcm2837\src\bsp\memory.rs`). `bus::to_bus()` ORs in
`0xC000_0000`; `bus::to_physical()` masks the alias off.

Second, once the MMU is on, the ARM's own mapping of the buffer is Normal
cacheable. `Mailbox::flush` walks each 64-byte cache line and calls
`mmu::clean_invalidate_line`, then issues ordering (`cogware-bcm2837\src\drivers\mailbox.rs`).
Before `mmu::enable()`, all access is Device-nGnRnE and uncached, so early boot
mailbox transactions skip cache maintenance. This is exactly why
`probe_memory()` runs before `mmu::init()`.

Full details, including the public tags, clocking and GPU path, are in
[11-videocore-mailbox.md](11-videocore-mailbox.md).

## Memory split and the new memory type

The mailbox is now part of memory-map construction. `kernel_boot_primary` in
`cogware-bcm2837\src\boot.rs` calls `drivers::mailbox::probe_memory()` before
`mmu::init()`.
`probe_memory` queries `GetVcMemory` and calls `bsp::memory::set_vc_memory`
(`cogware-bcm2837\src\drivers\mailbox.rs`).

`set_vc_memory` rounds the VideoCore base down to a 2 MiB boundary and records
it in `VC_MEM_BASE`, `VC_MEM_SIZE` and `VC_MEM_PROBED`
(`cogware-bcm2837\src\bsp\memory.rs`). Rounding down may map a little ARM memory
non-cacheable, but it avoids mapping GPU-owned memory cacheable, which is the
safe direction.

`mmu::memory_map()` now returns eight regions (`cogware-bcm2837\src\arch\mmu.rs`).
It emits `free DRAM (ARM)` as `kernel_end..vc_mem_base()` and
`VideoCore DRAM (GPU)` as `vc_mem_base()..min(base + size, PERIPHERAL_BASE)`
(`cogware-bcm2837\src\arch\mmu.rs`). The clamp matters because the firmware
reports the reservation as extending to the top of SDRAM, while the top 16 MiB
is shadowed by the peripheral window in the ARM view.

`MemoryKind::NormalNonCacheable` is the third memory type
(`cogware-bcm2837\src\arch\mmu.rs`). It uses MAIR attribute index 2, byte `0x44`,
so `MAIR_EL1` is `0x0044_FF00` (`cogware-bcm2837\src\arch\mmu.rs`). GPU-shared
memory is Normal-NC rather than Device because Device memory forbids unaligned
access and makes ordinary `memcpy` illegal, while a framebuffer needs both.

## Board and clock reporting

The shell command `b` calls `report_board()` (`cogware-bcm2837\examples\shell\main.rs`). That
function locks `MAILBOX`, prints the board revision and serial, ARM and GPU DRAM
ranges, temperature, and current/max clock rates for `Clock::Arm`, `Core`,
`V3d`, `Sdram` and `Uart` (`cogware-bcm2837\examples\shell\main.rs`).

In QEMU the verified board report identifies revision `0x00a02082`, a genuine
Raspberry Pi 3 Model B code, and a 960 MiB ARM / 64 MiB GPU split. The same
command prints the headline CPU-clocking recipe:

```rust
let max = MAILBOX.lock().max_clock_rate(Clock::Arm)?;
MAILBOX.lock().set_clock_rate(Clock::Arm, max, false)?;
```

The kernel does not do this automatically. Raising the ARM clock without
matching `config.txt` voltage and frequency settings can make boards unstable or
corrupt SD cards. Use the mailbox to query and request clocks, but treat the
firmware's returned rate as authoritative. See
[11-videocore-mailbox.md](11-videocore-mailbox.md) for the warnings and the GPU
bring-up hooks.

## Locking discipline

The live driver locks are GPIO, UART, MAILBOX, FRAMEBUFFER, EMMC, SPI0, I2C0
and I2C1.

UART, MAILBOX and EMMC are still taken one at a time and never nested. There
are exactly **two** nested orders, and both need stating explicitly rather than
being left to be discovered:

```text
FRAMEBUFFER  ->  MAILBOX          allowed, and unavoidable
MAILBOX      ->  FRAMEBUFFER      forbidden

SPI0/I2C0/I2C1  ->  GPIO          allowed, and unavoidable
GPIO  ->  SPI0/I2C0/I2C1          forbidden
```

The first is unavoidable because setting a mode, releasing a buffer and
flipping a page are all property-mailbox transactions, and they are all
operations *on* the framebuffer. `Framebuffer::init`, `release` and `flip` each
take `MAILBOX` internally, and callers legitimately hold `FRAMEBUFFER` across
them:

```rust
let mut fb = drivers::FRAMEBUFFER.lock();
// ... drawing ...
let result = fb.flip();   // takes MAILBOX while FRAMEBUFFER is held
drop(fb);
```

Deadlock freedom follows from the order being total and one-directional: no
code path ever takes `MAILBOX` first and then `FRAMEBUFFER`. If a future
mailbox operation needs the framebuffer, it must take `FRAMEBUFFER` first.

The second nesting is the same shape. `Spi0::init` and `I2c::init` have to
switch their pins to `ALT0`, which is a read-modify-write of `GPFSEL` and
therefore takes `GPIO`, while the caller already holds the bus lock:

```rust
let mut spi = drivers::SPI0.lock();
spi.init(Config::default());   // takes GPIO while SPI0 is held
```

The same appears in the bit-banged buses, whose `OpenDrainPin` transitions are
`GPFSEL` writes. Nothing ever takes `GPIO` and then a bus lock, so the order is
again total.

The *hot* GPIO paths take no lock. `GPSET`, `GPCLR` and `GPEDS` are
write-1-to-act and `GPLEV` is a pure read, so there is no read-modify-write to
protect and `set_high`/`is_high` compile to a single instruction. Only
`GPFSEL` and the `GPPUD` sequence take `GPIO`. See [`18-gpio.md`](18-gpio.md).

There is a third, subtler rule in the same code. `SpinLock` is not reentrant
and `println!` takes `UART`, so **nothing may print while holding
`FRAMEBUFFER`**. `draw_test_card` obeys this by ending with an explicit
`drop(fb)` before its report, and the expensive QOI decode happens *before* the
lock is taken so the framebuffer is not held across several milliseconds of
work that does not need it.

The same discipline applies to `EMMC`: `report_sd_card`, `dump_sd_block` and
`sd_write_test` each take the lock, complete one operation, and release it
rather than holding it across a whole command — which is why `sd_write_test`
locks twice, once to write and once to read back.

Early boot has two documented bypasses. `console::init` bypasses GPIO and UART
because the MMU is off and no secondary core is running
(`cogware-bcm2837\src\console.rs`). `drivers::mailbox::probe_memory` bypasses
MAILBOX for the same single-core, MMU-off reason
(`cogware-bcm2837\src\drivers\mailbox.rs`). After each core enables its MMU,
shared drivers must be accessed through their `SpinLock`s.
