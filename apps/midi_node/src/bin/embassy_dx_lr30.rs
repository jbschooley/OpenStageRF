// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

use defmt::info;
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_stm32::gpio::{Level, Output, Speed};
use embassy_time::Timer;
use panic_probe as _;

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_stm32::init(Default::default());

    info!("OpenStageRF v0.1.0 — DX-LR30 starting");

    // PC13 is the onboard status LED on most STM32F103C8 modules (active-low).
    // Verify against the DX-LR30 schematic during Milestone 1 and update here.
    let mut led = Output::new(p.PC13, Level::High, Speed::Low);

    let mut tick: u32 = 0;
    loop {
        led.set_low(); // LED on
        Timer::after_millis(100).await;
        led.set_high(); // LED off
        Timer::after_millis(900).await;
        tick += 1;
        info!("tick {}", tick);
    }
}
