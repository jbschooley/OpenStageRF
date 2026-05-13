// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

//! T114 receiver — single SX1262 + ST7789 TFT + user button, US 915 ISM, GFSK MIDI.

pub use osrf_board_t114::{
    button_user as input, display, led_status, midi_uart, radio0, vext_power,
};

pub const RF_FREQUENCY_HZ: u32 = 915_000_000;
pub const RF_BITRATE_BPS: u32 = 300_000;
