# Host setup

Everything needed to build, run and debug this kernel, on **Linux** and on
**Windows**, for **QEMU** and for **real hardware**. It is the practical
companion to [01-toolchain-and-build.md](01-toolchain-and-build.md), which
explains *why* the build is configured the way it is rather than how to install
it.

Work out which column you are in first — you do not need the whole table.

| You want to | Rust + target | QEMU | Cross binutils | USB-TTL cable | JTAG probe |
| --- | :-: | :-: | :-: | :-: | :-: |
| Build only | ● | | | | |
| Run under emulation | ● | ● | | | |
| Produce `kernel8.img` for a card | ● | | ● | | |
| Run on a real Pi 3 | ● | | ● | ● | |
| Source-level debug under QEMU | ● | ● | ● | | |
| Source-level debug on hardware | ● | | ● | ● | ● |

---

## 1. Rust, on either OS

Identical everywhere, and the only genuinely mandatory step.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh    # Linux
```
```powershell
winget install Rustlang.Rustup                                    # Windows
```

Then, from the repository root:

```bash
rustup show
```

`rust-toolchain.toml` is honoured automatically, so that single command
installs the pinned channel, the `aarch64-unknown-none` target and the
`rust-src`, `llvm-tools`, `rustfmt` and `clippy` components. There is nothing
else to add and no nightly involved.

```toml
[toolchain]
channel = "stable"
profile = "minimal"
components = ["rust-src", "llvm-tools", "rustfmt", "clippy"]
targets = ["aarch64-unknown-none"]
```

> **The target is `aarch64-unknown-none`, not `aarch64-unknown-none-softfloat`.**
> The hard-float target is a hard requirement, not a preference: the boot path
> ungates FP/SIMD and the trap frame saves all 32 NEON registers precisely so
> the FPU can be used. See [17-floating-point.md](17-floating-point.md).

`cargo build --release --examples` should now succeed with no further tools.
Everything below is for *running* the result.

---

## 2. Windows

The reference setup for this project, and the one every command in these docs
was verified against. No WSL, no administrator rights.

```powershell
# Scoop itself, if you do not already have it
Set-ExecutionPolicy -ExecutionPolicy RemoteSigned -Scope CurrentUser
Invoke-RestMethod -Uri https://get.scoop.sh | Invoke-Expression

scoop install qemu                      # main bucket
scoop bucket add extras                 # the toolchain lives here
scoop install gcc-aarch64-none-elf
```

Verified versions:

```text
QEMU 11.1.0                 qemu-system-aarch64          (scoop main)
Arm GNU Toolchain 15.2.Rel1 aarch64-none-elf-{gcc,gdb,objcopy,objdump,nm}  (scoop extras)
Rust 1.98.0 stable          edition 2024
```

`winget install SoftwareFreedomConservancy.QEMU` also works if you prefer it.
Neither winget nor Chocolatey packages the bare-metal Arm toolchain; if the
Scoop route is unavailable, take the
`arm-gnu-toolchain-<version>-mingw-w64-x86_64-aarch64-none-elf.zip` from
[Arm's download page](https://developer.arm.com/downloads/-/arm-gnu-toolchain-downloads)
and put its `bin` directory on `PATH`.

> **Scoop's plain `gdb` package is an x86 GDB and cannot debug AArch64 at all.**
> Use `aarch64-none-elf-gdb` from the Arm toolchain.

### PowerShell gotchas

Automation sessions inherit a stale `PATH`, so tools installed by Scoop can be
invisible even though they are installed. Refresh it:

```powershell
$env:PATH = [Environment]::GetEnvironmentVariable('Path','User') + ';' + $env:PATH
```

Use Windows Terminal rather than `cmd.exe`. QEMU's `-serial mon:stdio` emits
ANSI escapes that the legacy console renders badly.

### WSL2

Works, and is a reasonable choice if you prefer Linux tooling — follow §3
inside the distribution. The one thing it makes *harder* is hardware: a USB-TTL
cable or JTAG probe has to be forwarded with
[`usbipd-win`](https://github.com/dorssel/usbipd-win), re-attached after every
unplug and every WSL restart. If you are talking to a real board, native
Windows is less trouble.

---

## 3. Linux

### QEMU

```bash
sudo apt install qemu-system-arm        # Debian, Ubuntu
sudo dnf install qemu-system-aarch64    # Fedora
sudo pacman -S qemu-system-aarch64      # Arch
```

Version matters more here than it usually does:

| Distribution | Package | Ships | Verdict |
| --- | --- | --- | --- |
| Debian 12 Bookworm | `qemu-system-arm` | 7.2 | boots; no emulated bus devices |
| Ubuntu 24.04 LTS | `qemu-system-arm` | 8.2 | boots; I²C devices work |
| Ubuntu 22.04 LTS | `qemu-system-arm` | 6.2 | bare minimum; expect gaps |
| Fedora 44+ | `qemu-system-aarch64` | 10.2+ | everything |
| Arch | `qemu-system-aarch64` | current | everything |

The floor is **QEMU 6.2**: `raspi3b` arrived in 6.1, and 6.2 *removed* the old
`raspi3` spelling, so `-M raspi3` is an error on anything modern.

Two later additions matter if you want the `buses`, `async-bus` or `bitbang`
examples to talk to emulated chips:

* the **BSC (I²C)** controllers, which arrived during the 7.x–8.x series;
* the **SPI0** controller. QEMU's own `raspi` page still listed SPI under
  *Missing devices* as recently as 8.1.5, but it is present and working in
  **11.1.0** — `-device sst25vf016b,bus=spi` returns the real JEDEC ID
  `BF 25 41`, which is how this was established rather than taken on trust.

So: **QEMU ≥ 6.2 to boot, ≥ 8 for I²C, ≥ 11 for SPI.** If your distribution is
behind, build just the one target you need:

```bash
sudo apt-get install git build-essential ninja-build pkgconf python3-venv \
    libglib2.0-dev libfdt-dev libpixman-1-dev zlib1g-dev

git clone --depth 1 --branch v11.1.0 https://gitlab.com/qemu-project/qemu.git
cd qemu && mkdir build && cd build
../configure --target-list=aarch64-softmmu --disable-docs
make -j"$(nproc)"
sudo make install
```

`--target-list=aarch64-softmmu` cuts the build from all architectures to one
and takes it down to a few minutes.

### Cross binutils and a debugger

You need `objcopy` to produce `kernel8.img`, and a GDB that understands
AArch64. Three ways, in descending order of preference:

**a. Arm GNU Toolchain** — the same thing the Windows setup uses, so commands
in these docs match exactly.

```bash
curl -LO https://developer.arm.com/-/media/Files/downloads/gnu/<version>/binrel/arm-gnu-toolchain-<version>-x86_64-aarch64-none-elf.tar.xz
sudo tar xJf arm-gnu-toolchain-*-x86_64-aarch64-none-elf.tar.xz -C /opt
echo 'export PATH=/opt/arm-gnu-toolchain-<version>-x86_64-aarch64-none-elf/bin:$PATH' >> ~/.bashrc
```

Take the current `<version>` from
[Arm's download page](https://developer.arm.com/downloads/-/arm-gnu-toolchain-downloads);
the file naming scheme is stable but the version is not.

**b. Distribution packages.**

```bash
sudo apt install binutils-aarch64-linux-gnu gdb-multiarch
```

`aarch64-linux-gnu-objcopy` converts a bare-metal ELF perfectly well — the ELF
container is the same regardless of the OS ABI in the header. For debugging use
`gdb-multiarch`, **not** plain `gdb`: a distribution `gdb` on an x86-64 host is
built for that host only and refuses an AArch64 ELF. Fedora and Arch build
multi-architecture support into their ordinary `gdb` package, so there is no
separate `gdb-multiarch` to install there.

```gdb
(gdb) set architecture aarch64
(gdb) target remote :1234
```

**c. LLVM, already on your disk.** The pinned toolchain includes `llvm-tools`,
so after one `cargo install cargo-binutils` the `cargo image` and `cargo disasm`
aliases work with no system packages at all. That covers `objcopy` and
`objdump`, but gives you no debugger — pair it with `gdb-multiarch`.

### OpenOCD, only for hardware JTAG

```bash
sudo apt install openocd        # or dnf / pacman
find / -name bcm2837.cfg 2>/dev/null
```

---

## 4. Check the install

```bash
cargo build --release --examples
cargo run   --release --example hello-uart
```

Expected:

```text
Hello from the BCM2837.

  core            : 0
  exception level : EL1
  FP/SIMD         : yes
  MMU             : yes
  ...
Boot complete in 17.242192ms. All checks passed.
```

A kernel halts rather than exiting, so QEMU stays up; interrupt it with
`Ctrl-A x` (or `Ctrl+C` on the wrapper scripts).

On Windows the scripts are more convenient than `cargo run`, because they can
bound the run and capture a transcript:

```powershell
.\scripts\run-qemu.ps1 -Example hello-uart
.\scripts\qemu-console-test.ps1 -Example shell -TimeoutSec 40
```

`run-qemu.ps1` is the interactive one and also carries `-Gdb`, `-Trace`,
`-Profile` and `-NoBuild`. `qemu-console-test.ps1` drives the console
non-interactively through a named pipe and writes `console-test.log`, which is
what to reach for when something needs to be diffed rather than watched.

Two rules about QEMU that cause almost every "it boots but says nothing":

* **The first `-serial` is the PL011**, which is the kernel console. The second
  is the AUX mini UART and is discarded. Reversing them gives a silent boot.
* **Never pass `-smp`.** `raspi3b` fixes the core count at four, and overriding
  it produces a machine that cannot start.

See [09-qemu.md](09-qemu.md) for the emulated-device arguments and where the
emulation diverges from silicon.

---

## 5. Real hardware: the SD card

### 5.1 Partition

One **FAT32** primary partition, at least 64 MiB, marked bootable. The
VideoCore reads nothing else — there is no second partition and no filesystem
for the kernel to find.

```bash
sudo mkfs.vfat -F 32 -n BOOT /dev/sdX1     # check the device name twice
```

### 5.2 Firmware

The ARM is not the boot processor on this board: the VideoCore is. It runs
first, reads `config.txt`, loads the kernel and only then releases the ARM. So
three stock firmware blobs must be present, from
[raspberrypi/firmware](https://github.com/raspberrypi/firmware) under `boot/`:

| File | Why |
| --- | --- |
| `bootcode.bin` | first-stage GPU loader; brings up SDRAM. **Required on Pi 3** — only the Pi 4 moved this into an on-board EEPROM |
| `start.elf` | the GPU firmware proper: reads `config.txt`, loads the kernel, starts the ARM |
| `fixup.dat` | memory-split data `start.elf` needs to place the ARM/GPU boundary |

### 5.3 Card contents

```text
BOOT/
  bootcode.bin      from raspberrypi/firmware
  start.elf         from raspberrypi/firmware
  fixup.dat         from raspberrypi/firmware
  config.txt        from this repository: firmware/config.txt
  kernel8.img       your build
```

```powershell
cargo image --example shell -- -O binary kernel8.img
```

`image` is only a Cargo alias for `objcopy --release`, so the written-out form
is identical and works in any shell:

```bash
cargo objcopy --release --example shell -- -O binary kernel8.img
```

**It must be a flat image, not an ELF.** The firmware copies the file to
`0x80000` and branches to the first byte; hand it an ELF and it executes the
`7F 45 4C 46` header. QEMU is more forgiving because `-kernel` understands ELF
program headers, which is exactly why this bites only on hardware.

`kernel8.img` is the stripped image; the ELF beside it in
`target/aarch64-unknown-none/release/examples/` keeps the full DWARF and is
what you point GDB at.

### 5.4 config.txt

Copy [`firmware/config.txt`](../firmware/config.txt) as-is. It is commented
line by line; the settings that are not optional:

```ini
arm_64bit=1               # boot the ARM in AArch64
kernel=kernel8.img        # the default for a 64-bit kernel, stated anyway
disable_commandline_tags=1  # no device tree, no ATAGs
dtoverlay=disable-bt      # give the PL011 back to GPIO 14/15
enable_uart=1
init_uart_clock=48000000  # the divisors in pl011.rs assume exactly this
enable_jtag_gpio=1        # route GPIO 22-27 to JTAG before the kernel runs
```

Two of those repay a closer look.

**`dtoverlay=disable-bt`.** On the Pi 3 the good UART is wired to the Bluetooth
module by default, and the header gets the mini UART instead. This kernel
drives the PL011, so the two must be swapped back. The alternative overlay,
`miniuart-bt`, keeps Bluetooth alive on the mini UART, but then you also need
`core_freq=250`: the mini UART derives its baud rate from the VPU core clock,
which the firmware scales for thermal reasons, so its baud drifts unless the
clock is pinned. The PL011 has its own clock and does not care. Disabling
Bluetooth is simpler and this project has no use for it.

**`init_uart_clock=48000000`.** `bsp/memory.rs` sets
`PL011_CLOCK_HZ = 48_000_000` and `console.rs` computes the baud divisors
against it. If the firmware picks a different reference the console comes out
as garbage at every baud rate. QEMU reports 3 MHz and ignores the divisors
entirely, so this mismatch is invisible under emulation and instant on
hardware.

Leave the clocking block commented until you have read
[11-videocore-mailbox.md](11-videocore-mailbox.md). `force_turbo=1` in
particular sets a permanent, irreversible OTP warranty bit.

---

## 6. Real hardware: the serial console

A 3.3 V USB-TTL adapter on three pins:

| Signal | GPIO | Header pin | Adapter |
| --- | --- | --- | --- |
| Pi TXD | 14 | **8** | RX |
| Pi RXD | 15 | **10** | TX |
| GND | — | **6** | GND |

115200 8N1, no flow control. TX crosses to RX; wiring TX to TX gets you
silence.

> **3.3 V only.** The BCM2837's GPIO is not 5 V tolerant and a 5 V adapter will
> destroy it. Do **not** connect the adapter's 5 V pin to header pin 2 or 4
> while the Pi is also on its own supply.

```bash
picocom -b 115200 /dev/ttyUSB0        # Linux
```
```powershell
mode COM3 baud=115200 parity=n data=8 stop=1
.\scripts\minipush.ps1 -Serial COM3 -Payload kernel8.img   # console + chain-load
```

Power the Pi *after* the terminal is open, or the boot banner is gone before
you can read it.

---

## 7. Real hardware: JTAG

`enable_jtag_gpio=1` in `config.txt` routes six pins to their ALT4 JTAG
functions before the kernel runs, allowing a probe to attach after a boot
failure. The kernel's `debug::jtag` module sets the same routing again, so a
probe still works if the firmware setting is lost.

| GPIO | Signal | Header pin |
| --- | --- | --- |
| 22 | `ARM_TRST` | 15 |
| 23 | `ARM_RTCK` | 16 |
| 24 | `ARM_TDO` | 18 |
| 25 | `ARM_TCK` | 22 |
| 26 | `ARM_TDI` | 37 |
| 27 | `ARM_TMS` | 13 |

Plus a ground — pin 20 or 39 is convenient. The non-sequential physical pins
are correct: this is the standard 40-pin header mapping, where GPIO 26 lands on
pin 37 and GPIO 27 on pin 13.

### Probes

| Probe | OpenOCD interface file | Notes |
| --- | --- | --- |
| FT2232H (Olimex ARM-USB-OCD-H, JTAG-HS2) | `interface/ftdi/*.cfg` | the usual choice |
| SEGGER J-Link | `interface/jlink.cfg` | J-Link EDU Mini is enough |
| A second Raspberry Pi | `interface/raspberrypi2-native.cfg` | bit-banged, slow, free |

> The official **Raspberry Pi Debug Probe is SWD-only** and cannot drive the
> Pi 3's JTAG DAP. It is still a perfectly good USB-serial adapter for §6.

```bash
openocd -f interface/jlink.cfg \
        -c "transport select jtag" \
        -c "adapter speed 1000" \
        -f openocd/rpi3.cfg
```

`openocd/rpi3.cfg` in this repository declares all four Cortex-A53 cores.
Then, in another terminal:

```bash
aarch64-none-elf-gdb target/aarch64-unknown-none/release/examples/shell
(gdb) target extended-remote :3333
(gdb) break kernel_boot_primary
```

Debug information is kept in **every** profile — `debug = "full"`,
`strip = "none"`, frame pointers and unwind tables forced — so the release
build is the one you debug. [08-jtag-and-debugging.md](08-jtag-and-debugging.md)
has the full session.

---

## 8. Stop reflashing the card

Once the board boots at all, writing the SD card for every build is the slowest
thing left in the loop. Put the chain-loader on the card once and push each new
kernel down the serial line you already have connected:

```powershell
.\scripts\minipush.ps1 -Serial COM3 -Payload kernel8.img
```

Roughly five seconds instead of a card swap, and the card never comes out. See
[22-chainloading.md](22-chainloading.md).

---

## 9. When it does not work

| Symptom | Cause |
| --- | --- |
| QEMU boots, console silent | the PL011 is the **first** `-serial`; the second is the mini UART |
| `-M raspi3` unknown | removed in QEMU 6.2; use `raspi3b` |
| QEMU fails to start | you passed `-smp`; `raspi3b` is fixed at four cores |
| `-device …,bus=spi` rejected | QEMU older than 11; I²C needs ≥ 8 |
| Tools "not installed" in a script | stale `PATH`; re-import it (§2) |
| GDB: "incompatible architecture" | plain `gdb`; use `aarch64-none-elf-gdb` or `gdb-multiarch` |
| Hardware: nothing at all on serial | firmware files missing, or an ELF instead of a flat image |
| Hardware: garbage on serial | `init_uart_clock` is not 48 MHz, or `dtoverlay=disable-bt` is absent |
| Hardware: boots, but only one core | expected; `smp::start_secondaries()` releases the rest |
| `kernel8.img` grows by ~512 KiB | `.stacks` lost `NOLOAD` in `link.ld` |
| A build warning | treat as a failure. See `instructions.md` |

## The SD card must use an MBR partition table

The BCM2837 boot ROM reads **MBR only**. A GPT-partitioned card is not bootable,
no matter how correct the FAT32 partition and its contents are.

The failure is invisible from the host: Linux,
macOS and Windows all mount a GPT card's FAT32 partition without complaint, so
`bootcode.bin`, `start.elf`, `config.txt` and `kernel8.img` can all be verified
present and byte-correct on a card the Pi will never boot. The only symptom is a
board that does nothing.

Check before writing anything:

```bash
lsblk -o NAME,SIZE,FSTYPE,LABEL,PTTYPE /dev/sda
```

`PTTYPE` must read `dos`. The boot partition must be the first partition, FAT32,
and marked bootable. Cards formatted by the official Raspberry Pi Imager are
already correct; cards repartitioned by hand with `gdisk`, or by some desktop
disk utilities that default to GPT, are not.

## Mounting the card over SSH

`udisksctl mount` fails over a plain SSH session: polkit refuses because there
is no controlling terminal, and the error does not say so. Either mount from a
local terminal on the host, re-seat the reader so udisks auto-mounts it, or use
`sudo mount` directly.

`udisksctl unmount` does work over SSH, so the flash-then-eject half of the
cycle can be scripted.

---

## Related

* [01-toolchain-and-build.md](01-toolchain-and-build.md) — why the flags,
  linker script and profiles are what they are
* [09-qemu.md](09-qemu.md) — emulated devices, and where QEMU lies
* [08-jtag-and-debugging.md](08-jtag-and-debugging.md) — GDB and OpenOCD in anger
* [22-chainloading.md](22-chainloading.md) — the serial push loop
* [23-scripts.md](23-scripts.md) — every host script, Windows and Linux
* [16-using-the-bsp.md](16-using-the-bsp.md) — the same setup for a downstream crate
