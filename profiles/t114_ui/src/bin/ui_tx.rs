// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! Transmitter-side UI binary for the T114.  Top menu shows
//! Channel, Scan, Band Plan, TX Power, About.  Idle banner shows
//! TX power + channel.  Drives the same `osrf-ui` core as `ui_rx`
//! but constructed with [`Role::Tx`].

use defmt_rtt as _;
use embassy_executor::Spawner;
use osrf_board_t114 as board;
// Panic handler lives in `osrf_profile_t114_ui` (lib).
use osrf_profile_t114_ui::{run, TxSource};
use osrf_ui::Role;

#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    board::bootloader_handoff();
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    run(spawner, Role::Tx, TxSource::Uart).await;
}
