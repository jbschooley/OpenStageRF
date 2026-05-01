// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! Milestone 3 — T114 MIDI bench RX loop.
//!
//! Logging modes (see `t114_blink/src/main.rs` for the full story):
//! * default (no feature): `defmt::*` over RTT.
//! * `usb-log`:            bin-level `log::*` over USB-CDC.  Per-event
//!                         `defmt::info!("event: ...")` calls inside
//!                         `osrf-app-midi-bench::run_rx` remain RTT-only.
//!                         USB users see a coarse heartbeat.

use embassy_executor::Spawner;
#[cfg(feature = "usb-log")]
use embassy_time::Timer;
use osrf_board_t114 as board;

// Keep `defmt-rtt` linked unconditionally — `panic-probe` and the
// unmodified `osrf-app-midi-bench` still emit defmt frames.
use defmt_rtt as _;
use panic_probe as _;

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    #[cfg(feature = "usb-log")]
    let r = {
        let (r, usbd) = board::resources_and_usbd_with(board::clocks::usb_config());
        board::usb_log::spawn(&spawner, usbd);
        Timer::after_millis(500).await;
        spawner.spawn(usb_heartbeat().unwrap());
        r
    };
    #[cfg(not(feature = "usb-log"))]
    let r = {
        let _ = &spawner;
        board::resources()
    };

    defmt::info!("OpenStageRF MIDI bench RX — T114 starting");
    #[cfg(feature = "usb-log")]
    log::info!("OpenStageRF MIDI bench RX — T114 starting (USB-CDC log)");

    osrf_app_midi_bench::run_rx(r.midi_uart).await
}

/// Coarse "still alive" pulse for the USB log channel.  Per-event RX
/// activity lives in the app crate and stays on RTT.
#[cfg(feature = "usb-log")]
#[embassy_executor::task]
async fn usb_heartbeat() {
    let mut n: u32 = 0;
    loop {
        Timer::after_millis(2000).await;
        log::info!("midi_rx alive: t={}s (RX activity over RTT only)", n * 2);
        n = n.wrapping_add(1);
    }
}
