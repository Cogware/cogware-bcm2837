// SPDX-License-Identifier: GPL-3.0-only
//! Infallible digital pin traits.
//!
//! These mirror the shape of `embedded_hal::digital`, with one deliberate
//! difference: **nothing returns a `Result`**. Driving a BCM2837 GPIO pin is a
//! single store to a write-1-to-act register and sampling one is a single
//! load, so neither can fail.
//!
//! The `embedded-hal` traits are implemented on top of these when the feature
//! is on, so ecosystem drivers still work.
//!
//! The bit-banged buses in [`crate::hal::bitbang`] are generic over these
//! traits rather than over concrete pins, so they work with any pin type —
//! including the simulated ones the `bitbang` example uses to test the protocol
//! logic without hardware.

/// A pin that can be sampled.
pub trait InputPin {
    /// Reads the pad and reports whether it is above the logic threshold.
    fn is_high(&self) -> bool;

    /// The inverse of [`InputPin::is_high`].
    #[inline]
    fn is_low(&self) -> bool {
        !self.is_high()
    }
}

/// A pin that can be driven.
pub trait OutputPin {
    /// Drives the pin to the high rail.
    fn set_high(&mut self);

    /// Drives the pin to the low rail.
    fn set_low(&mut self);

    /// Drives the pin to `state`, high when `true`.
    #[inline]
    fn set_state(&mut self, state: bool) {
        if state {
            self.set_high();
        } else {
            self.set_low();
        }
    }
}

/// An output pin that can report its own level.
pub trait StatefulOutputPin: OutputPin {
    /// The level this pin is currently at.
    ///
    /// [`crate::hal::gpio::Pin`] answers this by reading the pad back through
    /// `GPLEVn` rather than by remembering what it was told, so a pin held down
    /// by something external reads low even though it was commanded high.
    fn is_set_high(&self) -> bool;

    /// The inverse of [`StatefulOutputPin::is_set_high`].
    #[inline]
    fn is_set_low(&self) -> bool {
        !self.is_set_high()
    }

    /// Drives the pin to the opposite of its current commanded level.
    #[inline]
    fn toggle(&mut self) {
        if self.is_set_high() {
            self.set_low();
        } else {
            self.set_high();
        }
    }
}

/// A pin that can pull a shared line low but never drives it high.
///
/// The electrical contract I2C needs: any device may pull SDA or SCL down,
/// nobody may drive them up, and an external resistor supplies the high level,
/// so two devices disagreeing produces a low line rather than a short between
/// a driver at 3V3 and a driver at 0V.
///
/// The BCM2837 has no open-drain pad mode, so [`crate::hal::gpio::Pin`]
/// emulates it by switching the pin's *direction*: driving low means becoming
/// an output already holding zero, and releasing means becoming an input, at
/// which point the pad stops driving and the pull-up wins. See
/// [`crate::hal::gpio::OpenDrain`].
pub trait OpenDrainPin {
    /// Actively pulls the line to ground.
    fn drive_low(&mut self);

    /// Stops driving, letting the pull-up take the line high.
    ///
    /// The line does not necessarily *go* high: another device may still be
    /// pulling it down, which is how I2C signals an ACK and how a slow slave
    /// stretches the clock.
    fn release(&mut self);

    /// Samples the line.
    ///
    /// Meaningful whether or not this pin is the one driving, since a released
    /// open-drain pin is an input.
    fn is_high(&self) -> bool;

    /// The inverse of [`OpenDrainPin::is_high`].
    #[inline]
    fn is_low(&self) -> bool {
        !self.is_high()
    }
}

// `&mut P` implements all of these, which lets a bit-banged bus borrow pins it
// does not own.

impl<P: InputPin + ?Sized> InputPin for &mut P {
    #[inline]
    fn is_high(&self) -> bool {
        P::is_high(self)
    }
}

impl<P: OutputPin + ?Sized> OutputPin for &mut P {
    #[inline]
    fn set_high(&mut self) {
        P::set_high(self);
    }

    #[inline]
    fn set_low(&mut self) {
        P::set_low(self);
    }
}

impl<P: StatefulOutputPin + ?Sized> StatefulOutputPin for &mut P {
    #[inline]
    fn is_set_high(&self) -> bool {
        P::is_set_high(self)
    }
}

impl<P: OpenDrainPin + ?Sized> OpenDrainPin for &mut P {
    #[inline]
    fn drive_low(&mut self) {
        P::drive_low(self);
    }

    #[inline]
    fn release(&mut self) {
        P::release(self);
    }

    #[inline]
    fn is_high(&self) -> bool {
        P::is_high(self)
    }
}
