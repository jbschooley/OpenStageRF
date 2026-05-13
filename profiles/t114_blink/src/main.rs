// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! Milestone 0 — blink the T114 status LED.
//!
//! Two logging modes selectable by Cargo feature:
//!
//! * default (no feature): `defmt::*` over RTT — needs a debug probe.
//! * `usb-log`:            `log::*` over USB-CDC — works with the UF2
//!                         bootloader, no probe required.
//!
//! `defmt-rtt` stays linked in both modes because (a) `panic-probe` and
//! the unmodified `osrf-app-blink` still use `defmt::*` and need a
//! global logger, and (b) the RTT buffer is cheap when nobody's reading.

use embassy_executor::Spawner;
#[cfg(feature = "usb-log")]
use embassy_time::Timer;
use osrf_board_t114 as board;

// `defmt-rtt` registers the defmt global_logger; keep it linked
// unconditionally so `defmt::*` calls (in apps and panic-probe) resolve.
use defmt_rtt as _;
use panic_probe as _;

// Runs at the very top of the cortex-m-rt reset handler, before .data
// copy / .bss zero / `main()`.  See `osrf_board_t114::bootloader_handoff()`
// for what it fixes — VTOR + leftover bootloader peripheral state.
#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    board::bootloader_handoff();
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // ── Hardware init ──────────────────────────────────────────────────────
    // In `usb-log` mode we have to use the HFXO-based clock config (USB
    // peripheral requires it) and we need the USBD peripheral token.
    #[cfg(feature = "usb-log")]
    let mut r = {
        let (r, usbd) = board::resources_and_usbd_with(board::clocks::usb_config());
        board::usb_log::spawn(&spawner, usbd);
        // Brief settle time so host enumeration completes before we start
        // hammering the pipe.  Optional, but spares the user a few lost
        // boot-time lines on a freshly-plugged port.
        Timer::after_millis(500).await;
        r
    };
    #[cfg(not(feature = "usb-log"))]
    let mut r = {
        let _ = &spawner; // reserved for future tasks
        board::resources()
    };

    // ── Bin-level boot banner ──────────────────────────────────────────────
    // `defmt::info!` -> RTT (visible only with a probe).
    defmt::info!("OpenStageRF blink — T114 starting");
    // `log::info!` -> USB-CDC when `usb-log` is on; no-op otherwise.
    #[cfg(feature = "usb-log")]
    log::info!("OpenStageRF blink — T114 starting (USB-CDC log)");

    // Spawn a heartbeat that emits via `log::*` so USB users see *something*
    // even though the app-internal `defmt::info!("tick {}")` is invisible.
    #[cfg(feature = "usb-log")]
    spawner.spawn(usb_heartbeat()).unwrap();

    osrf_app_blink::run(&mut r.status_led).await
}

/// Simple "I'm alive" pulse over the USB log path.  Frequency picked to be
/// noticeable on a serial terminal without flooding it.
#[cfg(feature = "usb-log")]
#[embassy_executor::task]
async fn usb_heartbeat() {
    let mut n: u32 = 0;
    loop {
        log::info!("blink heartbeat {n}");
        n = n.wrapping_add(1);
        Timer::after_millis(2000).await;
    }
}
