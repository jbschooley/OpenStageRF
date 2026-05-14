// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! Milestone 4 link-layer bench, RX side, T114 deployment.
//!
//! Wires the T114 board's `radio0` + `status_led` into
//! `osrf_app_link_bench::run_rx` with the [`DefmtLogSink`]: every accepted
//! MIDI message is logged via defmt, and on watchdog expiry the link-lost
//! event triggers a logged ALL_NOTES_OFF.
//!
//! When the MIDI FeatherWing arrives (Milestone 3 hardware), swap the
//! `DefmtLogSink` import for a `BufferedUarteSink` (or equivalent) — no
//! changes to `run_rx` itself.

use embassy_executor::Spawner;
use osrf_app_link_bench::{
    run_rx, synthetic::DefmtLogSink, test_aead_chacha, LinkBenchConfig, LinkStatsCell,
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
    let mut r = board::resources();
    defmt::info!("OpenStageRF link bench RX — T114 starting");

    // RF + link-layer config.  Must match the TX side's config.  Today:
    // hardcoded default.  Future: load from flash so the UI can edit
    // frequency / sync word / watchdog timing.
    let config = LinkBenchConfig::default_915();

    let mut sink = DefmtLogSink;
    // Paired with the TX side's `test_aead_chacha()` — same key,
    // same device_id, same direction.  RX derives the expected
    // `key_fp` from the cipher + key, so any packet with the wrong
    // header fingerprint or a failed tag fires `RxDrop::AeadFail` /
    // `KeyFpMismatch` and is logged + counted.
    let aead = Some(test_aead_chacha());
    defmt::info!("link bench RX: AEAD = ChaCha20-Poly1305 (test stub key)");
    run_rx(
        &mut r.radio0,
        &mut r.status_led,
        &mut sink,
        &config,
        &STATS,
        None,
        None,
        None,
        aead,
    )
    .await
}
