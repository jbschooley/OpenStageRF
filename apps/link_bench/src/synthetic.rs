// SPDX-License-Identifier: AGPL-3.0-or-later

//! Synthetic [`crate::MidiSource`] / [`crate::MidiSink`] impls for the
//! link bench when no real MIDI hardware is attached.
//!
//! TX: emits a hard-coded C-major chord at startup (three NoteOn messages),
//! then idles forever.  This matches the M4 exit-criterion scenario: the
//! receiver should be holding three notes when TX power is killed; the
//! watchdog then forces all-notes-off.
//!
//! RX: logs every received MIDI message and the all-notes-off event via
//! defmt.  When the FeatherWing arrives, swap this for a `BufferedUarte`-
//! backed sink that writes the bytes out to MIDI.

use crate::{MidiSink, MidiSource};

// ── Synthetic source: hold a C major chord ──────────────────────────────────

/// One-shot source that emits NoteOn for C, E, G at boot then idles forever.
pub struct ChordHoldSource {
    sent: u8,
}

impl ChordHoldSource {
    pub const fn new() -> Self {
        Self { sent: 0 }
    }
}

impl Default for ChordHoldSource {
    fn default() -> Self {
        Self::new()
    }
}

/// MIDI status byte for NoteOn on channel 0 = `0x90`.  Velocity 100 is a
/// comfortable mezzo-forte; choose anything 1..=127 (0 would silently
/// turn the note off).
const CHORD_C_MAJOR: &[[u8; 3]] = &[
    [0x90, 60, 100], // C4
    [0x90, 64, 100], // E4
    [0x90, 67, 100], // G4
];

#[derive(Debug, Clone, Copy, defmt::Format)]
pub enum ChordSourceError {}

impl MidiSource for ChordHoldSource {
    type Error = ChordSourceError;

    async fn next_message(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        if let Some(msg) = CHORD_C_MAJOR.get(self.sent as usize) {
            buf[..3].copy_from_slice(msg);
            self.sent = self.sent.saturating_add(1);
            defmt::info!(
                "synthetic source: NoteOn ch=0 note={} vel={}",
                msg[1],
                msg[2]
            );
            Ok(3)
        } else {
            // Idle forever — the heartbeat timer fills the silence so the
            // receiver's watchdog stays fed.  Power-cycling TX while in
            // this state is the M4 exit-criterion scenario.
            core::future::pending::<()>().await;
            unreachable!("future::pending never resolves")
        }
    }
}

// ── Synthetic sink: defmt logger ────────────────────────────────────────────

/// Sink that logs incoming MIDI + all-notes-off events via defmt.  Swap for
/// a UART-backed sink when the MIDI FeatherWing arrives.
pub struct DefmtLogSink;

#[derive(Debug, Clone, Copy, defmt::Format)]
pub enum DefmtSinkError {}

impl MidiSink for DefmtLogSink {
    type Error = DefmtSinkError;

    async fn write_message(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        defmt::info!("MIDI OUT: {=[u8]:#x}", bytes);
        Ok(())
    }

    async fn all_notes_off(&mut self) -> Result<(), Self::Error> {
        defmt::warn!("MIDI OUT: ALL_NOTES_OFF (16 channels × CC 123 = 0)");
        // A real UART sink would write the 48 bytes to the MIDI port:
        //   for ch in 0..16 { uart.write([0xB0 | ch, 0x7B, 0x00]); }
        // Logging here is enough to confirm watchdog → all-notes-off
        // pipeline at the link layer.
        Ok(())
    }
}
