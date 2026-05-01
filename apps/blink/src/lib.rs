// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

//! Milestone 0 — board-agnostic LED blink loop.
//!
//! Generic over `embedded_hal::digital::StatefulOutputPin` so any board's
//! LED handle (which implements that trait) can be passed in.  The board
//! crate, the HAL, and pin polarity are all owned by the caller.

use embassy_time::Timer;
use embedded_hal::digital::StatefulOutputPin;

/// Toggle the LED at 1 Hz forever.  Logs a tick every 2 s via `defmt`.
pub async fn run<L: StatefulOutputPin>(led: &mut L) -> ! {
    let mut tick: u32 = 0;
    loop {
        let _ = led.toggle();
        Timer::after_millis(500).await;
        tick = tick.wrapping_add(1);
        if tick % 4 == 0 {
            defmt::info!("tick {}", tick);
        }
    }
}
