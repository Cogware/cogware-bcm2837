// SPDX-License-Identifier: GPL-3.0-only
//! Driver layer: raw MMIO access plus the peripherals this kernel needs.
//!
//! Every peripheral is a singleton wrapped in a [`SpinLock`], which is how
//! four cores share one register block safely. Nothing else in the kernel may
//! construct a second [`gpio::Gpio`], [`pl011::Pl011Uart`],
//! [`mailbox::Mailbox`] or [`framebuffer::Framebuffer`].
//!
//! # Adding a peripheral
//!
//! 1. Add its base address to [`crate::bsp::memory::map`]. It is already
//!    mapped Device-nGnRnE, because the MMU maps the whole peripheral window.
//! 2. Describe its registers as a `#[repr(C)]` struct of
//!    [`mmio::ReadWrite`] / [`mmio::ReadOnly`] / [`mmio::WriteOnly`] cells and
//!    wrap it in an [`mmio::RegisterBlock`], so offsets are compiler-checked.
//! 3. Add a `pub static` here wrapping it in a [`SpinLock`].
//!
//! Anything the VideoCore owns — the ARM and core clocks, power domains, the
//! framebuffer — is not an MMIO peripheral and goes through [`mailbox`]
//! instead. The peripheral clock generators are the exception: [`clock`]
//! programs them directly.

pub mod clock;
pub mod display;
pub mod dma;
pub mod dsi;
pub mod emmc;
pub mod font;
pub mod framebuffer;
pub mod gpio;
pub mod hvs;
pub mod i2c;
pub mod interrupt;
pub mod mailbox;
pub mod mmio;
pub mod pixelvalve;
pub mod pl011;
pub mod power;
pub mod pwm;
// Decoding produces a `Vec`, so this is the one module that needs `alloc`.
#[cfg(feature = "allocator")]
pub mod qoi;
pub mod rng;
pub mod spi;
pub mod systimer;
pub mod unicam;

use crate::sync::SpinLock;

/// The GPIO block, shared by every core.
///
/// Pin function selection is a read-modify-write of a register that several
/// unrelated drivers touch, so it must be serialised.
///
/// SAFETY: this is the only `Gpio` in the kernel; nothing else calls
/// `Gpio::new`.
pub static GPIO: SpinLock<gpio::Gpio> = SpinLock::new(unsafe { gpio::Gpio::new() });

/// The PL011 console UART, shared by every core.
///
/// SAFETY: as above, the only instance.
pub static UART: SpinLock<pl011::Pl011Uart> = SpinLock::new(unsafe { pl011::Pl011Uart::new() });

/// The power manager, shared by every core.
///
/// Only ever used to reset the board, so the lock is there for the same reason
/// the others are rather than because two cores contend for it.
///
/// SAFETY: as above, the only instance.
pub static POWER: SpinLock<power::Power> = SpinLock::new(unsafe { power::Power::new() });

/// The clock manager, shared by every core.
///
/// A leaf lock: drivers that clock themselves ([`PWM`], the camera and display
/// hosts) take it inside their own, and it never takes another.
///
/// SAFETY: as above, the only instance.
pub static CLOCKS: SpinLock<clock::Clocks> = SpinLock::new(unsafe { clock::Clocks::new() });

/// The PWM controller, shared by every core.
///
/// Starts unclocked; call `Pwm::init` before configuring a channel.
///
/// SAFETY: as above, the only instance.
pub static PWM: SpinLock<pwm::Pwm> = SpinLock::new(unsafe { pwm::Pwm::new() });

/// The hardware random number generator, shared by every core.
///
/// Call `Rng::init` once before reading.
///
/// SAFETY: as above, the only instance.
pub static RNG: SpinLock<rng::Rng> = SpinLock::new(unsafe { rng::Rng::new() });

/// Unicam receiver 0 (`CAM0`, two lanes), shared by every core.
///
/// Power it with `unicam::power_on` before `Unicam::start`.
///
/// SAFETY: as above, the only instance for this port.
pub static CAM0: SpinLock<unicam::Unicam> =
    SpinLock::new(unsafe { unicam::Unicam::new(unicam::Port::Cam0) });

/// Unicam receiver 1 (`CAM1`, four lanes), shared by every core.
///
/// SAFETY: as above, the only instance for this port.
pub static CAM1: SpinLock<unicam::Unicam> =
    SpinLock::new(unsafe { unicam::Unicam::new(unicam::Port::Cam1) });

/// The DSI0 host (`DISP0`, two lanes), shared by every core.
///
/// Power it with `dsi::power_on` before `Dsi::init`.
///
/// SAFETY: as above, the only instance for this port.
pub static DSI0: SpinLock<dsi::Dsi> = SpinLock::new(unsafe { dsi::Dsi::new(dsi::Port::Dsi0) });

/// The DSI1 host (`DISP1`, four lanes), shared by every core.
///
/// SAFETY: as above, the only instance for this port.
pub static DSI1: SpinLock<dsi::Dsi> = SpinLock::new(unsafe { dsi::Dsi::new(dsi::Port::Dsi1) });

/// The HVS display compositor, shared by every core.
///
/// The firmware drives channel 1 for HDMI; only channels 0 and 2 are ours.
///
/// SAFETY: as above, the only instance.
pub static HVS: SpinLock<hvs::Hvs> = SpinLock::new(unsafe { hvs::Hvs::new() });

/// Pixel valve 0 (DSI0, DPI), shared by every core.
///
/// SAFETY: as above, the only instance for this valve.
pub static PIXELVALVE0: SpinLock<pixelvalve::PixelValve> =
    SpinLock::new(unsafe { pixelvalve::PixelValve::new(pixelvalve::Valve::Zero) });

/// Pixel valve 1 (DSI1), shared by every core.
///
/// SAFETY: as above, the only instance for this valve.
pub static PIXELVALVE1: SpinLock<pixelvalve::PixelValve> =
    SpinLock::new(unsafe { pixelvalve::PixelValve::new(pixelvalve::Valve::One) });

/// The VideoCore property mailbox, shared by every core.
///
/// The lock protects more than the registers: all property transactions share a
/// single statically allocated, 16-byte-aligned message buffer, so two cores
/// must never be mid-transaction at once.
///
/// SAFETY: as above, the only instance.
pub static MAILBOX: SpinLock<mailbox::Mailbox> = SpinLock::new(unsafe { mailbox::Mailbox::new() });

/// The VideoCore framebuffer, shared by every core.
///
/// Starts unconfigured; nothing is allocated until `Framebuffer::init` runs.
/// The lock also serialises the mailbox transactions a mode change and a page
/// flip make, so `init` must not be called with [`MAILBOX`] already held.
///
/// SAFETY: as above, the only instance.
pub static FRAMEBUFFER: SpinLock<framebuffer::Framebuffer> =
    SpinLock::new(unsafe { framebuffer::Framebuffer::new() });

/// The Arasan EMMC controller driving the SD card slot, shared by every core.
///
/// Starts uninitialised: card discovery costs an ACMD41 busy-loop that can run
/// for up to a second on a slow card. Call `Emmc::init` before any transfer.
///
/// SAFETY: as above, the only instance.
pub static EMMC: SpinLock<emmc::Emmc> = SpinLock::new(unsafe { emmc::Emmc::new() });

/// The CogWare logo, as a QOI image.
///
/// Decoded on demand by `qoi::decode`, which needs the `allocator` feature.
pub static LOGO_QOI: &[u8] = include_bytes!("../../assets/CogWare.qoi");

/// The SPI0 master, shared by every core.
///
/// Starts unconfigured; call `Spi0::init` before any transfer. `init` claims
/// GPIO 7-11.
///
/// SAFETY: this is the only `Spi0` in the kernel.
pub static SPI0: SpinLock<spi::Spi0> = SpinLock::new(unsafe { spi::Spi0::new() });

/// BSC0, the I2C master on GPIO 0/1, shared by every core.
///
/// Not the 40-pin header bus — see [`I2C1`] for that. On a board with a HAT
/// fitted, these two pins carry its ID EEPROM.
///
/// SAFETY: this is the only `I2c` for BSC0.
pub static I2C0: SpinLock<i2c::I2c> = SpinLock::new(unsafe { i2c::I2c::new_bsc0() });

/// BSC1, the I2C master on GPIO 2/3, shared by every core.
///
/// The bus on pins 3 and 5 of the 40-pin header. Starts unconfigured; call
/// `I2c::init` before any transfer.
///
/// SAFETY: this is the only `I2c` for BSC1.
pub static I2C1: SpinLock<i2c::I2c> = SpinLock::new(unsafe { i2c::I2c::new_bsc1() });
