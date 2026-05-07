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
    configure_radio, run_rx, run_tx, LinkConfig, MidiSink, MidiSource, RF_PAYLOAD_MAX,
};

/// Compatibility alias — the config struct used to be called
/// `LinkBenchConfig` when this crate owned the runtime.  Kept here so
/// existing profile mains don't need to be touched as part of the
/// runtime extraction.  New code should use [`LinkConfig`] directly.
pub type LinkBenchConfig = LinkConfig;
