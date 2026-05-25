// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! Stage 2 receive-diversity bench, T114 deployment (dual-SPI variant).
//!
//! Identical to `t114_link_rx` but drives **two** SX1262s into one shared
//! `LinkReceiver` via [`run_rx_diversity`]:
//!   - `radio0` — the on-board Heltec LR1262 (TWISPI0, DIO2 RF switch).
//!   - `radio1` — a header-wired DX-LR30-900M22S on SPI3 (see the board
//!     crate's `dual_spi_diff_bus_radio1` pinout).  Same concrete type, so
//!     it slots into the same generic runtime.
//!
//! Both radios listen on the same channel; whichever demodulates a packet
//! first feeds the receiver, and its packet-`seq` replay window discards
//! the duplicate from the other radio.  That dedup *is* the diversity
//! arbitration — no extra logic here.
//!
//! Pair against the same `t114_link_tx` bench as `t114_link_rx` (same key,
//! same config).  To see the diversity win, shield or detune one antenna
//! and confirm the link survives on the other.

use embassy_executor::Spawner;
use osrf_app_link_bench::{
    run_rx_diversity, synthetic::DefmtLogSink, test_aead_chacha, LinkBenchConfig, LinkStatsCell,
};
use osrf_board_t114 as board;

use defmt_rtt as _;
use panic_probe as _;

static STATS: LinkStatsCell = LinkStatsCell::new();

#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    board::bootloader_handoff();
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let (mut r, mut radio1) = board::resources_with_diversity();
    defmt::info!("OpenStageRF diversity RX — T114 (dual-SPI) starting");

    // RF + link-layer config — must match the TX side.  Both radios are
    // configured from this single config inside `run_rx_diversity`.
    let config = LinkBenchConfig::default_915();

    let mut sink = DefmtLogSink;
    // Same paired AEAD stub key/device_id/direction as `t114_link_rx`.
    let aead = Some(test_aead_chacha());
    defmt::info!("diversity RX: AEAD = ChaCha20-Poly1305 (test stub key, strict)");
    // Normal order: on-board radio0 is primary (rx0), DX-LR30 radio1 is
    // the diversity radio (rx1).  (A diagnostic build once swapped these
    // to isolate the DX-LR30's RF path — see git history if that's needed
    // again.)
    run_rx_diversity(
        &mut r.radio0,
        &mut radio1,
        &mut r.status_led,
        &mut sink,
        &config,
        &STATS,
        None, // no shutdown signal on the bench
        aead,
        // Strict — paired AEAD test, plaintext would be a setup error.
        false,
    )
    .await
}
