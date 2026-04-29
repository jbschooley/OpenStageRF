// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

pub mod clocks;
pub mod pins;

/// Initialise the DX-LR30 hardware and return the Embassy peripheral tokens.
///
/// Default: HSI + PLL at 64 MHz (no external crystal required).
/// Override with `--features hsi` to drop to bare 8 MHz HSI — useful if
/// the PLL fails to lock or for the lowest-power bring-up check.
pub fn init() -> embassy_stm32::Peripherals {
    #[cfg(feature = "hsi")]
    return embassy_stm32::init(clocks::hsi_config());
    #[cfg(not(feature = "hsi"))]
    return embassy_stm32::init(clocks::hsi_64mhz_config());
}
