// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! Diagnostic TX binary: same UI / display / joystick / link
//! runtime stack as `ui_tx`, but the MIDI source is the synthetic
//! `osrf_app_link_bench::synthetic::ScenarioSource` instead of the
//! FeatherWing UART.  Lets us reproduce the burst-pattern stress
//! tests from the `t114_link_tx` bench *while the UI is active*
//! — useful for confirming that UI rendering, scan-mode entry/
//! exit, joystick handling, and live-config-update plumbing don't
//! cause additional packet loss versus the pure-bench TX profile.
//!
//! Pair with `ui_rx` on the receiving board; same operating
//! channel, same link config.

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
    run(spawner, Role::Tx, TxSource::Scenario).await;
}
