// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

//! First-edition receiver — DX-LR30, single SX1262, US 915 ISM, GFSK MIDI.

pub use osrf_board_dx_lr30::{joystick, led_status, midi_uart, oled_i2c, radio0};

pub const RF_FREQUENCY_HZ: u32 = 915_000_000;
pub const RF_BITRATE_BPS: u32 = 300_000;
