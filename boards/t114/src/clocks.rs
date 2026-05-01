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

use embassy_nrf::config::{Config, HfclkSource, LfclkSource};

/// Default T114 clock config: LFXO from the on-board 32.768 kHz crystal,
/// HFINT (64 MHz internal RC) for SYSCLK.
pub fn default_config() -> Config {
    let mut c = Config::default();
    c.hfclk_source = HfclkSource::Internal;     // explicit for clarity (also default)
    c.lfclk_source = LfclkSource::ExternalXtal; // Heltec's 32.768 kHz X2
    c
}
