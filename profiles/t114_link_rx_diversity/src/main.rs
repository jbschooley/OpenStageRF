// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! Stage 2 receive-diversity bench, T114 deployment (dual-SPI variant).
//!
//! Drives **two** SX1262s into one shared `LinkReceiver` with a
//! producer/consumer split (no single-task `select` race — that left the
//! loser radio's DIO1 stuck high and stormed the GPIOTE PORT):
//!   - `radio0` — on-board Heltec LR1262 (TWISPI0) — runs the *consumer*
//!     loop [`run_rx_diversity`] (its own receive + the shared decode/stats).
//!   - `radio1` — header-wired DX-LR30-900M22S on SPI3 — drained by the
//!     *producer* task [`run_rx_secondary`], which pushes crc-ok frames into
//!     `RADIO1_CH`.
//!
//! Both feed one `LinkReceiver`; its packet-`seq` replay window dedups the
//! duplicate copies. Each radio's `rx_recv` runs to completion in its own
//! task → neither DIO1 IRQ is ever left asserted → no spurious-wake storm.
//!
//! Pair against `t114_link_tx`. To see the diversity win, pull DIO1 (or the
//! antenna) on either radio and confirm the link survives on the other.

use embassy_executor::Spawner;
use osrf_app_link_bench::{
    run_rx_diversity, run_rx_secondary, synthetic::DefmtLogSink, test_aead_chacha,
    DiversityRxChannel, LinkBenchConfig, LinkConfig, LinkConfigSignal, LinkStatsCell,
};
use osrf_board_t114 as board;

use defmt_rtt as _;
use panic_probe as _;

static STATS: LinkStatsCell = LinkStatsCell::new();
/// Handoff channel: secondary radio's drain task → primary consumer loop.
static RADIO1_CH: DiversityRxChannel = DiversityRxChannel::new();
/// Live-config forward to the secondary task. Unused on the bench (fixed
/// config) but required by the API; never signalled here.
static SECONDARY_CFG: LinkConfigSignal = LinkConfigSignal::new();

#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    board::bootloader_handoff();
}

/// Producer task: owns radio1 (DX-LR30), drains it into `RADIO1_CH`.
#[embassy_executor::task]
async fn secondary_task(mut radio1: board::Radio1, config: LinkConfig) -> ! {
    run_rx_secondary(
        &mut radio1,
        &config,
        Some(&SECONDARY_CFG),
        RADIO1_CH.sender(),
    )
    .await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let (mut r, radio1) = board::resources_with_diversity();
    defmt::info!("OpenStageRF diversity RX — T114 (dual-SPI) starting");

    // RF + link-layer config — must match the TX side.  Both the consumer
    // and the producer task use this same config.
    let config = LinkBenchConfig::default_915();

    // Spawn the secondary radio's drain task (owns radio1).
    spawner.spawn(secondary_task(radio1, config).expect("alloc secondary_task"));

    let mut sink = DefmtLogSink;
    // Same paired AEAD stub key/device_id/direction as `t114_link_rx`.
    let aead = Some(test_aead_chacha());
    defmt::info!("diversity RX: AEAD = ChaCha20-Poly1305 (test stub key, strict)");
    // Primary consumer: on-board radio0 (rx0) + frames from radio1 (rx1)
    // via RADIO1_CH.
    run_rx_diversity(
        &mut r.radio0,
        RADIO1_CH.receiver(),
        &SECONDARY_CFG,
        &mut r.status_led,
        &mut sink,
        &config,
        &STATS,
        None, // config_updates: fixed config on the bench
        None, // scan: no UI on the bench
        None, // shutdown: no soft-off on the bench
        aead,
        // Strict — paired AEAD test, plaintext would be a setup error.
        false,
        None, // aead_updates: fixed key on the bench
    )
    .await
}
