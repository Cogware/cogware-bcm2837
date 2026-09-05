// SPDX-License-Identifier: MIT OR Apache-2.0
//! Panic handling.
//!
//! With four cores and one UART, the panicking core may already hold the
//! console lock, and other cores may panic while the first is still printing.
//! Both cases are handled here.

use core::{
    panic::PanicInfo,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{
    arch::{cpu, mmu},
    console,
};

/// Set by the first core to panic, so later ones do not interleave output.
static PANICKING: AtomicBool = AtomicBool::new(false);

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    cpu::mask_interrupts();
    let already_panicking = if mmu::is_enabled() {
        // Claim the report across cores, including a recursive formatting panic.
        PANICKING.swap(true, Ordering::AcqRel)
    } else {
        // Boot is single-core here, and exclusive atomics require the MMU.
        let previous = PANICKING.load(Ordering::Relaxed);
        PANICKING.store(true, Ordering::Relaxed);
        previous
    };
    if already_panicking {
        cpu::halt();
    }

    // There is no second diagnostic channel if the UART itself fails.
    let _ = console::with_emergency_writer(|w| {
        writeln!(w)?;
        writeln!(w, "############### KERNEL PANIC ###############")?;
        writeln!(w, "core   : {}", cpu::core_id())?;
        writeln!(w, "EL     : {}", cpu::current_el())?;
        match info.location() {
            Some(location) => writeln!(
                w,
                "at     : {}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            )?,
            None => writeln!(w, "at     : <unknown location>")?,
        }
        writeln!(w, "message: {}", info.message())?;
        writeln!(w, "###########################################")
    });
    cpu::halt();
}
