// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! T114 UI diagnostic-bench transmitter.  Identical to
//! `t114_ui_tx` except the MIDI source is the synthetic
//! [`ScenarioSource`](osrf_app_link_bench::synthetic::ScenarioSource):
//! cycles through scale / chord-progression / glissando / key-smash
//! / quick-stabs / pitch-wheel / mod-wheel scenarios at realistic
//! cadences.  Pair with `t114_ui_rx` on the receiving board for
//! end-to-end link validation while the full UI stack runs (vs the
//! `t114_link_tx` / `t114_link_rx` profiles which exercise the link
//! runtime alone without the UI overhead).

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
    run(spawner, Role::Tx, TxSource::Scenario).await;
}
