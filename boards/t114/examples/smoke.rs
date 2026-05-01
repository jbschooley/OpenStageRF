// SPDX-License-Identifier: AGPL-3.0-or-later
//! Milestone 1 smoke test — Heltec Mesh Node T114 GPIO / peripheral bring-up.
//!
//! Flash this binary and observe the RTT log with `probe-rs attach`.  Each
//! test prints PASS/WARN/FAIL with reasoning; use a multimeter or logic
//! analyser to confirm GPIO assertions match real signal levels.
//!
//! Run:
//!   cargo run --example smoke -p osrf-board-t114 --target thumbv7em-none-eabihf
#![no_std]
#![no_main]

use defmt::{info, warn};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_nrf::gpio::{Input, Level, Output, OutputDrive, Pull};
use embassy_nrf::uarte::{Config as UarteConfig, Uarte};
use embassy_nrf::{bind_interrupts, peripherals, uarte};
use embassy_time::Timer;
use osrf_board_t114 as board;
use panic_probe as _;

bind_interrupts!(struct Irqs {
    UARTE1 => uarte::InterruptHandler<peripherals::UARTE1>;
});

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = board::init();

    info!("══════════════════════════════════════");
    info!("  OpenStageRF T114 Smoke Test");
    info!("══════════════════════════════════════");

    // ── Status LED (P1_03, active-high) ───────────────────────────────────────
    info!("[LED] configuring P1_03 (active-high green)");
    let mut led = Output::new(p.P1_03, Level::Low, OutputDrive::Standard);
    led.set_high();
    Timer::after_millis(200).await;
    led.set_low();
    info!("[LED] PASS — blinked once; confirm visually");

    // ── User button (P1_10, pulled high externally) ───────────────────────────
    info!("[BTN] configuring P1_10 user button (active-low when pressed)");
    let button = Input::new(p.P1_10, Pull::Up);
    info!("[BTN] state at power-on: {} (high = released)", button.is_high());

    // ── Display power rail (VEXT_ENABLE on P0_21) ─────────────────────────────
    // VEXT gates the 3.3 V rail to the TFT.  Pulse it once to confirm the FET
    // responds; downstream display init lives in a separate driver test.
    info!("[VEXT] toggling P0_21 (display power enable)");
    let mut vext = Output::new(p.P0_21, Level::Low, OutputDrive::Standard);
    vext.set_high();
    Timer::after_millis(50).await;
    vext.set_low();
    info!("[VEXT] toggle done — probe P0_21 to verify");

    // ── SX1262 control pins ───────────────────────────────────────────────────
    // No TXEN/RXEN — the SX1262 DIO2 output drives the UPG2179 RF switch
    // directly.  CS, NRESET are outputs (idle-high); BUSY, DIO1 are inputs.
    info!("[RADIO] configuring SX1262 GPIO pins");

    let mut cs          = Output::new(p.P0_24, Level::High, OutputDrive::Standard);
    let mut radio_reset = Output::new(p.P0_25, Level::High, OutputDrive::Standard);
    let busy            = Input::new(p.P0_17, Pull::None);
    let dio1            = Input::new(p.P0_20, Pull::Down);

    info!("[RADIO] pre-reset  — BUSY={} DIO1={}", busy.is_high(), dio1.is_high());

    // Hardware reset: hold NRESET low ≥100 µs, release, wait ≥3 ms.
    radio_reset.set_low();
    Timer::after_micros(200).await;
    radio_reset.set_high();
    Timer::after_millis(5).await;

    let busy_after = busy.is_high();
    let dio1_after = dio1.is_high();
    info!("[RADIO] post-reset — BUSY={} DIO1={}", busy_after, dio1_after);
    if busy_after {
        warn!("[RADIO] BUSY still high after reset + 5 ms — SX1262 may be absent or wiring wrong");
    } else {
        info!("[RADIO] BUSY low — SX1262 ready (or pin floating; confirm with scope)");
    }

    info!("[RADIO] toggling CS (P0_24): high→low→high");
    cs.set_low();
    Timer::after_millis(1).await;
    cs.set_high();
    info!("[RADIO] CS toggle done — probe P0_24 to verify");

    // ── MIDI UART (UARTE1 at 31250 baud, P0_09 RX / P0_10 TX) ─────────────────
    // Requires `nfc-pins-as-gpio` feature (P0_09/P0_10 are NFC-dedicated by default).
    info!("[UART] initialising UARTE1 at 31250 baud");
    let mut uart_cfg = UarteConfig::default();
    uart_cfg.baudrate = embassy_nrf::uarte::Baudrate::BAUD31250;
    let _uart = Uarte::new(p.UARTE1, p.P0_09, p.P0_10, Irqs, uart_cfg);
    info!("[UART] PASS — UARTE1 initialised at 31250 baud");

    info!("══════════════════════════════════════");
    info!("  Smoke test complete.  Check RTT log");
    info!("  and verify signal levels with meter.");
    info!("══════════════════════════════════════");

    // Fast-blink LED to signal end-of-test.
    loop {
        led.set_high();
        Timer::after_millis(100).await;
        led.set_low();
        Timer::after_millis(100).await;
    }
}
