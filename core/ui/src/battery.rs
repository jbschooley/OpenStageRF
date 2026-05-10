// SPDX-License-Identifier: AGPL-3.0-or-later

//! Battery state-of-charge model.
//!
//! Single-cell LiPo open-circuit-voltage → percent table from
//! Meshtastic's default OCV reference (`firmware/src/power.h`), which
//! their heltec_mesh_node_t114 variant inherits unchanged.  Eleven
//! anchors at every 10 % SoC; [`voltage_to_percent`] linear-
//! interpolates within whichever segment the measured value lands in.
//! Accurate enough given that real LiPo OCV varies ±50 mV unit-to-
//! unit anyway.

/// Open-circuit-voltage (mV) anchors for SoC percent.  Index
/// `OCV_TABLE[i]` corresponds to `i * 10` % SoC — i.e.
/// `OCV_TABLE[0]` = 0 %, `OCV_TABLE[10]` = 100 %.
pub const OCV_TABLE: [u16; 11] = [
    3100, 3300, 3420, 3530, 3630, 3720, 3800, 3890, 3990, 4050, 4190,
];

/// Below this reading we treat the cell as "no battery present."
///
/// Meshtastic uses 2600 mV (`OCV[0] - 500`) but their boards ship
/// with cells, so they never hit the dev-time "STLink only, no
/// battery" case.  On the T114 with only the SWD probe powering
/// the 3 V3 rail, leakage through the TP4054 charger to the BAT
/// pad lifts the divided reading to ~2.78 V — which would slot in
/// between Meshtastic's threshold and the OCV[0] floor (3100 mV)
/// and report a phantom "0 %" battery.
///
/// 3000 mV is the pragmatic floor: it's well below any LiPo a
/// reasonable BMS would still be allowing through (most cut at
/// 2.5-2.8 V) and well above the STLink leakage seen empirically.
/// Edge case: an unprotected cell that's been deep-discharged to
/// 2.9 V would also be flagged as "no battery" — but a cell that
/// low is already past chemical-damage threshold and our safety
/// net failed earlier; no point pretending we can rescue it now.
pub const NO_BATTERY_MV: u16 = 3000;
/// Above this reading we cap percent at 100 % and the cell is
/// likely on charge.  Meshtastic uses `OCV[0] + 10` as a charging
/// flag; we keep the same.
pub const CHARGING_FLOOR_MV: u16 = 4200;
/// "Warn user" threshold — UI flags the indicator at this SoC or
/// below.  Profile-level low-battery actions trigger here.
pub const LOW_THRESHOLD_PCT: u8 = 20;
/// "Aggressive power save" threshold — backlight off sooner, scan
/// auto-disable, etc.
pub const CRITICAL_THRESHOLD_PCT: u8 = 10;
/// Emergency shutdown floor (mV).  Sustained reads at-or-below this
/// for several samples should trigger an orderly save-and-halt —
/// landed in M7 alongside flash persistence.  Below this an
/// unprotected cell starts taking permanent capacity damage.
pub const SHUTDOWN_MV: u16 = 3100;

/// Convert a measured battery voltage (mV) into 0..=100 % SoC via
/// linear interpolation within the [`OCV_TABLE`] segment that
/// contains the measurement.  Returns 100 for any reading ≥ 4190 mV
/// and 0 for any reading ≤ 3100 mV.
pub fn voltage_to_percent(mv: u16) -> u8 {
    if mv <= OCV_TABLE[0] {
        return 0;
    }
    if mv >= OCV_TABLE[10] {
        return 100;
    }
    // Find segment [OCV_TABLE[i], OCV_TABLE[i+1]] containing `mv`,
    // then linear-interp within it.
    let mut i = 0;
    while i < 10 && mv > OCV_TABLE[i + 1] {
        i += 1;
    }
    let lo = OCV_TABLE[i] as u32;
    let hi = OCV_TABLE[i + 1] as u32;
    // Position within segment, in tenths of a percent for rounding.
    let frac = ((mv as u32 - lo) * 10) / (hi - lo);
    (i as u8) * 10 + frac as u8
}

/// Shared state between the profile's battery monitor task and the
/// UI rendering path.  `Copy` so it can live in a
/// `critical_section::Mutex<Cell<_>>` for cross-task sharing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct BatteryStatus {
    /// Measured terminal voltage (mV).  `0` = no reading yet —
    /// renderer should show placeholders rather than misleading
    /// numbers in that case.
    pub voltage_mv: u16,
    /// Latest SoC percent, derived via [`voltage_to_percent`].  Stays
    /// `0` until the first reading is in.
    pub percent: u8,
    /// USB power present.  On the T114 there's no charger-`STAT` pin
    /// routed, so we can't distinguish "actively charging" from
    /// "charge done" — this is just "plugged in."
    pub plugged_in: bool,
}

impl BatteryStatus {
    /// Pre-first-reading default.
    pub const UNKNOWN: Self = Self {
        voltage_mv: 0,
        percent: 0,
        plugged_in: false,
    };
    /// Construct from a fresh voltage reading + USB-present flag.
    /// If the voltage looks like an absent battery (< [`NO_BATTERY_MV`])
    /// the percent is reported as 0 — caller checks `voltage_mv` to
    /// decide whether to render "—%" or the real value.
    pub fn from_reading(voltage_mv: u16, plugged_in: bool) -> Self {
        let percent = if voltage_mv >= NO_BATTERY_MV {
            voltage_to_percent(voltage_mv)
        } else {
            0
        };
        Self {
            voltage_mv,
            percent,
            plugged_in,
        }
    }
    /// SoC ≤ [`LOW_THRESHOLD_PCT`].  Used by the UI to switch the
    /// indicator into a warning style.
    pub fn is_low(&self) -> bool {
        self.voltage_mv >= NO_BATTERY_MV && self.percent <= LOW_THRESHOLD_PCT
    }
    /// SoC ≤ [`CRITICAL_THRESHOLD_PCT`].  Used by the profile to
    /// enter aggressive-save mode (shorter backlight timeout, scan
    /// disabled, etc.).
    pub fn is_critical(&self) -> bool {
        self.voltage_mv >= NO_BATTERY_MV && self.percent <= CRITICAL_THRESHOLD_PCT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_match_meshtastic_reference() {
        assert_eq!(OCV_TABLE[0], 3100);
        assert_eq!(OCV_TABLE[1], 3300);
        assert_eq!(OCV_TABLE[2], 3420);
        assert_eq!(OCV_TABLE[3], 3530);
        assert_eq!(OCV_TABLE[4], 3630);
        assert_eq!(OCV_TABLE[5], 3720);
        assert_eq!(OCV_TABLE[6], 3800);
        assert_eq!(OCV_TABLE[7], 3890);
        assert_eq!(OCV_TABLE[8], 3990);
        assert_eq!(OCV_TABLE[9], 4050);
        assert_eq!(OCV_TABLE[10], 4190);
    }

    #[test]
    fn table_is_monotonic() {
        for w in OCV_TABLE.windows(2) {
            assert!(w[1] > w[0], "non-monotonic at {}", w[0]);
        }
    }

    #[test]
    fn voltage_to_percent_boundaries() {
        assert_eq!(voltage_to_percent(0), 0);
        assert_eq!(voltage_to_percent(3000), 0);
        assert_eq!(voltage_to_percent(3100), 0);
        assert_eq!(voltage_to_percent(4190), 100);
        assert_eq!(voltage_to_percent(4500), 100);
    }

    #[test]
    fn voltage_to_percent_anchors_round_trip() {
        assert_eq!(voltage_to_percent(3300), 10);
        assert_eq!(voltage_to_percent(3420), 20);
        assert_eq!(voltage_to_percent(3530), 30);
        assert_eq!(voltage_to_percent(3720), 50);
        assert_eq!(voltage_to_percent(3990), 80);
    }

    #[test]
    fn status_thresholds() {
        let mid = BatteryStatus::from_reading(3720, false);
        assert!(!mid.is_low());
        assert!(!mid.is_critical());
        let low = BatteryStatus::from_reading(3420, false);
        assert!(low.is_low());
        assert!(!low.is_critical());
        let crit = BatteryStatus::from_reading(3300, true);
        assert!(crit.is_low());
        assert!(crit.is_critical());
        assert!(crit.plugged_in);
    }
}
