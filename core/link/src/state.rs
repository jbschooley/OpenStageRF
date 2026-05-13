// SPDX-License-Identifier: AGPL-3.0-or-later

//! MIDI note-state tracking for the link layer's stuck-note failsafe.
//!
//! [`ChannelNoteCounts`] (TX-side) tracks how many notes are currently
//! pressed on each of 16 channels.  Its [`active_mask`] method returns a
//! 16-bit bitmap suitable for inclusion in heartbeat packets:
//!
//! ```text
//! bit i = 1   ⇔   any note pressed on channel i
//! ```
//!
//! [`PressedNotes`] (RX-side) tracks WHICH notes are pressed (per
//! channel × note bitmap).  When a heartbeat arrives carrying the TX
//! mask, the receiver compares — any channel where TX says "silent" but
//! RX still has notes pressed indicates one or more lost NoteOff
//! packets.  The receiver fires `CC 123 (All Notes Off)` for that
//! channel to recover.
//!
//! Why this layer matters even with K=3 retransmits + time-spread
//! NoteOff copies: bursty correlated RF interference can occasionally
//! kill all 5 wire copies of a single NoteOff, leaving a stuck note.
//! The heartbeat-state failsafe catches that within a few heartbeat
//! intervals (~30 ms with the default `HEARTBEAT_MS = 10`).
//!
//! [`active_mask`]: ChannelNoteCounts::active_mask

/// TX-side note-count tracker.  Maintains a per-channel count of
/// currently-pressed notes; intended use is to call [`Self::observe`]
/// on every successful `MidiTxQueue::push_channel_voice` and read
/// [`Self::active_mask`] when building heartbeat bodies.
///
/// Counts saturate at 255 — far above any realistic chord size.  Two
/// consecutive NoteOns for the same note are intentionally counted
/// twice (TX dedup doesn't merge re-strikes), but the matching
/// NoteOffs decrement back to zero, so transient over-counts don't
/// accumulate.
#[derive(Debug, Clone)]
pub struct ChannelNoteCounts {
    counts: [u8; 16],
}

impl ChannelNoteCounts {
    pub const fn new() -> Self {
        Self { counts: [0; 16] }
    }

    pub fn reset(&mut self) {
        self.counts = [0; 16];
    }

    /// Update the count from a single MIDI message.  No-op for
    /// non-Note messages.  NoteOn with velocity 0 is treated as
    /// NoteOff per MIDI convention.
    pub fn observe(&mut self, midi: &[u8]) {
        let status = match midi.first() {
            Some(&s) => s,
            None => return,
        };
        let ch = (status & 0x0F) as usize;
        let vel = midi.get(2).copied().unwrap_or(0);
        match status & 0xF0 {
            0x90 if vel > 0 => {
                self.counts[ch] = self.counts[ch].saturating_add(1);
            }
            0x80 | 0x90 /* NoteOn vel=0 == NoteOff per MIDI 1.0 */ => {
                self.counts[ch] = self.counts[ch].saturating_sub(1);
            }
            _ => {}
        }
    }

    /// 16-bit active-channel bitmap for heartbeat inclusion.
    /// Bit `i` set ⇔ channel `i` has at least one note pressed.
    pub fn active_mask(&self) -> u16 {
        let mut mask = 0u16;
        for ch in 0..16 {
            if self.counts[ch] > 0 {
                mask |= 1 << ch;
            }
        }
        mask
    }

    /// For diagnostics / tests.  Direct read of the per-channel count.
    pub fn count(&self, ch: u8) -> u8 {
        self.counts[(ch as usize) & 0x0F]
    }
}

impl Default for ChannelNoteCounts {
    fn default() -> Self {
        Self::new()
    }
}

/// RX-side note-state tracker.  Per-channel 128-bit bitmap of which
/// notes are currently pressed.  [`Self::observe`] should be called on
/// every accepted `RxEvent::ChannelVoice`; [`Self::missing_clear`]
/// computes which channels have stuck notes the TX has already
/// released.
#[derive(Debug, Clone)]
pub struct PressedNotes {
    /// `pressed[ch]` bit `n` set ⇔ note `n` is currently pressed on
    /// channel `ch`.
    pressed: [u128; 16],
}

impl PressedNotes {
    pub const fn new() -> Self {
        Self { pressed: [0; 16] }
    }

    pub fn reset(&mut self) {
        self.pressed = [0; 16];
    }

    pub fn observe(&mut self, midi: &[u8]) {
        let status = match midi.first() {
            Some(&s) => s,
            None => return,
        };
        let ch = (status & 0x0F) as usize;
        let note = midi.get(1).copied().unwrap_or(0) & 0x7F;
        let vel = midi.get(2).copied().unwrap_or(0);
        let bit = 1u128 << note;
        match status & 0xF0 {
            0x90 if vel > 0 => {
                self.pressed[ch] |= bit;
            }
            0x80 | 0x90 => {
                self.pressed[ch] &= !bit;
            }
            _ => {}
        }
    }

    /// Returns a 16-bit bitmap of channels where TX reports silent
    /// (`tx_mask` bit clear) but RX has at least one pressed note.
    /// These channels need a CC 123 (All Notes Off) recovery from the
    /// caller.
    pub fn missing_clear(&self, tx_mask: u16) -> u16 {
        let mut needed = 0u16;
        for ch in 0..16 {
            let tx_off = tx_mask & (1 << ch) == 0;
            let rx_has = self.pressed[ch] != 0;
            if tx_off && rx_has {
                needed |= 1 << ch;
            }
        }
        needed
    }

    /// Clear all pressed-note state for a channel.  Caller invokes
    /// after sending the CC 123 silencing message.
    pub fn clear_channel(&mut self, ch: u8) {
        self.pressed[(ch as usize) & 0x0F] = 0;
    }

    /// Diagnostic accessor.  Returns the 128-bit pressed bitmap for a channel.
    pub fn pressed_on(&self, ch: u8) -> u128 {
        self.pressed[(ch as usize) & 0x0F]
    }

    /// True if any note is pressed on any channel.  Useful for the
    /// watchdog path's all-notes-off decision.
    pub fn any_pressed(&self) -> bool {
        self.pressed.iter().any(|&p| p != 0)
    }
}

impl Default for PressedNotes {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ChannelNoteCounts ────────────────────────────────────────────────

    #[test]
    fn note_on_increments_count() {
        let mut s = ChannelNoteCounts::new();
        s.observe(&[0x90, 60, 100]);
        assert_eq!(s.count(0), 1);
        assert_eq!(s.active_mask(), 0x0001);
    }

    #[test]
    fn note_off_decrements_count() {
        let mut s = ChannelNoteCounts::new();
        s.observe(&[0x90, 60, 100]); // NoteOn
        s.observe(&[0x80, 60, 0]); // NoteOff
        assert_eq!(s.count(0), 0);
        assert_eq!(s.active_mask(), 0);
    }

    #[test]
    fn note_on_velocity_zero_is_note_off() {
        let mut s = ChannelNoteCounts::new();
        s.observe(&[0x90, 60, 100]); // NoteOn
        s.observe(&[0x90, 60, 0]); // pseudo NoteOff
        assert_eq!(s.count(0), 0);
    }

    #[test]
    fn chord_increments_three_times() {
        let mut s = ChannelNoteCounts::new();
        s.observe(&[0x90, 60, 100]);
        s.observe(&[0x90, 64, 100]);
        s.observe(&[0x90, 67, 100]);
        assert_eq!(s.count(0), 3);
        assert_eq!(s.active_mask(), 0x0001);
    }

    #[test]
    fn separate_channels_independent() {
        let mut s = ChannelNoteCounts::new();
        s.observe(&[0x90, 60, 100]); // ch 0
        s.observe(&[0x91, 60, 100]); // ch 1
        s.observe(&[0x95, 60, 100]); // ch 5
        assert_eq!(s.count(0), 1);
        assert_eq!(s.count(1), 1);
        assert_eq!(s.count(5), 1);
        assert_eq!(s.active_mask(), 0b0000_0000_0010_0011);
    }

    #[test]
    fn cc_and_pb_dont_change_count() {
        let mut s = ChannelNoteCounts::new();
        s.observe(&[0xB0, 7, 100]); // CC volume
        s.observe(&[0xE0, 0, 64]); // PitchBend
        s.observe(&[0xC0, 42]); // ProgramChange
        assert_eq!(s.count(0), 0);
        assert_eq!(s.active_mask(), 0);
    }

    #[test]
    fn unmatched_note_off_saturates_at_zero() {
        let mut s = ChannelNoteCounts::new();
        s.observe(&[0x80, 60, 0]); // NoteOff with no prior NoteOn
        assert_eq!(s.count(0), 0);
    }

    #[test]
    fn count_saturates_at_max() {
        let mut s = ChannelNoteCounts::new();
        for _ in 0..300 {
            s.observe(&[0x90, 60, 100]);
        }
        assert_eq!(s.count(0), 255);
    }

    #[test]
    fn empty_message_is_noop() {
        let mut s = ChannelNoteCounts::new();
        s.observe(&[]);
        assert_eq!(s.active_mask(), 0);
    }

    #[test]
    fn reset_clears_all_counts() {
        let mut s = ChannelNoteCounts::new();
        for ch in 0..16 {
            s.observe(&[0x90 | ch, 60, 100]);
        }
        assert_eq!(s.active_mask(), 0xFFFF);
        s.reset();
        assert_eq!(s.active_mask(), 0);
    }

    // ── PressedNotes ─────────────────────────────────────────────────────

    #[test]
    fn rx_note_on_marks_pressed() {
        let mut s = PressedNotes::new();
        s.observe(&[0x90, 60, 100]);
        assert_eq!(s.pressed_on(0), 1u128 << 60);
        assert!(s.any_pressed());
    }

    #[test]
    fn rx_note_off_clears_pressed() {
        let mut s = PressedNotes::new();
        s.observe(&[0x90, 60, 100]);
        s.observe(&[0x80, 60, 0]);
        assert_eq!(s.pressed_on(0), 0);
        assert!(!s.any_pressed());
    }

    #[test]
    fn rx_chord_marks_three_notes() {
        let mut s = PressedNotes::new();
        s.observe(&[0x90, 60, 100]);
        s.observe(&[0x90, 64, 100]);
        s.observe(&[0x90, 67, 100]);
        let expected = (1u128 << 60) | (1u128 << 64) | (1u128 << 67);
        assert_eq!(s.pressed_on(0), expected);
    }

    #[test]
    fn missing_clear_detects_stuck_channel() {
        let mut s = PressedNotes::new();
        s.observe(&[0x90, 60, 100]); // ch 0 has note 60
                                     // TX claims all silent (mask = 0).
        let needed = s.missing_clear(0x0000);
        assert_eq!(needed, 0x0001, "ch 0 should need clearing");
    }

    #[test]
    fn missing_clear_quiet_when_in_sync() {
        let mut s = PressedNotes::new();
        s.observe(&[0x90, 60, 100]); // ch 0 has note
                                     // TX agrees ch 0 has notes.
        let needed = s.missing_clear(0x0001);
        assert_eq!(needed, 0, "no recovery needed when TX/RX agree");
    }

    #[test]
    fn missing_clear_per_channel() {
        let mut s = PressedNotes::new();
        s.observe(&[0x90, 60, 100]); // ch 0
        s.observe(&[0x91, 60, 100]); // ch 1
        s.observe(&[0x95, 60, 100]); // ch 5
                                     // TX says ch 1 still has notes; ch 0 and ch 5 are silent on TX.
        let needed = s.missing_clear(0x0002);
        assert_eq!(
            needed, 0b0000_0000_0010_0001,
            "ch 0 and ch 5 stuck (TX silent, RX has notes); ch 1 in sync"
        );
    }

    #[test]
    fn clear_channel_resets_only_one_channel() {
        let mut s = PressedNotes::new();
        s.observe(&[0x90, 60, 100]);
        s.observe(&[0x91, 60, 100]);
        s.clear_channel(0);
        assert_eq!(s.pressed_on(0), 0);
        assert_eq!(s.pressed_on(1), 1u128 << 60);
    }

    #[test]
    fn rx_velocity_zero_note_on_clears() {
        let mut s = PressedNotes::new();
        s.observe(&[0x90, 60, 100]);
        s.observe(&[0x90, 60, 0]); // NoteOn vel 0 ≡ NoteOff
        assert_eq!(s.pressed_on(0), 0);
    }

    #[test]
    fn cc_doesnt_affect_pressed_state() {
        let mut s = PressedNotes::new();
        s.observe(&[0xB0, 7, 100]);
        s.observe(&[0xE0, 0, 64]);
        assert_eq!(s.pressed_on(0), 0);
        assert!(!s.any_pressed());
    }

    #[test]
    fn reset_clears_all() {
        let mut s = PressedNotes::new();
        for ch in 0..16 {
            s.observe(&[0x90 | ch, 60, 100]);
        }
        assert!(s.any_pressed());
        s.reset();
        assert!(!s.any_pressed());
    }
}
