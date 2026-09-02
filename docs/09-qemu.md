# QEMU on Windows

This note records how to run the kernel under QEMU and, just as importantly,
where QEMU differs from a real Raspberry Pi 3. It complements
[01-toolchain-and-build.md](01-toolchain-and-build.md),
[02-boot-and-exception-levels.md](02-boot-and-exception-levels.md),
[03-memory-and-mmu.md](03-memory-and-mmu.md), [04-smp.md](04-smp.md),
[07-exceptions-and-irq.md](07-exceptions-and-irq.md),
[08-jtag-and-debugging.md](08-jtag-and-debugging.md), and
[11-videocore-mailbox.md](11-videocore-mailbox.md).

The setup is Windows-native. The verified emulator is QEMU 11.1.0 from Scoop:

```text
C:\Users\ninja\scoop\apps\qemu\current\qemu-system-aarch64.exe
```

It has the `raspi3b` machine.

## Supported entry points

The supported ways to run the kernel are now the two PowerShell scripts in
`scripts\`. They encode the Windows quirks, the correct UART ordering, and the
QEMU machine options, so prefer them over hand-written command lines.

`scripts\run-qemu.ps1` is for live interactive runs. It refreshes the user
`PATH`, optionally builds the selected profile, locates
`target\aarch64-unknown-none\<profile>\kernel`, and launches QEMU with the
PL011 UART attached to the console so boot output can be watched as it happens.
Its QEMU argument list is:

```text
-M raspi3b -serial mon:stdio -serial null -display none -kernel <elf>
```

The parameters are:

```text
-Example     example kernel to boot; default shell
-Gdb         add -s -S; gdbstub on tcp::1234, CPU halted at reset
-Trace       add -d int,guest_errors -D int.log for exception logging
-Profile     release or debug; default release
-NoBuild     skip the cargo build step and boot the existing ELF
-Display     open a graphics window showing the VideoCore framebuffer
-SdImage     attach a raw disk image as the SD card, creating it if absent
-SdSizeMb    size of an image the script has to create; default 64
-AutoDraw    implies -Display; send -Startup once boot settles, then stay
             interactive
-Startup     what -AutoDraw sends; default 'f','g'
-MonitorPort with -AutoDraw, expose the HMP monitor on a loopback port
-BusDevices  attach the emulated I2C and SPI chips the `buses` example wants
             (implied when -Example buses)
```

Examples:

```powershell
.\scripts\run-qemu.ps1
.\scripts\run-qemu.ps1 -Example hello-uart
.\scripts\run-qemu.ps1 -NoBuild
.\scripts\run-qemu.ps1 -Trace
.\scripts\run-qemu.ps1 -Gdb
.\scripts\run-qemu.ps1 -Display -SdImage .\sd.img
.\scripts\run-qemu.ps1 -AutoDraw -SdImage .\sd.img
```

`-AutoDraw` exists because a demo that requires the viewer to type two keys
before anything appears is a bad demo. It cannot use `mon:stdio` — see the
Windows console trap below — so it switches the console onto a named pipe and
relays it by hand, forwarding real keystrokes with `[Console]::ReadKey` once
the startup commands have gone out. The window stays fully interactive.

`scripts\qemu-console-test.ps1` is for non-interactive shell tests and
transcripts. It does not build; build the chosen profile first. It boots the ELF
and drives the PL011 programmatically, echoing output and writing a transcript.
Its key parameters are:

```text
-Commands    console keystrokes to send, in order
-Example     example kernel to boot; default shell
-LogPath     transcript path; default console-test.log
-TimeoutSec  hard limit for the run; default 60
-SdImage     raw disk image to attach as the SD card
-Screenshot  write a PPM screendump once all commands have been sent
-MonitorPort loopback port for the monitor socket -Screenshot drives
-BusDevices  attach the emulated I2C and SPI chips the `buses` example wants
             (implied when -Example buses)
```

The script also has `-SettleMs` for the quiet period before the next command and
`-Profile` for `release` or `debug`. Examples:

```powershell
.\scripts\qemu-console-test.ps1 -Commands 'h','m','s','b'
.\scripts\qemu-console-test.ps1 -Commands 'd' -LogPath abort.log
.\scripts\qemu-console-test.ps1 -Commands 'f','g' -Screenshot card.ppm
.\scripts\qemu-console-test.ps1 -Commands 'i','x' -SdImage sd.img
.\scripts\qemu-console-test.ps1 -Example buses      # adds the chips itself
.\scripts\qemu-console-test.ps1 -Example bitbang
```

`-Screenshot` is what makes the framebuffer testable without a human looking at
a window: the PPM is a plain `P6` file of `width * height * 3` bytes plus a
short header, so it can be diffed or converted.

## Cargo runner and artefacts

`cargo run --release --example shell` works because `.cargo\config.toml` sets
this runner:

```text
qemu-system-aarch64 -M raspi3b -serial mon:stdio -serial null -display none -kernel
```

Cargo appends the linked example ELF after `-kernel`. QEMU reads the ELF
headers directly, so no flat binary is needed for emulation. The `image` alias
is for the real SD-card artefact:

```text
cargo image --example shell -- -O binary kernel8.img
```

Point QEMU and GDB at the ELF, not `kernel8.img`; the ELF keeps full DWARF.
The `image`, `disasm` and `syms` aliases require `cargo-binutils`,
`llvm-tools` and `rust-src`.

## Machine and serial traps

`-M raspi3b` means BCM2837, 1 GiB RAM, and four Cortex-A53 cores. Never pass
`-smp`: the machine hard-codes `min == max == 4`, and an explicit `-smp` makes
QEMU fail before the kernel runs.

Serial order matters. QEMU `serial0` is the PL011 UART0 and is the kernel
console, so the first serial backend must be `mon:stdio` or the test harness's
`pipe:<name>`. QEMU `serial1` is the AUX mini UART, which this kernel discards
with `-serial null`.

`-serial mon:stdio` multiplexes the QEMU monitor onto the UART channel:

```text
Ctrl-A then X    quit QEMU
Ctrl-A then C    toggle between UART and monitor
```

## Windows console harness trap

QEMU's Windows stdio chardev will not accept piped or redirected stdin. A
command such as `$input | qemu-system-aarch64 -serial stdio` silently fails to
deliver bytes to the guest. Do not “simplify” the harness back to stdio.

`scripts\qemu-console-test.ps1` therefore uses:

```text
-serial pipe:<name>
```

On Windows this makes QEMU create a duplex named pipe at:

```text
\\.\pipe\<name>
```

The script connects to that pipe with a .NET `NamedPipeClientStream`, giving a
bidirectional channel to the kernel's PL011.

There is a second trap in the harness itself: it must carry the pending
`ReadAsync` task across loop iterations rather than abandoning it on a
`Wait()` timeout. An abandoned read still completes into the shared buffer
where nothing collects it, clipping the first chunk of the next response.
That is the general rule for async reads over named pipes: a timeout on the
wait is not cancellation of the I/O.

## Monitor trap: only one `pipe:` chardev

Three approaches to getting a screendump out of QEMU on Windows look correct
and fail silently. Recorded here so nobody repeats them:

```text
-monitor pipe:mon                 breaks the SERIAL pipe
-monitor tcp:...,server,nowait    connects, then never answers
-mon chardev=...                  deprecated in QEMU 11, same silent failure
```

The first is the nasty one. On Windows QEMU only ever creates **one** `pipe:`
chardev, so adding `-monitor pipe:` makes the `-serial pipe:` one quietly
disappear and the harness fails with "could not connect to `\\.\pipe\...`".
The two look unrelated, so the obvious conclusion — that the serial pipe name
is wrong — is wrong.

The combination that works:

```text
-chardev socket,id=mon0,host=127.0.0.1,port=N,server=on,wait=off
-object monitor-hmp,id=hmp0,chardev=mon0
```

HMP is then a plain line protocol over TCP: connect, allow about 500 ms for the
banner, write the command, and allow a couple of seconds for QEMU to walk the
framebuffer.

```text
screendump C:/forward/slashes/out.ppm
```

Use forward slashes. The monitor treats a backslash as an escape character, so
a Windows path silently produces a mangled filename or no file at all.

## Display trap: `-show-cursor` was removed

`-show-cursor` is a hard parse error in QEMU 11, not a warning:

```text
qemu-system-aarch64.exe: -show-cursor: invalid option
```

It is a display sub-option now:

```text
-display default,show-cursor=on
```

Because it fails at argument parsing, QEMU exits before printing anything the
guest would have produced. If a launcher script "does nothing" and no window
appears, check the QEMU argument list before suspecting the kernel.

## GDB stub

Use the wrapper:

```powershell
.\scripts\run-qemu.ps1 -Gdb
```

or the Cargo runner form:

```powershell
cargo run --release --example shell -- -s -S
```

Then attach from another PowerShell window:

```powershell
aarch64-none-elf-gdb target\aarch64-unknown-none\release\examples\shell -ex "target remote :1234"
```

Use `aarch64-none-elf-gdb` from the ARM GNU Toolchain. Scoop's separate `gdb`
package is x86-only and cannot debug AArch64. Point GDB at the ELF, not the raw
`kernel8.img`.

The stub is verified for symbol resolution, source-level breakpoints,
backtraces through inlined frames, and all four CPUs appearing as GDB threads.
At reset, `cpsr` reads `0x400003cd`, proving QEMU enters at EL3. Harmless noise
to ignore:

```text
warning: could not convert 'main' from the host encoding (CP1252) to UTF-32
Backtrace stopped: previous frame identical to this frame (corrupt stack?)
```

The second message appears at `_start`, which has no caller. See
[08-jtag-and-debugging.md](08-jtag-and-debugging.md) for the fuller debugging
workflow, including OpenOCD on real hardware.

## Where QEMU lies

QEMU is good enough to exercise much of the kernel, but it is not the board.
Record the differences, because several of them cost real debugging time.

The largest SMP difference is reset behaviour. Real Raspberry Pi 3 firmware
parks cores 1-3 in the spin table at physical address `0xd8`, and only core 0
enters the kernel. QEMU's `raspi3b` releases all four cores at the kernel entry
point, `0x80000`, at reset; the spin table is never actually used. That is why
`cogware-bcm2837\src\smp.rs` has `MMU_GATE`: secondaries must wait until core 0 has
published the translation tables before enabling their MMU. Without that gate,
they can fetch through zeroed `.bss` tables and loop on a level-1 instruction
abort at `FAR=ELR=0x86200`.

QEMU enters at EL3, not EL2, so the EL3-to-EL2-to-EL1 path is genuinely
exercised. The generic timer frequency is also QEMU-specific:

```text
CNTFRQ_EL0 = 62500000
```

Real Pi 3 firmware programmes 19.2 MHz. The kernel reads `CNTFRQ_EL0`, so it
uses the right value in both places.

Set/way data-cache maintenance with `dc isw` is a no-op under QEMU. It is still
required on real silicon, and QEMU cannot validate it.

The VideoCore mailbox is emulated, and usefully so. QEMU returns the genuine
Raspberry Pi 3 Model B revision code, a realistic DRAM split, and a stable
temperature:

```text
revision      0x00a02082
ARM DRAM      0x0000000000 + 960 MiB
GPU DRAM      0x003c000000 + 64 MiB
temperature   25.0 C
```

It also reports the VideoCore region at `0x3C00_0000`, which the MMU maps as
Normal non-cacheable. See [11-videocore-mailbox.md](11-videocore-mailbox.md).

The lies are in identity and clocks. `board_serial` returns zero under QEMU,
where real hardware returns a board-specific value. QEMU reports these clocks:

```text
ARM    700 MHz
core   350 MHz
V3D    700 MHz
SDRAM  700 MHz
UART     3 MHz
```

A real Pi 3B is 1200 MHz ARM, 400 MHz core, 300 MHz V3D, 450 MHz SDRAM, and a
48 MHz PL011 reference clock. QEMU also ignores the PL011 baud divisors, so a
UART clock mismatch that would garble output on hardware is invisible in
emulation. The `init_uart_clock=48000000` setting in `firmware\config.txt` can
only be validated on silicon.

The framebuffer is emulated faithfully enough to be useful. QEMU grants the
requested 1280x720 exactly, reports `pitch` 5120 with no row padding, returns
`PixelOrder::Bgr`, and hands back a bus address of `0x3C100000` with **no alias
bits set**. That last detail hides a bug class: `bus::to_physical` masks the top
two bits, and under QEMU the masked and unmasked values are identical, so a
driver that forgot to mask would still work here and fail on real firmware.

What QEMU does not emulate at all is **V3D**. There is no 3D core and no shader
execution regardless of what the clock report says, so GPU compute has to be
developed against real silicon. See [15-framebuffer.md](15-framebuffer.md).

The SD card is emulated when `-drive file=...,if=sd,format=raw` is passed, and
the emulated card is a CSD v1, SD v2 standard-capacity card that reports
product `QEMU!`, serial `0xdeadbeef`, and a **randomised RCA on every boot** —
never hard-code it. What it does not exercise is real card timing (`ACMD41`
completes almost instantly), voltage switching, UHS modes, or a controller that
starts in a bad state. See [14-emmc.md](14-emmc.md).

**SPI0 and all three BSC (I²C) controllers are properly modelled**, as of QEMU
9.0 — `hw/ssi/bcm2835_spi.c` and `hw/i2c/bcm2835_i2c.c` are real
implementations, not stubs, and you can hang real virtual chips off them:

```powershell
-device ds1338,address=0x68,bus=i2c-bus.1     # DS1338 real-time clock
-device tmp105,address=0x49,bus=i2c-bus.1     # TMP105 temperature sensor
-device sst25vf016b,bus=spi                   # 16 Mbit SPI flash
```

The bus IDs are `i2c-bus.0` / `i2c-bus.1` / `i2c-bus.2` and `spi`. Both scripts
add all three devices for the `buses` example, or on `-BusDevices`. The example
reads a genuine JEDEC ID out of the flash and a genuine date out of the RTC, so
this is a real test of the drivers rather than a register-poke check.

Three caveats:

- **AUX SPI1 and SPI2 do not exist** in QEMU. Not stubbed — absent.
- **Repeated START is not emulated.** `bcm2835_i2c.c` says so in a comment, so
  `write_read` degrades to STOP-then-START. It still reads correctly here, and
  the repeated START is still emitted on silicon.
- QEMU begins an I²C transfer on **any** write to `C` with `I2CEN` set, not
  just one with `ST`, and its I²C core only appends to `current_devs`. A
  separate FIFO-clear write therefore leaves a transfer open and makes the
  *next* address answer too — a bus scan reports every device twice. The driver
  folds `CLEAR` into the `ST` write, which the datasheet explicitly sanctions
  and which is faster anyway. See [19-spi-and-i2c.md](19-spi-and-i2c.md).

**GPIO is only half modelled.** `GPSET`/`GPCLR` update an internal level that
`GPLEV` reads back, so output loopback works and `examples/gpio.rs` can verify
it. But `GPEDS` and the four edge/level-enable registers are **absent**, so
async pin waits never fire, and `GPPUD`/`GPPUDCLK` are absent too, so pulls do
nothing and a floating input reads a stale latch. Nothing outside the machine
can drive a pin either. `hal::asynch::edge_detection_works()` probes this at
runtime so an example can say so instead of hanging; `examples/bitbang.rs`
sidesteps it entirely by simulating wired-AND nets in ordinary Rust.

## Expected QEMU boot

A healthy QEMU boot brings all four cores to EL1 with MMU, D-cache, I-cache and
FP/SIMD enabled. The current boot takes about 32-40 ms under emulation.

The memory report is now printed as two ASCII tables: first the physical memory
map, then the kernel image table. The map includes the runtime-probed VideoCore
DRAM region as Normal-NC, and the image table includes the per-core stack rows.
The exact linked bounds still come from linker symbols rather than constants.

The shared tally is deterministic because `BOOT_BARRIER` holds core 0 until the
secondaries have finished their own visits:

```text
Shared tally (SpinLock<Tally>)
  visits    : 4
```

The current help text is exactly:

```text
Console commands
  h  this help          m  memory map
  s  SMP status         j  JTAG status
  t  uptime             c  CPU / cache state
  b  board / clocks     k  async task table
  w  dispatch jobs      a  heap stats + self-test
  f  display mode       g  draw the test card
  i  SD card info       r  hexdump block 0
  x  SD write self-test e  raise BRK #0
  d  raise a data abort p  panic
  anything else echoes
```

The `b` command queries the VideoCore mailbox for board identity, DRAM split,
temperature and clocks. `f` and `g` bring up the display and draw the test
card; `i`, `r` and `x` identify, read and write the SD card, and all three
report "not initialised" unless a card is attached. The `e`, `d` and `p`
commands continue to exercise the breakpoint, data-abort and panic paths.

## Quick checklist

```text
1. Use run-qemu.ps1 (Windows) or run-qemu.sh (Linux) for live runs.
2. Use qemu-console-test.ps1/.sh for scripted input; do not pipe stdin.
3. First serial backend must be PL011: mon:stdio or pipe:<name>.
4. Machine must be -M raspi3b with no -smp.
5. With -Gdb or -S, continue from GDB before expecting output.
6. For early exceptions, use -Trace and inspect int.log.
7. Never add a second pipe: chardev; the serial one disappears.
8. For the monitor use -chardev socket + -object monitor-hmp, nothing else.
9. screendump paths need forward slashes.
```

Every option of every script, on both platforms, is in
[23-scripts.md](23-scripts.md).
