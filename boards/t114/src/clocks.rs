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
use embassy_nrf::interrupt::Priority;

/// Default T114 clock config: LFXO (external 32.768 kHz crystal,
/// ~±20 ppm) for LFCLK, HFINT (64 MHz internal RC) for SYSCLK.
///
/// **LFCLK source choice history:** an earlier revision used
/// `ExternalXtal` and reverted to `InternalRC` after some v2.0 units
/// produced visibly irregular timer intervals (suspected LFXO
/// startup failure leaving the driver on a misconfigured fallback).
/// Re-tried on v2.1 hardware — LFXO starts cleanly, time driver
/// runs at the much tighter ±20 ppm.  Revert to `InternalRC` if a
/// future board fails to start.
#[allow(unreachable_code)] // when `softdevice` feature is on, the function returns early
pub fn default_config() -> Config {
    let mut c = Config::default();

    // SoftDevice reserves interrupt priorities **P0, P1, and P4**
    // (per nrf-softdevice README); app-allowed are P2/P3/P5+.
    // embassy-nrf's default is P0, which hard-faults under an active
    // SD.  P2 is the nrf-softdevice docs' recommendation for time +
    // GPIOTE — applied unconditionally so the same config covers
    // SD-on and SD-off builds.
    c.time_interrupt_priority = Priority::P2;
    c.gpiote_interrupt_priority = Priority::P2;

    // ── SoftDevice-aware path ───────────────────────────────────────────
    // When SD is active, CLOCK + POWER are owned by SD.  Direct writes
    // to HFCLK/LFCLK source or DCDCEN fault — symptom is the chip
    // hanging silently inside `embassy_nrf::init()`, the next defmt
    // log line never appearing.  Leave both at their `Config::default()`
    // values (`HfclkSource::Internal`, `LfclkSource::InternalRC`,
    // `dcdc.reg1 = false`) so embassy-nrf skips those registers
    // entirely.  SD has already configured HFCLK + LFCLK + DCDC
    // during its enable (LF-clock source from our SD config; HFXO
    // started on demand by SD; DCDC enabled by `nrf_softdevice` if
    // we ever ask for it via `sd_power_dcdc_mode_set`).
    #[cfg(feature = "softdevice")]
    return c;

    // ── SD-less path ────────────────────────────────────────────────────
    // No SoftDevice — embassy-nrf owns the chip and we can configure
    // crystals + DCDC directly.
    c.hfclk_source = HfclkSource::ExternalXtal;
    c.lfclk_source = LfclkSource::ExternalXtal;
    c.dcdc.reg1 = true;
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
