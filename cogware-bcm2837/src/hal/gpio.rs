// SPDX-License-Identifier: MIT OR Apache-2.0
//! Owned GPIO pins, with the mode tracked in the type.
//!
//! [`crate::drivers::gpio`] is the register-level driver: one `Gpio` behind one
//! lock, addressed by pin number. Nothing stops `set_high(9)` on a pin
//! configured as an input, or handing the same pin to two drivers.
//!
//! This module can. A [`Pin`] is a zero-sized token carrying its number *and
//! its mode* in the type:
//!
//! ```ignore
//! let pins = Pins::take().unwrap();
//! let mut led = pins.p21.into_output_low();
//! led.set_high();                       // fine: it is an Output
//!
//! let button = pins.p16.into_pull_up_input();
//! button.set_high();                    // does not compile: it is an Input
//! ```
//!
//! `Pins::take` hands out each pin exactly once and a mode change consumes the
//! pin by value, so a pin moved into a driver is gone: two drivers cannot hold
//! GPIO 10 at the same time.
//!
//! # What costs a lock and what does not
//!
//! Changing a pin's mode writes `GPFSELn`, where six pins share one register.
//! That is a read-modify-write, so it takes [`crate::drivers::GPIO`].
//!
//! Driving and sampling do not. `GPSETn`/`GPCLRn` are write-1-to-act and
//! `GPLEVn` is a pure read, so [`OutputPin::set_high`], [`OutputPin::set_low`]
//! and [`InputPin::is_high`] are a single store or load with no
//! synchronisation at all.
//!
//! [`OpenDrain`] is the exception: it signals by changing *direction*, so every
//! transition is a `GPFSEL` write and does take the lock. See [`OpenDrain`].
//!
//! # Pins the BSP has already claimed
//!
//! `Pins::take` hands out all 54 pins, including ones boot has configured.
//! Reconfiguring these will break the thing that was using them:
//!
//! | Pins | Used by | Effect of repurposing |
//! | --- | --- | --- |
//! | 14, 15 | PL011 console (ALT0) | `print!`/`println!` stop working |
//! | 22-27 | JTAG (ALT4), with the `jtag` feature | a debug probe loses the target |
//! | 48-53 | EMMC / SD card (ALT3) | [`crate::drivers::EMMC`] stops working |

use core::{
    marker::PhantomData,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::drivers::{
    GPIO,
    gpio::{Function, MAX_PIN, Pull, clear_event, read_event, read_level, write_high, write_low},
};

pub use crate::hal::digital::{InputPin, OpenDrainPin, OutputPin, StatefulOutputPin};

// ---------------------------------------------------------------------------
// Mode markers
// ---------------------------------------------------------------------------

/// Pull-resistor marker: no pull, the pad floats when nothing drives it.
pub struct Floating;

/// Pull-resistor marker: the internal pull-up is enabled.
pub struct PullUp;

/// Pull-resistor marker: the internal pull-down is enabled.
pub struct PullDown;

/// Mode marker: the pin is however boot or the firmware left it.
///
/// Every pin starts here. An `Unknown` pin can only be converted.
pub struct Unknown;

/// Mode marker: the pin samples the outside world. `PULL` records which
/// internal resistor is enabled.
pub struct Input<PULL> {
    _pull: PhantomData<PULL>,
}

/// Mode marker: the pin drives push-pull, to both rails.
pub struct Output;

/// Mode marker: the pin drives low or releases, but never drives high.
///
/// The BCM2837 pad has no open-drain mode, so this is emulated with the pin's
/// direction: the output latch is preloaded with zero and never changed, and
/// [`OpenDrainPin::drive_low`] / [`OpenDrainPin::release`] switch `GPFSELn`
/// between Output (driving the stored zero) and Input (not driving at all, so
/// an external pull-up wins).
///
/// An I2C line must never be driven high: a master driving 3V3 into a slave
/// holding 0V is a short circuit rather than a bus.
///
/// Every transition is therefore a `GPFSELn` read-modify-write and takes
/// [`crate::drivers::GPIO`]. The bus delays dominate: half a bit period at
/// 100 kHz is 5 µs against a few cycles for an uncontended lock.
///
/// The internal pull-ups are 50 kΩ or so, where the I2C spec wants 1.8 kΩ to
/// 10 kΩ, so fit external resistors. [`Pin::into_open_drain`] enables the
/// internal pull-up anyway, which is enough for a bench test with one short
/// wire.
pub struct OpenDrain;

/// Mode marker: the pin is routed to alternate function `A`, where `A` is 0-5.
///
/// Which peripheral that is depends on the pin. See the BCM2835 ARM
/// Peripherals datasheet, table 6-31.
pub struct Alt<const A: u8>;

/// Maps an `Alt<A>` index to the `GPFSEL` encoding, which is not sequential.
const fn alt_function(alt: u8) -> Function {
    match alt {
        0 => Function::Alt0,
        1 => Function::Alt1,
        2 => Function::Alt2,
        3 => Function::Alt3,
        4 => Function::Alt4,
        5 => Function::Alt5,
        _ => panic!("alternate function must be 0-5"),
    }
}

/// Applies a function and a pull to one pin, taking the GPIO lock once.
fn configure(pin: u8, function: Function, pull: Pull) {
    let mut gpio = GPIO.lock();
    gpio.set_function(pin, function);
    gpio.set_pull(&[pin], pull);
}

// ---------------------------------------------------------------------------
// Pin
// ---------------------------------------------------------------------------

/// A single GPIO pin, numbered by `N` and moded by `MODE`.
///
/// Zero-sized: both parameters are compile-time, so a `Pin` costs nothing to
/// pass around and every operation compiles to the bare load or store.
pub struct Pin<const N: u8, MODE> {
    _mode: PhantomData<MODE>,
}

impl<const N: u8, MODE> Pin<N, MODE> {
    /// The only way to make one, other than [`Pins::take`].
    const NEW: Self = Self { _mode: PhantomData };

    /// This pin's number.
    #[inline]
    pub const fn number(&self) -> u8 {
        N
    }

    /// Reconfigures the pin as a floating input.
    pub fn into_input(self) -> Pin<N, Input<Floating>> {
        configure(N, Function::Input, Pull::None);
        Pin::NEW
    }

    /// Reconfigures the pin as an input with the internal pull-up enabled.
    pub fn into_pull_up_input(self) -> Pin<N, Input<PullUp>> {
        configure(N, Function::Input, Pull::Up);
        Pin::NEW
    }

    /// Reconfigures the pin as an input with the internal pull-down enabled.
    pub fn into_pull_down_input(self) -> Pin<N, Input<PullDown>> {
        configure(N, Function::Input, Pull::Down);
        Pin::NEW
    }

    /// Reconfigures the pin as a push-pull output, initially low.
    ///
    /// Equivalent to [`Pin::into_output_low`]; spelled without the level for
    /// the common case where the initial value does not matter.
    pub fn into_output(self) -> Pin<N, Output> {
        self.into_output_low()
    }

    /// Reconfigures the pin as a push-pull output driving low.
    ///
    /// The level is written *before* the direction, so the pin never briefly
    /// drives whatever happened to be in the output latch.
    pub fn into_output_low(self) -> Pin<N, Output> {
        write_low(N);
        configure(N, Function::Output, Pull::None);
        Pin::NEW
    }

    /// Reconfigures the pin as a push-pull output driving high.
    ///
    /// As with [`Pin::into_output_low`], the level is written first so there is
    /// no glitch.
    pub fn into_output_high(self) -> Pin<N, Output> {
        write_high(N);
        configure(N, Function::Output, Pull::None);
        Pin::NEW
    }

    /// Reconfigures the pin as an emulated open-drain line, initially released.
    ///
    /// See [`OpenDrain`] for how the emulation works and why the internal
    /// pull-up it enables is not a substitute for a real one.
    pub fn into_open_drain(self) -> Pin<N, OpenDrain> {
        // Preload the output latch with zero *before* the output direction can
        // be selected, so `drive_low` only ever flips the direction.
        write_low(N);
        configure(N, Function::Input, Pull::Up);
        Pin::NEW
    }

    /// Routes the pin to alternate function `A`.
    ///
    /// # Panics
    ///
    /// If `A` is not 0-5. `A` is a compile-time constant.
    pub fn into_alt<const A: u8>(self) -> Pin<N, Alt<A>> {
        configure(N, alt_function(A), Pull::None);
        Pin::NEW
    }

    /// Erases the pin number from the type, keeping the mode.
    ///
    /// Use when pins have to live in an array or a `Vec`: `Pin<3, Output>` and
    /// `Pin<4, Output>` are different types, but both degrade to
    /// [`AnyPin<Output>`].
    pub fn degrade(self) -> AnyPin<MODE> {
        AnyPin {
            pin: N,
            _mode: PhantomData,
        }
    }
}

impl<const N: u8, MODE> InputPin for Pin<N, MODE>
where
    Pin<N, MODE>: sealed::Readable,
{
    #[inline]
    fn is_high(&self) -> bool {
        read_level(N)
    }
}

impl<const N: u8> OutputPin for Pin<N, Output> {
    #[inline]
    fn set_high(&mut self) {
        write_high(N);
    }

    #[inline]
    fn set_low(&mut self) {
        write_low(N);
    }
}

impl<const N: u8> StatefulOutputPin for Pin<N, Output> {
    #[inline]
    fn is_set_high(&self) -> bool {
        read_level(N)
    }
}

impl<const N: u8> OpenDrainPin for Pin<N, OpenDrain> {
    #[inline]
    fn drive_low(&mut self) {
        // The output latch still holds the zero written by `into_open_drain`,
        // so selecting the output direction is all "drive low" needs.
        GPIO.lock().set_function(N, Function::Output);
    }

    #[inline]
    fn release(&mut self) {
        GPIO.lock().set_function(N, Function::Input);
    }

    #[inline]
    fn is_high(&self) -> bool {
        read_level(N)
    }
}

impl<const N: u8, MODE> Pin<N, MODE>
where
    Pin<N, MODE>: sealed::Readable,
{
    /// Arms or disarms edge detection on this pin.
    ///
    /// A detected edge latches until [`Pin::take_event`] clears it, so an edge
    /// between two polls is not lost. [`crate::hal::asynch::WaitForEdge`] is
    /// built on this.
    ///
    /// QEMU does not model the edge-detect registers, so this does nothing
    /// there.
    pub fn set_edge_detect(&mut self, rising: bool, falling: bool) {
        GPIO.lock().set_edge_detect(N, rising, falling);
    }

    /// Reports whether an edge has been latched, and clears it if so.
    #[inline]
    pub fn take_event(&mut self) -> bool {
        if read_event(N) {
            clear_event(N);
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// AnyPin
// ---------------------------------------------------------------------------

/// A pin whose number is only known at run time.
///
/// Produced by [`Pin::degrade`]. Carries one `u8`; the mode is still in the
/// type, and it is still unique.
pub struct AnyPin<MODE> {
    pin: u8,
    _mode: PhantomData<MODE>,
}

impl<MODE> AnyPin<MODE> {
    /// This pin's number.
    #[inline]
    pub const fn number(&self) -> u8 {
        self.pin
    }

    /// Reconfigures the pin as a floating input.
    pub fn into_input(self) -> AnyPin<Input<Floating>> {
        configure(self.pin, Function::Input, Pull::None);
        self.remode()
    }

    /// Reconfigures the pin as an input with the internal pull-up enabled.
    pub fn into_pull_up_input(self) -> AnyPin<Input<PullUp>> {
        configure(self.pin, Function::Input, Pull::Up);
        self.remode()
    }

    /// Reconfigures the pin as an input with the internal pull-down enabled.
    pub fn into_pull_down_input(self) -> AnyPin<Input<PullDown>> {
        configure(self.pin, Function::Input, Pull::Down);
        self.remode()
    }

    /// Reconfigures the pin as a push-pull output driving low.
    pub fn into_output_low(self) -> AnyPin<Output> {
        write_low(self.pin);
        configure(self.pin, Function::Output, Pull::None);
        self.remode()
    }

    /// Reconfigures the pin as a push-pull output driving high.
    pub fn into_output_high(self) -> AnyPin<Output> {
        write_high(self.pin);
        configure(self.pin, Function::Output, Pull::None);
        self.remode()
    }

    /// Reconfigures the pin as an emulated open-drain line, initially released.
    pub fn into_open_drain(self) -> AnyPin<OpenDrain> {
        write_low(self.pin);
        configure(self.pin, Function::Input, Pull::Up);
        self.remode()
    }

    /// Routes the pin to alternate function `alt`, which must be 0-5.
    ///
    /// # Panics
    ///
    /// If `alt` is greater than 5.
    pub fn into_alt(self, alt: u8) -> AnyPin<Unknown> {
        configure(self.pin, alt_function(alt), Pull::None);
        self.remode()
    }

    /// Rebrands the mode without touching hardware. Private: every public
    /// caller has just reconfigured the pin to match.
    fn remode<NEW>(self) -> AnyPin<NEW> {
        AnyPin {
            pin: self.pin,
            _mode: PhantomData,
        }
    }
}

impl<MODE> InputPin for AnyPin<MODE>
where
    AnyPin<MODE>: sealed::Readable,
{
    #[inline]
    fn is_high(&self) -> bool {
        read_level(self.pin)
    }
}

impl OutputPin for AnyPin<Output> {
    #[inline]
    fn set_high(&mut self) {
        write_high(self.pin);
    }

    #[inline]
    fn set_low(&mut self) {
        write_low(self.pin);
    }
}

impl StatefulOutputPin for AnyPin<Output> {
    #[inline]
    fn is_set_high(&self) -> bool {
        read_level(self.pin)
    }
}

impl OpenDrainPin for AnyPin<OpenDrain> {
    #[inline]
    fn drive_low(&mut self) {
        GPIO.lock().set_function(self.pin, Function::Output);
    }

    #[inline]
    fn release(&mut self) {
        GPIO.lock().set_function(self.pin, Function::Input);
    }

    #[inline]
    fn is_high(&self) -> bool {
        read_level(self.pin)
    }
}

/// Restricts level reads to modes where the pad is actually being sampled.
mod sealed {
    /// Implemented for the pin modes whose level is meaningful to read.
    pub trait Readable {}
}

impl<const N: u8, PULL> sealed::Readable for Pin<N, Input<PULL>> {}
impl<const N: u8> sealed::Readable for Pin<N, Output> {}
impl<const N: u8> sealed::Readable for Pin<N, OpenDrain> {}
impl<PULL> sealed::Readable for AnyPin<Input<PULL>> {}
impl sealed::Readable for AnyPin<Output> {}
impl sealed::Readable for AnyPin<OpenDrain> {}

// ---------------------------------------------------------------------------
// The pin set
// ---------------------------------------------------------------------------

/// Generates the `Pins` struct: one field per GPIO, all starting [`Unknown`].
macro_rules! define_pins {
    ($($field:ident = $number:literal),+ $(,)?) => {
        /// Every GPIO on the chip, each owned exactly once.
        ///
        /// Obtained from [`Pins::take`]. Move the fields you want out; the
        /// borrow checker does the rest.
        ///
        /// Note which pins boot has already claimed — see the module
        /// documentation.
        #[allow(missing_docs)]
        pub struct Pins {
            $(pub $field: Pin<$number, Unknown>,)+
        }

        impl Pins {
            const ALL: Self = Self {
                $($field: Pin::NEW,)+
            };
        }
    };
}

define_pins! {
    p0 = 0, p1 = 1, p2 = 2, p3 = 3, p4 = 4, p5 = 5, p6 = 6,
    p7 = 7, p8 = 8, p9 = 9, p10 = 10, p11 = 11, p12 = 12, p13 = 13,
    p14 = 14, p15 = 15, p16 = 16, p17 = 17, p18 = 18, p19 = 19, p20 = 20,
    p21 = 21, p22 = 22, p23 = 23, p24 = 24, p25 = 25, p26 = 26, p27 = 27,
    p28 = 28, p29 = 29, p30 = 30, p31 = 31, p32 = 32, p33 = 33, p34 = 34,
    p35 = 35, p36 = 36, p37 = 37, p38 = 38, p39 = 39, p40 = 40, p41 = 41,
    p42 = 42, p43 = 43, p44 = 44, p45 = 45, p46 = 46, p47 = 47, p48 = 48,
    p49 = 49, p50 = 50, p51 = 51, p52 = 52, p53 = 53,
}

/// Guards [`Pins::take`] so the pins are handed out at most once, on one core.
static PINS_TAKEN: AtomicBool = AtomicBool::new(false);

impl Pins {
    /// Takes ownership of every GPIO pin.
    ///
    /// Returns `Some` exactly once per boot, to whichever core asks first, and
    /// `None` to everybody afterwards, which is what makes a [`Pin`] a proof of
    /// unique ownership.
    pub fn take() -> Option<Self> {
        PINS_TAKEN
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self::ALL)
    }

    /// Takes the pins whether or not somebody already has them.
    ///
    /// # Safety
    ///
    /// The caller must ensure no other `Pin` for the same GPIO is live, or the
    /// uniqueness the rest of this module relies on is gone. Intended for
    /// panic handlers and early bring-up.
    pub unsafe fn steal() -> Self {
        PINS_TAKEN.store(true, Ordering::Release);
        Self::ALL
    }
}

/// Highest pin number this module will hand out, from the driver layer.
pub const LAST_PIN: u8 = MAX_PIN;
