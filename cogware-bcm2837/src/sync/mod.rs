// SPDX-License-Identifier: MIT OR Apache-2.0
//! Cross-core synchronisation primitives.
//!
//! All of these rely on `LDXR`/`STXR`, so they may only be used once the
//! calling core's MMU is enabled. See `spinlock`.
//!
//! `Once` and some of `SpinLock`'s methods are unused by the boot demo, hence
//! the `dead_code` allowance.
#![allow(dead_code)]

mod barrier;
mod once;
mod spinlock;

#[allow(unused_imports)]
pub use barrier::Barrier;
#[allow(unused_imports)]
pub use once::Once;
#[allow(unused_imports)]
pub use spinlock::{SpinLock, SpinLockGuard};
