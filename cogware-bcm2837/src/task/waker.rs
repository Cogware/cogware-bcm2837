// SPDX-License-Identifier: GPL-3.0-only
//! The [`RawWaker`] implementation.
//!
//! A waker carries no ownership: its data pointer *is* the task index, so
//! `clone` is a copy, `drop` is a no-op and there is no reference count. This
//! is why the runtime needs no allocator.
//!
//! A waker outliving its task will, if the slot has since been reused, wake
//! whatever task now occupies it. That costs one spurious poll, which the
//! [`Future`](core::future::Future) contract permits and every future here
//! tolerates by re-checking its condition on entry.

use core::{
    ptr,
    task::{RawWaker, RawWakerVTable},
};

use super::{TaskId, executor};

/// The one and only waker vtable.
static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);

/// Builds a waker for `task`.
pub(super) fn raw_waker(task: TaskId) -> RawWaker {
    // `without_provenance`: this is an integer wearing a pointer's clothing,
    // never dereferenced.
    RawWaker::new(ptr::without_provenance(task.index()), &VTABLE)
}

/// Marks the encoded task runnable.
fn wake_index(data: *const ()) {
    if let Some(task) = TaskId::from_index(data.addr()) {
        executor::wake(task);
    }
}

/// # Safety
///
/// Trivially safe: the data pointer is an integer and is only copied.
unsafe fn clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &VTABLE)
}

/// # Safety
///
/// Trivially safe: see [`clone`].
unsafe fn wake(data: *const ()) {
    wake_index(data);
}

/// # Safety
///
/// Trivially safe: see [`clone`].
unsafe fn wake_by_ref(data: *const ()) {
    wake_index(data);
}

/// # Safety
///
/// Trivially safe: there is nothing to release.
unsafe fn drop_waker(_data: *const ()) {}
