// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! Milestone 3 — DX-LR30 MIDI bench TX loop.

use defmt::info;
use defmt_rtt as _;
use embassy_executor::Spawner;
use osrf_board_dx_lr30 as board;
use panic_probe as _;

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    info!("OpenStageRF MIDI bench TX — DX-LR30 starting");
    let r = board::resources();
    osrf_app_midi_bench::run_tx(r.midi_uart).await
}
