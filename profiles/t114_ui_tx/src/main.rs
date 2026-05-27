// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! T114 UI transmitter binary.  Drives the production UART MIDI
//! source via `osrf_profile_t114_ui::run(spawner, Role::Tx,
//! TxSource::Uart)`.  All UI / link / soft-off logic lives in the
//! shared lib — this binary is just the role + source pick.

use defmt_rtt as _;
use embassy_executor::Spawner;
use osrf_board_t114 as board;
use osrf_profile_t114_ui::{run, TxSource};
use osrf_ui::{Role, BAND_PLANS_915};

#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    board::bootloader_handoff();
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    run(spawner, Role::Tx, TxSource::Uart, false, BAND_PLANS_915).await;
}
