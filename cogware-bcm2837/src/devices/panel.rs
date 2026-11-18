// SPDX-License-Identifier: GPL-3.0-only
//! What a DSI panel driver provides, and bringing one up.
//!
//! A panel driver is a mode and some commands: the controller's
//! initialisation table before the video starts, display-on after. Panel
//! vendors publish those tables as lists of DCS and generic writes with
//! delays between, which is what [`Step`] describes and [`run`] sends. The
//! sequence around them — host, PLL, compositor, valve, stream — is
//! [`start`].

use crate::{
    drivers::{
        DSI1,
        display::{self, DsiVideo},
        dsi::{self, Dsi},
        hvs::Plane,
        pixelvalve::Timing,
    },
    time,
};

/// One entry of an initialisation table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step<'a> {
    /// A DCS command and its parameters.
    Dcs(u8, &'a [u8]),
    /// A generic write: the manufacturer command set most controllers use
    /// for their private registers.
    Generic(&'a [u8]),
    /// A 32-bit register write to a DSI bridge that takes 16-bit addresses
    /// in generic long writes, as Toshiba's bridges do.
    Bridge(u16, u32),
    /// A pause, in milliseconds.
    DelayMs(u32),
}

/// Sends `steps` in order over `host`. On failure, says which step.
pub fn run(host: &mut Dsi, steps: &[Step<'_>]) -> Result<(), (usize, dsi::Error)> {
    for (index, step) in steps.iter().enumerate() {
        let result = match *step {
            Step::Dcs(command, params) => host.dcs_write(command, params),
            Step::Generic(params) => host.generic_write(params),
            Step::Bridge(register, value) => bridge_write(host, register, value),
            Step::DelayMs(ms) => {
                time::spin_for_micros(u64::from(ms) * 1000);
                Ok(())
            }
        };
        result.map_err(|error| (index, error))?;
    }
    Ok(())
}

/// Writes a bridge register: address low byte first, then the value, little
/// endian, as one generic long write.
pub fn bridge_write(host: &mut Dsi, register: u16, value: u32) -> Result<(), dsi::Error> {
    let [a0, a1] = register.to_le_bytes();
    let [v0, v1, v2, v3] = value.to_le_bytes();
    host.transfer(
        dsi::data_type::GENERIC_LONG_WRITE,
        &[a0, a1, v0, v1, v2, v3],
        &mut [],
    )
    .map(|_| ())
}

/// A DSI panel driver.
pub trait Panel {
    /// What can go wrong: at least a DSI error, and whatever else the panel
    /// talks over, such as I2C to a power controller.
    type Error: From<dsi::Error>;

    /// The panel's port, lanes, pixel format and mode.
    fn video(&self) -> DsiVideo;

    /// Powers the panel and sends its initialisation, with the host up in
    /// low-power mode and no video yet. Board work — a reset line, a power
    /// controller on I2C — happens here too.
    fn prepare(&mut self, host: &mut Dsi) -> Result<(), Self::Error>;

    /// Turns the display on, with video running.
    fn enable(&mut self, host: &mut Dsi) -> Result<(), Self::Error> {
        Ok(host.dcs_write(dsi::dcs::SET_DISPLAY_ON, &[])?)
    }

    /// Turns the display off before video stops.
    fn disable(&mut self, host: &mut Dsi) -> Result<(), Self::Error> {
        Ok(host.dcs_write(dsi::dcs::SET_DISPLAY_OFF, &[])?)
    }
}

/// Why [`start`] failed, and at which stage.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error<E> {
    /// Bringing the host up.
    BringUp(display::Error),
    /// The panel's own initialisation.
    Prepare(E),
    /// Starting compositor, valve and stream.
    Start(display::Error),
    /// Turning the display on or off.
    Enable(E),
}

impl<E: core::fmt::Display> core::fmt::Display for Error<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BringUp(error) => write!(f, "bringing up the host: {error}"),
            Self::Prepare(error) => write!(f, "panel initialisation: {error}"),
            Self::Start(error) => write!(f, "starting video: {error}"),
            Self::Enable(error) => write!(f, "display on: {error}"),
        }
    }
}

/// Brings `panel` up showing `plane` over `background` (`0xRRGGBB`), and
/// returns the mode actually running. Takes [`DSI1`] and the display locks
/// one at a time.
pub fn start<P: Panel>(
    panel: &mut P,
    plane: Option<&Plane>,
    background: u32,
) -> Result<Timing, Error<P::Error>> {
    let video = panel.video();
    let timing = display::bring_up(&video).map_err(Error::BringUp)?;
    panel.prepare(&mut DSI1.lock()).map_err(Error::Prepare)?;
    display::start(video.port, &timing, plane, background).map_err(Error::Start)?;
    panel.enable(&mut DSI1.lock()).map_err(Error::Enable)?;
    Ok(timing)
}

/// Turns `panel` off and stops the video pipeline.
pub fn stop<P: Panel>(panel: &mut P) -> Result<(), Error<P::Error>> {
    let port = panel.video().port;
    panel.disable(&mut DSI1.lock()).map_err(Error::Enable)?;
    display::stop(port).map_err(Error::Start)
}
