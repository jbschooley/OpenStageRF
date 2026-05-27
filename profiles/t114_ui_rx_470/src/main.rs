// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! T114 UI receiver, **470–510 MHz (SX1268)** band. Identical to
//! `t114_ui_rx` but passes the 470 band-plan list. Single radio; for
//! dual-radio diversity on 470 add a `t114_ui_rx_diversity_470` mirroring
//! `t114_ui_rx_diversity` (pass `true` + `BAND_PLANS_470`).

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
    // tx_source is meaningless for Rx.
    run(spawner, Role::Rx, TxSource::Uart, false, BAND_PLANS_470).await;
}
