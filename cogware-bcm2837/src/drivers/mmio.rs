// SPDX-License-Identifier: MIT OR Apache-2.0
//! Typed, volatile access to memory-mapped registers.
//!
//! A hand-rolled stand-in for crates like `tock-registers`, providing:
//!
//! * a [`RegisterBlock`] wrapper that ties a base address to a `#[repr(C)]`
//!   struct describing the peripheral, so register offsets are compiler-checked;
//! * [`ReadWrite`], [`ReadOnly`] and [`WriteOnly`] cells that can only be
//!   touched with volatile accesses, so LLVM can never merge, reorder or elide
//!   an MMIO access;
//! * no interior locking — every driver is owned by a
//!   [`crate::sync::SpinLock`] instead.
#![allow(dead_code)]

use core::{cell::UnsafeCell, marker::PhantomData, ptr};

/// A register that can be both read and written.
#[repr(transparent)]
pub struct ReadWrite<T: Copy> {
    value: UnsafeCell<T>,
}

impl<T: Copy> ReadWrite<T> {
    /// Reads the register.
    #[inline(always)]
    pub fn read(&self) -> T {
        unsafe { ptr::read_volatile(self.value.get()) }
    }

    /// Writes the register.
    #[inline(always)]
    pub fn write(&self, value: T) {
        unsafe { ptr::write_volatile(self.value.get(), value) }
    }

    /// Read-modify-write. Not atomic: hold the driver's lock around this.
    #[inline(always)]
    pub fn modify(&self, f: impl FnOnce(T) -> T) {
        self.write(f(self.read()));
    }
}

/// A register that is only ever read.
#[repr(transparent)]
pub struct ReadOnly<T: Copy> {
    value: UnsafeCell<T>,
}

impl<T: Copy> ReadOnly<T> {
    /// Reads the register.
    #[inline(always)]
    pub fn read(&self) -> T {
        unsafe { ptr::read_volatile(self.value.get()) }
    }
}

/// A register that is only ever written. Reads are meaningless or have side
/// effects, so no read accessor is provided.
#[repr(transparent)]
pub struct WriteOnly<T: Copy> {
    value: UnsafeCell<T>,
}

impl<T: Copy> WriteOnly<T> {
    /// Writes the register.
    #[inline(always)]
    pub fn write(&self, value: T) {
        unsafe { ptr::write_volatile(self.value.get(), value) }
    }
}

/// Binds a physical base address to the `#[repr(C)]` struct that describes the
/// peripheral's register layout.
///
/// Deliberately not [`Sync`]: a peripheral must be owned by exactly one
/// [`crate::sync::SpinLock`] before it can be shared across cores.
pub struct RegisterBlock<T> {
    base: usize,
    _layout: PhantomData<*const T>,
}

// The base address is just an integer, so moving a driver between cores is
// fine; sharing it without a lock is not (hence no `Sync`).
unsafe impl<T> Send for RegisterBlock<T> {}

impl<T> RegisterBlock<T> {
    /// Creates a handle to the peripheral mapped at `base`.
    ///
    /// # Safety
    ///
    /// * `base` must be the address of a `T`-shaped register block that stays
    ///   mapped and valid for the lifetime of the returned value.
    /// * No other `RegisterBlock` may alias the same peripheral, unless all of
    ///   them are behind the same lock.
    pub const unsafe fn new(base: usize) -> Self {
        Self {
            base,
            _layout: PhantomData,
        }
    }

    /// The base address this block was created with.
    #[inline(always)]
    pub const fn base_addr(&self) -> usize {
        self.base
    }

    /// Borrows the register layout.
    #[inline(always)]
    pub fn regs(&self) -> &T {
        // SAFETY: upheld by the caller of `new`.
        unsafe { &*(self.base as *const T) }
    }
}

/// Ensures previously issued MMIO writes have been observed by the peripheral
/// before any subsequent memory access is issued.
///
/// Device-nGnRnE memory already forbids reordering by the CPU, but the
/// compiler's view still needs the barrier when mixing MMIO with normal memory.
#[inline(always)]
pub fn barrier() {
    // SAFETY: `dsb sy` has no operands and no memory effects of its own.
    unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) };
}
