// SPDX-License-Identifier: GPL-3.0-only
//! A DSI video display: the DSI host, a pixel valve and an HVS channel, put
//! together in the order they have to come up.
//!
//! Two steps, so a panel's own commands fit between them, as most panels
//! need: [`bring_up`] powers and clocks the host and readies its PHY, the
//! caller then sends the panel's initialisation over the host in low-power
//! mode, and [`start`] starts the compositor, the valve and the video stream.
//! [`flip`] then swaps the plane's image, and [`stop`] undoes it all.
//!
//! DSI1 only: DSI0's high-speed clock comes from PLLA, which nothing here
//! drives. QEMU has none of these blocks; untested on hardware.

use super::{
    dsi::{self, PixelFormat, Port},
    hvs::{Channel, Plane},
    pixelvalve::{Output, Timing},
};

/// What a DSI video display needs from the panel side.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DsiVideo {
    /// Which host.
    pub port: Port,
    /// Data lanes the panel uses.
    pub lanes: u8,
    /// How pixels go on the link.
    pub format: PixelFormat,
    /// The panel's mode.
    pub timing: Timing,
    /// Keep the clock lane in high-speed mode between packets.
    pub continuous_clock: bool,
}

/// Why a display could not be brought up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The DSI host failed.
    Dsi(dsi::Error),
    /// The port has no video path here: DSI0.
    Unsupported,
    /// The mode needs a link faster than the PLL can run.
    TooFast,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Dsi(error) => write!(f, "{error}"),
            Self::Unsupported => f.write_str("video mode is DSI1 only"),
            Self::TooFast => f.write_str("mode needs a faster link than the PLL gives"),
        }
    }
}

impl From<dsi::Error> for Error {
    fn from(error: dsi::Error) -> Self {
        Self::Dsi(error)
    }
}

/// Stretches `timing` to run on a link that is PLLD divided by a whole number.
///
/// The link needs `pixel clock * bits / lanes`; the PLL can only give
/// `2 GHz / n`. So the fastest such rate at or above the need is taken, the
/// pixel clock rises with it, and the horizontal front porch grows to keep the
/// refresh rate where it was — what Linux and the firmware both do. Returns
/// the adjusted mode and the PLL rate.
pub const fn fit_timing(timing: Timing, bits_per_pixel: u32, lanes: u32) -> Option<(Timing, u32)> {
    let per_lane = bits_per_pixel / lanes;
    let needed = timing.pixel_clock_hz as u64 * per_lane as u64;
    let vco = super::clock::PLLD_VCO_HZ as u64;
    if needed == 0 || needed > vco {
        return None;
    }
    let divider = vco / needed;
    let pll = vco / divider;
    let pixel = pll / per_lane as u64;
    let htotal = pixel * timing.htotal() as u64 / timing.pixel_clock_hz as u64;
    let mut fitted = timing;
    fitted.pixel_clock_hz = pixel as u32;
    fitted.hfp += htotal as u32 - timing.htotal();
    Some((fitted, pll as u32))
}

// An 800x480 mode at 30 MHz on one 24-bit lane needs 720 MHz; the PLL gives
// 1 GHz (2 GHz / 2), so the pixel clock becomes 41.67 MHz and the front porch
// grows until the refresh rate is unchanged.
const _: () = {
    let mode = Timing {
        pixel_clock_hz: 30_000_000,
        hactive: 800,
        hfp: 40,
        hsync: 10,
        hbp: 50,
        vactive: 480,
        vfp: 10,
        vsync: 2,
        vbp: 20,
    };
    let Some((fitted, pll)) = fit_timing(mode, 24, 1) else {
        panic!()
    };
    assert!(pll == 1_000_000_000);
    assert!(fitted.pixel_clock_hz == 41_666_666);
    assert!(fitted.htotal() == 1249);
    assert!(fitted.refresh_millihz().abs_diff(mode.refresh_millihz()) < 100);
};

fn channel(port: Port) -> Channel {
    match port {
        Port::Dsi0 => Channel::Zero,
        Port::Dsi1 => Channel::Two,
    }
}

/// Powers and clocks the host, brings its PHY up in high-speed mode and
/// readies the video stream, without starting it. Returns the mode actually
/// used, after [`fit_timing`].
///
/// The host is left idle and ready for commands: send the panel's
/// initialisation through [`super::DSI1`] next, then call [`start`].
pub fn bring_up(video: &DsiVideo) -> Result<Timing, Error> {
    if video.port != Port::Dsi1 {
        return Err(Error::Unsupported);
    }
    if video.lanes == 0 {
        return Err(Error::Dsi(dsi::Error::Lanes));
    }
    let (timing, pll) =
        fit_timing(video.timing, video.format.bits(), video.lanes as u32).ok_or(Error::TooFast)?;
    dsi::power_on(video.port)?;
    let mut host = super::DSI1.lock();
    host.init(dsi::Config {
        lanes: video.lanes,
        virtual_channel: 0,
        hs_clock_hz: Some(pll),
        continuous_clock: video.continuous_clock,
    })?;
    host.configure_video(video.format)?;
    Ok(timing)
}

/// Starts the compositor on `plane` over `background` (`0xRRGGBB`), the pixel
/// valve on `timing`, and the video stream. With no plane the panel shows the
/// background alone, which proves the whole pipeline without any image memory.
pub fn start(
    port: Port,
    timing: &Timing,
    plane: Option<&Plane>,
    background: u32,
) -> Result<(), Error> {
    if port != Port::Dsi1 {
        return Err(Error::Unsupported);
    }
    super::HVS.lock().start(
        channel(port),
        timing.hactive,
        timing.vactive,
        plane,
        background,
    );
    super::PIXELVALVE1.lock().start(timing, Output::Dsi);
    super::DSI1.lock().start_video()?;
    Ok(())
}

/// Shows the image at `address` (the same shape as the plane given to
/// [`start`]) from the next frame, after waiting up to a frame for the
/// vertical blank so the switch does not tear.
pub fn flip(port: Port, address: usize, bytes: usize) {
    super::PIXELVALVE1.lock().wait_vblank(50_000);
    super::HVS.lock().flip(channel(port), address, bytes);
}

/// Stops the stream, the valve and the compositor channel, and shuts the
/// host down. The host's power and escape clock stay on for commands.
pub fn stop(port: Port) -> Result<(), Error> {
    if port != Port::Dsi1 {
        return Err(Error::Unsupported);
    }
    super::DSI1.lock().stop_video()?;
    super::PIXELVALVE1.lock().stop();
    super::HVS.lock().stop(channel(port));
    super::DSI1.lock().shutdown()?;
    Ok(())
}
