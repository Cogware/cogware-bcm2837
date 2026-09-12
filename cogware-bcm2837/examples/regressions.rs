// SPDX-License-Identifier: GPL-3.0-only
//! Regression checks for guard auto-traits, console exclusion and timer fairness.
//!
//! Run with `cargo run --release --example regressions`. Success prints
//! `regressions: PASS`; any violated invariant panics instead.

#![no_std]
#![no_main]

use core::{
    cell::Cell,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use cogware_bcm2837::{
    BootInfo, arch::exception, console, cpu, entry, println, smp, sync::SpinLockGuard, task, time,
};

entry!(primary = main, secondary = secondary);

// Inference is ambiguous if the guard implements Sync. This checks the
// negative bound without a dependency or an intentionally failing build.
macro_rules! assert_not_sync {
    ($guard:ty) => {
        const _: fn() = || {
            trait AmbiguousIfSync<A> {
                fn check() {}
            }
            impl<T: ?Sized> AmbiguousIfSync<()> for T {}
            impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for T {}
            let _ = <$guard as AmbiguousIfSync<_>>::check;
        };
    };
}

assert_not_sync!(SpinLockGuard<'static, Cell<u32>>);
assert_not_sync!(task::MutexGuard<'static, Cell<u32>>);

const _: fn() = || {
    fn send<T: Send>() {}
    fn sync<T: Sync>() {}
    send::<SpinLockGuard<'static, Cell<u32>>>();
    send::<task::MutexGuard<'static, Cell<u32>>>();
    sync::<SpinLockGuard<'static, u32>>();
    sync::<task::MutexGuard<'static, u32>>();
};

static CONSOLE_HELD: AtomicBool = AtomicBool::new(false);
static START_CONTENDER: AtomicBool = AtomicBool::new(false);
static CONTENDER_STARTED: AtomicBool = AtomicBool::new(false);
static CONTENDER_ENTERED: AtomicBool = AtomicBool::new(false);
static CONSOLE_ALIASED: AtomicBool = AtomicBool::new(false);
static TIMER_DONE: AtomicBool = AtomicBool::new(false);

fn main(boot: BootInfo) -> ! {
    assert!(boot.healthy(), "regression kernel did not boot cleanly");
    // SAFETY: core 0, once, after the BSP has initialised the MMU and console.
    unsafe { smp::start_secondaries() };
    assert_eq!(smp::wait_for_cores(4, 1_000_000), 4);

    let (counts_unchanged, emergency_ok) = console::with_writer(|uart| {
        CONSOLE_HELD.store(true, Ordering::Release);
        START_CONTENDER.store(true, Ordering::Release);
        // Deliberately keep this lock held during a bounded contention check.
        // Production code must not wait for other cores while holding UART.
        wait_for(&CONTENDER_STARTED);
        time::spin_for_micros(10_000);

        let before = uart.chars_written();
        let emergency_ok = console::with_emergency_writer(|writer| {
            writeln!(writer, "emergency output while UART is held")
        })
        .is_ok();
        exception::trigger_breakpoint();
        let counts_unchanged = uart.chars_written() == before;
        CONSOLE_HELD.store(false, Ordering::Release);
        (counts_unchanged, emergency_ok)
    });
    wait_for(&CONTENDER_ENTERED);
    assert!(!CONSOLE_ALIASED.load(Ordering::Acquire));
    assert!(counts_unchanged, "emergency output touched driver counters");
    assert!(emergency_ok, "emergency UART output timed out");
    println!("guard and console regressions: PASS");

    task::spawn("sleeping-regression", async {
        task::sleep_millis(5).await;
        TIMER_DONE.store(true, Ordering::Release);
    })
    .expect("timer regression must fit in an empty task table");
    task::spawn("yielding-regression", async {
        let deadline = time::uptime() + Duration::from_secs(1);
        while !TIMER_DONE.load(Ordering::Acquire) {
            assert!(
                time::uptime() < deadline,
                "a yielding task starved the timer"
            );
            task::yield_now().await;
        }
        println!("regressions: PASS");
        cpu::halt();
    })
    .expect("yield regression must fit in an empty task table");
    task::run()
}

fn secondary(boot: BootInfo) -> ! {
    if boot.core_id == 1 {
        wait_for(&START_CONTENDER);
        CONTENDER_STARTED.store(true, Ordering::Release);
        console::with_writer(|_| {
            CONSOLE_ALIASED.store(CONSOLE_HELD.load(Ordering::Acquire), Ordering::Release);
        });
        CONTENDER_ENTERED.store(true, Ordering::Release);
    }
    cpu::halt()
}

fn wait_for(flag: &AtomicBool) {
    let deadline = time::uptime() + Duration::from_secs(1);
    while !flag.load(Ordering::Acquire) {
        assert!(time::uptime() < deadline, "regression handshake timed out");
        core::hint::spin_loop();
    }
}
