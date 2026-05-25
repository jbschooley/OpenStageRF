// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

//! Link-layer bench, board-agnostic.
//!
//! Thin wrapper over [`osrf_link_runtime`] that supplies a synthetic
//! MIDI source / sink for hardware verification of the link layer
//! end-to-end without real-MIDI hardware.  All the runtime logic
//! (radio configuration, TX queue + heartbeat loop, RX dedup +
//! watchdog + stuck-note recovery) lives in `osrf-link-runtime`;
//! this crate just exposes the synthetic [`synthetic::ScenarioSource`]
//! and [`synthetic::DefmtLogSink`] for the link-bench profile binaries.
//!
//! Production firmware (`osrf-app-midi-node`) uses the same runtime
//! crate but with UART-backed source/sink that reads the FeatherWing's
//! DIN MIDI in/out instead.

pub mod synthetic;

// Re-export the runtime's full public API so existing profile binaries
// (`profiles/t114_link_{rx,tx}` etc.) continue to compile against
// `osrf_app_link_bench::*`.
pub use osrf_link_runtime::{
    configure_radio, run_rx, run_rx_diversity, run_tx, AeadConfig, CipherId, Direction, LinkConfig,
    LinkConfigSignal, LinkStats, LinkStatsCell, MidiSink, MidiSource, ScanController,
    RF_PAYLOAD_MAX, SCAN_MAX_CHANNELS, SCAN_RSSI_NONE,
};

/// Compatibility alias — the config struct used to be called
/// `LinkBenchConfig` when this crate owned the runtime.  Kept here so
/// existing profile mains don't need to be touched as part of the
/// runtime extraction.  New code should use [`LinkConfig`] directly.
pub type LinkBenchConfig = LinkConfig;

/// Stage-3 testability stub: a hardcoded `AeadConfig` that paired
/// `t114_link_tx` / `t114_link_rx` profiles can opt into to exercise
/// the encrypt → decrypt path on real hardware.
///
/// **Not for production.**  The key is a fixed `[0x42; 32]` byte
/// pattern visible in the firmware binary.  The actual UI add-key
/// flow + flash persistence (tasks #17 / #18 in the Stage 3 plan)
/// supersede this once they land.
///
/// To enable encryption on a link-bench profile, pass
/// `Some(test_aead_chacha())` (or `test_aead_aes()`) as the final
/// `aead` argument to [`run_tx`] / [`run_rx`].  Both ends MUST use
/// the same helper — otherwise their fingerprints differ and every
/// packet drops at the `key_fp` check.
pub fn test_aead_chacha() -> AeadConfig {
    AeadConfig {
        cipher: CipherId::ChaCha20Poly1305,
        key: [0x42; 32],
        // Hardcoded device_id so paired units agree without needing
        // to exchange their FICR.DEVICEID values out-of-band.  When
        // multi-TX deployments land, this becomes
        // `board::device_id::device_id()` and an RX-side allowlist.
        device_id: 0x0000_0001,
        direction: Direction::TxToRx,
    }
}

/// AES-128-CCM variant of [`test_aead_chacha`].  Same caveats; flip
/// between the two by changing which helper the profile calls.
pub fn test_aead_aes() -> AeadConfig {
    AeadConfig {
        cipher: CipherId::Aes128Ccm,
        ..test_aead_chacha()
    }
}
