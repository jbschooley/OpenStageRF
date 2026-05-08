// SPDX-License-Identifier: AGPL-3.0-or-later

//! Band plans and channel maps.
//!
//! A "band plan" is a named list of channels.  Each channel has a
//! human-readable label (`"01"`, `"G1"`, `"Ch3"`) and an absolute
//! frequency.  The user picks a band plan, then a channel within
//! it.  Different band plans can have different channel layouts —
//! e.g., a plan optimised for coordination with Sennheiser G-band
//! systems uses different frequencies than the default evenly-
//! spaced 915 MHz ISM plan.
//!
//! v1 frequencies live in the 902–928 MHz US ISM band where the
//! current SX1262 link operates.  When Stage 4 audio lands on the
//! 470–608 MHz TVWS band, additional band plans can be added here
//! for coordination with pro wireless gear in that band.
//!
//! ## Why band plans matter
//!
//! At a venue running pro wireless mics or IEMs (Sennheiser EW,
//! Shure ULX-D, etc.), the audio engineer coordinates frequencies
//! to avoid third-order intermodulation between transmitters —
//! every wireless channel produces sum-and-difference products
//! that can land on neighbours.  Pro systems ship with pre-
//! computed "groups" that already account for this.  For an
//! OpenStageRF unit operating alongside that gear, picking a
//! frequency that aligns with the venue's plan keeps the link
//! out of trouble.
//!
//! For v1 (MIDI-only at 915 MHz), the actual interference risk is
//! minimal — pro mics don't operate at 915 MHz.  The structure
//! here is forward-compatible with audio-band coordination once
//! that work begins.

use heapless::String;

/// One channel within a band plan.  Static data — embedded in
/// `&'static [ChannelInfo]`s defined below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelInfo {
    /// Short label shown in lists (≤ 4 chars, e.g. `"01"`, `"G1"`,
    /// `"Ch3"`).
    pub label: &'static str,
    /// Absolute frequency in kHz.  `915_000` = 915.000 MHz.  kHz
    /// rather than Hz to fit common values in `u32` without going
    /// near the upper limit.
    pub frequency_khz: u32,
}

impl ChannelInfo {
    /// Format the frequency as `"915.000 MHz"` etc.
    pub fn format_frequency(&self) -> String<16> {
        let mhz_int = self.frequency_khz / 1000;
        let mhz_frac = self.frequency_khz % 1000;
        let mut out: String<16> = String::new();
        use core::fmt::Write as _;
        let _ = write!(&mut out, "{}.{:03} MHz", mhz_int, mhz_frac);
        out
    }
}

/// One band plan: a name plus a list of channels.
#[derive(Debug, Clone, Copy)]
pub struct BandPlanInfo {
    /// Short label shown in band-plan lists and the Idle banner
    /// (≤ 12 chars, e.g. `"ISM 915"`, `"Senn G compat"`).
    pub label: &'static str,
    /// Static channel list.  Index 0 is the default for this plan.
    pub channels: &'static [ChannelInfo],
}

/// Identifier for one of the [`BAND_PLANS`] entries.  Stored in
/// [`crate::Settings::band_plan`] as a small integer — survives
/// serialisation cleanly and supports compile-time validation of
/// channel indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum BandPlan {
    /// Default 915 MHz ISM, 5 channels evenly spaced across the
    /// lower half of the band.  Spaced at 500 kHz intervals — well
    /// past our ~600 kHz channel allocation, so even adjacent
    /// channels won't intermodulate at any reasonable TX power.
    Ism915,
    /// Sennheiser G-compatible coordination.  Channel frequencies
    /// chosen to align with Sennheiser EW G-band group offsets so
    /// an OpenStageRF link operating alongside Sennheiser gear at
    /// a venue stays out of their intermod nulls.  v1 stub —
    /// frequencies still in 915 MHz ISM but at non-uniform offsets
    /// that mirror Sennheiser's typical 8/16/24/32 channel groups.
    /// Refine once we have real venue data.
    Sennheiser,
    /// Shure G50 / H50 / J50-style coordination.  Same idea as the
    /// Sennheiser plan; offsets chosen to avoid Shure's typical
    /// IEM/microphone group nulls.  v1 stub.
    Shure,
    /// Tight 100 kHz grid — for situations where you have many
    /// OpenStageRF units close together and want maximum channel
    /// density at the cost of some adjacent-channel risk.
    Dense,
}

impl BandPlan {
    /// Return the static info for this plan.
    pub fn info(self) -> &'static BandPlanInfo {
        match self {
            BandPlan::Ism915 => &ISM_915,
            BandPlan::Sennheiser => &SENNHEISER_COMPAT,
            BandPlan::Shure => &SHURE_COMPAT,
            BandPlan::Dense => &DENSE_GRID,
        }
    }
}

/// All available band plans, in the order they appear in the
/// BandPlanSelect screen.
pub const BAND_PLANS: &[BandPlan] = &[
    BandPlan::Ism915,
    BandPlan::Sennheiser,
    BandPlan::Shure,
    BandPlan::Dense,
];

// ── Plan definitions ────────────────────────────────────────────────────────

/// Default 915 MHz ISM plan.  24 channels at 1 MHz spacing across
/// 903–926 MHz, inside the 902–928 MHz US ISM allocation with
/// margin at both edges.
///
/// Channel-spacing math: our 300 kbps GFSK with 50 kHz deviation
/// occupies ~700 kHz (Carson's rule); the receiver's IF filter is
/// 467 kHz wide.  1 MHz spacing puts adjacent-channel power
/// ~150 kHz outside the receiver's filter passband, well rejected.
/// Two OpenStageRF systems can run simultaneously on adjacent
/// channels at this spacing without measurable cross-talk.
static ISM_915: BandPlanInfo = BandPlanInfo {
    label: "ISM 915",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 903_000 },
        ChannelInfo { label: "02", frequency_khz: 904_000 },
        ChannelInfo { label: "03", frequency_khz: 905_000 },
        ChannelInfo { label: "04", frequency_khz: 906_000 },
        ChannelInfo { label: "05", frequency_khz: 907_000 },
        ChannelInfo { label: "06", frequency_khz: 908_000 },
        ChannelInfo { label: "07", frequency_khz: 909_000 },
        ChannelInfo { label: "08", frequency_khz: 910_000 },
        ChannelInfo { label: "09", frequency_khz: 911_000 },
        ChannelInfo { label: "10", frequency_khz: 912_000 },
        ChannelInfo { label: "11", frequency_khz: 913_000 },
        ChannelInfo { label: "12", frequency_khz: 914_000 },
        ChannelInfo { label: "13", frequency_khz: 915_000 },
        ChannelInfo { label: "14", frequency_khz: 916_000 },
        ChannelInfo { label: "15", frequency_khz: 917_000 },
        ChannelInfo { label: "16", frequency_khz: 918_000 },
        ChannelInfo { label: "17", frequency_khz: 919_000 },
        ChannelInfo { label: "18", frequency_khz: 920_000 },
        ChannelInfo { label: "19", frequency_khz: 921_000 },
        ChannelInfo { label: "20", frequency_khz: 922_000 },
        ChannelInfo { label: "21", frequency_khz: 923_000 },
        ChannelInfo { label: "22", frequency_khz: 924_000 },
        ChannelInfo { label: "23", frequency_khz: 925_000 },
        ChannelInfo { label: "24", frequency_khz: 926_000 },
    ],
};

/// Sennheiser-compat plan.  v1 stub frequencies — placeholders for
/// real Sennheiser-G-band-coordination data once we have it.
static SENNHEISER_COMPAT: BandPlanInfo = BandPlanInfo {
    label: "Senn G compat",
    channels: &[
        ChannelInfo { label: "G1", frequency_khz: 914_725 },
        ChannelInfo { label: "G2", frequency_khz: 915_375 },
        ChannelInfo { label: "G3", frequency_khz: 916_125 },
        ChannelInfo { label: "G4", frequency_khz: 916_875 },
        ChannelInfo { label: "G5", frequency_khz: 917_625 },
    ],
};

/// Shure-compat plan.  v1 stub frequencies — placeholders for real
/// Shure G50 / H50 / J50 coordination data once we have it.
static SHURE_COMPAT: BandPlanInfo = BandPlanInfo {
    label: "Shure compat",
    channels: &[
        ChannelInfo { label: "S1", frequency_khz: 914_850 },
        ChannelInfo { label: "S2", frequency_khz: 915_650 },
        ChannelInfo { label: "S3", frequency_khz: 916_450 },
        ChannelInfo { label: "S4", frequency_khz: 917_250 },
    ],
};

/// Dense 100 kHz grid — channels OVERLAP at this spacing (our 300
/// kbps GFSK occupies ~700 kHz; 100 kHz steps mean adjacent
/// channels share most of their spectrum).  Only useful for
/// **single-unit** deployments where you want fine-grained
/// frequency adjustment to dodge a specific narrowband
/// interferer (microwave oven, ISM-band sensor, neighbouring
/// WiFi sidelobe).  Two OpenStageRF systems CANNOT run on
/// adjacent Dense channels simultaneously — they'll desense
/// each other badly.
static DENSE_GRID: BandPlanInfo = BandPlanInfo {
    label: "Dense 100k",
    channels: &[
        ChannelInfo { label: "D1", frequency_khz: 915_000 },
        ChannelInfo { label: "D2", frequency_khz: 915_100 },
        ChannelInfo { label: "D3", frequency_khz: 915_200 },
        ChannelInfo { label: "D4", frequency_khz: 915_300 },
        ChannelInfo { label: "D5", frequency_khz: 915_400 },
        ChannelInfo { label: "D6", frequency_khz: 915_500 },
        ChannelInfo { label: "D7", frequency_khz: 915_600 },
        ChannelInfo { label: "D8", frequency_khz: 915_700 },
    ],
};

/// Look up a channel within a band plan by index, clamping to the
/// plan's range.  `(plan, idx)` is the canonical (Settings-stored)
/// representation; this is how the runtime resolves the actual
/// frequency.
pub fn channel(plan: BandPlan, idx: u8) -> ChannelInfo {
    let info = plan.info();
    let i = (idx as usize).min(info.channels.len().saturating_sub(1));
    info.channels[i]
}

/// Maximum channel index for a band plan (0-based).  Used by the
/// UI's value-clamping logic.
pub fn max_channel_index(plan: BandPlan) -> u8 {
    plan.info().channels.len().saturating_sub(1) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_plans_have_at_least_one_channel() {
        for &p in BAND_PLANS {
            assert!(!p.info().channels.is_empty(), "plan {:?} has no channels", p);
        }
    }

    #[test]
    fn channel_label_fits_in_4_chars() {
        for &p in BAND_PLANS {
            for c in p.info().channels {
                assert!(c.label.len() <= 4, "label too long: {:?}", c.label);
            }
        }
    }

    #[test]
    fn frequencies_in_ism_band() {
        // All v1 plans should sit in 902–928 MHz US ISM.
        for &p in BAND_PLANS {
            for c in p.info().channels {
                assert!(
                    c.frequency_khz >= 902_000 && c.frequency_khz <= 928_000,
                    "channel {:?} of plan {:?} at {} kHz is outside US ISM",
                    c.label,
                    p,
                    c.frequency_khz
                );
            }
        }
    }

    #[test]
    fn format_frequency_renders_three_decimals() {
        let c = ChannelInfo { label: "test", frequency_khz: 915_000 };
        assert_eq!(c.format_frequency().as_str(), "915.000 MHz");
        let c = ChannelInfo { label: "test", frequency_khz: 916_125 };
        assert_eq!(c.format_frequency().as_str(), "916.125 MHz");
    }

    #[test]
    fn channel_lookup_clamps_overflow() {
        let c = channel(BandPlan::Ism915, 99);
        assert_eq!(c.label, "24"); // last channel in ISM_915
    }
}
