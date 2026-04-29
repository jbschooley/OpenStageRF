// SPDX-License-Identifier: AGPL-3.0-or-later

//! Clock configuration for the DX-LR30 (STM32F103C8T6).
//!
//! The DX-LR30-900M22SP has **no 8 MHz HSE crystal** — the main oscillator
//! footprint (X1) is absent on the PCB.  All configs here use the internal
//! HSI oscillator (8 MHz, ±1% accuracy).
//!
//! MIDI baud rate note: 31250 divides evenly from 8, 48, and 64 MHz, so
//! the divisor error is exactly zero.  The only frequency error is the HSI's
//! ±1% drift, which is at the MIDI spec limit but works fine in practice at
//! room temperature.  RF carrier frequency is set by the SX1262 module's own
//! oscillator and is unaffected by the MCU clock.

use embassy_stm32::rcc::{AHBPrescaler, APBPrescaler, Pll, PllMul, PllPreDiv, PllSource, Sysclk};
use embassy_stm32::Config;

/// 64 MHz via HSI/2 → PLL×16.  Recommended default: maximises SPI
/// throughput to the SX1262 without requiring an external crystal.
///
/// APB1 = 32 MHz (÷2, within its 36 MHz max).
/// APB2 = 64 MHz.
pub fn hsi_64mhz_config() -> Config {
    let mut config = Config::default();
    config.rcc.pll = Some(Pll {
        src: PllSource::HSI,
        prediv: PllPreDiv::DIV2, // HSI/2 = 4 MHz into PLL
        mul: PllMul::MUL16,      // 4 × 16 = 64 MHz
    });
    config.rcc.sys = Sysclk::PLL1_P;
    config.rcc.ahb_pre = AHBPrescaler::DIV1;  // HCLK  = 64 MHz
    config.rcc.apb1_pre = APBPrescaler::DIV2; // PCLK1 = 32 MHz (max 36 MHz)
    config.rcc.apb2_pre = APBPrescaler::DIV1; // PCLK2 = 64 MHz
    config
}

/// 8 MHz HSI with no PLL — safest fallback for initial bring-up or
/// if the PLL fails to lock.
pub fn hsi_config() -> Config {
    Config::default()
}

/// 72 MHz via external 8 MHz HSE crystal → PLL×9.
/// Not usable on the DX-LR30-900M22SP (no crystal fitted), but kept for
/// future boards or custom DX-LR30 variants that do populate X1.
#[allow(dead_code)]
pub fn hse_72mhz_config() -> Config {
    use embassy_stm32::rcc::{Hse, HseMode};
    use embassy_stm32::time::Hertz;
    let mut config = Config::default();
    config.rcc.hse = Some(Hse {
        freq: Hertz(8_000_000),
        mode: HseMode::Oscillator,
    });
    config.rcc.pll = Some(Pll {
        src: PllSource::HSE,
        prediv: PllPreDiv::DIV1,
        mul: PllMul::MUL9,
    });
    config.rcc.sys = Sysclk::PLL1_P;
    config.rcc.ahb_pre = AHBPrescaler::DIV1;
    config.rcc.apb1_pre = APBPrescaler::DIV2;
    config.rcc.apb2_pre = APBPrescaler::DIV1;
    config
}
