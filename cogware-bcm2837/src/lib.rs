// SPDX-License-Identifier: GPL-3.0-only
//! # cogware-bcm2837
//!
//! A bare-metal board support package for the Broadcom **BCM2837** — the
//! quad-core Cortex-A53 SoC in the Raspberry Pi 3 — written for current stable
//! Rust and the **hard-float** `aarch64-unknown-none` target.
//!
//! The crate takes a kernel image from the firmware's first instruction to a
//! point where safe Rust runs on four cores with the MMU, caches, FPU and
//! `alloc` all live.
//!
//! ## What it provides
//!
//! * Boots at EL1 on all four cores, with per-core stacks and zeroed `.bss`.
//! * Builds and enables an identity-mapped stage-1 MMU with per-section
//!   permissions: `.text` is read-execute, everything else no-execute.
//! * Ungates FP/SIMD for this hard-float target.
//! * Installs a full exception vector table that saves the entire NEON
//!   register file.
//! * Brings up the PL011 UART and routes `print!`/`println!` to it.
//! * Initialises an O(1) [`heap`] for `Box`, `Vec` and `String`.
//! * Routes JTAG to GPIO 22-27 and unlocks the debug registers.
//! * Offers MMIO drivers for GPIO, the VideoCore mailbox, the SD/EMMC
//!   controller, the framebuffer, DMA, PWM, the clock manager, the camera
//!   receivers and the DSI hosts, each behind a [`sync::SpinLock`] so any core
//!   may use them, and an interrupt dispatcher whose handlers wake tasks.
//! * Provides a no-`alloc` cooperative [`task`] executor, so each core can run
//!   an `async` main loop.
//!
//! The `embedded-hal` and `embedded-hal-async` trait definitions are the only
//! optional dependencies, both off by default.
//!
//! ## Getting started
//!
//! A kernel is a `#![no_std] #![no_main]` binary that hands the BSP a
//! function to call once the machine is up:
//!
//! ```ignore
//! #![no_std]
//! #![no_main]
//!
//! use cogware_bcm2837::{BootInfo, entry, println};
//!
//! entry!(main);
//!
//! fn main(boot: BootInfo) -> ! {
//!     println!("hello from core {} at EL{}", boot.core_id, boot.exception_level);
//!     cogware_bcm2837::arch::cpu::halt()
//! }
//! ```
//!
//! By the time `main` is called the console, MMU, caches, exception vectors and
//! heap are up. Cores 1-3 are parked; [`smp::start_secondaries`] releases
//! them into the second function passed to [`entry!`].
//!
//! Downstream crates need the target, the linker script and a handful of
//! `rustflags`. See `docs/16-using-the-bsp.md`.
//!
//! ## Examples
//!
//! Every example in `examples/` is a complete, bootable kernel:
//!
//! | Example | Shows |
//! | --- | --- |
//! | `hello-uart` | the smallest possible kernel |
//! | `multicore` | releasing cores 1-3 and sharing a lock |
//! | `async-tasks` | the executor, channels and timers |
//! | `heap` | `Box`/`Vec`/`String` and heap statistics |
//! | `display` | framebuffer mode set, blitting and text |
//! | `sdcard` | EMMC bring-up, block read and write |
//! | `gpio` | typestate pins, alt functions, `embedded-hal` |
//! | `bitbang` | software SPI and I²C against simulated devices |
//! | `buses` | hardware SPI0 and BSC1, shared across cores |
//! | `async-bus` | `embedded-hal-async` buses, delays and pin waits |
//! | `peripherals` | clock manager, system timer, RNG, DMA and PWM |
//! | `interrupts` | per-core ticks, IPIs and interrupt-driven DMA |
//! | `camera` | a CSI-2 receiver capturing one frame (hardware only) |
//! | `dsi-panel` | a DSI panel in command mode (hardware only) |
//! | `dsi-video` | a DSI panel in video mode (hardware only) |
//! | `camera-preview` | a raw camera frame rendered on HDMI |
//! | `shell` | all of the above, interactively |
//! | `chainloader` | receives a kernel over serial and boots it |
//!
//! Run one with `cargo run --release --example hello-uart`.
//!
//! ## Safety model
//!
//! Boot runs with the MMU off, mutates system registers and reaches hardware
//! through raw pointers; all of that is confined to this crate. Every `unsafe`
//! block carries a `SAFETY:` comment naming the invariant it relies on, and the
//! public API is safe wherever it can be. Where it cannot —
//! [`smp::start_secondaries`] and the `init` functions — the contract is
//! documented under a `# Safety` heading.

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

// `alloc` follows the `allocator` feature. Only the QOI decoder needs it, so
// turning the feature off must leave a BSP that links without a global
// allocator at all.
#[cfg(feature = "allocator")]
extern crate alloc;

#[macro_use]
pub mod print;

pub mod arch;
pub mod boot;
pub mod bsp;
pub mod console;
pub mod debug;
pub mod devices;
pub mod drivers;
pub mod hal;
pub mod heap;
pub mod math;
pub mod smp;
pub mod sync;
pub mod task;
pub mod time;

// A binary may contain exactly one panic handler. A downstream crate that
// wants its own turns this feature off.
#[cfg(feature = "panic-handler")]
mod panic;

pub use arch::cpu;
pub use boot::BootInfo;
