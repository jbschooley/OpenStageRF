// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

//! Dual-radio diversity receiver — DX-LR30 with a second SX1262 on SPI2.
//!
//! radio0 is the built-in module on SPI1; radio1 is wired to the SPI2
//! expansion header using the board's default dual_spi_diff_bus pinout.

pub use osrf_board_dx_lr30::{
    dual_spi_diff_bus_radio1 as radio1,
    led_status,
    midi_uart,
    radio0,
};

pub const RF_FREQUENCY_HZ: u32 = 915_000_000;
pub const RF_BITRATE_BPS:  u32 = 300_000;
