// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

//! Dual-radio diversity receiver — T114 + a second SX1262 on SPI3 via the
//! GPIO header.  Built-in radio0 stays on TWISPI0; radio1 lives on the
//! dedicated SPI3 peripheral with the board's default header pinout.

pub use osrf_board_t114::{
    button_user as input, display, dual_spi_diff_bus_radio1 as radio1, led_status, midi_uart,
    radio0, vext_power,
};

pub const RF_FREQUENCY_HZ: u32 = 915_000_000;
pub const RF_BITRATE_BPS: u32 = 300_000;
