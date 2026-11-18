// SPDX-License-Identifier: GPL-3.0-only
//! The clock manager, system timer, RNG, DMA and PWM, each checked once.
//!
//! Run with `cargo run --release --example peripherals`.
//!
//! Every line reports pass or fail against something read back from the
//! hardware. QEMU emulates all of these except PWM, whose registers it
//! accepts and ignores, so the PWM line there proves only that nothing hangs;
//! on a board, a scope on GPIO 12 should show 1 kHz at 25 % duty.

#![no_std]
#![no_main]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

use cogware_bcm2837::{
    BootInfo, cpu,
    drivers::{
        self,
        clock::{ClockId, Mash, Source},
        dma::{self, Kind},
        pwm, systimer,
    },
    entry, println, time,
};

entry!(main);

fn main(_boot: BootInfo) -> ! {
    println!();
    println!("Peripheral checks");
    println!();

    let mut failures = 0;
    let mut check = |name: &str, ok: bool, detail: &dyn core::fmt::Display| {
        println!(
            "  {name:<10}: {} {detail}",
            if ok { "ok  " } else { "FAIL" }
        );
        failures += usize::from(!ok);
    };

    // GPCLK0 at 4.8 MHz is an exact divide of the oscillator, so the rate read
    // back from the registers must match to the hertz.
    let gp0 =
        drivers::CLOCKS
            .lock()
            .enable(ClockId::Gp0, Source::Oscillator, 4_800_000, Mash::Integer);
    let readback = drivers::CLOCKS.lock().rate(ClockId::Gp0);
    check(
        "clock",
        gp0 == Ok(4_800_000) && readback == Some(4_800_000),
        &format_args!("GPCLK0 {gp0:?}, reads back {readback:?}"),
    );
    let _ = drivers::CLOCKS.lock().disable(ClockId::Gp0);

    // 10 ms of the architectural timer should be 10 000 system-timer ticks;
    // allow for the time the reads themselves take.
    let start = systimer::micros();
    time::spin_for_micros(10_000);
    let elapsed = systimer::micros() - start;
    check(
        "systimer",
        (9_900..11_000).contains(&elapsed),
        &format_args!("{elapsed} us across a 10 ms spin"),
    );

    systimer::set_compare_in(systimer::Channel::Three, 2_000);
    let armed = !systimer::matched(systimer::Channel::Three);
    time::spin_for_micros(3_000);
    let fired = systimer::matched(systimer::Channel::Three);
    systimer::clear(systimer::Channel::Three);
    check(
        "compare",
        armed && fired && !systimer::matched(systimer::Channel::Three),
        &format_args!("channel 3 matched after 2 ms"),
    );

    let mut rng = drivers::RNG.lock();
    rng.init();
    let words = [rng.next_u32(), rng.next_u32(), rng.next_u32()];
    drop(rng);
    let ok = words.iter().all(Result::is_ok) && (words[0] != words[1] || words[1] != words[2]);
    check("rng", ok, &format_args!("{words:x?}"));

    dma_checks(&mut check);

    let pwm = pwm_check();
    check("pwm", pwm.is_ok(), &format_args!("{pwm:?}"));

    println!();
    if failures == 0 {
        println!("All peripheral checks passed.");
    } else {
        println!("{failures} peripheral check(s) FAILED.");
    }
    cpu::halt()
}

/// Copy and fill through a full and a lite channel, checking every word.
fn dma_checks(check: &mut impl FnMut(&str, bool, &dyn core::fmt::Display)) {
    let mut src = [0u32; 4096];
    let mut dst = [0u32; 4096];
    for (i, word) in src.iter_mut().enumerate() {
        *word = (i as u32).wrapping_mul(0x9e37_79b9);
    }

    for kind in [Kind::Full, Kind::Lite] {
        let Some(mut channel) = dma::claim(kind) else {
            check("dma", false, &format_args!("no {kind:?} channel free"));
            continue;
        };
        dst.fill(0);
        let copied = channel.copy(&src, &mut dst);
        let copy_ok = copied.is_ok() && src == dst;
        let filled = channel.fill(&mut dst, 0xdead_beef);
        let fill_ok = filled.is_ok() && dst.iter().all(|&w| w == 0xdead_beef);
        check(
            "dma",
            copy_ok && fill_ok,
            &format_args!(
                "channel {} ({kind:?}): 16 KiB copy {copied:?}, fill {filled:?}",
                channel.index()
            ),
        );
    }
}

/// 1 kHz at 25 % on GPIO 12.
fn pwm_check() -> Result<u32, pwm::Error> {
    let mut pwm = drivers::PWM.lock();
    pwm.init(1_000_000)?;
    let channel = pwm.route(12)?;
    let range = pwm.set_frequency(channel, 1_000);
    pwm.set_duty(channel, range / 4);
    Ok(range)
}
