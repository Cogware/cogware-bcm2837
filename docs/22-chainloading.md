# Chain-loading

Writing an SD card for every build is the slowest step in bare-metal
development: eject, adapter, copy, unmount, insert, power-cycle — a minute or
two per iteration, for a two-second change. The chain-loader removes it. Flash
the card **once**, then push each new kernel down the serial cable you already
have connected for the console.

```powershell
.\scripts\minipush.ps1 -Serial COM3 -Payload kernel8.img
```

About five seconds, and the card never leaves the board.

The idea comes from tutorial 06 of
[rust-raspberrypi-OS-tutorials](https://github.com/rust-embedded/rust-raspberrypi-OS-tutorials/tree/master/06_uart_chainloader),
and the wire protocol here is deliberately byte-identical to that project's
`minipush.rb` so either host tool drives either target. The *mechanism* is
different, for reasons in §2.

---

## 1. What happens

```text
  power on
     │
     ▼
  VideoCore reads config.txt, copies kernel8.img (the loader) to 0x80000
     │
     ▼
  loader boots: MMU on, PL011 up, cores 1-3 released and re-parked  (§4)
     │
     ▼
  loader copies its relocation stub to 0x800000                     (§2)
     │
     ▼
  loader prints "\x03\x03\x03"  ─────────────────────────►  host starts sending
     │                                                              │
     ▼                                                              ▼
  loader reads a u32 length, replies "OK", stores the        host sends length,
  payload at 0x1000000                                       waits for OK, sends
     │                                                       the image
     ▼
  loader cleans the caches, drops the MMU, branches to the stub     (§3)
     │
     ▼
  stub copies 0x1000000 -> 0x80000 and branches to 0x80000
     │
     ▼
  the payload boots, exactly as if the firmware had loaded it
```

From the payload's point of view nothing unusual happened: it is at `0x80000`,
in EL1, MMU off, caches cold, with the same register state the firmware
provides. That is the whole objective — a kernel must not need to know it was
chain-loaded.

---

## 2. Why a relocating stub

The awkward part is that the loader is *already occupying the address the
payload has to go to*. Both are `kernel8.img` files; both are linked for
`0x80000`. Copying the payload into place with a loop that is itself running at
`0x80000` overwrites the loop mid-flight.

The tutorial solves this by linking the chain-loader at `0x200000` and
relocating **the whole loader** there in its boot assembly before Rust starts.
That is clean, but it needs a second linker script and a second boot path, and
Cargo cannot give one example of a crate different link arguments from its
siblings without splitting the crate. Not worth it for one example.

So this implementation moves the *smallest possible* piece instead. Only the
final copy loop has to run outside `0x80000`, and that loop is 132 bytes of
position-independent assembly:

| Constant | Value | Role |
| --- | --- | --- |
| `STUB_ADDR` | `0x0080_0000` (8 MiB) | where the stub is relocated to |
| `STAGING_ADDR` | `0x0100_0000` (16 MiB) | where the payload is received |
| `MAX_PAYLOAD` | 8 MiB | staging to just below the heap |

Both sit well above the loader's own image and below the heap, so nothing
overlaps. `install_stub()` copies the stub with
`copy_nonoverlapping` and cleans the destination to the point of coherency.

The stub must remain position-independent. A literal pool or an `adrp` fixup
would reference the original link address and read from the wrong place after
relocation. Verify with:

```powershell
aarch64-none-elf-objdump -d --start-address=0x... chainloader
```

Every branch in it is PC-relative (`b`, `cbz`), the one absolute value it needs
(`0x80000`) is built with `movz`, and it touches no memory it did not compute
itself. It also never touches the stack, because by the time it runs there is
no guaranteed stack left.

---

## 3. Three ways to brick the handover

All three look like a hang with no output.

### W^X makes the stub unexecutable

The MMU maps DRAM outside `.text` as **execute-never**
([03-memory-and-mmu.md](03-memory-and-mmu.md)). `0x800000` is data as far as
the page tables are concerned, so branching to the stub takes an instruction
permission fault *before* the stub's own first instruction — including the
instruction that would have turned the MMU off. The MMU must therefore be
dropped by the **caller**, while still executing from `.text`.

### Turning off the D-cache mid-function corrupts the stack

The obvious shape — a `disable_mmu()` helper that returns, then a branch — is
broken. The moment `SCTLR_EL1.C` clears, loads and stores bypass the caches and
go to DRAM, but the caller's live stack frame is still sitting dirty in a cache
line that has not been written back. The function returns to a garbage address.

There is no way to write this safely as two steps, so it is one:

```rust
unsafe fn leave_mmu_and_branch(target: usize, x0: usize, x1: usize, x2: usize) -> ! {
    // Clear M/C/I and branch, with nothing in between that touches memory.
    asm!(
        "dsb sy",
        "isb",
        "mrs x9, sctlr_el1",
        "bic x9, x9, #(1 << 0)",    // M: address translation
        "bic x9, x9, #(1 << 2)",    // C: data cache
        "bic x9, x9, #(1 << 12)",   // I: instruction cache
        "msr sctlr_el1, x9",
        "isb",
        "tlbi vmalle1",
        "dsb sy",
        "isb",
        "br  x4",
        in("x4") target, in("x0") x0, in("x1") x1, in("x2") x2,
        options(noreturn)
    )
}
```

Two constraints that the compiler enforces in unhelpful ways:

* `out("x9") _` is rejected with `options(noreturn)` — the block never returns,
  so there is no point at which a clobber could be observed. `x9` is a caller-
  saved scratch register and nothing after this block runs.
* `in(reg)` lets the register allocator pick, and it may well pick `x9` — which
  the `mrs` immediately destroys. `target` is therefore pinned to `x4`.

Everything that must survive is flushed to the point of coherency before this
runs -- but "everything" is a larger set than it first appears, which is the
subject of the next failure.

### Disabling the D-cache does not empty it

This one booted perfectly under QEMU and hung on silicon every time.

Clearing `SCTLR_EL1.C` stops the cache being *used*. It does not write anything
back, and it does not evict anything. Every dirty line stays exactly where it
was, and the hardware is still free to write it out later. Later, in a
chain-loader, means *after the payload has been copied over that address*.

The loader had flushed the two regions it knew a stub would read -- the stub
itself and the staged payload -- with `DC CIVAC`. It had not flushed:

* its own stack and statics, which live inside the load region at `0x80000`;
* anything still cached for the load region from when the loader booted there;
* the secondaries' stacks, dirtied on three other cores before they parked.

So the stub would copy a correct payload into DRAM, branch to it, and then a
stale line from the *loader* would land on top of the payload's code.

The fix is to clean before disabling, not merely disable:

```rust
// Core 0, immediately before dropping the MMU. Safe to walk the shared L2
// here, because the secondaries are already parked with their caches off.
unsafe { mmu::clean_invalidate_all() };
```

```rust
// Each secondary, before it parks with its cache off. L1 only -- core 0 is
// still running, and the L2 is shared.
unsafe { mmu::clean_invalidate_l1() };
```

`cache.s` provides four entry points over one shared walk. The distinction that
matters is *clean* versus *invalidate*: the invalidate-only variants
(`__dcache_invalidate_all`, `__dcache_invalidate_l1`) **discard** dirty lines
and belong only at reset, before anything of value exists. Anywhere else, and
especially before handing memory to a core running with its caches off, use
`__dcache_clean_invalidate_all` or `__dcache_clean_invalidate_l1`.

QEMU cannot reproduce any of this. It treats set/way maintenance as a no-op and
has no store buffer to leave dirty, so the emulated handover succeeds no matter
how wrong the maintenance is. A chain-loader that works under QEMU has proved
nothing about its cache handling.

---

## 4. Keeping all four cores

A chain-loader that parks cores 1–3 in a plain `wfe` loop appears to work and
quietly costs you three quarters of the machine — the chain-loaded `multicore`
example reported *"Only 1/4 cores checked in"*. On real hardware it is worse
than a lost core: the loader has already taken the secondaries **out of the
firmware's spin table permanently**, so the payload's own
`smp::start_secondaries()` has nothing left to release, and the failure
persists for every kernel pushed afterwards.

The fix is that the loader's park loop **is** a spin table — a faithful
re-implementation of the one in `armstub8.S`, which is also what QEMU's
`write_smpboot64` installs:

```asm
__chainload_park:
    mov     x5, #0xd8               // SPIN_TABLE_BASE
    mrs     x6, mpidr_el1
    and     x6, x6, #3
    str     xzr, [x5, x6, lsl #3]   // clear our own slot first
    dsb     sy
1:  wfe
    ldr     x4, [x5, x6, lsl #3]
    cbz     x4, 1b
    br      x4
```

Clearing the slot before waiting is not optional. A slot left holding the
loader's `_start` sends the core straight back into an image that is in the
middle of being overwritten.

The payload is then released by its own `start_secondaries()`, through the
normal path, and behaviour is identical on QEMU and hardware.
[04-smp.md](04-smp.md) covers the protocol; two details of it matter here:

* the `wfe` is **before** the load, so a release must be followed by `sev`;
* the secondary reads with MMU and caches **off**, so the writing core must
  push the value past the point of coherency with `dc civac` — QEMU is
  forgiving about this and silicon is not.

A compile-time assertion keeps the hand-written `0xd8` in sync with the
memory map:

```rust
const _: () = assert!(memory::SPIN_TABLE_BASE == 0xd8, "spin table moved");
```

---

## 5. The protocol

| Direction | Bytes | Meaning |
| --- | --- | --- |
| target → host | text | boot banner |
| target → host | `03 03 03` | three `ETX`: "send me a kernel" |
| host → target | `u32` LE | payload length in bytes |
| target → host | `4F 4B` | `OK` — length accepted |
| host → target | *length* bytes | the raw image |

Unframed, no checksum, no retransmit — appropriate for a short cable, and
identical to `minipush.rb`. Both ends independently print a CRC-32/ISO-HDLC of
what they handled, so a corrupt push is visible immediately:

```text
host  : CRC32 0x232e4b41
target: CRC32 0x232e4b41
```

The loader rejects a payload whose first four bytes are `7F 45 4C 46`. An ELF
is the most common mistake here and it produces a hang with no output;
the check turns that into a message.

---

## 6. Host tools

Full reference for every script on both platforms, with complete option tables,
is in [23-scripts.md](23-scripts.md). The chain-loading subset:

### `cargo chain`

The shortest path: `cargo chain --example shell`. An alias in
`.cargo/config.toml` overrides the target's runner with
`scripts/chainboot-runner.sh`, so `cargo run` pushes to the board instead of
launching QEMU. The runner objcopies Cargo's ELF to a flat image beside it in
`target/` and calls `minipush.py`; everything after `--` is forwarded.
Linux only, and it must be run from the workspace root because the runner path
is relative. Full description in [`23-scripts.md`](23-scripts.md) §3.1.

### `scripts/chainboot.sh` and `scripts/chainboot.ps1`

The one-command loop, and the only one of these you need day to day. Builds the
example, converts it to a flat binary, pushes it:

```bash
./scripts/chainboot.sh              # default example (shell)
./scripts/chainboot.sh multicore
```

```powershell
.\scripts\chainboot.ps1 -Serial COM3
.\scripts\chainboot.ps1 multicore -Serial COM3
```

Both read `required-features` out of `Cargo.toml` at run time, so examples that
need `embedded-hal` or `embedded-hal-async` build correctly without the script
knowing anything about them. `--list` / `-List` prints the available names.

The Linux version refuses to start if another process already owns the serial
port, and names it — that failure otherwise looks exactly like a dead board.

Everything below is what these wrap. Use them directly when you already have an
image, or when you want to push to QEMU by hand.

### `scripts/minipush.ps1`

```powershell
# Real hardware
.\scripts\minipush.ps1 -Serial COM3 -Payload kernel8.img

# QEMU, over TCP
.\scripts\minipush.ps1 -Tcp 127.0.0.1:5555 -Payload payload.img

# Automation: push, capture 20 s, exit
.\scripts\minipush.ps1 -Tcp 127.0.0.1:5555 -Payload payload.img `
                       -NoTerminal -RunSeconds 20 -LogPath boot.log
```

| Option | Meaning |
| --- | --- |
| `-Serial <port>` | COM port; add `-Baud` if not 115200 |
| `-Tcp <host:port>` | QEMU's serial socket |
| `-Payload <file>` | flat image to send |
| `-NoTerminal` | do not enter interactive terminal mode afterwards |
| `-RunSeconds <n>` | capture for *n* seconds, then exit |
| `-LogPath <file>` | tee the session to a file |

It waits for the `ETX` token, so you can start it before the board is powered
and it will pick the boot up when it happens.

### `scripts/minipush.py`

The Linux and macOS equivalent, and the one to use when the USB-serial adapter
is on a Linux box. Same wire protocol, byte for byte. It has no third-party
dependencies -- it configures the port through `termios` directly -- so there
is no virtualenv or `pyserial` install to get wrong.

```bash
# Real hardware, using the stable by-id path
python3 scripts/minipush.py --payload kernel8.img

# A different adapter, or a different line rate
python3 scripts/minipush.py --payload kernel8.img --port /dev/ttyUSB1 --baud 115200

# QEMU, over TCP
python3 scripts/minipush.py --payload payload.img --tcp 127.0.0.1:4444

# Automation: push, capture 20 s, exit
python3 scripts/minipush.py --payload payload.img --tcp 127.0.0.1:4444 \
        --no-terminal --run-seconds 20 --log boot.log
```

| Option | Meaning |
| --- | --- |
| `--payload <file>` | flat image to send; refuses an ELF outright |
| `--port <dev>` | serial device, defaults to the FTDI `by-id` path |
| `--tcp <host:port>` | QEMU's serial socket, instead of `--port` |
| `--baud <n>` | line rate, default 115200 |
| `--no-terminal` | exit after the push instead of becoming a terminal |
| `--run-seconds <n>` | with the above, how long to capture output. Default 5 |
| `--log <file>` | tee everything received to a file |
| `--handshake-timeout <s>` | how long to wait for the token. Default 30 |

It is a full peer of `minipush.ps1` — same transports, same automation options,
same wire protocol byte for byte.

Like the PowerShell version it waits for the `ETX` token before doing anything,
so start it *before* you power the board and it will catch the boot. Press
<kbd>Ctrl-]</kbd> to leave the terminal afterwards.

**Only one process may own the serial port.** Stop `screen` first. If `screen`
is still attached it consumes the `ETX` token and the push sits at `waiting for
the loader` forever, which looks exactly like a dead board.

Prefer the `by-id` path over `/dev/ttyUSB0`: the numbered node moves when
adapters are re-plugged, and a stale root-owned file left at that path will
shadow the real device.

### `scripts/chainboot-qemu.ps1`

Builds loader and payload, starts QEMU, pushes:

```powershell
.\scripts\chainboot-qemu.ps1 -Payload multicore
.\scripts\chainboot-qemu.ps1 -Payload hello-uart -RunSeconds 15
```

It starts QEMU with `-serial tcp::5555,server=on,wait=on`. **`wait=on` is
required**: with `wait=off` the guest runs immediately and everything printed
before the host connects is discarded — including the `ETX` token, so the push
never starts and it looks like a hang.

### `scripts/chainboot-qemu.sh`

The same thing on Linux:

```bash
./scripts/chainboot-qemu.sh --payload multicore
./scripts/chainboot-qemu.sh --payload hello-uart --run-seconds 15 --log boot.log
```

It builds the payload by calling `chainboot.sh --build-only`, so feature
resolution and `objcopy` discovery live in exactly one place. Because `wait=on`
means the *first connection* is what releases the guest, it waits for the
listening socket with `ss` rather than connecting to probe.

Remember what this does not test: QEMU treats set/way cache maintenance as a
no-op and has no store buffer to leave dirty, so a loader that passes here can
still corrupt the handover on real silicon (§3).

---

## 7. On real hardware

Once, to the SD card ([21-host-setup.md](21-host-setup.md)):

```powershell
cargo image --example chainloader -- -O binary kernel8.img
```

Then, for every build after that, one command:

```bash
cargo chain --example shell         # or whichever example
```

```powershell
.\scripts\chainboot.ps1 shell -Serial COM3
```

Power-cycle the Pi and the push begins on its own. **Start the script first.**
The loader sends its request token the instant it boots; if nothing is
listening at that moment the token is gone and the push waits forever.

`cargo chain` builds the example and pushes it in one step, and because it is
`cargo run` underneath, Cargo decides whether a rebuild is needed. Arguments
after `--` reach the pusher:

```bash
cargo chain --example shell -- --port /dev/ttyUSB0 --baud 115200
cargo chain-debug --example shell           # dev profile, same push
```

`./scripts/chainboot.sh shell` does the same job without Cargo's runner
indirection, and is the one to use when you want `--build-only`, `--list`, or
to push an image you already have. On Windows use `chainboot.ps1`; there is no
PowerShell runner. [`23-scripts.md`](23-scripts.md) §3.1 covers the mechanism.

The long way, if you want the steps separately:

```bash
cargo image --example shell -- -O binary kernel8.img
python3 scripts/minipush.py --payload kernel8.img
```

Keep the terminal open; the payload's console arrives on the same connection.

Two things to know:

* The loader stays on the card. Only a change to the **loader** needs the card
  out again.
* Serial is roughly 11 KB/s at 115200 baud, so a 100 KB kernel is about nine
  seconds. The PL011 handles 921600 comfortably, but the rate is not a
  `config.txt` knob: `boot.s` writes `IBRD`/`FBRD` directly and
  `bsp::memory::CONSOLE_BAUD` is 115200. Raising it means changing those, the
  loader, and the `--baud` / `-Baud` given to the pusher, together.

---

## 8. When it does not work

| Symptom | Cause |
| --- | --- |
| Host waits forever for the token | QEMU started with `wait=off`; or the board is not running the loader |
| Push starts, target then hangs | payload was an ELF — rebuild with `cargo image --example <name> -- -O binary kernel8.img` |
| CRCs differ | serial corruption; drop the baud rate or shorten the cable |
| Payload sees only one core | park loop is not clearing its spin-table slot (§4) |
| Instruction abort near `0x800000` | MMU still on at the branch (§3) |
| Return to a garbage address | D-cache cleared in a function that then returned (§3) |

---

## Related

* [23-scripts.md](23-scripts.md) — every host script, both platforms
* [21-host-setup.md](21-host-setup.md) — cable, card and `config.txt`
* [04-smp.md](04-smp.md) — the spin-table protocol the park loop implements
* [03-memory-and-mmu.md](03-memory-and-mmu.md) — W^X, and why the stub faults
* [08-jtag-and-debugging.md](08-jtag-and-debugging.md) — the other way to avoid reflashing
