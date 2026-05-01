// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! Milestone 0 — blink the DX-LR30 status LED.

use defmt::info;
use defmt_rtt as _;
use embassy_executor::Spawner;
use osrf_board_dx_lr30 as board;
use panic_probe as _;

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    info!("OpenStageRF blink — DX-LR30 starting");
    let mut r = board::resources();
    osrf_app_blink::run(&mut r.status_led).await
}