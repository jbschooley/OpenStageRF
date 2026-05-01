// SPDX-License-Identifier: AGPL-3.0-or-later

//! The active profile.
//!
//! Each profile feature re-exports exactly one profile crate's contents into
//! this module.  Application code reads `crate::profile::radio0::Cs`,
//! `crate::profile::display::Sck`, `crate::profile::RF_FREQUENCY_HZ`, etc.,
//! without caring which profile is selected.

#[cfg(feature = "dx_lr30_tx_basic")]
pub use osrf_profile_dx_lr30_tx_basic::*;

#[cfg(feature = "dx_lr30_rx_basic")]
pub use osrf_profile_dx_lr30_rx_basic::*;

#[cfg(feature = "dx_lr30_rx_diversity")]
pub use osrf_profile_dx_lr30_rx_diversity::*;
