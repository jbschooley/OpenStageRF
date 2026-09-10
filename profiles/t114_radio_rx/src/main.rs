// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! Milestone 2 — T114 SX1262 bench RX loop.
//!
//! Logging modes (see `t114_blink/src/main.rs` for the full story):
//! * default (no feature): `defmt::*` over RTT.
//! * `usb-log`:            bin-level `log::*` over USB-CDC.  Per-packet
//!                         `defmt::info!("RX #N len=L rssi=...")` calls
//!                         inside `osrf-app-radio-bench::run_rx` remain
//!                         RTT-only.  USB users see a coarse heartbeat.

use embassy_executor::Spawner;
#[cfg(feature = "usb-log")]
use embassy_time::Timer;
use osrf_board_t114 as board;

// Keep `defmt-rtt` linked unconditionally — `panic-probe` and the
// unmodified `osrf-app-radio-bench` still emit defmt frames.
use defmt_rtt as _;
use panic_probe as _;

// Required for any T114 binary — VTOR + bootloader peripheral teardown.
// See `osrf_board_t114::bootloader_handoff()` for the rationale.
#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    board::bootloader_handoff();
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // Clear DEMCR before anything else — see the `t114_dap_idle_freeze`
    // note and `profiles/t114_ui/src/lib.rs`.  `probe-rs run` arms the
    // HardFault / reset vector catches; with the SoftDevice bootloader a
    // transient HardFault then halts the core (probe-rs shows it as
    // "Firmware exited unexpectedly: Exception") instead of being handled.
    const DEMCR: *mut u32 = 0xE000_EDFC as *mut u32;
    unsafe { core::ptr::write_volatile(DEMCR, 0) };

    #[cfg(feature = "usb-log")]
    let mut r = {
        let (r, usbd) = board::resources_and_usbd_with(board::clocks::usb_config());
        board::usb_log::spawn(&spawner, usbd);
        Timer::after_millis(500).await;
        spawner.spawn(usb_heartbeat()).unwrap();
        r
    };
    #[cfg(not(feature = "usb-log"))]
    let mut r = {
        let _ = &spawner;
        board::resources()
    };

    defmt::info!("OpenStageRF radio bench RX — T114 starting");
    #[cfg(feature = "usb-log")]
    log::info!("OpenStageRF radio bench RX — T114 starting (USB-CDC log)");

    osrf_app_radio_bench::run_rx(&mut r.radio0, &mut r.status_led).await
}

/// Coarse "still alive" pulse for the USB log channel.  Per-packet RX
/// events live in the app crate and stay on RTT.
#[cfg(feature = "usb-log")]
#[embassy_executor::task]
async fn usb_heartbeat() {
    let mut n: u32 = 0;
    loop {
        Timer::after_millis(2000).await;
        log::info!("radio_rx alive: t={}s (RX activity over RTT only)", n * 2);
        n = n.wrapping_add(1);
    }
}
