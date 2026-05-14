// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! MIDI node RX, T114 deployment.
//!
//! Wires the T114 board's `radio0` + `status_led` + `midi_uart` into
//! [`osrf_app_midi_node::run_rx`] with a [`UartMidiSink`] driving the
//! FeatherWing's DIN MIDI OUT.  Each accepted channel-voice event is
//! written verbatim out the UART; on watchdog expiry, 16 channels of
//! CC#123 (All Notes Off) are emitted to silence whatever's playing
//! on the synth.
//!
//! Hardware setup:
//!   - T114 P0_10 → FeatherWing `TX` (D1)
//!   - FeatherWing `3V` → T114 3V3
//!   - FeatherWing `GND` → T114 GND
//!   - FeatherWing **DIN OUT** jack → MIDI cable → synth's MIDI IN
//!   - DIN IN jack and FeatherWing `RX` pin: leave disconnected.

use embassy_executor::Spawner;
use osrf_app_midi_node::{run_rx, LinkConfig, LinkStatsCell, UartMidiSink};
use osrf_board_t114 as board;

/// Cross-task shared link-runtime stats.  Single producer
/// (`run_rx` / `run_tx` in `main`); no other consumer in this
/// profile yet — kept for symmetry with profiles that drive a
/// UI from the same numbers.
static STATS: LinkStatsCell = LinkStatsCell::new();

use defmt_rtt as _;
use panic_probe as _;

#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    board::bootloader_handoff();
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let mut r = board::resources();
    defmt::info!("OpenStageRF MIDI node RX — T114 starting");

    let config = LinkConfig::default_915();
    let mut sink = UartMidiSink::new(r.midi_uart);

    run_rx(
        &mut r.radio0,
        &mut r.status_led,
        &mut sink,
        &config,
        &STATS,
        None,
        None,
        None,
        None, // AEAD off — production midi_node profile, no key wiring yet.
    )
    .await
}
