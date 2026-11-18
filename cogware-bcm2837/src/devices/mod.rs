// SPDX-License-Identifier: GPL-3.0-only
//! The generic side of off-board devices: the register bus camera sensors
//! speak, a sensor interface, raw-image rendering, and a panel interface with
//! an initialisation-table runner.
//!
//! No device is described here. A particular sensor or panel is a register
//! table and a mode, and lives in a crate of its own that implements
//! [`sensor::Sensor`] or [`panel::Panel`] on top of these pieces.

pub mod bayer;
pub mod cci;
pub mod panel;
pub mod sensor;
