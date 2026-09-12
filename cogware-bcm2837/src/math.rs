// SPDX-License-Identifier: GPL-3.0-only
//! Floating-point operations that `core` does not provide.
//!
//! `core` gives `f32`/`f64` arithmetic, comparison, `abs`, `min`, `max` and
//! `copysign`. `sqrt`, the rounding family and `mul_add` live in `std`, since
//! on a general target those are libm calls.
//!
//! On a Cortex-A53 built for the hard-float `aarch64-unknown-none` target each
//! is a single FPU instruction, issued directly here.
//!
//! ```no_run
//! use cogware_bcm2837::math::Float;
//!
//! let hypot = (3.0f64 * 3.0 + 4.0 * 4.0).sqrt(); // 5.0
//! let ticks = (1.6f64).ceil();                   // 2.0
//! ```
//!
//! # Not provided
//!
//! No transcendentals (`sin`, `ln`, `powf`): the A53 has no instruction for
//! them, and the crate takes no external dependency to get them.
//!
//! # Rounding modes
//!
//! The five rounding operations differ only in tie-breaking and direction, and
//! all respect the IEEE-754 special cases:
//!
//! | Method | Instruction | Behaviour |
//! | --- | --- | --- |
//! | [`Float::floor`] | `frintm` | toward negative infinity |
//! | [`Float::ceil`] | `frintp` | toward positive infinity |
//! | [`Float::trunc`] | `frintz` | toward zero |
//! | [`Float::round`] | `frinta` | nearest, ties away from zero |
//! | [`Float::round_ties_even`] | `frintn` | nearest, ties to even |
//!
//! They match the `std` methods of the same names.

use core::arch::asm;

mod sealed {
    /// Prevents [`super::Float`] being implemented outside this module.
    pub trait Sealed {}
    impl Sealed for f32 {}
    impl Sealed for f64 {}
}

/// The floating-point operations the FPU has and `core` does not expose.
///
/// Implemented for [`f32`] and [`f64`]. Every method is one instruction with
/// no call overhead.
pub trait Float: sealed::Sealed + Copy {
    /// Square root, correctly rounded.
    ///
    /// A negative operand yields NaN; `-0.0` yields `-0.0`. `fsqrt` cannot
    /// fault.
    #[must_use]
    fn sqrt(self) -> Self;

    /// Largest integer less than or equal to `self`.
    #[must_use]
    fn floor(self) -> Self;

    /// Smallest integer greater than or equal to `self`.
    #[must_use]
    fn ceil(self) -> Self;

    /// The integer part, discarding any fractional digits.
    #[must_use]
    fn trunc(self) -> Self;

    /// Nearest integer, with ties rounded away from zero.
    #[must_use]
    fn round(self) -> Self;

    /// Nearest integer, with ties rounded to the even neighbour.
    ///
    /// The IEEE-754 default mode; unlike [`Float::round`] it does not
    /// accumulate an upward bias when summing many rounded values.
    #[must_use]
    fn round_ties_even(self) -> Self;

    /// The fractional part, `self - self.trunc()`.
    #[must_use]
    fn fract(self) -> Self;

    /// `self * multiplier + addend`, with a single rounding at the end.
    ///
    /// The intermediate product is not rounded.
    #[must_use]
    fn mul_add(self, multiplier: Self, addend: Self) -> Self;
}

impl Float for f64 {
    #[inline]
    fn sqrt(self) -> Self {
        let out: f64;
        // SAFETY: a register-to-register FPU instruction: no memory, no stack,
        // cannot fault; out-of-domain inputs produce NaN rather than a trap.
        unsafe {
            asm!("fsqrt {out:d}, {inp:d}", out = lateout(vreg) out, inp = in(vreg) self,
                 options(pure, nomem, nostack));
        }
        out
    }

    #[inline]
    fn floor(self) -> Self {
        round_d::<b'm'>(self)
    }

    #[inline]
    fn ceil(self) -> Self {
        round_d::<b'p'>(self)
    }

    #[inline]
    fn trunc(self) -> Self {
        round_d::<b'z'>(self)
    }

    #[inline]
    fn round(self) -> Self {
        round_d::<b'a'>(self)
    }

    #[inline]
    fn round_ties_even(self) -> Self {
        round_d::<b'n'>(self)
    }

    #[inline]
    fn fract(self) -> Self {
        self - self.trunc()
    }

    #[inline]
    fn mul_add(self, multiplier: Self, addend: Self) -> Self {
        let out: f64;
        // SAFETY: as `sqrt`; `fmadd` is a pure register operation.
        unsafe {
            asm!("fmadd {out:d}, {a:d}, {b:d}, {c:d}",
                 out = lateout(vreg) out,
                 a = in(vreg) self, b = in(vreg) multiplier, c = in(vreg) addend,
                 options(pure, nomem, nostack));
        }
        out
    }
}

impl Float for f32 {
    #[inline]
    fn sqrt(self) -> Self {
        let out: f32;
        // SAFETY: as the `f64` implementation, on `s` registers.
        unsafe {
            asm!("fsqrt {out:s}, {inp:s}", out = lateout(vreg) out, inp = in(vreg) self,
                 options(pure, nomem, nostack));
        }
        out
    }

    #[inline]
    fn floor(self) -> Self {
        round_s::<b'm'>(self)
    }

    #[inline]
    fn ceil(self) -> Self {
        round_s::<b'p'>(self)
    }

    #[inline]
    fn trunc(self) -> Self {
        round_s::<b'z'>(self)
    }

    #[inline]
    fn round(self) -> Self {
        round_s::<b'a'>(self)
    }

    #[inline]
    fn round_ties_even(self) -> Self {
        round_s::<b'n'>(self)
    }

    #[inline]
    fn fract(self) -> Self {
        self - self.trunc()
    }

    #[inline]
    fn mul_add(self, multiplier: Self, addend: Self) -> Self {
        let out: f32;
        // SAFETY: as the `f64` implementation, on `s` registers.
        unsafe {
            asm!("fmadd {out:s}, {a:s}, {b:s}, {c:s}",
                 out = lateout(vreg) out,
                 a = in(vreg) self, b = in(vreg) multiplier, c = in(vreg) addend,
                 options(pure, nomem, nostack));
        }
        out
    }
}

/// `frint<MODE>` on a `d` register.
///
/// The mode is a const generic because `asm!` templates cannot take a runtime
/// string.
#[inline]
fn round_d<const MODE: u8>(value: f64) -> f64 {
    let out: f64;
    // SAFETY: `frint*` is a pure register-to-register FPU instruction: no
    // memory, no stack, no faulting cases.
    unsafe {
        match MODE {
            b'm' => asm!("frintm {out:d}, {inp:d}", out = lateout(vreg) out, inp = in(vreg) value,
                         options(pure, nomem, nostack)),
            b'p' => asm!("frintp {out:d}, {inp:d}", out = lateout(vreg) out, inp = in(vreg) value,
                         options(pure, nomem, nostack)),
            b'z' => asm!("frintz {out:d}, {inp:d}", out = lateout(vreg) out, inp = in(vreg) value,
                         options(pure, nomem, nostack)),
            b'a' => asm!("frinta {out:d}, {inp:d}", out = lateout(vreg) out, inp = in(vreg) value,
                         options(pure, nomem, nostack)),
            _ => asm!("frintn {out:d}, {inp:d}", out = lateout(vreg) out, inp = in(vreg) value,
                      options(pure, nomem, nostack)),
        }
    }
    out
}

/// `frint<MODE>` on an `s` register. See [`round_d`].
#[inline]
fn round_s<const MODE: u8>(value: f32) -> f32 {
    let out: f32;
    // SAFETY: as `round_d`, on a single-precision register.
    unsafe {
        match MODE {
            b'm' => asm!("frintm {out:s}, {inp:s}", out = lateout(vreg) out, inp = in(vreg) value,
                         options(pure, nomem, nostack)),
            b'p' => asm!("frintp {out:s}, {inp:s}", out = lateout(vreg) out, inp = in(vreg) value,
                         options(pure, nomem, nostack)),
            b'z' => asm!("frintz {out:s}, {inp:s}", out = lateout(vreg) out, inp = in(vreg) value,
                         options(pure, nomem, nostack)),
            b'a' => asm!("frinta {out:s}, {inp:s}", out = lateout(vreg) out, inp = in(vreg) value,
                         options(pure, nomem, nostack)),
            _ => asm!("frintn {out:s}, {inp:s}", out = lateout(vreg) out, inp = in(vreg) value,
                      options(pure, nomem, nostack)),
        }
    }
    out
}
