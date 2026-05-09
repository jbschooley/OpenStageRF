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

use embassy_nrf::config::Config;
use embassy_nrf::interrupt::Priority;

/// Default T114 clock config used by `embassy_nrf::init()`.
///
/// SoftDevice is always enabled on T114 binaries (see
/// `board::softdevice`), and SD owns CLOCK + POWER once it's
/// activated.  embassy-nrf's init must therefore *not* configure
/// HFCLK / LFCLK / DCDC — direct writes to those registers fault
/// under SD ownership.  We leave the clock and DCDC fields at
/// their `Config::default()` values (HFINT, internal-RC LFCLK
/// fallback, no DCDC); SD then configures HFCLK/LFCLK from the
/// LF-clock-cfg passed to `Softdevice::enable()`, and DC-DC via
/// `sd_power_dcdc_mode_set` in `softdevice::enable()`.
///
/// What we *do* set: peripheral interrupt priorities at P2
/// (SD-allowed; P0/P1/P4 are reserved per the nrf-softdevice
/// README).  embassy's defaults at P0 trigger
/// `SdmIncorrectInterruptConfiguration` panics inside SD enable.
pub fn default_config() -> Config {
    let mut c = Config::default();
    c.time_interrupt_priority = Priority::P2;
    c.gpiote_interrupt_priority = Priority::P2;
    c
}

/// USB-suitable clock config.  Currently identical to
/// [`default_config()`] — under the SoftDevice, HFCLK source is
/// SD-owned (we can't write `HfclkSource::ExternalXtal` ourselves
/// without faulting), so the previous "force HFXO" recipe doesn't
/// apply.  Profiles enabling the `usb-log` feature need to call
/// `sd_clock_hfclk_request()` *via* SD when their USB-CDC
/// connection becomes active so SD keeps HFXO running for the
/// 48 MHz USB derivation; that wiring isn't done yet.  Kept as a
/// distinct alias for forward-compatibility with profile binaries
/// that already say `clocks::usb_config()`.
pub fn usb_config() -> Config {
    default_config()
}
