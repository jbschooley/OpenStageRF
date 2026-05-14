// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![allow(async_fn_in_trait)]

//! Milestone 4 integration target — board-agnostic MIDI node over the
//! wireless link.
//!
//! This crate plugs UART-backed MIDI I/O into [`osrf_link_runtime`].
//! The link runtime itself (TX queue + heartbeat + watchdog +
//! stuck-note recovery + retransmits) is shared with `osrf-app-link-bench`;
//! the only difference here is that the source reads real MIDI bytes
//! from a `BufferedUarte` (FeatherWing DIN IN), parses them via
//! [`osrf_midi_din::MidiParser`], and re-encodes complete channel-voice
//! events as wire MIDI for [`run_tx`]; and the sink writes raw MIDI
//! bytes from [`run_rx`] back out to a different `BufferedUarte`
//! (FeatherWing DIN OUT) for the synth.
//!
//! Each board is one direction.  The keyboard-side board flashes
//! [`run_tx`] (its UART is read-only); the synth-side board flashes
//! [`run_rx`] (its UART is write-only).
//!
//! SysEx is intentionally not handled in v1.  The MIDI parser still
//! tracks SysEx state correctly so it doesn't get confused, but
//! complete SysEx bodies are silently dropped instead of being
//! framed as fragments through `MidiTxQueue::push_sysex`.  Adding it
//! later is a contained change to [`uart::UartMidiSource::wait_ready`].

pub mod uart;

// Re-export the runtime's public API so profile binaries can drive
// `run_tx` / `run_rx` and reference [`LinkConfig`] without depending
// on `osrf-link-runtime` directly.
pub use osrf_link_runtime::{
    aead_fp, configure_radio, fp_to_bytes, osrf_crypto, run_rx, run_tx, AeadConfig, AeadUpdate,
    AeadUpdateSignal, CipherId, Direction, KeyFp, LinkConfig, LinkConfigSignal, LinkStats,
    LinkStatsCell, ScanController, MAX_RX_KEYS, RF_PAYLOAD_MAX, SCAN_MAX_CHANNELS, SCAN_RSSI_NONE,
};

pub use uart::{UartMidiSink, UartMidiSource};
