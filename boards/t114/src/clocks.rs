// SPDX-License-Identifier: AGPL-3.0-or-later

//! Clock configuration for the Heltec Mesh Node T114 (nRF52840).
//!
//! nRF52840 boots at 64 MHz from HFINT (internal RC) — no PLL gymnastics
//! required.  HFINT is sufficient for SPI/UART traffic in this project.
//!
//! The only deliberate choice is **LFCLK source**: Embassy's `time-driver-rtc1`
//! is clocked from LFCLK, so its accuracy is bounded by it.
//!
//!   - LFRC  (internal RC, ~500 ppm) — Embassy's default; drift compounds fast.
//!   - LFXO  (external 32.768 kHz crystal, ~±20 ppm) — what we want.
//!
//! The Heltec T114 v2.0 schematic populates the LFXO; switching the source is
//! a single field change.
//!
//! HFXO (32 MHz crystal) and DC/DC regulators stay at defaults — neither is
//! needed for the SX1262 link (the radio module has its own TCXO) and the
//! board is USB-powered during bring-up.  Revisit when BLE or battery
//! operation lands.
//!
//! USB exception: the nRF52840 USB peripheral requires a stable 48 MHz USB
//! reference derived from HFXO (the internal RC isn't accurate enough for
//! USB 2.0 timing).  When the `usb-log` feature is in play, callers should
//! use [`usb_config()`] instead of [`default_config()`] to switch HFCLK to
//! the 32 MHz crystal that the T114 v2.0 schematic populates.

use embassy_nrf::config::{Config, HfclkSource, LfclkSource};

/// Default T114 clock config: LFXO from the on-board 32.768 kHz crystal,
/// HFINT (64 MHz internal RC) for SYSCLK.
pub fn default_config() -> Config {
    let mut c = Config::default();
    c.hfclk_source = HfclkSource::Internal;     // explicit for clarity (also default)
    c.lfclk_source = LfclkSource::ExternalXtal; // Heltec's 32.768 kHz X2
    c
}

/// USB-suitable clock config: same as [`default_config()`] but flips HFCLK
/// to the on-board 32 MHz crystal (HFXO).  The nRF52840 USB peripheral
/// internally derives its 48 MHz reference from HFCLK and won't enumerate
/// reliably on HFINT.  Use this when initialising the chip in any profile
/// that enables the `usb-log` feature.
pub fn usb_config() -> Config {
    let mut c = default_config();
    c.hfclk_source = HfclkSource::ExternalXtal;
    c
}
