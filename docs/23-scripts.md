# Host scripts

Everything in `scripts/` exists so that the edit-build-run loop is one command,
on either platform. Every job has a Windows script and a Linux/macOS script,
with the same behaviour, the same defaults and the same option names modulo
each shell's conventions.

Nothing here is required to *build* the project — `cargo build` and `cargo run`
work on their own ([01-toolchain-and-build.md](01-toolchain-and-build.md)).
These scripts exist for the things Cargo cannot do: drive QEMU with the right
machine flags, talk the chain-loader protocol, and automate the console.

---

## 1. The set at a glance

| Job | Windows | Linux / macOS |
| --- | --- | --- |
| Build an example and chain-load it to the board | `chainboot.ps1` | `chainboot.sh` or `cargo chain` |
| Cargo runner behind `cargo chain` | — | `chainboot-runner.sh` |
| Boot an example in QEMU | `run-qemu.ps1` | `run-qemu.sh` |
| Chain-load under QEMU, no board | `chainboot-qemu.ps1` | `chainboot-qemu.sh` |
| Drive the console automatically | `qemu-console-test.ps1` | `qemu-console-test.sh` |
| Push an already-built image | `minipush.ps1` | `minipush.py` |
| Convert a QEMU screendump to PNG | `ppm2png.py` | `ppm2png.py` |

`ppm2png.py` is plain Python and runs on both, so there is only one of it.

---

## 2. Prerequisites

**Windows**

* PowerShell 7+ (`pwsh`)
* `qemu-system-aarch64` and `qemu-img` on `PATH`
* `rust-objcopy` (`cargo install cargo-binutils`), or `llvm-objcopy`

**Linux / macOS**

* `bash` 5+, `python3` (**standard library only**), `coreutils`
* `qemu-system-aarch64` and `qemu-img` on `PATH`
* `rust-objcopy`, `llvm-objcopy` or `aarch64-linux-gnu-objcopy` — whichever is
  found first is used
* `ss` (iproute2) — optional, only makes `chainboot-qemu.sh` start faster

There is deliberately **no third-party Python dependency**. `minipush.py`
configures the serial port through `termios` itself rather than pulling in
`pyserial`, so there is no virtualenv to create and nothing to install.

---

## 3. `chainboot` — the one-command hardware loop

This is the everyday command once the loader is on the SD card. It builds the
example, converts it to a flat binary, and pushes it — replacing the old
three-step `cargo build` / `objcopy` / `minipush` sequence.

```bash
# Linux
./scripts/chainboot.sh              # default example (shell)
./scripts/chainboot.sh multicore
./scripts/chainboot.sh hello-uart --build-only
```

```powershell
# Windows
.\scripts\chainboot.ps1 -Serial COM3
.\scripts\chainboot.ps1 multicore -Serial COM3
.\scripts\chainboot.ps1 hello-uart -BuildOnly
```

| Linux | Windows | Meaning |
| --- | --- | --- |
| `[example]` | `[-Example]` | Example to build and push. Default `shell` |
| `--list` | `-List` | List the examples this crate defines, then exit |
| `--build-only` | `-BuildOnly` | Build and objcopy only; print the `.img` path |
| `--profile <p>` | `-Profile <p>` | `release` (default) or `debug` |
| `--debug` | — | Shorthand for `--profile debug` |
| `--port <dev>` | `-Serial <port>` | Serial device. Linux defaults to the FTDI `by-id` path; Windows requires it |
| `--baud <n>` | `-Baud <n>` | Line rate, default 115200 |

**Start the script first, then power-cycle the Pi.** The loader announces
itself with three `0x03` bytes the instant it boots. If nothing is listening at
that moment the token is gone and the push waits forever.

Two details:

* **Features are resolved from the manifest, not hardcoded.** Every
  `[[example]]` declares its own `required-features` and they differ — `gpio`
  and `buses` need `embedded-hal`, `async-bus` needs `embedded-hal-async`. Both
  scripts read `Cargo.toml` at run time, so a new example works with no edit
  here.
* **`--build-only` prints the image path on stdout and nothing else**; progress
  goes to stderr. That is a deliberate contract — `chainboot-qemu.sh` consumes
  it, which keeps feature resolution and objcopy discovery in exactly one place.

The Linux script refuses to start when the serial port is already open by
another process, and tells you which one. That failure is otherwise
indistinguishable from a dead board (§10).

### 3.1 `cargo chain` — the same loop, without leaving Cargo

`cargo chain` is an alias in `.cargo/config.toml` that does the whole thing
through Cargo:

```bash
cargo chain --example shell
cargo chain --example multicore -- --port /dev/ttyUSB0
cargo chain-debug --example hello-uart          # dev profile
```

It works by *replacing the target's runner*. `cargo run` normally builds the
example and hands the ELF to `qemu-system-aarch64`; the alias passes
`--config target.aarch64-unknown-none.runner='./scripts/chainboot-runner.sh'`,
so Cargo hands the ELF to that script instead, which objcopies it and pushes it
to the loader.

The override travels as `--config` rather than an environment variable because
a Cargo alias can only *prefix* a command line — it cannot set
`CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUNNER`.

Two consequences follow:

* **Cargo does the dependency tracking**, so pushing a stale image after editing
  a source file is no longer possible. `chainboot.sh` rebuilds unconditionally
  and is equally safe, but it cannot tell you *nothing changed*.
* **Everything after `--` goes to `minipush.py`**, so the full pusher option set
  is available without `chainboot.sh` having to forward each flag by hand.

Use `chainboot.sh` when you want `--build-only`, `--list`, or to push an image
you already have; use `cargo chain` for the ordinary edit-push-watch loop. There
is no PowerShell equivalent of the runner yet — on Windows use `chainboot.ps1`.

The runner path is relative to the working directory, so run `cargo chain` from
the workspace root.

---

## 4. `run-qemu` — boot an example

```bash
./scripts/run-qemu.sh                       # shell, headless
./scripts/run-qemu.sh hello-uart
./scripts/run-qemu.sh display --display
./scripts/run-qemu.sh --gdb
./scripts/run-qemu.sh --auto-draw --sd-image ./sd.img
```

```powershell
.\scripts\run-qemu.ps1
.\scripts\run-qemu.ps1 -Example hello-uart
.\scripts\run-qemu.ps1 -Example display -Display
.\scripts\run-qemu.ps1 -Gdb
.\scripts\run-qemu.ps1 -AutoDraw -SdImage .\sd.img
```

| Linux | Windows | Default | Meaning |
| --- | --- | --- | --- |
| `[example]` / `--example` | `-Example` | `shell` | Example kernel to boot |
| `--profile <p>` | `-Profile` | `release` | `release` or `debug` |
| `--no-build` | `-NoBuild` | off | Boot whatever is already built |
| `--gdb` | `-Gdb` | off | GDB stub on `:1234`, CPU halted before the first instruction |
| `--trace` | `-Trace` | off | Log exceptions and interrupts to `int.log` |
| `--display` | `-Display` | off | Open a window showing the framebuffer |
| `--sd-image <path>` | `-SdImage` | none | Raw image to attach as the SD card, created if absent |
| `--sd-size-mb <n>` | `-SdSizeMb` | 64 | Size of an image the script creates |
| `--auto-draw` | `-AutoDraw` | off | Send `--startup` once boot goes quiet, then hand the keyboard back. Implies display |
| `--startup <cmds>` | `-Startup` | `f g` | Commands `--auto-draw` sends |
| `--monitor-port <n>` | `-MonitorPort` | 0 | With auto-draw, expose the QEMU monitor on this loopback port |
| `--bus-devices` | `-BusDevices` | off | Attach the virtual I2C/SPI devices |

Quit with <kbd>Ctrl-A</kbd> then <kbd>X</kbd>, or <kbd>Ctrl-C</kbd> under
`--auto-draw`.

The bus examples (`buses`, `async-bus`) get the virtual DS1338 RTC, TMP105
sensor and SST25VF016B flash attached automatically; `--bus-devices` forces
them on for anything else.

Neither script ever passes `-smp`: the `raspi3b` machine hard-codes
min == max == 4 cores and refuses to start if you try.

### How `--auto-draw` differs between the platforms

Same behaviour, different plumbing. QEMU's *Windows* stdio backend cannot be
driven programmatically, so `run-qemu.ps1` moves the PL011 onto a named pipe
and relays it. On Linux the script uses a unix socket with `wait=on`, which
holds the CPU until the relay has attached, so the start of the banner cannot
be missed.

---

## 5. `qemu-console-test` — automated console runs

Boots an example, waits for the console to go quiet, sends keystrokes, captures
a transcript, and optionally takes a screendump. Exits non-zero on failure, so
it is safe in a gate.

```bash
./scripts/qemu-console-test.sh --commands "h m s t c e"
./scripts/qemu-console-test.sh --commands "d" --log-path abort.log
./scripts/qemu-console-test.sh --commands "f g" --screenshot card.ppm
```

```powershell
.\scripts\qemu-console-test.ps1 -Commands 'h','m','s','t','c','e'
.\scripts\qemu-console-test.ps1 -Commands 'd' -LogPath abort.log
.\scripts\qemu-console-test.ps1 -Commands 'f','g' -Screenshot card.ppm
```

| Linux | Windows | Default | Meaning |
| --- | --- | --- | --- |
| `--commands "<list>"` | `-Commands a,b,c` | none | Keystrokes to send, in order |
| `--settle-ms <n>` | `-SettleMs` | 1200 | Silence required before the next command |
| `--timeout-sec <n>` | `-TimeoutSec` | 60 | Hard limit on the whole run |
| `--log-path <path>` | `-LogPath` | `console-test.log` | Transcript destination |
| `--screenshot <path>` | `-Screenshot` | none | PPM screendump once all commands are sent |
| `--monitor-port <n>` | `-MonitorPort` | 55555 | Loopback port for the monitor socket |
| `--sd-image <path>` | `-SdImage` | none | Raw image to attach as the SD card |
| `--sd-size-mb <n>` | `-SdSizeMb` | 64 | Size of an image the script creates |
| `--profile <p>` | `-Profile` | `release` | `release` or `debug` |
| `[example]` / `--example` | `-Example` | `shell` | Example kernel to boot |
| `--bus-devices` | `-BusDevices` | off | Attach the virtual I2C/SPI devices |

Note the shell difference: Linux takes one space-separated string
(`--commands "h m"`), PowerShell takes an array (`-Commands 'h','m'`).

**Neither script builds for you.** Build first, or the run fails with a message
telling you which `cargo build` to run. That is deliberate: a test harness that
silently rebuilds can mask which binary actually failed.

Convert a screendump with `python3 scripts/ppm2png.py card.ppm card.png`.

---

## 6. `chainboot-qemu` — the hardware loop, emulated

Builds loader and payload, starts QEMU, pushes, captures the payload's output.
Use it to check a change to the loader or to the protocol without touching a
board.

```bash
./scripts/chainboot-qemu.sh
./scripts/chainboot-qemu.sh --payload multicore
./scripts/chainboot-qemu.sh --payload ./kernel8.img --run-seconds 15 --log boot.log
```

```powershell
.\scripts\chainboot-qemu.ps1
.\scripts\chainboot-qemu.ps1 -Payload multicore
.\scripts\chainboot-qemu.ps1 -Payload .\kernel8.img -RunSeconds 15 -LogPath boot.log
```

| Linux | Windows | Default | Meaning |
| --- | --- | --- | --- |
| `--payload <name\|path>` | `-Payload` | `hello-uart` | Example name to build, or an existing flat image |
| `--port <n>` | `-Port` | 4444 | Loopback-only TCP port for the emulated PL011 |
| `--run-seconds <n>` | `-RunSeconds` | 8 | How long to capture the payload's output |
| `--log <path>` | `-LogPath` | none | Write the whole transcript here as well |
| `--pad-to-max` | `-PadToMax` | off | Push a private copy padded to 8 MiB and verify the received length |
| `--expect <text>` | `-Expect` | none | Fail unless this literal payload output appears in the transcript |

The regression options retain a transcript under `target/` when no explicit
log path is supplied. Padding never changes the input image, and its temporary
copy is removed on exit. For example:

```bash
./scripts/chainboot-qemu.sh --payload multicore --pad-to-max \
    --expect 'All 4 cores online'
```

The large-BSS and pre-release allocation fixture is documented in
[22-chainloading.md](22-chainloading.md).

Both start QEMU with `wait=on`, which is **required**: with `wait=off` the
guest runs immediately and everything printed before the pusher connects is
discarded — including the request token, so the push never starts and it looks
like a hang.

For the same reason the Linux script never probes the port by connecting to it:
under `wait=on` the first connection *is* what releases the guest. It looks for
a listening socket with `ss` instead.

> **A loader that passes here has proved nothing about its cache maintenance.**
> QEMU treats set/way cache operations as no-ops and has no store buffer to
> leave dirty, so a handover that corrupts itself on real silicon passes this
> test cleanly. See [22-chainloading.md](22-chainloading.md) §3.

---

## 7. `minipush` — the low-level pusher

`chainboot` calls this; reach for it directly when you already have an image,
or when you want to push to QEMU by hand.

```bash
python3 scripts/minipush.py --payload kernel8.img
python3 scripts/minipush.py --payload kernel8.img --port /dev/ttyUSB1
python3 scripts/minipush.py --payload kernel8.img --tcp localhost:4444
python3 scripts/minipush.py --payload k.img --tcp localhost:4444 \
        --no-terminal --run-seconds 20 --log boot.log
```

```powershell
.\scripts\minipush.ps1 -Serial COM3 -Payload kernel8.img
.\scripts\minipush.ps1 -Tcp localhost:4444 -Payload kernel8.img
.\scripts\minipush.ps1 -Tcp localhost:4444 -Payload k.img `
                       -NoTerminal -RunSeconds 20 -LogPath boot.log
```

| Linux | Windows | Default | Meaning |
| --- | --- | --- | --- |
| `--payload <file>` | `-Payload` | required | Flat image to send; an ELF is refused outright |
| `--port <dev>` | `-Serial <port>` | FTDI `by-id` path / required | Serial transport |
| `--tcp <host:port>` | `-Tcp <host:port>` | none | QEMU serial socket transport |
| `--baud <n>` | `-Baud` | 115200 | Line rate |
| `--no-terminal` | `-NoTerminal` | off | Exit after the push instead of becoming a terminal |
| `--run-seconds <n>` | `-RunSeconds` | 5 | With the above, how long to capture output |
| `--log <file>` | `-LogPath` | none | Tee everything received to a file |
| `--handshake-timeout <s>` | `-HandshakeTimeoutSec` | 30 | How long to wait for the request token |

Both wait for the token before doing anything, so you can start either before
the board is powered and it will pick the boot up when it happens. Press
<kbd>Ctrl-]</kbd> to leave the terminal.

The wire protocol is byte-identical to `minipush.rb` from
rust-raspberrypi-OS-tutorials, so any of the three tools drives any of the
loaders.

---

## 8. Everyday workflows

**Iterating on kernel code, no hardware:**

```bash
./scripts/run-qemu.sh shell
```

**Iterating on kernel code, on the board** (loader already on the card):

```bash
./scripts/chainboot.sh shell     # then power-cycle the Pi
```

**Iterating on the chain-loader itself:**

```bash
./scripts/chainboot-qemu.sh --payload multicore    # fast, but see the caveat in §6
```

...then confirm on real silicon, because QEMU cannot see cache bugs.

**Checking the console still works after a change:**

```bash
./scripts/qemu-console-test.sh --commands "h m s t c e"
```

---

## 9. Why the platform split exists

The Windows scripts carry two workarounds that Linux does not need: QEMU's
Windows stdio backend rejects a redirected stdin, and QEMU on Windows only
ever creates one `pipe:` chardev. The Linux ports use unix sockets instead,
which removes both problems and lets them use `wait=on` everywhere, so early
boot output cannot be lost.

`minipush.py` has the same transports and the same automation options as
`minipush.ps1`, so `chainboot-qemu.sh` behaves the same on both platforms.

---

## 10. Gotchas that bite

| Symptom | Cause |
| --- | --- |
| Push sits at "waiting for the loader" forever | Something else owns the serial port — a detached `screen` is the usual culprit. It eats the request token silently. `chainboot.sh` checks for this and refuses to start |
| Push works, payload never runs | The image was an ELF, not a flat binary. `minipush` refuses these outright; a hand-rolled `objcopy` may not have |
| QEMU console test hangs with no output | `wait=off` and the reader attached late. Every script here uses `wait=on` for exactly this reason |
| Serial device moved from `ttyUSB0` to `ttyUSB2` | The numbered node is not stable across re-plugs. Always use `/dev/serial/by-id/...`, which is what the default is |
| "no `[[example]]` named ..." | Typo, or a new example that has not been added to `Cargo.toml`. The error lists the valid names |
| Chain-load works in QEMU, fails on the board | Almost certainly cache maintenance. QEMU cannot model it — [22-chainloading.md](22-chainloading.md) §3 |

---

## Related

* [01-toolchain-and-build.md](01-toolchain-and-build.md) — building without any of this
* [09-qemu.md](09-qemu.md) — what the emulator does and does not model
* [21-host-setup.md](21-host-setup.md) — cable, SD card and `config.txt`
* [22-chainloading.md](22-chainloading.md) — the protocol these scripts speak
* [08-jtag-and-debugging.md](08-jtag-and-debugging.md) — the other way to avoid reflashing