// SPDX-License-Identifier: GPL-3.0-only
//! What a camera sensor driver provides, and capturing a frame with one.
//!
//! A sensor driver knows its chip: how to tell it is there, the register
//! table for a mode, and how to start and stop streaming. Everything on the
//! SoC side — powering the receiver, the buffer, waiting for the frame — is
//! the same for every sensor, and is [`capture`].

use super::bayer::BayerOrder;
use crate::{
    drivers::unicam::{self, Packing, Port},
    sync::SpinLock,
};

/// What a sensor sends in its configured mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SensorMode {
    /// Pixels per line.
    pub width: u32,
    /// Lines per frame.
    pub height: u32,
    /// CSI-2 data type, see [`unicam::data_type`].
    pub data_type: u8,
    /// Bits per pixel on the link: 8, 10 or 12 for raw modes.
    pub bits: u8,
    /// Data lanes the sensor drives.
    pub lanes: u8,
    /// The colour filter's layout, for a raw mode.
    pub bayer: Option<BayerOrder>,
    /// Whether the sensor keeps its clock lane in high-speed mode.
    pub continuous_clock: bool,
}

impl SensorMode {
    /// Bytes from one line to the next in memory, as received: the packed
    /// line length rounded up to the receiver's alignment.
    pub const fn stride(&self) -> u32 {
        let bytes = (self.width * self.bits as u32).div_ceil(8);
        bytes.next_multiple_of(unicam::BUFFER_ALIGN as u32)
    }

    /// The receiver configuration that captures this mode as sent.
    pub const fn receiver(&self, continuous: bool) -> unicam::Config {
        unicam::Config {
            lanes: self.lanes,
            data_type: self.data_type,
            virtual_channel: 0,
            stride: self.stride(),
            height: self.height,
            packing: Packing::AsReceived,
            continuous_clock: self.continuous_clock,
            continuous,
        }
    }
}

// 1640 RAW10 pixels are 2050 bytes, padded to 2064.
const _: () = assert!(
    SensorMode {
        width: 1640,
        height: 1232,
        data_type: unicam::data_type::RAW10,
        bits: 10,
        lanes: 2,
        bayer: Some(BayerOrder::Bggr),
        continuous_clock: true,
    }
    .stride()
        == 2064
);

/// A camera sensor driver.
pub trait Sensor {
    /// What can go wrong talking to it, usually its I2C bus's error.
    type Error;

    /// Reads the chip's identification and checks it is the expected part,
    /// returning the ID it read.
    fn identify(&mut self) -> Result<u32, Self::Error>;

    /// Programs the mode and leaves the sensor ready but not streaming.
    fn configure(&mut self) -> Result<SensorMode, Self::Error>;

    /// Starts streaming. The receiver must already be listening.
    fn start(&mut self) -> Result<(), Self::Error>;

    /// Stops streaming.
    fn stop(&mut self) -> Result<(), Self::Error>;
}

/// Why [`capture`] failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CaptureError<E> {
    /// The sensor did.
    Sensor(E),
    /// The receiver did.
    Receiver(unicam::Error),
}

/// Captures one frame from `sensor` on `port` into `buffer`, waiting up to
/// `micros` for it, and returns the mode it was captured in.
///
/// Powers the receiver, configures the sensor, starts the receiver before the
/// sensor so the first frame start is seen, and stops both afterwards. The
/// receiver is left powered for the next capture.
///
/// # Safety
///
/// `buffer..buffer + len` must be ARM DRAM nothing else touches until this
/// returns; the receiver writes it behind the cores' backs.
pub unsafe fn capture<S: Sensor>(
    sensor: &mut S,
    receiver: &SpinLock<unicam::Unicam>,
    port: Port,
    buffer: usize,
    len: usize,
    micros: u64,
) -> Result<SensorMode, CaptureError<S::Error>> {
    unicam::power_on(port).map_err(CaptureError::Receiver)?;
    sensor.identify().map_err(CaptureError::Sensor)?;
    let mode = sensor.configure().map_err(CaptureError::Sensor)?;
    let mut cam = receiver.lock();
    // SAFETY: passed through from the caller.
    unsafe { cam.start(&mode.receiver(false), buffer, len) }.map_err(CaptureError::Receiver)?;
    let started = sensor.start();
    let frame = started
        .map_err(CaptureError::Sensor)
        .and_then(|()| cam.wait_frame(micros).map_err(CaptureError::Receiver));
    let _ = sensor.stop();
    cam.stop();
    frame.map(|_| mode)
}
