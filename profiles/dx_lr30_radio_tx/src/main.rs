// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! Milestone 2 — DX-LR30 SX1262 bench TX loop.

use defmt::info;
use defmt_rtt as _;
use embassy_executor::Spawner;
use osrf_board_dx_lr30 as board;
use panic_probe as _;

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    info!("OpenStageRF radio bench TX — DX-LR30 starting");
    let mut r = board::resources();
    osrf_app_radio_bench::run_tx(&mut r.radio0, &mut r.status_led).await
}
