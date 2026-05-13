// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! Milestone 0 — blink an LED on the DX-LR30.
//!
//! Drives **PB0** (broken out on H3 pin 8) instead of the on-board LED2 on
//! PC13.  Reasons:
//!   1. LED2 is wired through R2 = 4.7 KΩ to PC13 (a low-drive pin) → ~64 µA
//!      through a blue LED, essentially invisible under room light.
//!   2. The LED2 on at least one bring-up unit appears to be physically
//!      damaged.
//!   3. Wire a 5 mm LED + 1 KΩ resistor between H3 pin 8 (PB0) and any GND
//!      pin (H3 pin 3) and you get a brightly visible 1 Hz blink.
//!
//! This profile also intentionally bypasses `board::resources()` for now —
//! that constructor brings up SPI1, USART3, I²C1, and the SX1262 reset
//! pulse, any of which can hang at bring-up time if the corresponding
//! peripheral isn't physically wired (no OLED → I²C BUSY hang risk, etc.).
//! Milestone 0's only goal is "code runs and toggles a GPIO" — switch back
//! to `resources()` once the surrounding peripherals are validated.

use defmt::info;
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_stm32::gpio::{Level, Output, Speed};
use osrf_board_dx_lr30 as board;
use panic_probe as _;

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    info!("OpenStageRF blink — DX-LR30 starting");
    let p = board::init();
    let mut led = Output::new(p.PB0, Level::Low, Speed::Low);
    osrf_app_blink::run(&mut led).await
}
