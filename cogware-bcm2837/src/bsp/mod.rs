// SPDX-License-Identifier: GPL-3.0-only
//! Board support package for the Raspberry Pi 3 (BCM2837).

pub mod memory;

/// Human-readable board name, printed in the boot banner.
pub const BOARD_NAME: &str = "Raspberry Pi 3 (BCM2837, Cortex-A53)";
