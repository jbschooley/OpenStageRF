// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! Receiver-side UI binary for the T114.  Top menu shows Channel,
//! Scan, Band Plan, Link Stats, About.  Idle banner shows link
//! status + RSSI.  Drives the same `osrf-ui` core as `ui_tx` but
//! constructed with [`Role::Rx`].

use defmt_rtt as _;
use embassy_executor::Spawner;
use osrf_board_t114 as board;
use osrf_profile_t114_ui::{run, TxSource};
use osrf_ui::Role;
use panic_probe as _;

#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    board::bootloader_handoff();
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // `tx_source` is meaningless for the Rx role; pass any value.
    run(spawner, Role::Rx, TxSource::Uart).await;
}
