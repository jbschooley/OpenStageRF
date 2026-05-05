// SPDX-License-Identifier: AGPL-3.0-or-later

//! MIDI-aware transmit queue with round-robin redundancy.
//!
//! Each message gets queued with `repeat_count` "send credits".  On
//! every `pop_send` call we take the entry at the front and consume one
//! credit; if any remain, the entry is re-inserted at the position
//! corresponding to its remaining-credit count — keeping the queue
//! ordered by descending credit (FIFO within the same credit level).
//!
//! That gives two desirable properties at once:
//!
//! 1. **Round-robin within a burst.**  A 3-note chord pushed in rapid
//!    succession comes out as `C, E, G, C, E, G, C, E, G` — first round
//!    delivers all three notes with ~one-message-time spread; rounds
//!    two and three insure against per-packet RF loss.
//! 2. **New events preempt later rounds of older events.**  If a new
//!    note is tapped while an existing chord is on its 2nd or 3rd round,
//!    the new note (3 credits remaining) jumps in front of the older
//!    entries (≤ 2 credits remaining) for its **first** send.  The older
//!    entries' subsequent rounds resume immediately after — they only
//!    lose ~1.5 ms of wait time per burst, never miss out on their
//!    redundancy.
//!
//! On `push`, MIDI status semantics are used to cancel stale queued
//! messages so the queue never holds opposing-state ghosts:
//!
//! | Incoming | Cancels in queue |
//! |----------|------------------|
//! | NoteOff (0x8X) note N | NoteOns (0x9X) on same channel, same note |
//! | NoteOn (0x9X) note N | NoteOffs (0x8X) on same channel, same note |
//! | PolyAftertouch (0xAX) note N | PolyAT on same channel, same note |
//! | Control Change (0xBX) ctrl C | CC on same channel, same controller |
//! | Program Change (0xCX) | PC on same channel |
//! | Channel Pressure (0xDX) | CP on same channel |
//! | Pitch Bend (0xEX) | PB on same channel |
//!
//! Real-time (0xF8–0xFF) and SysEx are never deduped — each carries
//! unique semantics.  Two NoteOns for the same note (without intervening
//! NoteOff) also aren't deduped — those are legitimate rapid re-strikes
//! (legato attacks with different velocities).  Same for two NoteOffs.

use heapless::Vec;

/// Maximum queued messages (each ≤ 4 wire bytes).  Sized for worst-case
/// keyboard chord bursts (10-finger chord = 10 events × 3 send credits =
/// 30 slots) plus headroom.  At ~10 bytes per entry that's ~640 B RAM.
pub const QUEUE_CAPACITY: usize = 64;

/// Default redundancy: each message is sent this many times round-robin.
pub const DEFAULT_REPEAT_COUNT: u8 = 3;

/// Maximum bytes per MIDI message stored in the queue.  Channel messages
/// are 1–3 bytes; we round up to 4 so 4-byte System Exclusive headers
/// (e.g., `F0 7F` followed by a single ID byte) also fit in the same slot.
/// SysEx body fragments don't go through this queue — they're streamed
/// separately via the `Body::SysExFragment` path.
const MAX_MSG_BYTES: usize = 4;

/// Priority value used for system real-time messages (0xF8–0xFF).  These
/// carry tempo / transport semantics (Timing Clock, Start, Stop, Continue,
/// Active Sensing, Reset) that are jitter-sensitive and should preempt
/// any pending channel-voice traffic.  We park them above the regular
/// priority range so they always sort to the front of the queue.
const REALTIME_PRIORITY: u8 = u8::MAX;

#[derive(Debug, Clone)]
struct Entry {
    bytes: Vec<u8, MAX_MSG_BYTES>,
    sends_remaining: u8,
    /// Queue ordering key — higher = closer to the front.  For regular
    /// messages this tracks `sends_remaining` (so a fresh event with 3
    /// credits outranks any in-progress event with ≤ 2 left, but as it
    /// ages through its rounds it falls back).  For real-time messages
    /// it's pinned at `REALTIME_PRIORITY` so they stay at the front
    /// until exhausted.
    priority: u8,
}

/// Round-robin redundant MIDI transmit queue with status-aware dedup.
pub struct MidiTxQueue {
    entries: Vec<Entry, QUEUE_CAPACITY>,
    repeat_count: u8,
}

impl MidiTxQueue {
    pub fn new() -> Self {
        Self::with_repeat_count(DEFAULT_REPEAT_COUNT)
    }

    pub fn with_repeat_count(repeat_count: u8) -> Self {
        Self {
            entries: Vec::new(),
            repeat_count: repeat_count.max(1),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Queue a MIDI message for redundant send.  Applies dedup rules
    /// (cancelling stale queued messages of the same kind/target) before
    /// appending.  Returns `false` if the queue is full and the new
    /// message had to be dropped — caller should treat that as a bug
    /// (queue size should be tuned so it never fills up under realistic
    /// load).  Empty / oversize messages are silently ignored.
    pub fn push(&mut self, bytes: &[u8]) -> bool {
        if bytes.is_empty() || bytes.len() > MAX_MSG_BYTES {
            return false;
        }
        let status = bytes[0];

        // System real-time (0xF8..=0xFF): jitter-sensitive.  Single-send,
        // top-priority — they preempt any pending channel-voice traffic.
        // No dedup (each carries unique semantics; e.g., two adjacent
        // Timing Clocks aren't redundant — they advance the receiver's
        // tempo counter independently).
        if status >= 0xF8 {
            let mut entry = Entry {
                bytes: Vec::new(),
                sends_remaining: 1,
                priority: REALTIME_PRIORITY,
            };
            let _ = entry.bytes.extend_from_slice(bytes);
            return self.insert_by_priority(entry).is_ok();
        }

        match status & 0xF0 {
            0x80 => {
                // NoteOff: cancel pending NoteOn with same channel + note.
                let ch = status & 0x0F;
                let note = bytes.get(1).copied().unwrap_or(0);
                self.entries.retain(|e| !is_note_on(e, ch, note));
            }
            0x90 => {
                // NoteOn: cancel pending NoteOff with same channel + note.
                // (Don't cancel pending NoteOn — that's a legitimate
                // re-strike with potentially different velocity.)
                let ch = status & 0x0F;
                let note = bytes.get(1).copied().unwrap_or(0);
                self.entries.retain(|e| !is_note_off(e, ch, note));
            }
            0xA0 => {
                let ch = status & 0x0F;
                let note = bytes.get(1).copied().unwrap_or(0);
                self.entries.retain(|e| !is_poly_at(e, ch, note));
            }
            0xB0 => {
                let ch = status & 0x0F;
                let ctrl = bytes.get(1).copied().unwrap_or(0);
                self.entries.retain(|e| !is_cc(e, ch, ctrl));
            }
            0xC0 => {
                let ch = status & 0x0F;
                self.entries.retain(|e| !is_status(e, 0xC0, ch));
            }
            0xD0 => {
                let ch = status & 0x0F;
                self.entries.retain(|e| !is_status(e, 0xD0, ch));
            }
            0xE0 => {
                let ch = status & 0x0F;
                self.entries.retain(|e| !is_status(e, 0xE0, ch));
            }
            _ => {} // NoteOn, SysEx, system common — no dedup
        }

        let mut entry = Entry {
            bytes: Vec::new(),
            sends_remaining: self.repeat_count,
            priority: self.repeat_count,
        };
        // SAFETY: bytes.len() ≤ MAX_MSG_BYTES (checked above).
        let _ = entry.bytes.extend_from_slice(bytes);
        self.insert_by_priority(entry).is_ok()
    }

    /// Pop the next message to send.  Copies its bytes into `out`,
    /// returns the number of bytes written.  Front of the queue is
    /// always the highest-credit, oldest-arrived entry — so this
    /// returns either a brand-new event's first copy or an in-progress
    /// event's next round, whichever is more "urgent" by credit count.
    /// If the popped entry has any credits remaining, it's re-inserted
    /// at the position matching its (now-decremented) credit level.
    /// Returns `None` if the queue is empty.
    pub fn pop_send(&mut self, out: &mut [u8]) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        // remove(0) is O(n) but n is bounded small; fine.
        let mut entry = self.entries.remove(0);
        let n = entry.bytes.len();
        if n > out.len() {
            return None; // caller's buffer too small
        }
        out[..n].copy_from_slice(&entry.bytes);
        entry.sends_remaining = entry.sends_remaining.saturating_sub(1);
        if entry.sends_remaining > 0 {
            // Regular messages: priority follows credits (3 → 2 → 1 → done).
            // Real-time messages: priority pinned at REALTIME_PRIORITY for
            // their whole lifetime so they stay at the front.
            if entry.priority != REALTIME_PRIORITY {
                entry.priority = entry.sends_remaining;
            }
            let _ = self.insert_by_priority(entry);
        }
        Some(n)
    }

    /// Insert `entry` at the position that keeps the queue ordered by
    /// descending `priority`, with FIFO within the same level.  Returns
    /// `Err(entry)` if the queue is full (caller can decide whether to
    /// drop or retry).
    fn insert_by_priority(&mut self, entry: Entry) -> Result<(), Entry> {
        // Find the first existing entry with strictly less priority —
        // we go before it.  If none, append to the end.
        let mut pos = self.entries.len();
        for (i, e) in self.entries.iter().enumerate() {
            if e.priority < entry.priority {
                pos = i;
                break;
            }
        }
        self.entries.insert(pos, entry)
    }
}

impl Default for MidiTxQueue {
    fn default() -> Self {
        Self::new()
    }
}

// ---- helpers ----

fn is_note_on(e: &Entry, ch: u8, note: u8) -> bool {
    let s = e.bytes.first().copied().unwrap_or(0);
    s & 0xF0 == 0x90
        && s & 0x0F == ch
        && e.bytes.get(1).copied().unwrap_or(255) == note
}

fn is_note_off(e: &Entry, ch: u8, note: u8) -> bool {
    let s = e.bytes.first().copied().unwrap_or(0);
    s & 0xF0 == 0x80
        && s & 0x0F == ch
        && e.bytes.get(1).copied().unwrap_or(255) == note
}

fn is_poly_at(e: &Entry, ch: u8, note: u8) -> bool {
    let s = e.bytes.first().copied().unwrap_or(0);
    s & 0xF0 == 0xA0
        && s & 0x0F == ch
        && e.bytes.get(1).copied().unwrap_or(255) == note
}

fn is_cc(e: &Entry, ch: u8, ctrl: u8) -> bool {
    let s = e.bytes.first().copied().unwrap_or(0);
    s & 0xF0 == 0xB0
        && s & 0x0F == ch
        && e.bytes.get(1).copied().unwrap_or(255) == ctrl
}

fn is_status(e: &Entry, status_high: u8, ch: u8) -> bool {
    let s = e.bytes.first().copied().unwrap_or(0);
    s & 0xF0 == status_high && s & 0x0F == ch
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn pop_all(q: &mut MidiTxQueue) -> Vec<std::vec::Vec<u8>, 64> {
        let mut out = [0u8; MAX_MSG_BYTES];
        let mut all: Vec<std::vec::Vec<u8>, 64> = Vec::new();
        while let Some(n) = q.pop_send(&mut out) {
            let _ = all.push(out[..n].to_vec());
        }
        all
    }

    #[test]
    fn round_robin_chord() {
        // C, E, G (on ch 0, vel 100) → expect C E G C E G C E G.
        let mut q = MidiTxQueue::new();
        q.push(&[0x90, 60, 100]);
        q.push(&[0x90, 64, 100]);
        q.push(&[0x90, 67, 100]);
        let drained = pop_all(&mut q);
        assert_eq!(drained.len(), 9);
        // Pattern: notes 60, 64, 67 repeating.
        let expected_notes = [60, 64, 67, 60, 64, 67, 60, 64, 67];
        for (i, msg) in drained.iter().enumerate() {
            assert_eq!(msg[0], 0x90, "msg {i}: status");
            assert_eq!(msg[1], expected_notes[i], "msg {i}: note");
        }
    }

    #[test]
    fn new_event_preempts_in_progress_round_robin() {
        // Send chord (C, E, G), simulate one round of round-robin sends
        // (drain 3 sends), then push a new note B — B's first copy
        // should jump in front of the chord notes' second copies.
        let mut q = MidiTxQueue::new();
        q.push(&[0x90, 60, 100]); // C
        q.push(&[0x90, 64, 100]); // E
        q.push(&[0x90, 67, 100]); // G

        // Simulate sending C, E, G one round each.
        let mut tmp = [0u8; MAX_MSG_BYTES];
        for _ in 0..3 {
            assert!(q.pop_send(&mut tmp).is_some());
        }
        // At this point chord notes have 2 credits remaining each.

        // New tap arrives.  Should jump ahead.
        q.push(&[0x90, 71, 100]); // B (new, 3 credits)

        // Next pop should be B (priority 3 > existing priority 2).
        let n = q.pop_send(&mut tmp).unwrap();
        assert_eq!(&tmp[..n], &[0x90, 71, 100]);

        // After B's first send (now 2 credits), it goes back into
        // round-robin with the chord notes — they all have 2 credits.
        // Next pop should resume with C, then E, G, then B again.
        let drained = pop_all(&mut q);
        let notes: std::vec::Vec<u8> = drained.iter().map(|m| m[1]).collect();
        // Order: C, E, G, B, C, E, G, B  (rounds 2 and 3 of all notes,
        // with B appended after the priority-2 group).
        assert_eq!(notes, vec![60, 64, 67, 71, 60, 64, 67, 71]);
    }

    #[test]
    fn note_off_cancels_pending_note_on() {
        let mut q = MidiTxQueue::new();
        q.push(&[0x90, 60, 100]); // NoteOn C
        q.push(&[0x90, 64, 100]); // NoteOn E
        q.push(&[0x80, 60, 0]); // NoteOff C — should cancel NoteOn C
        let drained = pop_all(&mut q);
        // Should NOT contain any NoteOn for note 60.
        for m in &drained {
            if m[0] & 0xF0 == 0x90 {
                assert_ne!(m[1], 60, "stale NoteOn(C) survived NoteOff");
            }
        }
        // NoteOn E should still be sent 3 times.
        let e_count = drained
            .iter()
            .filter(|m| m[0] == 0x90 && m[1] == 64)
            .count();
        assert_eq!(e_count, 3);
        // NoteOff C should be sent 3 times (insurance against any NoteOn copy
        // already in flight).
        let off_c_count = drained
            .iter()
            .filter(|m| m[0] == 0x80 && m[1] == 60)
            .count();
        assert_eq!(off_c_count, 3);
    }

    #[test]
    fn cc_overrides_pending_cc_same_controller() {
        let mut q = MidiTxQueue::new();
        q.push(&[0xB0, 7, 50]); // Volume 50
        q.push(&[0xB0, 7, 90]); // Volume 90 — should cancel the 50
        q.push(&[0xB0, 64, 127]); // Sustain ON — different ctrl, kept
        let drained = pop_all(&mut q);
        // Only volumes of 90 should survive.
        for m in &drained {
            if m[0] == 0xB0 && m[1] == 7 {
                assert_eq!(m[2], 90, "stale Volume CC value survived");
            }
        }
        let vol_count = drained
            .iter()
            .filter(|m| m[0] == 0xB0 && m[1] == 7)
            .count();
        assert_eq!(vol_count, 3);
        let sus_count = drained
            .iter()
            .filter(|m| m[0] == 0xB0 && m[1] == 64)
            .count();
        assert_eq!(sus_count, 3);
    }

    #[test]
    fn pitch_bend_overrides_pending_pitch_bend() {
        let mut q = MidiTxQueue::new();
        q.push(&[0xE0, 0, 64]); // Centre
        q.push(&[0xE0, 0x40, 0x70]); // Bent up — should cancel centre
        let drained = pop_all(&mut q);
        // Only the bent-up value should appear.
        for m in &drained {
            if m[0] == 0xE0 {
                assert_eq!((m[1], m[2]), (0x40, 0x70));
            }
        }
        assert_eq!(drained.iter().filter(|m| m[0] == 0xE0).count(), 3);
    }

    #[test]
    fn note_on_cancels_pending_note_off_same_note() {
        // Rapid release-then-re-press: NoteOff queued, then NoteOn arrives.
        // Without cancellation, the pending NoteOff copies would arrive at
        // the receiver after the NoteOn and turn the re-struck note off.
        let mut q = MidiTxQueue::new();
        q.push(&[0x80, 60, 0]); // NoteOff C
        q.push(&[0x90, 60, 100]); // NoteOn C — should cancel the NoteOff
        let drained = pop_all(&mut q);
        // No NoteOff for note 60 should survive.
        for m in &drained {
            if m[0] == 0x80 {
                assert_ne!(m[1], 60, "stale NoteOff(C) survived NoteOn");
            }
        }
        let on_count = drained
            .iter()
            .filter(|m| m[0] == 0x90 && m[1] == 60)
            .count();
        assert_eq!(on_count, 3); // NoteOn fully triple-sent
    }

    #[test]
    fn note_on_not_deduped_against_other_note_on() {
        // Same note pressed twice in a row shouldn't have the first cancelled.
        let mut q = MidiTxQueue::new();
        q.push(&[0x90, 60, 100]);
        q.push(&[0x90, 60, 100]);
        let drained = pop_all(&mut q);
        let n_count = drained
            .iter()
            .filter(|m| m[0] == 0x90 && m[1] == 60)
            .count();
        assert_eq!(n_count, 6); // both events, 3× each
    }

    #[test]
    fn different_channels_dont_interfere() {
        let mut q = MidiTxQueue::new();
        q.push(&[0xB0, 7, 50]); // ch 0, Vol 50
        q.push(&[0xB1, 7, 90]); // ch 1, Vol 90 — different channel, kept
        let drained = pop_all(&mut q);
        let ch0 = drained.iter().filter(|m| m[0] == 0xB0).count();
        let ch1 = drained.iter().filter(|m| m[0] == 0xB1).count();
        assert_eq!(ch0, 3);
        assert_eq!(ch1, 3);
    }

    #[test]
    fn realtime_message_preempts_chord() {
        // Press a chord, then a Timing Clock arrives mid-burst — TC should
        // jump to the absolute front of the queue and send single-shot.
        let mut q = MidiTxQueue::new();
        q.push(&[0x90, 60, 100]); // C
        q.push(&[0x90, 64, 100]); // E
        q.push(&[0x90, 67, 100]); // G

        // Pretend C and E got their first send already (round 1 in flight).
        let mut tmp = [0u8; MAX_MSG_BYTES];
        q.pop_send(&mut tmp); // C
        q.pop_send(&mut tmp); // E

        // Timing Clock arrives.
        q.push(&[0xF8]);

        // Next pop should be Timing Clock (priority MAX, ahead of even
        // fresh-credit-3 entries).
        let n = q.pop_send(&mut tmp).unwrap();
        assert_eq!(&tmp[..n], &[0xF8]);
        // TC has sends_remaining=1 originally, now 0 → dropped.
        // Next pop should be G (still priority 3, freshest).
        let n = q.pop_send(&mut tmp).unwrap();
        assert_eq!(tmp[..n], [0x90, 67, 100]);
    }

    #[test]
    fn realtime_message_single_send_only() {
        // Real-time messages aren't redundantly sent (they're frequent and
        // miss-tolerant; Timing Clock at 48 Hz with single-send means a
        // lost clock skews tempo by 1/48 of a quarter note for one beat).
        let mut q = MidiTxQueue::new();
        q.push(&[0xF8]); // Timing Clock
        let drained = pop_all(&mut q);
        assert_eq!(drained.len(), 1, "real-time message sent more than once");
    }

    #[test]
    fn queue_overflow_returns_false() {
        let mut q = MidiTxQueue::new();
        for _ in 0..QUEUE_CAPACITY {
            assert!(q.push(&[0x90, 60, 100]));
        }
        // One more should fail.
        assert!(!q.push(&[0x90, 64, 100]));
    }
}
