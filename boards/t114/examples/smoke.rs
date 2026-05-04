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
use embassy_time::Timer;
use osrf_board_t114 as board;
use panic_probe as _;

// UARTE1 IRQ binding lives in the board crate (it owns the MIDI UART
// since Milestone 3).  Re-binding it here would cause a duplicate-
// symbol link error; the UART-init smoke check is exercised end-to-end
// by `board::resources()` instead — see the t114_midi_{rx,tx} profiles.

// VTOR + bootloader peripheral teardown — required for any T114 binary
// loaded via UF2.  See `osrf_board_t114::bootloader_handoff()` docs.
#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    board::bootloader_handoff();
}

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

    // MIDI UART smoke check moved to the t114_midi_{rx,tx} profile bin
    // builds (see PLAN.md Milestone 3).  Re-binding UARTE1 here would
    // collide with the board crate's bind_interrupts! since the MIDI
    // UART now lives inside board::Resources.

    info!("══════════════════════════════════════");
    info!("  Smoke test complete.  Check RTT log");
    info!("  and verify signal levels with meter.");
    info!("══════════════════════════════════════");

    // Fast-blink LED + periodic heartbeat so RTT viewers that attach late
    // (probe-rs needs ~hundreds of ms to discover the control block) still
    // see fresh data and don't display an empty "defmt" channel.  Without
    // the heartbeat all info!()s above run during boot, then RTT goes
    // silent and the host has nothing to render.
    let mut tick: u32 = 0;
    loop {
        led.set_high();
        Timer::after_millis(100).await;
        led.set_low();
        Timer::after_millis(100).await;
        tick = tick.wrapping_add(1);
        if tick % 10 == 0 {
            info!("smoke heartbeat tick={}", tick);
        }
    }
}
