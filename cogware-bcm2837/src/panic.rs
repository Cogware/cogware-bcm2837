// SPDX-License-Identifier: MIT OR Apache-2.0
//! Panic handling.
//!
//! With four cores and one UART, the panicking core may already hold the
//! console lock, and other cores may panic while the first is still printing.
//! Both cases are handled here.

use core::{
    panic::PanicInfo,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use crate::{arch::cpu, console};

/// Set by the first core to panic, so later ones do not interleave output.
static PANICKING: AtomicBool = AtomicBool::new(false);

/// Guards against a panic raised from inside the panic handler itself.
static PANIC_DEPTH: AtomicU32 = AtomicU32::new(0);

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    // A nested panic means formatting or the console is broken; stop.
    if PANIC_DEPTH.fetch_add(1, Ordering::Relaxed) > 0 {
        cpu::halt();
    }

    // The current console holder will never finish; steal the lock.
    //
    // SAFETY: the panic path; a garbled line is preferable to a silent hang.
    unsafe { console::force_unlock() };

    if PANICKING.swap(true, Ordering::AcqRel) {
        // Another core is already printing; report briefly and stop.
        crate::println!("[core {}] panicked as well, stopping.", cpu::core_id());
        cpu::halt();
    }

    crate::println!();
    crate::println!("############### KERNEL PANIC ###############");
    crate::println!("core   : {}", cpu::core_id());
    crate::println!("EL     : {}", cpu::current_el());

    match info.location() {
        Some(location) => crate::println!(
            "at     : {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        ),
        None => crate::println!("at     : <unknown location>"),
    }

    crate::println!("message: {}", info.message());
    crate::println!("###########################################");

    console::flush();
    cpu::halt();
}
