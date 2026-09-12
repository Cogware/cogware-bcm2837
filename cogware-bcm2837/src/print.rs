// SPDX-License-Identifier: GPL-3.0-only
//! `print!` / `println!` for the kernel console.
//!
//! The macros are `#[macro_export]`ed, so they live at the crate root and are
//! reached as `crate::print!` / `crate::println!`.

/// Writes formatted output to the console UART.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::console::_print(::core::format_args!($($arg)*))
    };
}

/// Writes formatted output to the console UART, followed by a newline.
///
/// The driver expands `\n` to `\r\n`, so terminals do not need `stty onlcr`.
#[macro_export]
macro_rules! println {
    () => {
        $crate::print!("\n")
    };
    ($($arg:tt)*) => {
        $crate::console::_print(::core::format_args!("{}\n", ::core::format_args!($($arg)*)))
    };
}

/// Like [`println!`], but prefixed with the calling core's id.
#[macro_export]
macro_rules! cprintln {
    () => {
        $crate::println!("[core {}]", $crate::cpu::core_id())
    };
    ($($arg:tt)*) => {
        $crate::println!(
            "[core {}] {}",
            $crate::cpu::core_id(),
            ::core::format_args!($($arg)*)
        )
    };
}
