// SPDX-License-Identifier: AGPL-3.0-or-later
//! Milestone 1 smoke test — DX-LR30 GPIO / peripheral bring-up.
//!
//! Flash this binary and observe the RTT log with `probe-rs attach`.
//! Each test prints PASS or FAIL with reasoning.  Use a multimeter or
//! logic analyser to verify the GPIO assertions match real signal levels.
//!
//! Run: cargo xtask run dx_lr30_tx_basic --bin smoke_dx_lr30
#![no_std]
#![no_main]

use defmt::{error, info, warn};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_stm32::gpio::{Input, Level, Output, Pull, Speed};
use embassy_stm32::usart::{Config as UartConfig, Uart};
use embassy_time::Timer;
use osrf_board_dx_lr30 as board;
use panic_probe as _;

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = board::init();

    info!("══════════════════════════════════════");
    info!("  OpenStageRF DX-LR30 Smoke Test");
    info!("══════════════════════════════════════");

    // ── Status LED ────────────────────────────────────────────────────────
    info!("[LED] configuring PC13 (active-low)");
    let mut led = Output::new(p.PC13, Level::High, Speed::Low);
    led.set_low();
    Timer::after_millis(200).await;
    led.set_high();
    info!("[LED] PASS — blinked once; confirm visually");

    // ── SX1262 control pins ───────────────────────────────────────────────
    // CS and RESET are outputs (idle-high); BUSY and DIO1 are inputs.
    info!("[RADIO] configuring SX1262 GPIO pins");

    // Chip select: idle high (SX1262 deselected)
    let mut cs = Output::new(p.PA4, Level::High, Speed::Medium);
    // NRESET: idle high; pulse low to reset the SX1262
    let mut radio_reset = Output::new(p.PA3, Level::High, Speed::Medium);
    // BUSY: SX1262 drives high while executing a command (PA2)
    let busy = Input::new(p.PA2, Pull::None);
    // DIO1: IRQ line driven by SX1262 (PC15 / OSC32_OUT repurposed as GPIO)
    let dio1 = Input::new(p.PC15, Pull::Down);
    // TXEN / RXEN: drive low initially (module idle, neither TX nor RX selected)
    let mut txen = Output::new(p.PA0, Level::Low, Speed::Medium);
    let mut rxen = Output::new(p.PA1, Level::Low, Speed::Medium);
    let _ = (&mut txen, &mut rxen); // suppress unused warnings until radio driver lands

    info!("[RADIO] pre-reset — BUSY={} DIO1={}", busy.is_high(), dio1.is_high());

    // Issue hardware reset: hold NRESET low ≥100 µs, then release
    radio_reset.set_low();
    Timer::after_micros(200).await;
    radio_reset.set_high();
    // SX1262 needs up to 3 ms to come out of reset
    Timer::after_millis(5).await;

    // After reset, SX1262 briefly asserts BUSY then releases it
    let busy_after = busy.is_high();
    let dio1_after = dio1.is_high();
    info!("[RADIO] post-reset  — BUSY={} DIO1={}", busy_after, dio1_after);
    if busy_after {
        warn!("[RADIO] BUSY still high after reset + 5 ms — SX1262 may be absent or wiring wrong");
    } else {
        info!("[RADIO] BUSY low — SX1262 is ready (or pin is floating low; confirm with scope)");
    }

    // CS toggle — verify on multimeter/scope that PA4 drives low then high
    info!("[RADIO] toggling CS (PA4): high→low→high");
    cs.set_low();
    Timer::after_millis(1).await;
    cs.set_high();
    info!("[RADIO] CS toggle done — probe PA4 to verify");

    // ── MIDI UART ─────────────────────────────────────────────────────────
    // Verify USART2 can be initialised at 31250 baud (DIN MIDI standard).
    info!("[UART] initialising USART2 at 31250 baud");
    let mut uart_cfg = UartConfig::default();
    uart_cfg.baudrate = 31250;
    // PA2 = TX, PA3 = RX; DMA not required here (blocking smoke test)
    // USART3 on PB10 (TX) / PB11 (RX).
    // USART1 (PA9/PA10) is the CH340C debug/download port — do not use for MIDI.
    // USART2 (PA2/PA3) clashes with BUSY/NRST — do not use.
    match Uart::new_blocking(p.USART3, p.PB11, p.PB10, uart_cfg) {
        Ok(_uart) => {
            info!("[UART] PASS — USART2 initialised at 31250 baud");
        }
        Err(e) => {
            error!("[UART] FAIL — {:?}", e);
        }
    }

    // ── Summary ───────────────────────────────────────────────────────────
    info!("══════════════════════════════════════");
    info!("  Smoke test complete.  Check RTT log");
    info!("  and verify signal levels with meter.");
    info!("══════════════════════════════════════");

    // Fast-blink LED to signal end-of-test
    loop {
        led.set_low();
        Timer::after_millis(100).await;
        led.set_high();
        Timer::after_millis(100).await;
    }
}
