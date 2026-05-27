// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! T114 UI transmitter, **470–510 MHz (SX1268)** band. Identical to
//! `t114_ui_tx` but passes the 470 band-plan list, so the Band Plan menu
//! offers only the 470 plan and the device boots tuned to it.

use defmt_rtt as _;
use embassy_executor::Spawner;
use osrf_board_t114 as board;
use osrf_profile_t114_ui::{run, TxSource};
use osrf_ui::{Role, BAND_PLANS_470};

#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    board::bootloader_handoff();
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    run(spawner, Role::Tx, TxSource::Uart, false, BAND_PLANS_470).await;
}
