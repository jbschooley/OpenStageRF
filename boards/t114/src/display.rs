// SPDX-License-Identifier: AGPL-3.0-or-later

//! T114 ST7789 display — board-level wiring.
//!
//! The driver itself lives in `osrf-driver-display-st7789` (generic
//! over embedded-hal SPI / OutputPin / DelayNs).  This module just
//! pins the concrete embassy-nrf types it gets instantiated with
//! on this board so consumers can write `board::Display` instead of
//! the full generic spelling.
//!
//! Pin assignments per Heltec's official Heltec_nRF52 BSP variant.h
//! (HT-n5262):
//!   - SCK       = P1_08
//!   - MOSI      = P1_09
//!   - CS        = P0_11
//!   - DC        = P0_12
//!   - RESET     = P0_02
//!   - VTFT_CTRL = P0_03  (gates the TFT VDD rail, **active LOW**)
//!   - Backlight = P0_15  (TFT_LEDA_CTL, **active LOW** — owned by
//!                         `Resources` separately so apps can toggle
//!                         it after the first clear-to-background)
//!
//! Construction (in `build_resources`) configures SPIM1 in mode 0
//! @ 8 MHz, the CS/DC/RST/VTFT pins as Outputs, and a P1_08 PIN_CNF
//! fixup (see comment near `Spim::new_txonly` for the nRF52840 SPIM-
//! SCK gotcha).  The driver's `init()` is then called from async
//! context.

use embassy_nrf::gpio::Output;
use embassy_nrf::peripherals;
use embassy_nrf::spim::Spim;
use embassy_time::Delay;

// ── Pin / peripheral type aliases (board layout documentation) ──

/// SPIM peripheral driving the panel.
pub type Spi = peripherals::TWISPI1;
/// SPI clock pin.
pub type Sck = peripherals::P1_08;
/// SPI MOSI (data into the panel).
pub type Mosi = peripherals::P1_09;
/// SPI chip-select.
pub type Cs = peripherals::P0_11;
/// Data/command-select line — low for command bytes, high for data.
pub type Dc = peripherals::P0_12;
/// Hardware reset line.
pub type Reset = peripherals::P0_02;
/// Backlight enable (**active LOW**).
pub type Backlight = peripherals::P0_15;
/// VTFT_CTRL — gates the display panel's power rail (**active LOW**).
pub type PwrCtrl = peripherals::P0_03;

/// Concrete instantiation of the generic ST7789 driver for this
/// board.  Apps reference `board::Display` rather than spelling the
/// full generic chain.  Framebuffer + dirty-region tracking lives
/// in [`crate::framebuffer`] (re-export of the driver's framebuffer).
pub type St7789Display = osrf_driver_display_st7789::St7789<
    Spim<'static>,
    Output<'static>,
    Output<'static>,
    Output<'static>,
    Output<'static>,
    Delay,
>;
