// SPDX-License-Identifier: AGPL-3.0-or-later

//! Battery state-of-charge model.
//!
//! Originally LiPo-only; now selects between supported chemistries
//! via [`BatteryChemistry`].  Each variant carries an open-circuit
//! voltage table sampled at 10 % SoC anchors plus
//! threshold voltages (no-battery floor, emergency-shutdown, full-
//! charge indicator).  Profiles export a `const CHEMISTRY:
//! BatteryChemistry` and pass it through to [`BatteryStatus::from_reading`]
//! and the shutdown-eligibility check; the call surface is otherwise
//! unchanged from the LiPo-only version.
//!
//! ## Supported chemistries
//!
//! - **`LiPoSingle`** — single 3.7 V nominal LiPo cell.  Default for
//!   the T114's on-board pouch cell, the 14500 / 18650 swappable
//!   alternatives, and the DX-LR30.  OCV table from Meshtastic's
//!   default reference (`firmware/src/power.h`).
//!
//! - **`NimhPack { cells }`** — multi-cell NiMH (Eneloop / similar).
//!   Only `cells == 3` has a meaningful OCV table; smaller packs
//!   don't keep the chip's LDO above its dropout voltage, and
//!   larger packs exceed the chip's 3.6 V Vdd ceiling at the top
//!   of charge.  Other cell counts fall back to the 3-cell curve;
//!   the profile is expected to validate at build time.
//!
//! ## Chemistry trade-offs
//!
//! NiMH packs have a much **flatter** discharge curve than LiPo —
//! mostly hanging out around 1.2 V/cell for hours, with a sharp
//! knee at end-of-discharge.  Translating that to a 0..100 % SoC
//! indicator is inherently lossy: the gauge will sit at "~50 %"
//! for the bulk of the runtime then drop quickly through the last
//! 10 %.  Users running NiMH packs should expect this behaviour and
//! treat the low-battery warning as their primary signal rather
//! than the percentage.
//!
//! Charging: **the T114's on-board TP4054 is LiPo-only and will
//! over-charge NiMH cells**.  NiMH users must either de-pop the
//! TP4054 / cut its output, or simply never plug USB while NiMH
//! cells are installed.  Firmware can detect VBUS but cannot
//! prevent charging.  See PLAN.md M8 → "external-charging story
//! for NiMH."

/// Battery chemistry — selects OCV curve + voltage thresholds.
///
/// Profile-level `const`; each profile picks one at compile time
/// based on what cells the deployment runs.  The default for both
/// the T114 and DX-LR30 boards is [`LiPoSingle`](Self::LiPoSingle).
///
/// **Important: this enum assumes the voltage you pass to
/// [`BatteryStatus::from_reading`] is the cell-stack voltage**, not
/// a post-regulator rail.  For chemistries with `cells == 1` or
/// `cells == 2` you'll need a pre-boost ADC tap (see PLAN.md M8 →
/// battery-chemistry section and the comment at the top of
/// `boards/t114/src/battery.rs` for the hardware path).  Without
/// that tap the SAADC sees ~3.3 V (the boost output) regardless of
/// cell state and the gauge is meaningless — use
/// [`Regulated`](Self::Regulated) instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum BatteryChemistry {
    /// Single-cell LiPo, 3.0–4.2 V working range, ~3.7 V nominal.
    /// OCV table from Meshtastic's heltec_mesh_node_t114 variant.
    /// Default for the T114's on-board pouch, the 14500 / 18650
    /// swappable alternatives, and the DX-LR30.
    LiPoSingle,
    /// NiMH (Eneloop-typical) pack.  `cells` is the number of
    /// series cells; supported values are 1 / 2 / 3.  All three
    /// share the same per-cell OCV curve (Panasonic Eneloop
    /// datasheet @ 0.2 C); the variant just scales the thresholds.
    /// **1-cell and 2-cell variants require a pre-boost ADC tap**
    /// because the chip's stock LDO needs Vin ≥ 3.45 V to regulate;
    /// see module docs.  3-cell can connect directly to VBat.
    NimhPack { cells: u8 },
    /// External voltage regulator (boost converter for 1×/2× NiMH,
    /// buck for 4+ NiMH, etc.) presenting a regulated bus voltage
    /// to the chip's existing SAADC.  Since the gauge can't see the
    /// underlying cells, SoC percent collapses to "OK / Low /
    /// Critical" based on bus-voltage thresholds.  When cells weaken
    /// to the point where the regulator runs out of headroom, the
    /// bus voltage droops below `low_mv` and eventually `shutdown_mv`;
    /// firmware uses those thresholds to drive the low-battery
    /// indicator and the soft-off trigger.
    Regulated {
        /// Below this bus-voltage reading we treat the regulator as
        /// failed and trigger the M7/M8 soft-off path.  For a 3.3 V
        /// boost converter, ~3000 mV is a reasonable choice — the
        /// boost is starting to give up on regulation but the chip
        /// can still finish a clean teardown.
        shutdown_mv: u16,
        /// Below this bus-voltage reading we flag the indicator as
        /// "low" so the operator gets visual warning.  Pick a value
        /// somewhere between `shutdown_mv` and the regulator's
        /// nominal output — e.g. 3100 mV for a 3.3 V boost.
        low_mv: u16,
    },
}

/// 11-anchor OCV table for single-cell LiPo, in mV.  Index `i`
/// corresponds to `i * 10 %` SoC; `[0]` = 0 %, `[10]` = 100 %.
/// Meshtastic default, used unchanged for the T114's on-board pouch
/// cell + the 14500 / 18650 swappable alternatives.
const LIPO_SINGLE_OCV: [u16; 11] = [
    3100, 3300, 3420, 3530, 3630, 3720, 3800, 3890, 3990, 4050, 4190,
];

/// 11-anchor OCV table for 1× NiMH cell (Eneloop-typical), in mV.
/// Per-cell values from the Panasonic Eneloop datasheet's 0.2 C
/// discharge curve.  Knees at top (1.40 → 1.30 V) and bottom
/// (1.16 → 1.00 V); ~80 % of runtime sits between 1.21 and 1.30 V.
/// Used directly for 1-cell builds and scaled (× 2 / × 3) for the
/// 2-cell and 3-cell variants below.
const NIMH_1CELL_OCV: [u16; 11] = [
    1000, 1100, 1160, 1190, 1210, 1230, 1240, 1250, 1270, 1300, 1350,
];

/// 11-anchor OCV table for 2× NiMH cells in series.
const NIMH_2CELL_OCV: [u16; 11] = [
    2000, 2200, 2320, 2380, 2420, 2460, 2480, 2500, 2540, 2600, 2700,
];

/// 11-anchor OCV table for 3× NiMH cells in series.
const NIMH_3CELL_OCV: [u16; 11] = [
    3000, 3300, 3480, 3570, 3630, 3690, 3720, 3750, 3810, 3900, 4050,
];

impl BatteryChemistry {
    /// 11-entry OCV table (0 %..100 % in 10 % steps), mV.  Returns
    /// `None` for [`Regulated`](Self::Regulated) — the post-regulator
    /// bus voltage doesn't map to a meaningful SoC curve.
    pub const fn ocv_table(self) -> Option<&'static [u16; 11]> {
        match self {
            Self::LiPoSingle => Some(&LIPO_SINGLE_OCV),
            Self::NimhPack { cells } => match cells {
                1 => Some(&NIMH_1CELL_OCV),
                2 => Some(&NIMH_2CELL_OCV),
                3 => Some(&NIMH_3CELL_OCV),
                // Unsupported pack size — fall back to 3-cell so the
                // gauge isn't nonsensical, but the profile should
                // have rejected this at build time.
                _ => Some(&NIMH_3CELL_OCV),
            },
            Self::Regulated { .. } => None,
        }
    }

    /// Below this terminal-voltage reading we treat the cell as
    /// **not present** (probe-only board, cells removed, regulator
    /// completely failed, etc.) and the indicator shows "—%" instead
    /// of a possibly-misleading percentage.
    ///
    /// Chemistry-dependent because the bottom of a chemistry's
    /// working range is a meaningful 0 % reading rather than "no
    /// cell" — for LiPo that's ~3.0 V; for NiMH it's 1.0 V/cell ×
    /// the cell count.  We set the no-battery floor below the
    /// chemistry's "empty" anchor (~10 % below) so a flat-but-
    /// installed pack isn't mistaken for an absent one.
    pub const fn no_battery_mv(self) -> u16 {
        match self {
            Self::LiPoSingle => 3000,
            Self::NimhPack { cells } => match cells {
                1 => 900,  // ~0.9 V/cell — below safe-discharge floor
                2 => 1800,
                3 => 2700,
                _ => 2700,
            },
            // For Regulated, anything more than ~500 mV below the
            // soft-off threshold means the regulator has fully
            // collapsed — treat as "no power source."
            Self::Regulated { shutdown_mv, .. } => shutdown_mv.saturating_sub(500),
        }
    }

    /// Emergency soft-off floor.  Sustained reads at-or-below this
    /// trigger the M7/M8 low-battery shutdown path.  For raw cell
    /// chemistries, the convention is 1.0 V/cell for NiMH (going
    /// lower in series risks reverse-polarising the weakest cell)
    /// and 3.1 V for single-cell LiPo (a few hundred mV above the
    /// permanent-damage threshold).  For [`Regulated`](Self::Regulated),
    /// it's whatever the profile configured — typically the
    /// regulator's dropout voltage.
    pub const fn shutdown_mv(self) -> u16 {
        match self {
            Self::LiPoSingle => 3100,
            Self::NimhPack { cells } => match cells {
                1 => 1000,
                2 => 2000,
                3 => 3000,
                _ => 3000,
            },
            Self::Regulated { shutdown_mv, .. } => shutdown_mv,
        }
    }

    /// Above this terminal-voltage reading we cap the gauge at
    /// 100 % and treat the cell as on-charge / freshly-charged.
    /// Used to keep the indicator from flickering between 99 / 100 %
    /// during the constant-voltage tail of LiPo charge or the
    /// immediately-post-charge surface voltage of NiMH.
    pub const fn charging_floor_mv(self) -> u16 {
        match self {
            // LiPo CV phase sits at 4.20 V; anything higher is
            // measurement noise + USB-active offset.
            Self::LiPoSingle => 4200,
            // NiMH freshly off charge sits at ~1.40 V/cell briefly
            // before settling to 1.30 V/cell.  Scale per cell count.
            Self::NimhPack { cells } => match cells {
                1 => 1400,
                2 => 2800,
                3 => 4200,
                _ => 4200,
            },
            // For Regulated the bus voltage doesn't track charge
            // state — the regulator outputs its nominal regardless,
            // so there's no meaningful "almost full" reading.  Use a
            // high sentinel so the gauge never auto-caps.
            Self::Regulated { .. } => u16::MAX,
        }
    }

    /// Series cell count.  Used by the UI title bar (where space
    /// allows) to distinguish "1S LiPo" from "3S NiMH" alongside the
    /// percent indicator.  Returns 0 for [`Regulated`](Self::Regulated)
    /// where the cell stack is hidden behind the regulator.
    pub const fn cell_count(self) -> u8 {
        match self {
            Self::LiPoSingle => 1,
            Self::NimhPack { cells } => cells,
            Self::Regulated { .. } => 0,
        }
    }

    /// Convert a measured terminal voltage (mV) into 0..=100 % SoC.
    ///
    /// For chemistries with an OCV table, linear-interpolates within
    /// the segment that contains `mv`.  For [`Regulated`](Self::Regulated),
    /// collapses to a three-zone mapping:
    ///   - `mv > low_mv` → 100 %
    ///   - `shutdown_mv < mv ≤ low_mv` → 0..[`LOW_THRESHOLD_PCT`]
    ///     (linear, so the gauge enters "low" territory then drops
    ///     to 0 as the regulator gives up)
    ///   - `mv ≤ shutdown_mv` → 0 %
    pub fn voltage_to_percent(self, mv: u16) -> u8 {
        match self {
            Self::Regulated { shutdown_mv, low_mv } => {
                if mv <= shutdown_mv {
                    return 0;
                }
                if mv >= low_mv {
                    return 100;
                }
                // Map (shutdown_mv, low_mv] linearly to
                // (0, LOW_THRESHOLD_PCT] so `is_low()` fires the
                // moment we drop below `low_mv`.
                let range = (low_mv - shutdown_mv) as u32;
                let above = (mv - shutdown_mv) as u32;
                let pct = (above * LOW_THRESHOLD_PCT as u32) / range;
                pct.min(LOW_THRESHOLD_PCT as u32) as u8
            }
            _ => {
                // SAFETY of unwrap: only Regulated returns None
                // above; every other arm of this match returns
                // Some(table).
                let table = self.ocv_table().expect("non-Regulated chemistries have an OCV table");
                if mv <= table[0] {
                    return 0;
                }
                if mv >= table[10] {
                    return 100;
                }
                let mut i = 0;
                while i < 10 && mv > table[i + 1] {
                    i += 1;
                }
                let lo = table[i] as u32;
                let hi = table[i + 1] as u32;
                let frac = ((mv as u32 - lo) * 10) / (hi - lo);
                (i as u8) * 10 + frac as u8
            }
        }
    }
}

/// "Warn user" threshold — UI flags the indicator at this SoC or
/// below.  Chemistry-independent: 20 % means the same thing
/// regardless of what's behind the gauge.  Profile-level low-
/// battery actions trigger here.
pub const LOW_THRESHOLD_PCT: u8 = 20;

/// "Aggressive power save" threshold — backlight off sooner, scan
/// auto-disable, etc.  Chemistry-independent.
pub const CRITICAL_THRESHOLD_PCT: u8 = 10;

/// Shared state between the profile's battery monitor task and the
/// UI rendering path.  `Copy` so it can live in a
/// `critical_section::Mutex<Cell<_>>` for cross-task sharing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct BatteryStatus {
    /// Measured terminal voltage (mV).  `0` = no reading yet *or*
    /// no battery present (the chemistry-aware
    /// [`from_reading`](Self::from_reading) zeroes the field when
    /// the reading falls below the chemistry's no-battery floor,
    /// so the renderer can show a single placeholder for either
    /// case).
    pub voltage_mv: u16,
    /// Latest SoC percent derived via the chemistry's OCV table.
    /// `0` when no battery is present.
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

    /// Construct from a fresh voltage reading + USB-present flag,
    /// interpreted through the supplied chemistry's OCV table.  If
    /// the voltage is below the chemistry's [`no_battery_mv`] floor
    /// the result has `voltage_mv = 0` and `percent = 0`, which the
    /// renderer treats as "no battery present" (a single
    /// placeholder regardless of whether the cell is removed, the
    /// cable is unplugged, or we just haven't sampled yet).
    ///
    /// [`no_battery_mv`]: BatteryChemistry::no_battery_mv
    pub fn from_reading(
        voltage_mv: u16,
        plugged_in: bool,
        chemistry: BatteryChemistry,
    ) -> Self {
        if voltage_mv >= chemistry.no_battery_mv() {
            Self {
                voltage_mv,
                percent: chemistry.voltage_to_percent(voltage_mv),
                plugged_in,
            }
        } else {
            Self {
                voltage_mv: 0,
                percent: 0,
                plugged_in,
            }
        }
    }

    /// True iff a battery reading has been taken and the reading is
    /// inside the chemistry's expected range.
    pub const fn is_present(&self) -> bool {
        self.voltage_mv > 0
    }

    /// SoC ≤ [`LOW_THRESHOLD_PCT`].  Used by the UI to switch the
    /// indicator into a warning style.
    pub fn is_low(&self) -> bool {
        self.is_present() && self.percent <= LOW_THRESHOLD_PCT
    }

    /// SoC ≤ [`CRITICAL_THRESHOLD_PCT`].  Used by the profile to
    /// enter aggressive-save mode (shorter backlight timeout, scan
    /// disabled, etc.).
    pub fn is_critical(&self) -> bool {
        self.is_present() && self.percent <= CRITICAL_THRESHOLD_PCT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lipo_anchors_match_meshtastic_reference() {
        let table = BatteryChemistry::LiPoSingle.ocv_table().unwrap();
        assert_eq!(table[0], 3100);
        assert_eq!(table[1], 3300);
        assert_eq!(table[2], 3420);
        assert_eq!(table[3], 3530);
        assert_eq!(table[4], 3630);
        assert_eq!(table[5], 3720);
        assert_eq!(table[6], 3800);
        assert_eq!(table[7], 3890);
        assert_eq!(table[8], 3990);
        assert_eq!(table[9], 4050);
        assert_eq!(table[10], 4190);
    }

    #[test]
    fn nimh_3cell_anchors() {
        let table = BatteryChemistry::NimhPack { cells: 3 }.ocv_table().unwrap();
        // Bottom anchor matches the 3 × 1.00 V/cell discharge cutoff.
        assert_eq!(table[0], 3000);
        // Top anchor matches 3 × 1.35 V/cell (immediately-post-
        // charge OCV; brief 1.40 V flash decays in seconds).
        assert_eq!(table[10], 4050);
        // Middle of curve is famously flat — adjacent anchors at
        // 40-60 % should be within ~100 mV of each other.
        let mid_spread = table[6].saturating_sub(table[4]);
        assert!(
            mid_spread <= 120,
            "NiMH middle should be flat; got {} mV spread between 40-60 % anchors",
            mid_spread,
        );
    }

    /// 1- and 2-cell NiMH tables should be exact scales of the
    /// 3-cell curve — every entry × N == 3-cell entry × (N / 3).
    /// Catches typos in the per-cell-count anchor lists.
    #[test]
    fn nimh_cell_scaled_tables_consistent() {
        let one = BatteryChemistry::NimhPack { cells: 1 }.ocv_table().unwrap();
        let two = BatteryChemistry::NimhPack { cells: 2 }.ocv_table().unwrap();
        let three = BatteryChemistry::NimhPack { cells: 3 }.ocv_table().unwrap();
        for i in 0..11 {
            assert_eq!(
                two[i],
                one[i] * 2,
                "2-cell anchor {} should be 2× 1-cell anchor",
                i
            );
            assert_eq!(
                three[i],
                one[i] * 3,
                "3-cell anchor {} should be 3× 1-cell anchor",
                i
            );
        }
    }

    #[test]
    fn tables_are_monotonic() {
        for chem in [
            BatteryChemistry::LiPoSingle,
            BatteryChemistry::NimhPack { cells: 1 },
            BatteryChemistry::NimhPack { cells: 2 },
            BatteryChemistry::NimhPack { cells: 3 },
        ] {
            let table = chem.ocv_table().unwrap();
            for w in table.windows(2) {
                assert!(w[1] > w[0], "non-monotonic at {} ({:?})", w[0], chem);
            }
        }
    }

    /// Regulated chemistry has no OCV table — `ocv_table()` returns
    /// None and the gauge collapses to a three-zone mapping driven
    /// by the configured shutdown / low thresholds.
    #[test]
    fn regulated_has_no_ocv_table() {
        let reg = BatteryChemistry::Regulated {
            shutdown_mv: 3000,
            low_mv: 3100,
        };
        assert!(reg.ocv_table().is_none());
    }

    /// Regulated three-zone gauge: above `low_mv` reads 100 %,
    /// between `shutdown_mv` and `low_mv` reads in (0, LOW_THRESHOLD_PCT],
    /// at-or-below `shutdown_mv` reads 0 %.
    #[test]
    fn regulated_gauge_three_zones() {
        let reg = BatteryChemistry::Regulated {
            shutdown_mv: 3000,
            low_mv: 3100,
        };
        // Above low_mv → 100 %.
        assert_eq!(reg.voltage_to_percent(3300), 100);
        assert_eq!(reg.voltage_to_percent(3100), 100);
        // At shutdown_mv → 0 %.
        assert_eq!(reg.voltage_to_percent(3000), 0);
        assert_eq!(reg.voltage_to_percent(2500), 0);
        // Halfway between → ~half of LOW_THRESHOLD_PCT.
        let mid = reg.voltage_to_percent(3050);
        assert!(
            mid > 0 && mid <= LOW_THRESHOLD_PCT,
            "midpoint should be inside the low zone, got {} %",
            mid
        );
    }

    /// Regulated status is always either "OK + 100 %" or "low /
    /// critical with single-digit percent."  The gauge never lingers
    /// at intermediate normal SoCs the way an OCV-curve chemistry
    /// does, which is the correct semantics for a regulated bus.
    #[test]
    fn regulated_status_low_and_critical() {
        let reg = BatteryChemistry::Regulated {
            shutdown_mv: 3000,
            low_mv: 3100,
        };
        let ok = BatteryStatus::from_reading(3300, false, reg);
        assert_eq!(ok.percent, 100);
        assert!(!ok.is_low());
        let low = BatteryStatus::from_reading(3050, false, reg);
        assert!(low.is_low());
        // 3050 mV is halfway through the low zone, so percent < 20.
        assert!(low.percent <= LOW_THRESHOLD_PCT);
        // Just above shutdown → critical.
        let crit = BatteryStatus::from_reading(3001, false, reg);
        assert!(crit.is_critical());
    }

    #[test]
    fn lipo_voltage_to_percent_boundaries() {
        let lipo = BatteryChemistry::LiPoSingle;
        assert_eq!(lipo.voltage_to_percent(0), 0);
        assert_eq!(lipo.voltage_to_percent(3000), 0);
        assert_eq!(lipo.voltage_to_percent(3100), 0);
        assert_eq!(lipo.voltage_to_percent(4190), 100);
        assert_eq!(lipo.voltage_to_percent(4500), 100);
    }

    #[test]
    fn lipo_voltage_to_percent_anchors_round_trip() {
        let lipo = BatteryChemistry::LiPoSingle;
        assert_eq!(lipo.voltage_to_percent(3300), 10);
        assert_eq!(lipo.voltage_to_percent(3420), 20);
        assert_eq!(lipo.voltage_to_percent(3530), 30);
        assert_eq!(lipo.voltage_to_percent(3720), 50);
        assert_eq!(lipo.voltage_to_percent(3990), 80);
    }

    #[test]
    fn nimh_voltage_to_percent_anchors() {
        let nimh = BatteryChemistry::NimhPack { cells: 3 };
        // Anchor SoC% should round-trip exactly.  Table is
        // [3000, 3300, 3480, 3570, 3630, 3690, 3720, 3750, 3810, 3900, 4050]
        // at 10 %-anchors 0..100.
        for (anchor_mv, expected_pct) in [
            (3000u16, 0u8),
            (3300, 10),
            (3480, 20),
            (3690, 50),
            (3750, 70),
            (3810, 80),
            (4050, 100),
        ] {
            let got = nimh.voltage_to_percent(anchor_mv);
            assert_eq!(got, expected_pct, "anchor {} mV → expected {} %, got {} %", anchor_mv, expected_pct, got);
        }
    }

    #[test]
    fn nimh_no_battery_floor_distinguishes_zero_soc_from_absent() {
        let nimh = BatteryChemistry::NimhPack { cells: 3 };
        // 3000 mV is genuinely 0 % SoC on 3-cell NiMH — battery is
        // present, just empty.  The no-battery floor must sit
        // *below* this so the renderer doesn't flag a flat pack as
        // "no battery."
        let status = BatteryStatus::from_reading(3000, false, nimh);
        assert!(status.is_present(), "3.0 V should be present (0 % SoC), not absent");
        assert_eq!(status.percent, 0);
        // Below 2.7 V → absent.
        let absent = BatteryStatus::from_reading(2500, false, nimh);
        assert!(!absent.is_present());
        assert_eq!(absent.voltage_mv, 0);
    }

    #[test]
    fn lipo_no_battery_floor_zeros_voltage() {
        let lipo = BatteryChemistry::LiPoSingle;
        // STLink-only / probe-leakage scenario: reads ~2.78 V on
        // the divided rail.  Should be flagged absent.
        let status = BatteryStatus::from_reading(2780, false, lipo);
        assert!(!status.is_present());
        assert_eq!(status.voltage_mv, 0);
        assert_eq!(status.percent, 0);
    }

    #[test]
    fn shutdown_mv_chemistry_aware() {
        // LiPo cutoff is just above the bottom anchor.
        assert_eq!(BatteryChemistry::LiPoSingle.shutdown_mv(), 3100);
        // NiMH 3-cell cutoff = 1.0 V/cell × 3, the conventional
        // safe-discharge floor.
        assert_eq!(BatteryChemistry::NimhPack { cells: 3 }.shutdown_mv(), 3000);
    }

    #[test]
    fn status_thresholds_lipo() {
        let chem = BatteryChemistry::LiPoSingle;
        let mid = BatteryStatus::from_reading(3720, false, chem);
        assert!(!mid.is_low());
        assert!(!mid.is_critical());
        let low = BatteryStatus::from_reading(3420, false, chem);
        assert!(low.is_low());
        assert!(!low.is_critical());
        let crit = BatteryStatus::from_reading(3300, true, chem);
        assert!(crit.is_low());
        assert!(crit.is_critical());
        assert!(crit.plugged_in);
    }

}
