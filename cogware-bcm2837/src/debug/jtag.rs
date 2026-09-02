// SPDX-License-Identifier: MIT OR Apache-2.0
//! JTAG enablement for the Raspberry Pi 3.
//!
//! # What software can and cannot do
//!
//! Getting a debugger onto a Pi 3 needs three things, only two of which are
//! under the kernel's control:
//!
//! 1. **The pins must be muxed to the JTAG function.** GPIO 22-27 carry the
//!    full six-signal ARM JTAG port on ALT4. [`route_pins`] does this.
//! 2. **`DBGEN` must be asserted inside the SoC.** It is an input to the
//!    Cortex-A53 debug logic driven by the VideoCore, with *no* system register
//!    to set it; it is asserted by `enable_jtag_gpio=1` in `config.txt`.
//!    Without it the debug port enumerates but every halt request is ignored.
//! 3. **The OS lock and the OS double-lock must be released, and monitor debug
//!    must be enabled.** [`unlock_debug_registers`] does this.
//!
//! `enable_jtag_gpio=1` also performs step 1, so [`route_pins`] is redundant on
//! a correctly configured card but keeps the kernel self-contained.
//!
//! # Pinout (ALT4, the six-wire ARM JTAG port)
//!
//! | GPIO | Signal | 40-pin header |
//! |------|--------|---------------|
//! | 22   | TRST   | 15            |
//! | 23   | RTCK   | 16            |
//! | 24   | TDO    | 18            |
//! | 25   | TCK    | 22            |
//! | 26   | TDI    | 37            |
//! | 27   | TMS    | 13            |
//!
//! None of these clash with GPIO 14/15, so the UART console keeps working
//! while a debugger is attached.
//!
//! # QEMU
//!
//! QEMU has no JTAG transport; use `-s -S` and connect GDB to `localhost:1234`.
//! Everything in this module is harmless under emulation.

use crate::{
    arch::cpu::{read_sysreg, write_sysreg},
    drivers::{GPIO, gpio::Function},
};

/// GPIO pins carrying the JTAG signals on ALT4, in signal order.
pub const JTAG_PINS: [u8; 6] = [22, 23, 24, 25, 26, 27];

/// Signal names, indexed alongside [`JTAG_PINS`].
pub const JTAG_SIGNAL_NAMES: [&str; 6] = ["TRST", "RTCK", "TDO", "TCK", "TDI", "TMS"];

/// Muxes GPIO 22-27 to their ALT4 JTAG function.
///
/// Takes the shared GPIO lock, so the MMU must already be enabled on the
/// calling core.
pub fn route_pins() {
    GPIO.lock().set_functions(&JTAG_PINS, Function::Alt4);
}

/// Releases the debug OS locks and enables monitor debug on the calling core.
///
/// * `OSLAR_EL1 = 0` clears the OS Lock, which the architecture leaves *set*
///   after reset. While set, an external debugger cannot reach the core's
///   debug registers.
/// * `OSDLR_EL1 = 0` clears the OS Double Lock, which otherwise allows the
///   debug logic to be powered down.
/// * `MDSCR_EL1.MDE` enables breakpoint, watchpoint and vector-catch debug
///   exceptions. `KDE` is left clear: setting it would route those exceptions
///   to this same EL instead of to the external debugger.
///
/// These registers are banked per core, so every core must do this for itself.
///
/// # Safety
///
/// Must run at EL1. Enabling `MDE` with a stale hardware breakpoint programmed
/// would take an unexpected debug exception; after reset none are set.
pub unsafe fn unlock_debug_registers() {
    /// `MDSCR_EL1.MDE`, monitor debug events enable.
    const MDE: u64 = 1 << 15;

    unsafe {
        write_sysreg!("OSLAR_EL1", 0);
        write_sysreg!("OSDLR_EL1", 0);

        let mdscr = read_sysreg!("MDSCR_EL1");
        write_sysreg!("MDSCR_EL1", mdscr | MDE);
    }
}

/// Whether the OS Lock is currently released on the calling core.
pub fn os_lock_released() -> bool {
    /// `OSLSR_EL1.OSLK`.
    const OSLK: u64 = 1 << 1;
    read_sysreg!("OSLSR_EL1") & OSLK == 0
}

/// Whether monitor debug events are enabled on the calling core.
pub fn monitor_debug_enabled() -> bool {
    read_sysreg!("MDSCR_EL1") & (1 << 15) != 0
}

/// Full JTAG bring-up for the calling core.
///
/// Core 0 passes `route = true` to mux the pins; secondary cores only need
/// their own debug registers unlocked.
///
/// # Safety
///
/// EL1, MMU enabled (the GPIO lock is taken when `route` is set).
pub unsafe fn init(route: bool) {
    if route {
        route_pins();
    }
    unsafe { unlock_debug_registers() };
}

/// Prints the JTAG configuration, including the `config.txt` reminder.
pub fn report() {
    crate::println!("JTAG");
    for (pin, name) in JTAG_PINS.iter().zip(JTAG_SIGNAL_NAMES) {
        let function = GPIO.lock().function(*pin);
        let status = if function == Function::Alt4 as u32 {
            "ALT4 ok"
        } else {
            "NOT ALT4"
        };
        crate::println!("  GPIO{pin:<2} {name:<4} FSEL=0b{function:03b}  {status}");
    }
    crate::println!(
        "  OS lock released : {}",
        if os_lock_released() { "yes" } else { "no" }
    );
    crate::println!(
        "  MDSCR_EL1.MDE    : {}",
        if monitor_debug_enabled() {
            "set"
        } else {
            "clear"
        }
    );
    crate::println!("  config.txt needs `enable_jtag_gpio=1` to assert DBGEN on hardware.");
}
