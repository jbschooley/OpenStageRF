// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! Milestone 0 — blink the status LED.
//!
//! Board-agnostic: the only platform-specific line is the `use` statement
//! below, picked by feature flag.  Everything else (the HAL, the LED pin,
//! active-high vs active-low) is hidden behind the board crate's
//! `Resources` API.
//!
//! This file is the canonical bin source; `apps/blink_nrf/` references it via
//! `[[bin]] path` so both platform crates compile the same code.

use defmt::info;
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_time::Timer;
use panic_probe as _;

#[cfg(feature = "dx_lr30")]
use osrf_board_dx_lr30 as board;

#[cfg(feature = "t114")]
use osrf_board_t114 as board;

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let mut r = board::resources();

    info!("OpenStageRF blink — starting");

    let mut tick: u32 = 0;
    loop {
        r.status_led.toggle();
        Timer::after_millis(500).await;
        tick = tick.wrapping_add(1);
        if tick % 4 == 0 {
            info!("tick {}", tick);
        }
    }
}
