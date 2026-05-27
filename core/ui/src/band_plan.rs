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
    /// Lower third of the 902–928 MHz band at 100 kHz spacing —
    /// 87 channels, 902.0–910.6 MHz.  Channels overlap heavily
    /// (300 kbps GFSK occupies ~700 kHz) so this is a **scan and
    /// fine-tune** plan, not a multi-link coordination plan.
    /// Pair with the Scan screen to see the full lower band's
    /// noise floor at 100 kHz resolution.
    DenseLo,
    /// Middle third at 100 kHz spacing — 87 channels,
    /// 910.7–919.3 MHz.  Same scan/fine-tune use case as
    /// [`BandPlan::DenseLo`].
    DenseMid,
    /// Upper third at 100 kHz spacing — 87 channels,
    /// 919.4–928.0 MHz.  Same scan/fine-tune use case as
    /// [`BandPlan::DenseLo`].
    DenseHi,
    /// Whole 902–928 MHz band at 200 kHz spacing — 131 channels.
    /// Renders as a spectrum-trace overview on the Scan screen
    /// (bars too narrow for individual selection at typical panel
    /// widths, but reveals interference patterns at a glance).
    Wide,
    /// 470–510 MHz band, 39 channels at 1 MHz spacing (471–509 MHz,
    /// 1 MHz margin at each edge) — for the SX1268 (470-MHz) hardware
    /// variant.  Same even-spacing rationale as [`BandPlan::Ism915`]:
    /// 1 MHz spacing keeps adjacent ~700 kHz GFSK channels clear of
    /// the receiver's IF passband.  This band is **different silicon**
    /// (SX1268, not SX1262) — build a 470 profile, don't switch to it
    /// on 915 hardware (the radio can't tune here).
    Band470,
    /// 470 MHz coexistence plan aligned to **Sennheiser ew G4 band A1**
    /// (470–516 MHz; we span 471–506 within our 470–510 hardware range).
    /// For an OpenStageRF link running alongside Sennheiser A1 gear.
    /// **Channels are evenly spaced within A1, not Sennheiser's
    /// proprietary intermod-coordinated bank frequencies** — same stub
    /// caveat as [`BandPlan::Sennheiser`]; refine with real coordination data.
    SennheiserA1,
    /// Shure SLX-D **G58 Group 1 (Full Range 1)** — 26 ch ≤510 MHz.
    ShureG58G1,
    /// Shure SLX-D **G58 Group 2 (Full Range 2)** — 25 ch ≤510 MHz.
    ShureG58G2,
    /// Shure SLX-D **G58 Group 3 (Full Range 3)** — 27 ch ≤510 MHz.
    ShureG58G3,
    /// Shure SLX-D **G58 Group 4 (Multi-band G4 (compat w/ group 4 of H55 & J52))** — 24 ch ≤510 MHz.
    ShureG58G4,
    /// Shure SLX-D **G58 Group 5 (Multi-band G5 (group 5 of H55 & J52))** — 27 ch ≤510 MHz.
    ShureG58G5,
    /// Shure SLX-D **G58 Group 6 (Multi-band G6 (group 6 of H55 & J52))** — 26 ch ≤510 MHz.
    ShureG58G6,
    /// Shure SLX-D **G58 Group 7 (TV14)** — 10 ch ≤510 MHz.
    ShureG58G7,
    /// Shure SLX-D **G58 Group 8 (TV15)** — 10 ch ≤510 MHz.
    ShureG58G8,
    /// Shure SLX-D **G58 Group 9 (TV16)** — 10 ch ≤510 MHz.
    ShureG58G9,
    /// Shure SLX-D **G58 Group 10 (TV17)** — 10 ch ≤510 MHz.
    ShureG58G10,
    /// Shure SLX-D **G58 Group 11 (TV18)** — 9 ch ≤510 MHz.
    ShureG58G11,
    /// Shure SLX-D **G58 Group 12 (TV19)** — 10 ch ≤510 MHz.
    ShureG58G12,
    /// Shure SLX-D **G58 Group 13 (TV20)** — 7 ch ≤510 MHz.
    ShureG58G13,
    /// Shure SLX-D **G58 Group 15 (TV14 & TV16)** — 18 ch ≤510 MHz.
    ShureG58G15,
    /// Shure SLX-D **G58 Group 16 (TV15 & TV17)** — 18 ch ≤510 MHz.
    ShureG58G16,
    /// Shure SLX-D **G58 Group 17 (TV16 & TV18)** — 18 ch ≤510 MHz.
    ShureG58G17,
    /// Shure SLX-D **G58 Group 18 (TV17 & TV19)** — 18 ch ≤510 MHz.
    ShureG58G18,
    /// Shure SLX-D **G58 Group 19 (TV18 & TV20)** — 15 ch ≤510 MHz.
    ShureG58G19,
    /// Shure SLX-D **G58 Group 20 (TV19 & TV21)** — 10 ch ≤510 MHz.
    ShureG58G20,
}

impl BandPlan {
    /// Return the static info for this plan.
    pub fn info(self) -> &'static BandPlanInfo {
        match self {
            BandPlan::Ism915 => &ISM_915,
            BandPlan::Sennheiser => &SENNHEISER_COMPAT,
            BandPlan::Shure => &SHURE_COMPAT,
            BandPlan::DenseLo => &DENSE_LO,
            BandPlan::DenseMid => &DENSE_MID,
            BandPlan::DenseHi => &DENSE_HI,
            BandPlan::Wide => &WIDE,
            BandPlan::Band470 => &BAND_470,
            BandPlan::SennheiserA1 => &BAND_470_SENNHEISER_A1,
            BandPlan::ShureG58G1 => &BAND_470_SHURE_G58_G1,
            BandPlan::ShureG58G2 => &BAND_470_SHURE_G58_G2,
            BandPlan::ShureG58G3 => &BAND_470_SHURE_G58_G3,
            BandPlan::ShureG58G4 => &BAND_470_SHURE_G58_G4,
            BandPlan::ShureG58G5 => &BAND_470_SHURE_G58_G5,
            BandPlan::ShureG58G6 => &BAND_470_SHURE_G58_G6,
            BandPlan::ShureG58G7 => &BAND_470_SHURE_G58_G7,
            BandPlan::ShureG58G8 => &BAND_470_SHURE_G58_G8,
            BandPlan::ShureG58G9 => &BAND_470_SHURE_G58_G9,
            BandPlan::ShureG58G10 => &BAND_470_SHURE_G58_G10,
            BandPlan::ShureG58G11 => &BAND_470_SHURE_G58_G11,
            BandPlan::ShureG58G12 => &BAND_470_SHURE_G58_G12,
            BandPlan::ShureG58G13 => &BAND_470_SHURE_G58_G13,
            BandPlan::ShureG58G15 => &BAND_470_SHURE_G58_G15,
            BandPlan::ShureG58G16 => &BAND_470_SHURE_G58_G16,
            BandPlan::ShureG58G17 => &BAND_470_SHURE_G58_G17,
            BandPlan::ShureG58G18 => &BAND_470_SHURE_G58_G18,
            BandPlan::ShureG58G19 => &BAND_470_SHURE_G58_G19,
            BandPlan::ShureG58G20 => &BAND_470_SHURE_G58_G20,
        }
    }
}

/// All band plans across every hardware variant, in a stable order.
/// **This is the serialization index space** — `Settings::band_plan` is
/// stored as an index into this slice, so entries must only ever be
/// appended (never reordered/removed) to keep persisted settings valid.
/// The Band Plan *menu* shows a per-profile subset ([`BAND_PLANS_915`] /
/// [`BAND_PLANS_470`]) via [`crate::UiState::band_plans`], not this whole
/// list.
pub const BAND_PLANS: &[BandPlan] = &[
    BandPlan::Ism915,
    BandPlan::Sennheiser,
    BandPlan::Shure,
    BandPlan::DenseLo,
    BandPlan::DenseMid,
    BandPlan::DenseHi,
    BandPlan::Wide,
    BandPlan::Band470,
    BandPlan::SennheiserA1,
    BandPlan::ShureG58G1,
    BandPlan::ShureG58G2,
    BandPlan::ShureG58G3,
    BandPlan::ShureG58G4,
    BandPlan::ShureG58G5,
    BandPlan::ShureG58G6,
    BandPlan::ShureG58G7,
    BandPlan::ShureG58G8,
    BandPlan::ShureG58G9,
    BandPlan::ShureG58G10,
    BandPlan::ShureG58G11,
    BandPlan::ShureG58G12,
    BandPlan::ShureG58G13,
    BandPlan::ShureG58G15,
    BandPlan::ShureG58G16,
    BandPlan::ShureG58G17,
    BandPlan::ShureG58G18,
    BandPlan::ShureG58G19,
    BandPlan::ShureG58G20,
];

/// Menu band-plan list for 902–928 MHz (SX1262) builds.
pub const BAND_PLANS_915: &[BandPlan] = &[
    BandPlan::Ism915,
    BandPlan::Sennheiser,
    BandPlan::Shure,
    BandPlan::DenseLo,
    BandPlan::DenseMid,
    BandPlan::DenseHi,
    BandPlan::Wide,
];

/// Menu band-plan list for 470–510 MHz (SX1268) builds.
pub const BAND_PLANS_470: &[BandPlan] = &[
    BandPlan::Band470,
    BandPlan::SennheiserA1,
    BandPlan::ShureG58G1,
    BandPlan::ShureG58G2,
    BandPlan::ShureG58G3,
    BandPlan::ShureG58G4,
    BandPlan::ShureG58G5,
    BandPlan::ShureG58G6,
    BandPlan::ShureG58G7,
    BandPlan::ShureG58G8,
    BandPlan::ShureG58G9,
    BandPlan::ShureG58G10,
    BandPlan::ShureG58G11,
    BandPlan::ShureG58G12,
    BandPlan::ShureG58G13,
    BandPlan::ShureG58G15,
    BandPlan::ShureG58G16,
    BandPlan::ShureG58G17,
    BandPlan::ShureG58G18,
    BandPlan::ShureG58G19,
    BandPlan::ShureG58G20,
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
#[rustfmt::skip] // Channel table; one-line entries read as a frequency map.
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
#[rustfmt::skip] // Channel table; one-line entries read as a frequency map.
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
#[rustfmt::skip] // Channel table; one-line entries read as a frequency map.
static SHURE_COMPAT: BandPlanInfo = BandPlanInfo {
    label: "Shure compat",
    channels: &[
        ChannelInfo { label: "S1", frequency_khz: 914_850 },
        ChannelInfo { label: "S2", frequency_khz: 915_650 },
        ChannelInfo { label: "S3", frequency_khz: 916_450 },
        ChannelInfo { label: "S4", frequency_khz: 917_250 },
    ],
};

/// Dense plans — 902–928 MHz divided into three 87-channel slices
/// at 100 kHz spacing (DenseLo 902.0–910.6, DenseMid 910.7–919.3,
/// DenseHi 919.4–928.0).  Plus a Wide plan at 200 kHz spacing
/// covering the whole band (131 channels).  Channels OVERLAP at
/// these spacings (our 300 kbps GFSK occupies ~700 kHz) so they're
/// **scan / fine-tune** plans, not multi-link coordination plans.
/// Two OpenStageRF systems CANNOT run on adjacent Dense channels
/// simultaneously — they'll desense each other badly.
#[rustfmt::skip] // Channel table; one-line entries read as a frequency map.
static DENSE_LO: BandPlanInfo = BandPlanInfo {
    label: "Dense Lo 0.1",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 902_000 },
        ChannelInfo { label: "02", frequency_khz: 902_100 },
        ChannelInfo { label: "03", frequency_khz: 902_200 },
        ChannelInfo { label: "04", frequency_khz: 902_300 },
        ChannelInfo { label: "05", frequency_khz: 902_400 },
        ChannelInfo { label: "06", frequency_khz: 902_500 },
        ChannelInfo { label: "07", frequency_khz: 902_600 },
        ChannelInfo { label: "08", frequency_khz: 902_700 },
        ChannelInfo { label: "09", frequency_khz: 902_800 },
        ChannelInfo { label: "10", frequency_khz: 902_900 },
        ChannelInfo { label: "11", frequency_khz: 903_000 },
        ChannelInfo { label: "12", frequency_khz: 903_100 },
        ChannelInfo { label: "13", frequency_khz: 903_200 },
        ChannelInfo { label: "14", frequency_khz: 903_300 },
        ChannelInfo { label: "15", frequency_khz: 903_400 },
        ChannelInfo { label: "16", frequency_khz: 903_500 },
        ChannelInfo { label: "17", frequency_khz: 903_600 },
        ChannelInfo { label: "18", frequency_khz: 903_700 },
        ChannelInfo { label: "19", frequency_khz: 903_800 },
        ChannelInfo { label: "20", frequency_khz: 903_900 },
        ChannelInfo { label: "21", frequency_khz: 904_000 },
        ChannelInfo { label: "22", frequency_khz: 904_100 },
        ChannelInfo { label: "23", frequency_khz: 904_200 },
        ChannelInfo { label: "24", frequency_khz: 904_300 },
        ChannelInfo { label: "25", frequency_khz: 904_400 },
        ChannelInfo { label: "26", frequency_khz: 904_500 },
        ChannelInfo { label: "27", frequency_khz: 904_600 },
        ChannelInfo { label: "28", frequency_khz: 904_700 },
        ChannelInfo { label: "29", frequency_khz: 904_800 },
        ChannelInfo { label: "30", frequency_khz: 904_900 },
        ChannelInfo { label: "31", frequency_khz: 905_000 },
        ChannelInfo { label: "32", frequency_khz: 905_100 },
        ChannelInfo { label: "33", frequency_khz: 905_200 },
        ChannelInfo { label: "34", frequency_khz: 905_300 },
        ChannelInfo { label: "35", frequency_khz: 905_400 },
        ChannelInfo { label: "36", frequency_khz: 905_500 },
        ChannelInfo { label: "37", frequency_khz: 905_600 },
        ChannelInfo { label: "38", frequency_khz: 905_700 },
        ChannelInfo { label: "39", frequency_khz: 905_800 },
        ChannelInfo { label: "40", frequency_khz: 905_900 },
        ChannelInfo { label: "41", frequency_khz: 906_000 },
        ChannelInfo { label: "42", frequency_khz: 906_100 },
        ChannelInfo { label: "43", frequency_khz: 906_200 },
        ChannelInfo { label: "44", frequency_khz: 906_300 },
        ChannelInfo { label: "45", frequency_khz: 906_400 },
        ChannelInfo { label: "46", frequency_khz: 906_500 },
        ChannelInfo { label: "47", frequency_khz: 906_600 },
        ChannelInfo { label: "48", frequency_khz: 906_700 },
        ChannelInfo { label: "49", frequency_khz: 906_800 },
        ChannelInfo { label: "50", frequency_khz: 906_900 },
        ChannelInfo { label: "51", frequency_khz: 907_000 },
        ChannelInfo { label: "52", frequency_khz: 907_100 },
        ChannelInfo { label: "53", frequency_khz: 907_200 },
        ChannelInfo { label: "54", frequency_khz: 907_300 },
        ChannelInfo { label: "55", frequency_khz: 907_400 },
        ChannelInfo { label: "56", frequency_khz: 907_500 },
        ChannelInfo { label: "57", frequency_khz: 907_600 },
        ChannelInfo { label: "58", frequency_khz: 907_700 },
        ChannelInfo { label: "59", frequency_khz: 907_800 },
        ChannelInfo { label: "60", frequency_khz: 907_900 },
        ChannelInfo { label: "61", frequency_khz: 908_000 },
        ChannelInfo { label: "62", frequency_khz: 908_100 },
        ChannelInfo { label: "63", frequency_khz: 908_200 },
        ChannelInfo { label: "64", frequency_khz: 908_300 },
        ChannelInfo { label: "65", frequency_khz: 908_400 },
        ChannelInfo { label: "66", frequency_khz: 908_500 },
        ChannelInfo { label: "67", frequency_khz: 908_600 },
        ChannelInfo { label: "68", frequency_khz: 908_700 },
        ChannelInfo { label: "69", frequency_khz: 908_800 },
        ChannelInfo { label: "70", frequency_khz: 908_900 },
        ChannelInfo { label: "71", frequency_khz: 909_000 },
        ChannelInfo { label: "72", frequency_khz: 909_100 },
        ChannelInfo { label: "73", frequency_khz: 909_200 },
        ChannelInfo { label: "74", frequency_khz: 909_300 },
        ChannelInfo { label: "75", frequency_khz: 909_400 },
        ChannelInfo { label: "76", frequency_khz: 909_500 },
        ChannelInfo { label: "77", frequency_khz: 909_600 },
        ChannelInfo { label: "78", frequency_khz: 909_700 },
        ChannelInfo { label: "79", frequency_khz: 909_800 },
        ChannelInfo { label: "80", frequency_khz: 909_900 },
        ChannelInfo { label: "81", frequency_khz: 910_000 },
        ChannelInfo { label: "82", frequency_khz: 910_100 },
        ChannelInfo { label: "83", frequency_khz: 910_200 },
        ChannelInfo { label: "84", frequency_khz: 910_300 },
        ChannelInfo { label: "85", frequency_khz: 910_400 },
        ChannelInfo { label: "86", frequency_khz: 910_500 },
        ChannelInfo { label: "87", frequency_khz: 910_600 },
    ],
};

#[rustfmt::skip] // Channel table; one-line entries read as a frequency map.
static DENSE_MID: BandPlanInfo = BandPlanInfo {
    label: "Dense Mid 0.1",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 910_700 },
        ChannelInfo { label: "02", frequency_khz: 910_800 },
        ChannelInfo { label: "03", frequency_khz: 910_900 },
        ChannelInfo { label: "04", frequency_khz: 911_000 },
        ChannelInfo { label: "05", frequency_khz: 911_100 },
        ChannelInfo { label: "06", frequency_khz: 911_200 },
        ChannelInfo { label: "07", frequency_khz: 911_300 },
        ChannelInfo { label: "08", frequency_khz: 911_400 },
        ChannelInfo { label: "09", frequency_khz: 911_500 },
        ChannelInfo { label: "10", frequency_khz: 911_600 },
        ChannelInfo { label: "11", frequency_khz: 911_700 },
        ChannelInfo { label: "12", frequency_khz: 911_800 },
        ChannelInfo { label: "13", frequency_khz: 911_900 },
        ChannelInfo { label: "14", frequency_khz: 912_000 },
        ChannelInfo { label: "15", frequency_khz: 912_100 },
        ChannelInfo { label: "16", frequency_khz: 912_200 },
        ChannelInfo { label: "17", frequency_khz: 912_300 },
        ChannelInfo { label: "18", frequency_khz: 912_400 },
        ChannelInfo { label: "19", frequency_khz: 912_500 },
        ChannelInfo { label: "20", frequency_khz: 912_600 },
        ChannelInfo { label: "21", frequency_khz: 912_700 },
        ChannelInfo { label: "22", frequency_khz: 912_800 },
        ChannelInfo { label: "23", frequency_khz: 912_900 },
        ChannelInfo { label: "24", frequency_khz: 913_000 },
        ChannelInfo { label: "25", frequency_khz: 913_100 },
        ChannelInfo { label: "26", frequency_khz: 913_200 },
        ChannelInfo { label: "27", frequency_khz: 913_300 },
        ChannelInfo { label: "28", frequency_khz: 913_400 },
        ChannelInfo { label: "29", frequency_khz: 913_500 },
        ChannelInfo { label: "30", frequency_khz: 913_600 },
        ChannelInfo { label: "31", frequency_khz: 913_700 },
        ChannelInfo { label: "32", frequency_khz: 913_800 },
        ChannelInfo { label: "33", frequency_khz: 913_900 },
        ChannelInfo { label: "34", frequency_khz: 914_000 },
        ChannelInfo { label: "35", frequency_khz: 914_100 },
        ChannelInfo { label: "36", frequency_khz: 914_200 },
        ChannelInfo { label: "37", frequency_khz: 914_300 },
        ChannelInfo { label: "38", frequency_khz: 914_400 },
        ChannelInfo { label: "39", frequency_khz: 914_500 },
        ChannelInfo { label: "40", frequency_khz: 914_600 },
        ChannelInfo { label: "41", frequency_khz: 914_700 },
        ChannelInfo { label: "42", frequency_khz: 914_800 },
        ChannelInfo { label: "43", frequency_khz: 914_900 },
        ChannelInfo { label: "44", frequency_khz: 915_000 },
        ChannelInfo { label: "45", frequency_khz: 915_100 },
        ChannelInfo { label: "46", frequency_khz: 915_200 },
        ChannelInfo { label: "47", frequency_khz: 915_300 },
        ChannelInfo { label: "48", frequency_khz: 915_400 },
        ChannelInfo { label: "49", frequency_khz: 915_500 },
        ChannelInfo { label: "50", frequency_khz: 915_600 },
        ChannelInfo { label: "51", frequency_khz: 915_700 },
        ChannelInfo { label: "52", frequency_khz: 915_800 },
        ChannelInfo { label: "53", frequency_khz: 915_900 },
        ChannelInfo { label: "54", frequency_khz: 916_000 },
        ChannelInfo { label: "55", frequency_khz: 916_100 },
        ChannelInfo { label: "56", frequency_khz: 916_200 },
        ChannelInfo { label: "57", frequency_khz: 916_300 },
        ChannelInfo { label: "58", frequency_khz: 916_400 },
        ChannelInfo { label: "59", frequency_khz: 916_500 },
        ChannelInfo { label: "60", frequency_khz: 916_600 },
        ChannelInfo { label: "61", frequency_khz: 916_700 },
        ChannelInfo { label: "62", frequency_khz: 916_800 },
        ChannelInfo { label: "63", frequency_khz: 916_900 },
        ChannelInfo { label: "64", frequency_khz: 917_000 },
        ChannelInfo { label: "65", frequency_khz: 917_100 },
        ChannelInfo { label: "66", frequency_khz: 917_200 },
        ChannelInfo { label: "67", frequency_khz: 917_300 },
        ChannelInfo { label: "68", frequency_khz: 917_400 },
        ChannelInfo { label: "69", frequency_khz: 917_500 },
        ChannelInfo { label: "70", frequency_khz: 917_600 },
        ChannelInfo { label: "71", frequency_khz: 917_700 },
        ChannelInfo { label: "72", frequency_khz: 917_800 },
        ChannelInfo { label: "73", frequency_khz: 917_900 },
        ChannelInfo { label: "74", frequency_khz: 918_000 },
        ChannelInfo { label: "75", frequency_khz: 918_100 },
        ChannelInfo { label: "76", frequency_khz: 918_200 },
        ChannelInfo { label: "77", frequency_khz: 918_300 },
        ChannelInfo { label: "78", frequency_khz: 918_400 },
        ChannelInfo { label: "79", frequency_khz: 918_500 },
        ChannelInfo { label: "80", frequency_khz: 918_600 },
        ChannelInfo { label: "81", frequency_khz: 918_700 },
        ChannelInfo { label: "82", frequency_khz: 918_800 },
        ChannelInfo { label: "83", frequency_khz: 918_900 },
        ChannelInfo { label: "84", frequency_khz: 919_000 },
        ChannelInfo { label: "85", frequency_khz: 919_100 },
        ChannelInfo { label: "86", frequency_khz: 919_200 },
        ChannelInfo { label: "87", frequency_khz: 919_300 },
    ],
};

#[rustfmt::skip] // Channel table; one-line entries read as a frequency map.
static DENSE_HI: BandPlanInfo = BandPlanInfo {
    label: "Dense Hi 0.1",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 919_400 },
        ChannelInfo { label: "02", frequency_khz: 919_500 },
        ChannelInfo { label: "03", frequency_khz: 919_600 },
        ChannelInfo { label: "04", frequency_khz: 919_700 },
        ChannelInfo { label: "05", frequency_khz: 919_800 },
        ChannelInfo { label: "06", frequency_khz: 919_900 },
        ChannelInfo { label: "07", frequency_khz: 920_000 },
        ChannelInfo { label: "08", frequency_khz: 920_100 },
        ChannelInfo { label: "09", frequency_khz: 920_200 },
        ChannelInfo { label: "10", frequency_khz: 920_300 },
        ChannelInfo { label: "11", frequency_khz: 920_400 },
        ChannelInfo { label: "12", frequency_khz: 920_500 },
        ChannelInfo { label: "13", frequency_khz: 920_600 },
        ChannelInfo { label: "14", frequency_khz: 920_700 },
        ChannelInfo { label: "15", frequency_khz: 920_800 },
        ChannelInfo { label: "16", frequency_khz: 920_900 },
        ChannelInfo { label: "17", frequency_khz: 921_000 },
        ChannelInfo { label: "18", frequency_khz: 921_100 },
        ChannelInfo { label: "19", frequency_khz: 921_200 },
        ChannelInfo { label: "20", frequency_khz: 921_300 },
        ChannelInfo { label: "21", frequency_khz: 921_400 },
        ChannelInfo { label: "22", frequency_khz: 921_500 },
        ChannelInfo { label: "23", frequency_khz: 921_600 },
        ChannelInfo { label: "24", frequency_khz: 921_700 },
        ChannelInfo { label: "25", frequency_khz: 921_800 },
        ChannelInfo { label: "26", frequency_khz: 921_900 },
        ChannelInfo { label: "27", frequency_khz: 922_000 },
        ChannelInfo { label: "28", frequency_khz: 922_100 },
        ChannelInfo { label: "29", frequency_khz: 922_200 },
        ChannelInfo { label: "30", frequency_khz: 922_300 },
        ChannelInfo { label: "31", frequency_khz: 922_400 },
        ChannelInfo { label: "32", frequency_khz: 922_500 },
        ChannelInfo { label: "33", frequency_khz: 922_600 },
        ChannelInfo { label: "34", frequency_khz: 922_700 },
        ChannelInfo { label: "35", frequency_khz: 922_800 },
        ChannelInfo { label: "36", frequency_khz: 922_900 },
        ChannelInfo { label: "37", frequency_khz: 923_000 },
        ChannelInfo { label: "38", frequency_khz: 923_100 },
        ChannelInfo { label: "39", frequency_khz: 923_200 },
        ChannelInfo { label: "40", frequency_khz: 923_300 },
        ChannelInfo { label: "41", frequency_khz: 923_400 },
        ChannelInfo { label: "42", frequency_khz: 923_500 },
        ChannelInfo { label: "43", frequency_khz: 923_600 },
        ChannelInfo { label: "44", frequency_khz: 923_700 },
        ChannelInfo { label: "45", frequency_khz: 923_800 },
        ChannelInfo { label: "46", frequency_khz: 923_900 },
        ChannelInfo { label: "47", frequency_khz: 924_000 },
        ChannelInfo { label: "48", frequency_khz: 924_100 },
        ChannelInfo { label: "49", frequency_khz: 924_200 },
        ChannelInfo { label: "50", frequency_khz: 924_300 },
        ChannelInfo { label: "51", frequency_khz: 924_400 },
        ChannelInfo { label: "52", frequency_khz: 924_500 },
        ChannelInfo { label: "53", frequency_khz: 924_600 },
        ChannelInfo { label: "54", frequency_khz: 924_700 },
        ChannelInfo { label: "55", frequency_khz: 924_800 },
        ChannelInfo { label: "56", frequency_khz: 924_900 },
        ChannelInfo { label: "57", frequency_khz: 925_000 },
        ChannelInfo { label: "58", frequency_khz: 925_100 },
        ChannelInfo { label: "59", frequency_khz: 925_200 },
        ChannelInfo { label: "60", frequency_khz: 925_300 },
        ChannelInfo { label: "61", frequency_khz: 925_400 },
        ChannelInfo { label: "62", frequency_khz: 925_500 },
        ChannelInfo { label: "63", frequency_khz: 925_600 },
        ChannelInfo { label: "64", frequency_khz: 925_700 },
        ChannelInfo { label: "65", frequency_khz: 925_800 },
        ChannelInfo { label: "66", frequency_khz: 925_900 },
        ChannelInfo { label: "67", frequency_khz: 926_000 },
        ChannelInfo { label: "68", frequency_khz: 926_100 },
        ChannelInfo { label: "69", frequency_khz: 926_200 },
        ChannelInfo { label: "70", frequency_khz: 926_300 },
        ChannelInfo { label: "71", frequency_khz: 926_400 },
        ChannelInfo { label: "72", frequency_khz: 926_500 },
        ChannelInfo { label: "73", frequency_khz: 926_600 },
        ChannelInfo { label: "74", frequency_khz: 926_700 },
        ChannelInfo { label: "75", frequency_khz: 926_800 },
        ChannelInfo { label: "76", frequency_khz: 926_900 },
        ChannelInfo { label: "77", frequency_khz: 927_000 },
        ChannelInfo { label: "78", frequency_khz: 927_100 },
        ChannelInfo { label: "79", frequency_khz: 927_200 },
        ChannelInfo { label: "80", frequency_khz: 927_300 },
        ChannelInfo { label: "81", frequency_khz: 927_400 },
        ChannelInfo { label: "82", frequency_khz: 927_500 },
        ChannelInfo { label: "83", frequency_khz: 927_600 },
        ChannelInfo { label: "84", frequency_khz: 927_700 },
        ChannelInfo { label: "85", frequency_khz: 927_800 },
        ChannelInfo { label: "86", frequency_khz: 927_900 },
        ChannelInfo { label: "87", frequency_khz: 928_000 },
    ],
};

#[rustfmt::skip] // Channel table; one-line entries read as a frequency map.
static WIDE: BandPlanInfo = BandPlanInfo {
    label: "Wide 0.2",
    channels: &[
        ChannelInfo { label: "001", frequency_khz: 902_000 },
        ChannelInfo { label: "002", frequency_khz: 902_200 },
        ChannelInfo { label: "003", frequency_khz: 902_400 },
        ChannelInfo { label: "004", frequency_khz: 902_600 },
        ChannelInfo { label: "005", frequency_khz: 902_800 },
        ChannelInfo { label: "006", frequency_khz: 903_000 },
        ChannelInfo { label: "007", frequency_khz: 903_200 },
        ChannelInfo { label: "008", frequency_khz: 903_400 },
        ChannelInfo { label: "009", frequency_khz: 903_600 },
        ChannelInfo { label: "010", frequency_khz: 903_800 },
        ChannelInfo { label: "011", frequency_khz: 904_000 },
        ChannelInfo { label: "012", frequency_khz: 904_200 },
        ChannelInfo { label: "013", frequency_khz: 904_400 },
        ChannelInfo { label: "014", frequency_khz: 904_600 },
        ChannelInfo { label: "015", frequency_khz: 904_800 },
        ChannelInfo { label: "016", frequency_khz: 905_000 },
        ChannelInfo { label: "017", frequency_khz: 905_200 },
        ChannelInfo { label: "018", frequency_khz: 905_400 },
        ChannelInfo { label: "019", frequency_khz: 905_600 },
        ChannelInfo { label: "020", frequency_khz: 905_800 },
        ChannelInfo { label: "021", frequency_khz: 906_000 },
        ChannelInfo { label: "022", frequency_khz: 906_200 },
        ChannelInfo { label: "023", frequency_khz: 906_400 },
        ChannelInfo { label: "024", frequency_khz: 906_600 },
        ChannelInfo { label: "025", frequency_khz: 906_800 },
        ChannelInfo { label: "026", frequency_khz: 907_000 },
        ChannelInfo { label: "027", frequency_khz: 907_200 },
        ChannelInfo { label: "028", frequency_khz: 907_400 },
        ChannelInfo { label: "029", frequency_khz: 907_600 },
        ChannelInfo { label: "030", frequency_khz: 907_800 },
        ChannelInfo { label: "031", frequency_khz: 908_000 },
        ChannelInfo { label: "032", frequency_khz: 908_200 },
        ChannelInfo { label: "033", frequency_khz: 908_400 },
        ChannelInfo { label: "034", frequency_khz: 908_600 },
        ChannelInfo { label: "035", frequency_khz: 908_800 },
        ChannelInfo { label: "036", frequency_khz: 909_000 },
        ChannelInfo { label: "037", frequency_khz: 909_200 },
        ChannelInfo { label: "038", frequency_khz: 909_400 },
        ChannelInfo { label: "039", frequency_khz: 909_600 },
        ChannelInfo { label: "040", frequency_khz: 909_800 },
        ChannelInfo { label: "041", frequency_khz: 910_000 },
        ChannelInfo { label: "042", frequency_khz: 910_200 },
        ChannelInfo { label: "043", frequency_khz: 910_400 },
        ChannelInfo { label: "044", frequency_khz: 910_600 },
        ChannelInfo { label: "045", frequency_khz: 910_800 },
        ChannelInfo { label: "046", frequency_khz: 911_000 },
        ChannelInfo { label: "047", frequency_khz: 911_200 },
        ChannelInfo { label: "048", frequency_khz: 911_400 },
        ChannelInfo { label: "049", frequency_khz: 911_600 },
        ChannelInfo { label: "050", frequency_khz: 911_800 },
        ChannelInfo { label: "051", frequency_khz: 912_000 },
        ChannelInfo { label: "052", frequency_khz: 912_200 },
        ChannelInfo { label: "053", frequency_khz: 912_400 },
        ChannelInfo { label: "054", frequency_khz: 912_600 },
        ChannelInfo { label: "055", frequency_khz: 912_800 },
        ChannelInfo { label: "056", frequency_khz: 913_000 },
        ChannelInfo { label: "057", frequency_khz: 913_200 },
        ChannelInfo { label: "058", frequency_khz: 913_400 },
        ChannelInfo { label: "059", frequency_khz: 913_600 },
        ChannelInfo { label: "060", frequency_khz: 913_800 },
        ChannelInfo { label: "061", frequency_khz: 914_000 },
        ChannelInfo { label: "062", frequency_khz: 914_200 },
        ChannelInfo { label: "063", frequency_khz: 914_400 },
        ChannelInfo { label: "064", frequency_khz: 914_600 },
        ChannelInfo { label: "065", frequency_khz: 914_800 },
        ChannelInfo { label: "066", frequency_khz: 915_000 },
        ChannelInfo { label: "067", frequency_khz: 915_200 },
        ChannelInfo { label: "068", frequency_khz: 915_400 },
        ChannelInfo { label: "069", frequency_khz: 915_600 },
        ChannelInfo { label: "070", frequency_khz: 915_800 },
        ChannelInfo { label: "071", frequency_khz: 916_000 },
        ChannelInfo { label: "072", frequency_khz: 916_200 },
        ChannelInfo { label: "073", frequency_khz: 916_400 },
        ChannelInfo { label: "074", frequency_khz: 916_600 },
        ChannelInfo { label: "075", frequency_khz: 916_800 },
        ChannelInfo { label: "076", frequency_khz: 917_000 },
        ChannelInfo { label: "077", frequency_khz: 917_200 },
        ChannelInfo { label: "078", frequency_khz: 917_400 },
        ChannelInfo { label: "079", frequency_khz: 917_600 },
        ChannelInfo { label: "080", frequency_khz: 917_800 },
        ChannelInfo { label: "081", frequency_khz: 918_000 },
        ChannelInfo { label: "082", frequency_khz: 918_200 },
        ChannelInfo { label: "083", frequency_khz: 918_400 },
        ChannelInfo { label: "084", frequency_khz: 918_600 },
        ChannelInfo { label: "085", frequency_khz: 918_800 },
        ChannelInfo { label: "086", frequency_khz: 919_000 },
        ChannelInfo { label: "087", frequency_khz: 919_200 },
        ChannelInfo { label: "088", frequency_khz: 919_400 },
        ChannelInfo { label: "089", frequency_khz: 919_600 },
        ChannelInfo { label: "090", frequency_khz: 919_800 },
        ChannelInfo { label: "091", frequency_khz: 920_000 },
        ChannelInfo { label: "092", frequency_khz: 920_200 },
        ChannelInfo { label: "093", frequency_khz: 920_400 },
        ChannelInfo { label: "094", frequency_khz: 920_600 },
        ChannelInfo { label: "095", frequency_khz: 920_800 },
        ChannelInfo { label: "096", frequency_khz: 921_000 },
        ChannelInfo { label: "097", frequency_khz: 921_200 },
        ChannelInfo { label: "098", frequency_khz: 921_400 },
        ChannelInfo { label: "099", frequency_khz: 921_600 },
        ChannelInfo { label: "100", frequency_khz: 921_800 },
        ChannelInfo { label: "101", frequency_khz: 922_000 },
        ChannelInfo { label: "102", frequency_khz: 922_200 },
        ChannelInfo { label: "103", frequency_khz: 922_400 },
        ChannelInfo { label: "104", frequency_khz: 922_600 },
        ChannelInfo { label: "105", frequency_khz: 922_800 },
        ChannelInfo { label: "106", frequency_khz: 923_000 },
        ChannelInfo { label: "107", frequency_khz: 923_200 },
        ChannelInfo { label: "108", frequency_khz: 923_400 },
        ChannelInfo { label: "109", frequency_khz: 923_600 },
        ChannelInfo { label: "110", frequency_khz: 923_800 },
        ChannelInfo { label: "111", frequency_khz: 924_000 },
        ChannelInfo { label: "112", frequency_khz: 924_200 },
        ChannelInfo { label: "113", frequency_khz: 924_400 },
        ChannelInfo { label: "114", frequency_khz: 924_600 },
        ChannelInfo { label: "115", frequency_khz: 924_800 },
        ChannelInfo { label: "116", frequency_khz: 925_000 },
        ChannelInfo { label: "117", frequency_khz: 925_200 },
        ChannelInfo { label: "118", frequency_khz: 925_400 },
        ChannelInfo { label: "119", frequency_khz: 925_600 },
        ChannelInfo { label: "120", frequency_khz: 925_800 },
        ChannelInfo { label: "121", frequency_khz: 926_000 },
        ChannelInfo { label: "122", frequency_khz: 926_200 },
        ChannelInfo { label: "123", frequency_khz: 926_400 },
        ChannelInfo { label: "124", frequency_khz: 926_600 },
        ChannelInfo { label: "125", frequency_khz: 926_800 },
        ChannelInfo { label: "126", frequency_khz: 927_000 },
        ChannelInfo { label: "127", frequency_khz: 927_200 },
        ChannelInfo { label: "128", frequency_khz: 927_400 },
        ChannelInfo { label: "129", frequency_khz: 927_600 },
        ChannelInfo { label: "130", frequency_khz: 927_800 },
        ChannelInfo { label: "131", frequency_khz: 928_000 },
    ],
};

/// 470–510 MHz plan for the SX1268 hardware variant.  39 channels at
/// 1 MHz spacing, 471–509 MHz (1 MHz margin at each band edge), same
/// even-spacing rationale as [`ISM_915`].
#[rustfmt::skip] // Channel table; one-line entries read as a frequency map.
static BAND_470: BandPlanInfo = BandPlanInfo {
    label: "470-510",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 471_000 },
        ChannelInfo { label: "02", frequency_khz: 472_000 },
        ChannelInfo { label: "03", frequency_khz: 473_000 },
        ChannelInfo { label: "04", frequency_khz: 474_000 },
        ChannelInfo { label: "05", frequency_khz: 475_000 },
        ChannelInfo { label: "06", frequency_khz: 476_000 },
        ChannelInfo { label: "07", frequency_khz: 477_000 },
        ChannelInfo { label: "08", frequency_khz: 478_000 },
        ChannelInfo { label: "09", frequency_khz: 479_000 },
        ChannelInfo { label: "10", frequency_khz: 480_000 },
        ChannelInfo { label: "11", frequency_khz: 481_000 },
        ChannelInfo { label: "12", frequency_khz: 482_000 },
        ChannelInfo { label: "13", frequency_khz: 483_000 },
        ChannelInfo { label: "14", frequency_khz: 484_000 },
        ChannelInfo { label: "15", frequency_khz: 485_000 },
        ChannelInfo { label: "16", frequency_khz: 486_000 },
        ChannelInfo { label: "17", frequency_khz: 487_000 },
        ChannelInfo { label: "18", frequency_khz: 488_000 },
        ChannelInfo { label: "19", frequency_khz: 489_000 },
        ChannelInfo { label: "20", frequency_khz: 490_000 },
        ChannelInfo { label: "21", frequency_khz: 491_000 },
        ChannelInfo { label: "22", frequency_khz: 492_000 },
        ChannelInfo { label: "23", frequency_khz: 493_000 },
        ChannelInfo { label: "24", frequency_khz: 494_000 },
        ChannelInfo { label: "25", frequency_khz: 495_000 },
        ChannelInfo { label: "26", frequency_khz: 496_000 },
        ChannelInfo { label: "27", frequency_khz: 497_000 },
        ChannelInfo { label: "28", frequency_khz: 498_000 },
        ChannelInfo { label: "29", frequency_khz: 499_000 },
        ChannelInfo { label: "30", frequency_khz: 500_000 },
        ChannelInfo { label: "31", frequency_khz: 501_000 },
        ChannelInfo { label: "32", frequency_khz: 502_000 },
        ChannelInfo { label: "33", frequency_khz: 503_000 },
        ChannelInfo { label: "34", frequency_khz: 504_000 },
        ChannelInfo { label: "35", frequency_khz: 505_000 },
        ChannelInfo { label: "36", frequency_khz: 506_000 },
        ChannelInfo { label: "37", frequency_khz: 507_000 },
        ChannelInfo { label: "38", frequency_khz: 508_000 },
        ChannelInfo { label: "39", frequency_khz: 509_000 },
    ],
};

/// 470 MHz coexistence plan aligned to Sennheiser ew G4 band A1
/// (470–516 MHz).  8 channels evenly spaced within our 470–510 hardware
/// range (471–506 MHz, 5 MHz apart).  See [`BandPlan::SennheiserA1`] for
/// the stub caveat (not Sennheiser's coordinated bank frequencies).
#[rustfmt::skip] // Channel table; one-line entries read as a frequency map.
static BAND_470_SENNHEISER_A1: BandPlanInfo = BandPlanInfo {
    label: "Senn A1",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 471_000 },
        ChannelInfo { label: "02", frequency_khz: 476_000 },
        ChannelInfo { label: "03", frequency_khz: 481_000 },
        ChannelInfo { label: "04", frequency_khz: 486_000 },
        ChannelInfo { label: "05", frequency_khz: 491_000 },
        ChannelInfo { label: "06", frequency_khz: 496_000 },
        ChannelInfo { label: "07", frequency_khz: 501_000 },
        ChannelInfo { label: "08", frequency_khz: 506_000 },
    ],
};

/// Shure SLX-D G58 Group 1 (Full Range 1).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (26 of 31).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G1: BandPlanInfo = BandPlanInfo {
    label: "G58 Full1",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 470_525 },
        ChannelInfo { label: "02", frequency_khz: 472_450 },
        ChannelInfo { label: "03", frequency_khz: 473_400 },
        ChannelInfo { label: "04", frequency_khz: 474_600 },
        ChannelInfo { label: "05", frequency_khz: 475_125 },
        ChannelInfo { label: "06", frequency_khz: 476_075 },
        ChannelInfo { label: "07", frequency_khz: 477_350 },
        ChannelInfo { label: "08", frequency_khz: 478_325 },
        ChannelInfo { label: "09", frequency_khz: 478_975 },
        ChannelInfo { label: "10", frequency_khz: 480_325 },
        ChannelInfo { label: "11", frequency_khz: 480_875 },
        ChannelInfo { label: "12", frequency_khz: 483_025 },
        ChannelInfo { label: "13", frequency_khz: 483_725 },
        ChannelInfo { label: "14", frequency_khz: 484_925 },
        ChannelInfo { label: "15", frequency_khz: 492_000 },
        ChannelInfo { label: "16", frequency_khz: 493_300 },
        ChannelInfo { label: "17", frequency_khz: 495_825 },
        ChannelInfo { label: "18", frequency_khz: 497_850 },
        ChannelInfo { label: "19", frequency_khz: 498_875 },
        ChannelInfo { label: "20", frequency_khz: 500_650 },
        ChannelInfo { label: "21", frequency_khz: 501_350 },
        ChannelInfo { label: "22", frequency_khz: 504_150 },
        ChannelInfo { label: "23", frequency_khz: 504_650 },
        ChannelInfo { label: "24", frequency_khz: 506_075 },
        ChannelInfo { label: "25", frequency_khz: 507_300 },
        ChannelInfo { label: "26", frequency_khz: 509_625 },
    ],
};

/// Shure SLX-D G58 Group 2 (Full Range 2).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (25 of 30).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G2: BandPlanInfo = BandPlanInfo {
    label: "G58 Full2",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 470_350 },
        ChannelInfo { label: "02", frequency_khz: 470_925 },
        ChannelInfo { label: "03", frequency_khz: 471_750 },
        ChannelInfo { label: "04", frequency_khz: 472_200 },
        ChannelInfo { label: "05", frequency_khz: 472_850 },
        ChannelInfo { label: "06", frequency_khz: 473_750 },
        ChannelInfo { label: "07", frequency_khz: 474_375 },
        ChannelInfo { label: "08", frequency_khz: 475_550 },
        ChannelInfo { label: "09", frequency_khz: 476_250 },
        ChannelInfo { label: "10", frequency_khz: 479_125 },
        ChannelInfo { label: "11", frequency_khz: 483_600 },
        ChannelInfo { label: "12", frequency_khz: 484_825 },
        ChannelInfo { label: "13", frequency_khz: 485_725 },
        ChannelInfo { label: "14", frequency_khz: 486_250 },
        ChannelInfo { label: "15", frequency_khz: 486_975 },
        ChannelInfo { label: "16", frequency_khz: 488_675 },
        ChannelInfo { label: "17", frequency_khz: 489_925 },
        ChannelInfo { label: "18", frequency_khz: 494_600 },
        ChannelInfo { label: "19", frequency_khz: 495_700 },
        ChannelInfo { label: "20", frequency_khz: 499_925 },
        ChannelInfo { label: "21", frequency_khz: 502_450 },
        ChannelInfo { label: "22", frequency_khz: 503_800 },
        ChannelInfo { label: "23", frequency_khz: 505_900 },
        ChannelInfo { label: "24", frequency_khz: 506_775 },
        ChannelInfo { label: "25", frequency_khz: 507_375 },
    ],
};

/// Shure SLX-D G58 Group 3 (Full Range 3).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (27 of 30).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G3: BandPlanInfo = BandPlanInfo {
    label: "G58 Full3",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 470_600 },
        ChannelInfo { label: "02", frequency_khz: 471_550 },
        ChannelInfo { label: "03", frequency_khz: 472_875 },
        ChannelInfo { label: "04", frequency_khz: 473_525 },
        ChannelInfo { label: "05", frequency_khz: 474_575 },
        ChannelInfo { label: "06", frequency_khz: 476_000 },
        ChannelInfo { label: "07", frequency_khz: 478_250 },
        ChannelInfo { label: "08", frequency_khz: 478_900 },
        ChannelInfo { label: "09", frequency_khz: 479_800 },
        ChannelInfo { label: "10", frequency_khz: 480_250 },
        ChannelInfo { label: "11", frequency_khz: 481_025 },
        ChannelInfo { label: "12", frequency_khz: 482_600 },
        ChannelInfo { label: "13", frequency_khz: 485_650 },
        ChannelInfo { label: "14", frequency_khz: 487_850 },
        ChannelInfo { label: "15", frequency_khz: 489_400 },
        ChannelInfo { label: "16", frequency_khz: 493_375 },
        ChannelInfo { label: "17", frequency_khz: 497_125 },
        ChannelInfo { label: "18", frequency_khz: 497_975 },
        ChannelInfo { label: "19", frequency_khz: 499_300 },
        ChannelInfo { label: "20", frequency_khz: 500_100 },
        ChannelInfo { label: "21", frequency_khz: 501_875 },
        ChannelInfo { label: "22", frequency_khz: 503_275 },
        ChannelInfo { label: "23", frequency_khz: 505_500 },
        ChannelInfo { label: "24", frequency_khz: 506_200 },
        ChannelInfo { label: "25", frequency_khz: 507_450 },
        ChannelInfo { label: "26", frequency_khz: 508_925 },
        ChannelInfo { label: "27", frequency_khz: 509_650 },
    ],
};

/// Shure SLX-D G58 Group 4 (Multi-band G4 (compat w/ group 4 of H55 & J52)).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (24 of 29).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G4: BandPlanInfo = BandPlanInfo {
    label: "G58 Multi4",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 470_725 },
        ChannelInfo { label: "02", frequency_khz: 471_250 },
        ChannelInfo { label: "03", frequency_khz: 472_275 },
        ChannelInfo { label: "04", frequency_khz: 472_725 },
        ChannelInfo { label: "05", frequency_khz: 473_625 },
        ChannelInfo { label: "06", frequency_khz: 475_250 },
        ChannelInfo { label: "07", frequency_khz: 476_200 },
        ChannelInfo { label: "08", frequency_khz: 479_450 },
        ChannelInfo { label: "09", frequency_khz: 480_950 },
        ChannelInfo { label: "10", frequency_khz: 483_350 },
        ChannelInfo { label: "11", frequency_khz: 484_400 },
        ChannelInfo { label: "12", frequency_khz: 484_950 },
        ChannelInfo { label: "13", frequency_khz: 486_925 },
        ChannelInfo { label: "14", frequency_khz: 488_700 },
        ChannelInfo { label: "15", frequency_khz: 490_000 },
        ChannelInfo { label: "16", frequency_khz: 498_350 },
        ChannelInfo { label: "17", frequency_khz: 499_850 },
        ChannelInfo { label: "18", frequency_khz: 502_400 },
        ChannelInfo { label: "19", frequency_khz: 503_325 },
        ChannelInfo { label: "20", frequency_khz: 504_000 },
        ChannelInfo { label: "21", frequency_khz: 505_375 },
        ChannelInfo { label: "22", frequency_khz: 505_825 },
        ChannelInfo { label: "23", frequency_khz: 508_550 },
        ChannelInfo { label: "24", frequency_khz: 509_050 },
    ],
};

/// Shure SLX-D G58 Group 5 (Multi-band G5 (group 5 of H55 & J52)).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (27 of 30).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G5: BandPlanInfo = BandPlanInfo {
    label: "G58 Multi5",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 470_400 },
        ChannelInfo { label: "02", frequency_khz: 471_500 },
        ChannelInfo { label: "03", frequency_khz: 472_325 },
        ChannelInfo { label: "04", frequency_khz: 475_425 },
        ChannelInfo { label: "05", frequency_khz: 476_025 },
        ChannelInfo { label: "06", frequency_khz: 477_725 },
        ChannelInfo { label: "07", frequency_khz: 478_225 },
        ChannelInfo { label: "08", frequency_khz: 480_225 },
        ChannelInfo { label: "09", frequency_khz: 483_450 },
        ChannelInfo { label: "10", frequency_khz: 484_700 },
        ChannelInfo { label: "11", frequency_khz: 485_325 },
        ChannelInfo { label: "12", frequency_khz: 486_450 },
        ChannelInfo { label: "13", frequency_khz: 487_000 },
        ChannelInfo { label: "14", frequency_khz: 487_900 },
        ChannelInfo { label: "15", frequency_khz: 489_750 },
        ChannelInfo { label: "16", frequency_khz: 492_000 },
        ChannelInfo { label: "17", frequency_khz: 499_500 },
        ChannelInfo { label: "18", frequency_khz: 500_000 },
        ChannelInfo { label: "19", frequency_khz: 501_975 },
        ChannelInfo { label: "20", frequency_khz: 502_700 },
        ChannelInfo { label: "21", frequency_khz: 503_200 },
        ChannelInfo { label: "22", frequency_khz: 505_050 },
        ChannelInfo { label: "23", frequency_khz: 506_600 },
        ChannelInfo { label: "24", frequency_khz: 507_600 },
        ChannelInfo { label: "25", frequency_khz: 508_350 },
        ChannelInfo { label: "26", frequency_khz: 508_900 },
        ChannelInfo { label: "27", frequency_khz: 509_750 },
    ],
};

/// Shure SLX-D G58 Group 6 (Multi-band G6 (group 6 of H55 & J52)).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (26 of 30).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G6: BandPlanInfo = BandPlanInfo {
    label: "G58 Multi6",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 470_900 },
        ChannelInfo { label: "02", frequency_khz: 472_075 },
        ChannelInfo { label: "03", frequency_khz: 472_675 },
        ChannelInfo { label: "04", frequency_khz: 473_550 },
        ChannelInfo { label: "05", frequency_khz: 474_700 },
        ChannelInfo { label: "06", frequency_khz: 475_550 },
        ChannelInfo { label: "07", frequency_khz: 477_100 },
        ChannelInfo { label: "08", frequency_khz: 479_700 },
        ChannelInfo { label: "09", frequency_khz: 481_225 },
        ChannelInfo { label: "10", frequency_khz: 481_825 },
        ChannelInfo { label: "11", frequency_khz: 483_575 },
        ChannelInfo { label: "12", frequency_khz: 484_350 },
        ChannelInfo { label: "13", frequency_khz: 485_550 },
        ChannelInfo { label: "14", frequency_khz: 486_225 },
        ChannelInfo { label: "15", frequency_khz: 489_475 },
        ChannelInfo { label: "16", frequency_khz: 492_950 },
        ChannelInfo { label: "17", frequency_khz: 494_825 },
        ChannelInfo { label: "18", frequency_khz: 501_275 },
        ChannelInfo { label: "19", frequency_khz: 502_575 },
        ChannelInfo { label: "20", frequency_khz: 503_025 },
        ChannelInfo { label: "21", frequency_khz: 503_675 },
        ChannelInfo { label: "22", frequency_khz: 505_025 },
        ChannelInfo { label: "23", frequency_khz: 505_675 },
        ChannelInfo { label: "24", frequency_khz: 506_650 },
        ChannelInfo { label: "25", frequency_khz: 507_225 },
        ChannelInfo { label: "26", frequency_khz: 509_000 },
    ],
};

/// Shure SLX-D G58 Group 7 (TV14).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (10 of 10).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G7: BandPlanInfo = BandPlanInfo {
    label: "G58 TV14",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 470_175 },
        ChannelInfo { label: "02", frequency_khz: 470_625 },
        ChannelInfo { label: "03", frequency_khz: 471_475 },
        ChannelInfo { label: "04", frequency_khz: 471_925 },
        ChannelInfo { label: "05", frequency_khz: 472_575 },
        ChannelInfo { label: "06", frequency_khz: 473_475 },
        ChannelInfo { label: "07", frequency_khz: 474_000 },
        ChannelInfo { label: "08", frequency_khz: 474_775 },
        ChannelInfo { label: "09", frequency_khz: 475_225 },
        ChannelInfo { label: "10", frequency_khz: 475_875 },
    ],
};

/// Shure SLX-D G58 Group 8 (TV15).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (10 of 10).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G8: BandPlanInfo = BandPlanInfo {
    label: "G58 TV15",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 476_150 },
        ChannelInfo { label: "02", frequency_khz: 476_600 },
        ChannelInfo { label: "03", frequency_khz: 477_475 },
        ChannelInfo { label: "04", frequency_khz: 478_125 },
        ChannelInfo { label: "05", frequency_khz: 478_575 },
        ChannelInfo { label: "06", frequency_khz: 479_350 },
        ChannelInfo { label: "07", frequency_khz: 479_875 },
        ChannelInfo { label: "08", frequency_khz: 480_775 },
        ChannelInfo { label: "09", frequency_khz: 481_425 },
        ChannelInfo { label: "10", frequency_khz: 481_875 },
    ],
};

/// Shure SLX-D G58 Group 9 (TV16).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (10 of 10).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G9: BandPlanInfo = BandPlanInfo {
    label: "G58 TV16",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 482_125 },
        ChannelInfo { label: "02", frequency_khz: 482_575 },
        ChannelInfo { label: "03", frequency_khz: 483_450 },
        ChannelInfo { label: "04", frequency_khz: 484_125 },
        ChannelInfo { label: "05", frequency_khz: 484_575 },
        ChannelInfo { label: "06", frequency_khz: 485_350 },
        ChannelInfo { label: "07", frequency_khz: 485_900 },
        ChannelInfo { label: "08", frequency_khz: 486_800 },
        ChannelInfo { label: "09", frequency_khz: 487_450 },
        ChannelInfo { label: "10", frequency_khz: 487_900 },
    ],
};

/// Shure SLX-D G58 Group 10 (TV17).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (10 of 10).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G10: BandPlanInfo = BandPlanInfo {
    label: "G58 TV17",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 488_125 },
        ChannelInfo { label: "02", frequency_khz: 488_675 },
        ChannelInfo { label: "03", frequency_khz: 489_450 },
        ChannelInfo { label: "04", frequency_khz: 489_900 },
        ChannelInfo { label: "05", frequency_khz: 490_550 },
        ChannelInfo { label: "06", frequency_khz: 491_450 },
        ChannelInfo { label: "07", frequency_khz: 492_000 },
        ChannelInfo { label: "08", frequency_khz: 492_775 },
        ChannelInfo { label: "09", frequency_khz: 493_225 },
        ChannelInfo { label: "10", frequency_khz: 493_875 },
    ],
};

/// Shure SLX-D G58 Group 11 (TV18).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (9 of 9).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G11: BandPlanInfo = BandPlanInfo {
    label: "G58 TV18",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 494_250 },
        ChannelInfo { label: "02", frequency_khz: 494_900 },
        ChannelInfo { label: "03", frequency_khz: 495_350 },
        ChannelInfo { label: "04", frequency_khz: 496_000 },
        ChannelInfo { label: "05", frequency_khz: 496_850 },
        ChannelInfo { label: "06", frequency_khz: 497_375 },
        ChannelInfo { label: "07", frequency_khz: 498_125 },
        ChannelInfo { label: "08", frequency_khz: 499_075 },
        ChannelInfo { label: "09", frequency_khz: 499_650 },
    ],
};

/// Shure SLX-D G58 Group 12 (TV19).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (10 of 10).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G12: BandPlanInfo = BandPlanInfo {
    label: "G58 TV19",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 500_125 },
        ChannelInfo { label: "02", frequency_khz: 500_575 },
        ChannelInfo { label: "03", frequency_khz: 501_225 },
        ChannelInfo { label: "04", frequency_khz: 502_125 },
        ChannelInfo { label: "05", frequency_khz: 502_675 },
        ChannelInfo { label: "06", frequency_khz: 503_450 },
        ChannelInfo { label: "07", frequency_khz: 503_900 },
        ChannelInfo { label: "08", frequency_khz: 504_550 },
        ChannelInfo { label: "09", frequency_khz: 505_425 },
        ChannelInfo { label: "10", frequency_khz: 505_875 },
    ],
};

/// Shure SLX-D G58 Group 13 (TV20).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (7 of 10).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G13: BandPlanInfo = BandPlanInfo {
    label: "G58 TV20",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 506_150 },
        ChannelInfo { label: "02", frequency_khz: 506_600 },
        ChannelInfo { label: "03", frequency_khz: 507_250 },
        ChannelInfo { label: "04", frequency_khz: 508_150 },
        ChannelInfo { label: "05", frequency_khz: 508_675 },
        ChannelInfo { label: "06", frequency_khz: 509_450 },
        ChannelInfo { label: "07", frequency_khz: 509_900 },
    ],
};

/// Shure SLX-D G58 Group 15 (TV14 & TV16).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (18 of 18).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G15: BandPlanInfo = BandPlanInfo {
    label: "G58 14&16",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 470_125 },
        ChannelInfo { label: "02", frequency_khz: 470_825 },
        ChannelInfo { label: "03", frequency_khz: 471_750 },
        ChannelInfo { label: "04", frequency_khz: 472_325 },
        ChannelInfo { label: "05", frequency_khz: 473_175 },
        ChannelInfo { label: "06", frequency_khz: 473_625 },
        ChannelInfo { label: "07", frequency_khz: 474_300 },
        ChannelInfo { label: "08", frequency_khz: 475_200 },
        ChannelInfo { label: "09", frequency_khz: 475_725 },
        ChannelInfo { label: "10", frequency_khz: 482_350 },
        ChannelInfo { label: "11", frequency_khz: 483_300 },
        ChannelInfo { label: "12", frequency_khz: 484_025 },
        ChannelInfo { label: "13", frequency_khz: 484_550 },
        ChannelInfo { label: "14", frequency_khz: 485_450 },
        ChannelInfo { label: "15", frequency_khz: 486_100 },
        ChannelInfo { label: "16", frequency_khz: 486_550 },
        ChannelInfo { label: "17", frequency_khz: 487_350 },
        ChannelInfo { label: "18", frequency_khz: 487_850 },
    ],
};

/// Shure SLX-D G58 Group 16 (TV15 & TV17).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (18 of 18).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G16: BandPlanInfo = BandPlanInfo {
    label: "G58 15&17",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 476_550 },
        ChannelInfo { label: "02", frequency_khz: 477_050 },
        ChannelInfo { label: "03", frequency_khz: 478_050 },
        ChannelInfo { label: "04", frequency_khz: 478_800 },
        ChannelInfo { label: "05", frequency_khz: 479_250 },
        ChannelInfo { label: "06", frequency_khz: 480_125 },
        ChannelInfo { label: "07", frequency_khz: 480_800 },
        ChannelInfo { label: "08", frequency_khz: 481_250 },
        ChannelInfo { label: "09", frequency_khz: 488_175 },
        ChannelInfo { label: "10", frequency_khz: 488_725 },
        ChannelInfo { label: "11", frequency_khz: 489_475 },
        ChannelInfo { label: "12", frequency_khz: 489_925 },
        ChannelInfo { label: "13", frequency_khz: 490_575 },
        ChannelInfo { label: "14", frequency_khz: 491_475 },
        ChannelInfo { label: "15", frequency_khz: 492_000 },
        ChannelInfo { label: "16", frequency_khz: 492_775 },
        ChannelInfo { label: "17", frequency_khz: 493_225 },
        ChannelInfo { label: "18", frequency_khz: 493_875 },
    ],
};

/// Shure SLX-D G58 Group 17 (TV16 & TV18).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (18 of 18).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G17: BandPlanInfo = BandPlanInfo {
    label: "G58 16&18",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 482_150 },
        ChannelInfo { label: "02", frequency_khz: 482_600 },
        ChannelInfo { label: "03", frequency_khz: 483_275 },
        ChannelInfo { label: "04", frequency_khz: 484_150 },
        ChannelInfo { label: "05", frequency_khz: 484_625 },
        ChannelInfo { label: "06", frequency_khz: 485_375 },
        ChannelInfo { label: "07", frequency_khz: 486_400 },
        ChannelInfo { label: "08", frequency_khz: 486_850 },
        ChannelInfo { label: "09", frequency_khz: 487_800 },
        ChannelInfo { label: "10", frequency_khz: 494_250 },
        ChannelInfo { label: "11", frequency_khz: 494_900 },
        ChannelInfo { label: "12", frequency_khz: 495_350 },
        ChannelInfo { label: "13", frequency_khz: 496_000 },
        ChannelInfo { label: "14", frequency_khz: 496_850 },
        ChannelInfo { label: "15", frequency_khz: 497_375 },
        ChannelInfo { label: "16", frequency_khz: 498_125 },
        ChannelInfo { label: "17", frequency_khz: 499_075 },
        ChannelInfo { label: "18", frequency_khz: 499_650 },
    ],
};

/// Shure SLX-D G58 Group 18 (TV17 & TV19).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (18 of 18).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G18: BandPlanInfo = BandPlanInfo {
    label: "G58 17&19",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 488_125 },
        ChannelInfo { label: "02", frequency_khz: 488_800 },
        ChannelInfo { label: "03", frequency_khz: 489_250 },
        ChannelInfo { label: "04", frequency_khz: 489_900 },
        ChannelInfo { label: "05", frequency_khz: 491_450 },
        ChannelInfo { label: "06", frequency_khz: 492_775 },
        ChannelInfo { label: "07", frequency_khz: 493_225 },
        ChannelInfo { label: "08", frequency_khz: 493_875 },
        ChannelInfo { label: "09", frequency_khz: 500_125 },
        ChannelInfo { label: "10", frequency_khz: 500_575 },
        ChannelInfo { label: "11", frequency_khz: 501_225 },
        ChannelInfo { label: "12", frequency_khz: 502_125 },
        ChannelInfo { label: "13", frequency_khz: 502_675 },
        ChannelInfo { label: "14", frequency_khz: 503_450 },
        ChannelInfo { label: "15", frequency_khz: 503_900 },
        ChannelInfo { label: "16", frequency_khz: 504_550 },
        ChannelInfo { label: "17", frequency_khz: 505_425 },
        ChannelInfo { label: "18", frequency_khz: 505_875 },
    ],
};

/// Shure SLX-D G58 Group 19 (TV18 & TV20).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (15 of 18).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G19: BandPlanInfo = BandPlanInfo {
    label: "G58 18&20",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 494_250 },
        ChannelInfo { label: "02", frequency_khz: 494_750 },
        ChannelInfo { label: "03", frequency_khz: 495_650 },
        ChannelInfo { label: "04", frequency_khz: 496_100 },
        ChannelInfo { label: "05", frequency_khz: 496_825 },
        ChannelInfo { label: "06", frequency_khz: 498_250 },
        ChannelInfo { label: "07", frequency_khz: 498_700 },
        ChannelInfo { label: "08", frequency_khz: 499_875 },
        ChannelInfo { label: "09", frequency_khz: 506_125 },
        ChannelInfo { label: "10", frequency_khz: 506_575 },
        ChannelInfo { label: "11", frequency_khz: 507_425 },
        ChannelInfo { label: "12", frequency_khz: 508_075 },
        ChannelInfo { label: "13", frequency_khz: 508_525 },
        ChannelInfo { label: "14", frequency_khz: 509_275 },
        ChannelInfo { label: "15", frequency_khz: 509_825 },
    ],
};

/// Shure SLX-D G58 Group 20 (TV19 & TV21).  Source: Shure SLX-D Frequency
/// Compatibility Supplement (© 2020).  Channels >510 MHz omitted (10 of 14).
#[rustfmt::skip]
static BAND_470_SHURE_G58_G20: BandPlanInfo = BandPlanInfo {
    label: "G58 19&21",
    channels: &[
        ChannelInfo { label: "01", frequency_khz: 500_125 },
        ChannelInfo { label: "02", frequency_khz: 500_625 },
        ChannelInfo { label: "03", frequency_khz: 501_450 },
        ChannelInfo { label: "04", frequency_khz: 501_900 },
        ChannelInfo { label: "05", frequency_khz: 502_550 },
        ChannelInfo { label: "06", frequency_khz: 503_450 },
        ChannelInfo { label: "07", frequency_khz: 504_000 },
        ChannelInfo { label: "08", frequency_khz: 504_775 },
        ChannelInfo { label: "09", frequency_khz: 505_225 },
        ChannelInfo { label: "10", frequency_khz: 505_875 },
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
            assert!(
                !p.info().channels.is_empty(),
                "plan {:?} has no channels",
                p
            );
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
    fn frequencies_in_expected_band() {
        // Each plan's channels must sit inside the RF band it targets:
        // the 470 plan in 470–510 MHz, everything else in 902–928 US ISM.
        for &p in BAND_PLANS {
            for c in p.info().channels {
                // Every channel must sit in a band we support: 470–510 MHz
                // (SX1268) or 902–928 MHz US ISM (SX1262).
                let in_470 = (470_000..=510_000).contains(&c.frequency_khz);
                let in_ism = (902_000..=928_000).contains(&c.frequency_khz);
                assert!(
                    in_470 || in_ism,
                    "channel {:?} of plan {:?} at {} kHz is outside every supported band",
                    c.label,
                    p,
                    c.frequency_khz
                );
            }
        }
    }

    #[test]
    fn format_frequency_renders_three_decimals() {
        let c = ChannelInfo {
            label: "test",
            frequency_khz: 915_000,
        };
        assert_eq!(c.format_frequency().as_str(), "915.000 MHz");
        let c = ChannelInfo {
            label: "test",
            frequency_khz: 916_125,
        };
        assert_eq!(c.format_frequency().as_str(), "916.125 MHz");
    }

    #[test]
    fn channel_lookup_clamps_overflow() {
        let c = channel(BandPlan::Ism915, 99);
        assert_eq!(c.label, "24"); // last channel in ISM_915
    }
}
