# Toolchain and build
This note records the Windows-native toolchain and the reasons for the build
settings. It complements [02-boot-and-exception-levels.md](02-boot-and-exception-levels.md),
[03-memory-and-mmu.md](03-memory-and-mmu.md), [04-smp.md](04-smp.md),
[06-synchronisation.md](06-synchronisation.md),
[07-exceptions-and-irq.md](07-exceptions-and-irq.md), and
[09-qemu.md](09-qemu.md).
The project is deliberately small: stable Rust, `core`, hand-written assembly,
a hand-written linker script, no Docker, no Makefile, and no external crates
beyond two optional trait-only ones. The verified host setup is Windows
PowerShell:
```text
QEMU 11.1.0
C:\Users\ninja\scoop\apps\qemu\current\
ARM GNU Toolchain 15.2.Rel1
C:\Users\ninja\scoop\apps\gcc-aarch64-none-elf\current\bin
```
Use the ARM toolchain's `aarch64-none-elf-objcopy`,
`aarch64-none-elf-objdump`, `aarch64-none-elf-readelf`, and
`aarch64-none-elf-gdb`. Scoop's separate `gdb` package is x86-only and cannot
debug AArch64.

## Target choice
The target is:
```text
aarch64-unknown-none
```
That is the hard-float bare-metal AArch64 target. The project deliberately does
not use:
```text
aarch64-unknown-none-softfloat
```
The BCM2837 contains Cortex-A53 cores with integrated FP/SIMD hardware. The
kernel wants that hardware, so the normal AArch64 ABI is the right ABI.
The consequence is not limited to functions that mention `f32` or `f64`:
```text
LLVM may emit NEON/FP instructions in any function.
```
A copy, slice operation or optimised zeroing sequence may use `q` registers.
There is no stable Rust switch that makes one early function NEON-free.
Two invariants follow:
1. every core must enable `CPACR_EL1.FPEN` before any Rust runs there;
2. the exception frame must save all 32 NEON registers plus `FPSR` and `FPCR`.
The reset assembly satisfies the first invariant before branching to Rust. The
exception path satisfies the second; see
[07-exceptions-and-irq.md](07-exceptions-and-irq.md). This is a deliberate
trade: real FPU code in exchange for a full FP/SIMD-aware boot and trap path.

## Rust toolchain
`rust-toolchain.toml` pins the local toolchain:
```toml
[toolchain]
channel = "stable"
profile = "minimal"
components = ["rust-src", "llvm-tools", "rustfmt", "clippy"]
targets = ["aarch64-unknown-none"]
```
The intended line is current stable Rust, Rust 1.98 with edition 2024 for this
phase. Nothing in the tree requires nightly, `#![feature]`, or `-Z`.
Pinning matters for a `#![no_std]` kernel. The project builds `core` for a
bare-metal target, relies on stable code generation, and passes a custom linker
script to `rust-lld`. A toolchain bump is therefore a kernel change, not
workstation drift.
`rust-src` builds `core` for the target. `llvm-tools` feeds `cargo-binutils`.
`rustfmt` and `clippy` are installed with the same pinned toolchain.

## Manifests
The workspace root contains one member:
```toml
[workspace]
resolver = "3"
members = ["cogware-bcm2837"]

[workspace.package]
edition = "2024"
version = "0.1.0"
license = "MIT OR Apache-2.0"
repository = "https://codeberg.org/Cogware/cogware-bcm2837"

[profile.dev]
panic = "abort"
opt-level = 1
debug = "full"
strip = "none"
overflow-checks = true
debug-assertions = true
lto = false
codegen-units = 1

[profile.release]
panic = "abort"
opt-level = 2
debug = "full"
strip = "none"
overflow-checks = true
debug-assertions = false
lto = "thin"
codegen-units = 1
incremental = false
```
Profiles must be here because Cargo ignores profile tables in member manifests.
`cogware-bcm2837\Cargo.toml` defines the reusable BSP library:
```toml
[package]
name = "cogware-bcm2837"
description = "Bare-metal BSP for the Broadcom BCM2837 (Raspberry Pi 3): boot, MMU, SMP, drivers and a no-alloc async executor"
categories = ["embedded", "no-std", "hardware-support"]
keywords = ["bcm2837", "raspberry-pi", "aarch64", "bare-metal", "bsp"]
readme = "README.md"
edition.workspace = true
version.workspace = true
license.workspace = true
repository.workspace = true
build = "build.rs"

[lib]
name = "cogware_bcm2837"
path = "src/lib.rs"
test = false
bench = false
doctest = false

[features]
default = [
  "panic-handler",
  "allocator",
  "jtag",
  "embedded-hal",
  "embedded-hal-async",
]
panic-handler = []
allocator = []
jtag = []
embedded-hal = ["dep:embedded-hal"]
embedded-hal-async = ["dep:embedded-hal-async", "embedded-hal"]
embedded-sdmmc = ["dep:embedded-sdmmc"]   # not in `default`
```
The harnesses are disabled because a bare-metal `core` target has no process or
standard-library test runtime. The feature flags gate whole-binary resources:
`panic-handler` supplies `#[panic_handler]`, `allocator` supplies the
`#[global_allocator]`, and `jtag` routes GPIO22-27 during boot.
`embedded-hal-async` implies `embedded-hal` because the async traits are defined
in terms of the blocking ones' error types. `embedded-sdmmc` is the one feature
absent from `default`: it gates a FAT filesystem rather than a whole-binary
resource, and it is the only dependency that is not a trait-only crate.
The example binaries are complete bootable kernels. Every one declares
`required-features`, so a build with a reduced feature set skips the examples it
cannot satisfy instead of failing:
```toml
[[example]]
name = "hello-uart"
path = "examples/hello-uart.rs"
test = false
required-features = ["panic-handler", "allocator"]

# ... multicore, async-tasks, heap, display, sdcard, bitbang, chainloader,
#     shell: the same two ...

[[example]]
name = "buses"
path = "examples/buses.rs"
test = false
required-features = ["panic-handler", "allocator", "embedded-hal"]

[[example]]
name = "async-bus"
path = "examples/async-bus.rs"
test = false
required-features = ["panic-handler", "allocator", "embedded-hal-async"]
```
`panic-handler` and `allocator` appear on all of them because an example *is* a
kernel: it needs the `#[panic_handler]` and `#[global_allocator]` the BSP
supplies. A downstream crate that provides its own turns them off, which is
what the `minimal` member of the scratch consumer does.
The dependency table holds three entries, all optional:
```toml
[dependencies]
embedded-hal = { version = "1.0", optional = true }
embedded-hal-async = { version = "1.0", optional = true }
embedded-sdmmc = { version = "0.10", optional = true, default-features = false }
```
That mirrors the dependency policy: no external crates beyond the
`embedded-*` ecosystem, in two tiers — the first two are pure trait
crates with no transitive dependencies and no code to audit, so they are on by
default; the third is a real FAT implementation, so it is off by default. `--no-default-features` builds the whole BSP
with nothing linked in. `alloc` is used when the `allocator` feature is
enabled, but the allocator behind it is [our own](13-heap.md), not a
dependency. If the BSP needs a register wrapper, lock, or collection, it is
written in the tree so every line is inspectable.

## Cargo configuration
`.cargo\config.toml` sets the default target:
```toml
[build]
target = "aarch64-unknown-none"
```
Ordinary `cargo build`, `cargo run`, and `cargo clippy` therefore use the board
target without extra target flags. Because the package is now a library, build
or run an example when you want a bootable image.
The Rust flags are:
```toml
rustflags = [
  "-C", "target-cpu=cortex-a53",
  "-C", "relocation-model=static",
  "-C", "force-frame-pointers=yes",
  "-C", "force-unwind-tables=yes",
]
```
`target-cpu=cortex-a53` lets LLVM use the A53's actual feature set and schedule
for the real pipeline.
`relocation-model=static` matches the boot model. The firmware loads the kernel
at a fixed physical address, the MMU identity maps it, and there is no dynamic
loader. PIC would add overhead, and a GOT would need relocation processing that
does not exist. `cogware-bcm2837\link.ld` asserts the GOT is empty.
`force-frame-pointers=yes` costs a register. `force-unwind-tables=yes` costs
space. The project accepts both costs for GDB and JTAG backtraces through
optimised and inlined code.
The linker script for this package's examples is supplied by
`cogware-bcm2837\build.rs`, not by `.cargo\config.toml`, so the path is rooted
in the BSP crate and Cargo can track changes.

## Runner and aliases
The runner is:
```text
qemu-system-aarch64 -M raspi3b -serial mon:stdio -serial null -display none -kernel
```
Cargo appends the ELF path after `-kernel`, so:
```powershell
cargo run --release --example shell
```
boots the shell example directly. `-M raspi3b` supplies BCM2837, 1 GiB RAM and four
Cortex-A53 cores. Never pass `-smp`; this QEMU machine fixes the core count at
four. The first serial device is PL011 UART0, the kernel console. The second is
the unused AUX mini UART and is discarded.
The aliases are:
```toml
image  = "objcopy --release"
disasm = "objdump --release"
syms   = "nm --release"
```
A Cargo alias can only *prefix* a command line — whatever you type is appended
to the end. So the aliases deliberately stop at the Cargo options rather than
carrying a canned `-- <tool args>` block: if they carried one, `--example
shell` would land after the `--` and be passed to objcopy instead of to Cargo,
which fails with *"Could not determine the wanted artifact"*. Name the example
and give the tool arguments yourself:
```powershell
cargo image  --example shell -- -O binary kernel8.img
cargo disasm --example shell -- --disassemble --demangle --section-headers
cargo syms   --example shell -- --numeric-sort
```
The first produces the flat firmware image for the chosen example; the other
two inspect the linked ELF.

`cargo chain` is the exception to that shape, because it wraps `cargo run`
rather than a binutils tool:

```bash
cargo chain --example shell                     # build, objcopy, push
cargo chain --example multicore -- --port /dev/ttyUSB0
```

It swaps the target's QEMU `runner` for `scripts/chainboot-runner.sh` via
`--config`, so Cargo builds the example and then chain-loads it onto a board
instead of booting it in the emulator. See
[`23-scripts.md`](23-scripts.md) §3.1.

## Profiles and artefacts
The release profile keeps debug information:
```toml
[profile.release]
panic = "abort"
opt-level = 2
debug = "full"
strip = "none"
overflow-checks = true
debug-assertions = false
lto = "thin"
codegen-units = 1
incremental = false
```
`debug = "full"` in release is intentional. GDB and OpenOCD need DWARF, line
tables, locations and frame data while debugging the release kernel.
There are two artefacts:
```text
target\aarch64-unknown-none\release\examples\shell   ELF with symbols and DWARF
kernel8.img                                  raw image for Raspberry Pi firmware
```
QEMU can boot the ELF. Raspberry Pi firmware loads a raw image, not an ELF, so
`cargo image --example shell -- -O binary kernel8.img` runs `objcopy -O binary`
for the shell example. Debug the ELF; copy the raw image to the SD card.

## Build script
`cogware-bcm2837\build.rs` handles two different link-script consumers. For
this package's own examples, it passes the absolute linker-script path directly
to `rust-lld`:
```rust
println!("cargo::rustc-link-arg=-T{}", linker_script.display());
```
For `aarch64-unknown-none`, `rust-lld` is invoked directly, so this is not a
host-compiler `-Wl,` argument.
For downstream crates, the script also stages the linker script in `OUT_DIR`
and publishes that directory:
```rust
fs::copy(&linker_script, out_dir.join("link.ld")).expect("failed to stage link.ld in OUT_DIR");
println!("cargo::rustc-link-search={}", out_dir.display());
```
Link arguments do not propagate across a dependency edge, so a downstream
binary must still pass `-C link-arg=-Tlink.ld` in its own Cargo configuration.
The search path is what lets the bare `link.ld` name resolve without the
downstream crate knowing where the BSP stores the script.
The script also keeps build IDs out and tracks non-Rust inputs:
```rust
println!("cargo::rustc-link-arg=--build-id=none");
println!("cargo::rerun-if-changed={}", linker_script.display());
for source in ["src/arch/boot.s", "src/arch/cache.s", "src/arch/exception.s"] {
    println!("cargo::rerun-if-changed={}", manifest_dir.join(source).display());
}
```
Those `.s` files are included with `global_asm!(include_str!(...))`. Without the
explicit triggers, touching assembly can leave Cargo using a stale linked
image.

## Linker script
`cogware-bcm2837\link.ld` sets:
```ld
__kernel_load_addr = 0x80000;
ENTRY(_start)
```
The firmware loads a 64-bit `kernel8.img` to physical `0x80000` and enters the
first byte. QEMU's `raspi3b` follows the same convention.
Section starts are page-aligned:
```ld
PAGE_SIZE = 4K;
```
That lets the MMU apply 4 KiB permissions: `.text` is read-only executable,
`.rodata` is read-only execute-never, and writable data is execute-never.
The ELF program headers describe the same model:
```ld
PHDRS
{
    text   PT_LOAD FLAGS(5);   /* R-X */
    rodata PT_LOAD FLAGS(4);   /* R-- */
    data   PT_LOAD FLAGS(6);   /* RW- */
}
```
`.text` keeps `.text.boot` first, because `_start` must be the first byte of the
image. `.rodata` keeps `.eh_frame` and `.eh_frame_hdr` so GDB can unwind
optimised frames.
`.data` holds initialised writable data. `.bss` is `NOLOAD` and is zeroed by
core 0 before Rust. The translation tables live in `.bss`, so this happens
before `mmu::init()`.
The exported symbols include:
```text
__text_start   __text_end
__rodata_start __rodata_end
__data_start   __data_end
__bss_start    __bss_end
__stacks_start __stacks_end
```
`__data_end` is the page-aligned end of `.data` and `.bss` together. That is why
the boot table prints `.data` and `.bss` from their own symbols.

## Stacks and checks
`.stacks` is:
```ld
.stacks (NOLOAD) : ALIGN(PAGE_SIZE)
{
    __stacks_start = .;
    . += KERNEL_STACK_SIZE * CORE_COUNT;
    __stacks_end = .;
} :data
```
There are four 128 KiB stacks. The section must stay `NOLOAD` and after `.bss`.
If it moves before `.bss`, `kernel8.img` gains roughly half a megabyte of zero
padding. If it becomes part of the zeroed range, boot can erase a live stack.
The script rejects a GOT:
```ld
ASSERT(SIZEOF(.got) == 0,
       "GOT is not empty: the kernel must be linked position-dependent")
```
and rejects an image that outgrows the current fine-grained MMU window.

## Verified commands
The confirmed working commands are:
```powershell
cargo build --release --example shell
cargo build --release --examples
cargo image  --example shell -- -O binary kernel8.img
cargo disasm --example shell -- --disassemble --demangle --section-headers
cargo syms   --example shell -- --numeric-sort
cargo run --release --example shell
```
For a halted QEMU reset:
```powershell
cargo run --release --example shell -- -s -S
.\scripts\run-qemu.ps1 -Gdb            # Linux: ./scripts/run-qemu.sh --gdb
```

## Warning policy and troubleshooting
A build that emits a warning is a broken build. In a bare-metal
kernel, warnings often reveal ABI, layout or `unsafe` drift that QEMU may not
expose immediately. Do not weaken lints to hide them.
Automation PowerShell sessions can inherit a stale `PATH`. If Scoop tools are
installed but invisible, refresh the user path:
```powershell
$env:PATH = [Environment]::GetEnvironmentVariable('Path','User') + ';' + $env:PATH
```
If GDB fails, check that it is `aarch64-none-elf-gdb`, not plain `gdb`. If the
console is silent, check that the first serial is `mon:stdio` and the second is
`null`. If `kernel8.img` suddenly grows by about 512 KiB, inspect `.stacks` in
`cogware-bcm2837\link.ld`.
