// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! T114 UI receiver binary **with receive diversity**.  Identical to
//! `t114_ui_rx` but passes `diversity = true` to
//! [`osrf_profile_t114_ui::run`], which brings up the second SX1262
//! (radio1, on SPI3) and runs the receiver via `run_rx_diversity`.
//!
//! Both radios listen on the same channel and feed one shared receiver;
//! the packet-`seq` replay window dedups the duplicate copies.  Live
//! reconfigure (frequency/band/power) and channel-scan retune both radios.
//! Requires a second module (DX-LR30-900M22S) wired to the
//! `dual_spi_diff_bus_radio1` pinout — see the board crate.

use defmt_rtt as _;
use embassy_executor::Spawner;
use osrf_board_t114 as board;
use osrf_profile_t114_ui::{run, TxSource};
use osrf_ui::Role;

#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    board::bootloader_handoff();
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // `tx_source` is meaningless for Rx; pass any value.  `diversity = true`
    // enables the dual-SPI receive-diversity path.
    run(spawner, Role::Rx, TxSource::Uart, true).await;
}
