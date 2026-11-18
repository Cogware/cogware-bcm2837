// SPDX-License-Identifier: GPL-3.0-only
//! Owned pins, modes in the type system, and the `embedded-hal` traits.
//!
//! Run with `cargo run --release --example gpio`.
//!
//! Two things are on show.
//!
//! **Ownership.** [`Pins::take`] returns every GPIO exactly once, for the whole
//! machine. Move a pin into a driver and nothing else can touch it; ask for the
//! pins twice and the second call returns `None`.
//!
//! **Modes in the type.** A pin's mode is a type parameter, and changing it
//! consumes the pin by value. `Pin<16, Output>` has `set_high`;
//! `Pin<16, Input<PullUp>>` does not, and calling it does not compile.
//! `Pin<N, MODE>` is zero-sized, so `set_high` compiles to a single store of a
//! constant to a constant address.
//!
//! # What QEMU can and cannot show
//!
//! Stock QEMU's BCM2835 GPIO model keeps the output latch and lets `GPLEVn`
//! read it back, so the output checks below are meaningful everywhere. It
//! models no pulls, no edge detection and no external drive, so a floating
//! input reads whatever was last driven. The cogware QEMU fork models all
//! three, and the `gpio-events` example with `scripts/qemu-scenario.py`
//! exercises them; the pull readings below say which model they came from.

#![no_std]
#![no_main]

use cogware_bcm2837::{
    BootInfo, cpu, entry,
    hal::{
        Delay,
        digital::{InputPin, OpenDrainPin, OutputPin, StatefulOutputPin},
        gpio::{Alt, Input, OpenDrain, Output, Pin, Pins, PullUp},
    },
    println, time,
};

entry!(main);

/// A pin no peripheral on this board claims.
const SCRATCH: u8 = 16;

fn main(_boot: BootInfo) -> ! {
    println!();
    println!("GPIO demo.");

    let pins = Pins::take().expect("nothing else has taken the pins yet");

    println!(
        "  Pins::take() again -> {:?}  (there is only one set)",
        Pins::take().map(|_| "Some")
    );
    println!(
        "  size_of::<Pin<16, Output>>() = {}  (the pin number lives in the type)",
        core::mem::size_of::<Pin<SCRATCH, Output>>()
    );

    let scratch = outputs(pins.p16);
    let scratch = open_drain(scratch);
    let scratch = alt_functions(scratch);
    ecosystem(scratch);

    erased(pins.p17);
    ownership_notes();

    println!();
    println!("Done in {:?}.", time::uptime());
    cpu::halt()
}

/// Driving a pin, and reading the pad back to check it landed.
fn outputs(pin: Pin<SCRATCH, cogware_bcm2837::hal::gpio::Unknown>) -> Pin<SCRATCH, Output> {
    println!();
    println!("-- output --");

    // `into_output_low` sets the level *before* switching the pin to an output,
    // so the pin never briefly drives the wrong way. `into_output` leaves
    // whatever was in the latch.
    let mut pin = pin.into_output_low();
    println!("  into_output_low  -> reads {}", level(pin.is_set_high()));

    pin.set_high();
    println!("  set_high         -> reads {}", level(pin.is_set_high()));

    pin.toggle();
    println!("  toggle           -> reads {}", level(pin.is_set_high()));

    pin.set_state(true);
    println!("  set_state(true)  -> reads {}", level(pin.is_set_high()));

    // An output is readable as an input: `is_set_high` and `is_high` both go
    // through GPLEVn, which reads the pad rather than a remembered value.
    println!(
        "  is_high()        -> {}  (same register, read as an input)",
        InputPin::is_high(&pin)
    );

    // Toggling as fast as the bus allows, to measure a bit-bang loop.
    let start = time::uptime();
    for _ in 0..10_000 {
        pin.set_high();
        pin.set_low();
    }
    let elapsed = time::uptime() - start;
    let ns_per_edge = elapsed.as_nanos() / 20_000;
    println!(
        "  20,000 edges in {:?} -> {} ns/edge, about {} kHz square wave",
        elapsed,
        ns_per_edge,
        500_000 / ns_per_edge.max(1)
    );

    pin
}

/// Open drain: pull down, or let go. Never drive up.
fn open_drain(pin: Pin<SCRATCH, Output>) -> Pin<SCRATCH, OpenDrain> {
    println!();
    println!("-- open drain --");
    println!("  the pad has no open-drain mode, so releasing means becoming an");
    println!("  input and letting the external pull-up win");

    let mut pin = pin.into_open_drain();

    pin.drive_low();
    println!(
        "  drive_low        -> reads {}",
        level(OpenDrainPin::is_high(&pin))
    );

    pin.release();
    let high = OpenDrainPin::is_high(&pin);
    println!(
        "  release          -> reads {} ({})",
        level(high),
        if high {
            "the pull-up wins, as on silicon"
        } else {
            "the stale latch: stock QEMU models no pulls"
        }
    );

    pin
}

/// Alternate functions: handing a pin to a peripheral.
fn alt_functions(pin: Pin<SCRATCH, OpenDrain>) -> Pin<SCRATCH, Output> {
    println!();
    println!("-- alternate functions --");

    // GPIO 16 ALT4 is SPI1 CE2. Selecting it consumes the pin and produces a
    // `Pin<16, Alt<4>>`, which has no `set_high` and no `is_high`.
    let pin: Pin<SCRATCH, Alt<4>> = pin.into_alt::<4>();
    println!("  into_alt::<4>()  -> Pin<16, Alt<4>> (SPI1 CE2)");
    println!("     no set_high, no is_high: the peripheral drives it, not us");

    // Taking it back is another mode change.
    let pin = pin.into_output_low();
    println!("  into_output_low  -> ours again");

    println!();
    println!("  the buses claim their own pins in `init`, so this is only for");
    println!("  functions the BSP has no driver for -- PWM, PCM, SPI1, and so on");

    pin
}

/// Handing a pin to code that was written for some other chip.
fn ecosystem(pin: Pin<SCRATCH, Output>) {
    println!();
    println!("-- embedded-hal --");

    // `blink` below is written against `embedded_hal`, not against this BSP.
    let mut pin = pin;
    let flashes = blink(&mut pin, 3, 2);
    println!("  a generic embedded_hal::digital::OutputPin driver ran {flashes} flashes");
    println!("  errors are Infallible, so `unwrap` there compiles to nothing");
}

/// Ordinary ecosystem-style code: generic, fallible, chip-agnostic.
fn blink<P: embedded_hal::digital::OutputPin>(pin: &mut P, times: u32, ms: u32) -> u32 {
    use embedded_hal::delay::DelayNs;

    let mut delay = Delay::new();

    for _ in 0..times {
        pin.set_high().unwrap();
        delay.delay_ms(ms);
        pin.set_low().unwrap();
        delay.delay_ms(ms);
    }

    times
}

/// When the pin number is not known until run time.
fn erased(pin: Pin<17, cogware_bcm2837::hal::gpio::Unknown>) {
    println!();
    println!("-- runtime pin numbers --");

    // `degrade` trades the compile-time pin number for a `u8`, for an array of
    // pins or a number read from a config file. The mode stays in the type.
    let mut any = pin.into_output_low().degrade();
    any.set_high();

    println!(
        "  Pin<17, Output>::degrade() -> AnyPin<Output> on pin {}, reads {}",
        any.number(),
        level(any.is_set_high())
    );
    println!(
        "  size_of::<AnyPin<Output>>() = {} (one u8, the pin number)",
        core::mem::size_of_val(&any)
    );
}

/// The parts stated in prose rather than demonstrated.
fn ownership_notes() {
    println!();
    println!("-- what is enforced --");
    println!("  Pins::take()            once per boot, on one core");
    println!("  Pin<N, MODE>            zero-sized; N and MODE are compile time");
    println!("  mode change             consumes the pin by value");
    println!("  set_high on an input    does not compile");
    println!("  is_high on Alt<A>       does not compile");
    println!();
    println!("  boot has already claimed some pins; taking them back is allowed");
    println!("  but will break things:");
    println!("    14, 15    PL011 UART (this console)");
    println!("    22-27     JTAG, with the `jtag` feature on");
    println!("    48-53     EMMC / SD card");

    println!();
    println!("-- not modelled by stock QEMU (the cogware fork models them) --");
    println!("  GPPUD/GPPUDCLK   pull-up and pull-down selection");
    println!("  GPEDS, GPREN...  edge detection, so async pin waits never fire");
    println!("  external drive   nothing can pull a pin from outside");

    // The calls are well-formed even where the model is silent: a pulled-up
    // input still sets the registers. `steal` is the escape hatch — a second
    // set of pins, with the uniqueness guarantee handed to the caller.
    let pins = unsafe { Pins::steal() };
    let probe: Pin<5, Input<PullUp>> = pins.p5.into_pull_up_input();
    println!();
    let high = probe.is_high();
    println!(
        "  Pin<5, Input<PullUp>> reads {} ({})",
        level(high),
        if high {
            "pulled up"
        } else {
            "no pull modelled"
        }
    );
}

fn level(high: bool) -> &'static str {
    if high { "high" } else { "low " }
}
