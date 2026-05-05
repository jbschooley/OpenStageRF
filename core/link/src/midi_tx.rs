// SPDX-License-Identifier: AGPL-3.0-or-later

//! MIDI-aware transmit queue with round-robin redundancy.
//!
//! Each message is queued with a priority (regular = `REGULAR_PRIORITY`,
//! system real-time = `REALTIME_PRIORITY`).  `pop_send_batch` removes
//! the front-of-queue entries that share a priority, concatenating
//! their bytes — the caller (`run_tx`) then encodes that batch ONCE and
//! retransmits the same wire bytes K times for redundancy.  Same wire
//! bytes → same `seq` → the receiver's replay window automatically
//! dedups the retransmits, so the receiver's sink fires each logical
//! event exactly once.
//!
//! Trade-offs vs. an earlier per-credit round-robin design:
//!
//! * The receiver dedups for free instead of needing a separate
//!   content-aware dedup layer.
//! * Chord spread is already minimal (the whole chord goes in one
//!   packet thanks to batching), so the round-robin's main benefit no
//!   longer applies.
//! * Same-priority events that arrive mid-burst (after we've already
//!   popped a batch) can still preempt the remaining retransmit copies
//!   — `run_tx` checks the queue between copies and bails early if a
//!   new event is waiting.  The trade-off is the original batch may
//!   only get 1 or 2 of its 3 copies delivered (~0.2% miss rate
//!   instead of 8 × 10⁻⁹), in exchange for ~3 ms lower latency for the
//!   new event.
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
/// keyboard chord bursts plus headroom for system-real-time interleaving.
/// At ~10 bytes per entry that's ~640 B RAM.
pub const QUEUE_CAPACITY: usize = 64;

/// Priority value used for regular channel-voice / system-common messages.
/// Lower than `REALTIME_PRIORITY` so real-time events preempt them.
pub const REGULAR_PRIORITY: u8 = 1;

/// Maximum bytes per MIDI message stored in the queue.  Channel messages
/// are 1–3 bytes; we round up to 4 so 4-byte System Exclusive headers
/// (e.g., `F0 7F` followed by a single ID byte) also fit in the same slot.
/// SysEx body fragments don't go through this queue — they're streamed
/// separately via the `Body::SysExFragment` path.
const MAX_MSG_BYTES: usize = 4;

/// Priority value for system real-time messages (0xF8–0xFF).  These
/// carry tempo / transport semantics (Timing Clock, Start, Stop, Continue,
/// Active Sensing, Reset) that are jitter-sensitive and should preempt
/// any pending channel-voice traffic.
pub const REALTIME_PRIORITY: u8 = u8::MAX;

#[derive(Debug, Clone)]
struct Entry {
    bytes: Vec<u8, MAX_MSG_BYTES>,
    /// Queue ordering key — higher = closer to the front.  Real-time =
    /// `REALTIME_PRIORITY` (preempts everything); regular channel-voice
    /// = `REGULAR_PRIORITY`.
    priority: u8,
}

/// Priority-ordered MIDI transmit queue with status-aware dedup.
pub struct MidiTxQueue {
    entries: Vec<Entry, QUEUE_CAPACITY>,
}

impl MidiTxQueue {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Priority of the front entry, or `None` if empty.  Used by `run_tx`
    /// to check for preempting events between retransmit copies.
    pub fn front_priority(&self) -> Option<u8> {
        self.entries.first().map(|e| e.priority)
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

        // System real-time (0xF8..=0xFF): jitter-sensitive.  Top priority
        // — preempt any pending channel-voice traffic.  Caller is
        // responsible for sending real-time batches single-shot (no
        // retransmit) since they're frequent and miss-tolerant.  No
        // dedup (each carries unique semantics; e.g., two adjacent
        // Timing Clocks aren't redundant — they advance the receiver's
        // tempo counter independently).
        if status >= 0xF8 {
            let mut entry = Entry {
                bytes: Vec::new(),
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
            priority: REGULAR_PRIORITY,
        };
        // SAFETY: bytes.len() ≤ MAX_MSG_BYTES (checked above).
        let _ = entry.bytes.extend_from_slice(bytes);
        self.insert_by_priority(entry).is_ok()
    }

    /// Pop a batch of front-of-queue entries that share the same
    /// priority, concatenating their MIDI bytes into `out`.  Consumed
    /// entries are removed from the queue — no automatic retransmits.
    /// `run_tx` encodes this batch ONCE and resends the same wire bytes
    /// K times for redundancy (so the receiver's replay window dedups
    /// the copies by `seq`).
    ///
    /// Same-priority-only batching means real-time messages (priority
    /// MAX) always go in their own packet — never bundled with regular
    /// channel-voice events.
    ///
    /// Returns `Some((bytes_written, priority))` or `None` if the queue
    /// is empty.  `priority` lets the caller choose retransmit count
    /// (regular = K copies, real-time = 1 copy).
    pub fn pop_send_batch(&mut self, out: &mut [u8]) -> Option<(usize, u8)> {
        if self.entries.is_empty() {
            return None;
        }
        let target_priority = self.entries[0].priority;
        let mut total_len = 0usize;
        let mut consumed = 0usize;

        while let Some(entry) = self.entries.get(consumed) {
            if entry.priority != target_priority {
                break;
            }
            let n = entry.bytes.len();
            if total_len + n > out.len() {
                break;
            }
            out[total_len..total_len + n].copy_from_slice(&entry.bytes);
            total_len += n;
            consumed += 1;
        }

        if consumed == 0 {
            return None;
        }

        // Remove the consumed entries from the front.
        for _ in 0..consumed {
            self.entries.remove(0);
        }

        Some((total_len, target_priority))
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

    fn drain_batches(q: &mut MidiTxQueue) -> Vec<(std::vec::Vec<u8>, u8), 64> {
        let mut out = [0u8; 64];
        let mut all: Vec<(std::vec::Vec<u8>, u8), 64> = Vec::new();
        while let Some((n, priority)) = q.pop_send_batch(&mut out) {
            let _ = all.push((out[..n].to_vec(), priority));
        }
        all
    }

    #[test]
    fn chord_pops_as_single_batch() {
        let mut q = MidiTxQueue::new();
        q.push(&[0x90, 60, 100]);
        q.push(&[0x90, 64, 100]);
        q.push(&[0x90, 67, 100]);
        // All three are regular priority and FIFO — pop_send_batch
        // returns them concatenated in one batch and empties the queue.
        let batches = drain_batches(&mut q);
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0].0,
            vec![0x90, 60, 100, 0x90, 64, 100, 0x90, 67, 100]
        );
        assert_eq!(batches[0].1, REGULAR_PRIORITY);
    }

    #[test]
    fn realtime_preempts_chord() {
        // Press chord, then a Timing Clock arrives before the chord has
        // been popped.  TC should pop FIRST as its own batch.
        let mut q = MidiTxQueue::new();
        q.push(&[0x90, 60, 100]); // C
        q.push(&[0x90, 64, 100]); // E
        q.push(&[0x90, 67, 100]); // G
        q.push(&[0xF8]); // Timing Clock — arrives last, but priority MAX
        let batches = drain_batches(&mut q);
        assert_eq!(batches.len(), 2);
        // First batch: TC alone at priority MAX.
        assert_eq!(batches[0].0, vec![0xF8]);
        assert_eq!(batches[0].1, REALTIME_PRIORITY);
        // Second batch: chord at REGULAR_PRIORITY.
        assert_eq!(
            batches[1].0,
            vec![0x90, 60, 100, 0x90, 64, 100, 0x90, 67, 100]
        );
        assert_eq!(batches[1].1, REGULAR_PRIORITY);
    }

    #[test]
    fn new_event_arriving_after_pop_goes_to_back() {
        // Pop the chord, then a new note arrives — pop again returns it.
        let mut q = MidiTxQueue::new();
        q.push(&[0x90, 60, 100]);
        q.push(&[0x90, 64, 100]);
        let mut tmp = [0u8; 64];
        assert!(q.pop_send_batch(&mut tmp).is_some());
        assert!(q.is_empty());
        q.push(&[0x90, 71, 100]); // B
        let (n, _) = q.pop_send_batch(&mut tmp).unwrap();
        assert_eq!(&tmp[..n], &[0x90, 71, 100]);
    }

    /// Concatenate every batch's bytes into one flat Vec.  Used by the
    /// dedup tests where ordering within a batch doesn't matter — only
    /// which messages survive.
    fn drain_all_bytes(q: &mut MidiTxQueue) -> std::vec::Vec<u8> {
        let mut all = std::vec::Vec::new();
        for (bytes, _) in drain_batches(q) {
            all.extend_from_slice(&bytes);
        }
        all
    }

    /// Split a flat byte buffer into MIDI messages by status byte length.
    /// Only handles the channel-voice statuses our tests use.
    fn split_messages(bytes: &[u8]) -> std::vec::Vec<&[u8]> {
        let mut out = std::vec::Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let s = bytes[i];
            let n = match s & 0xF0 {
                0xC0 | 0xD0 => 2,
                0x80 | 0x90 | 0xA0 | 0xB0 | 0xE0 => 3,
                _ => 1,
            };
            out.push(&bytes[i..i + n]);
            i += n;
        }
        out
    }

    #[test]
    fn note_off_cancels_pending_note_on() {
        let mut q = MidiTxQueue::new();
        q.push(&[0x90, 60, 100]); // NoteOn C
        q.push(&[0x90, 64, 100]); // NoteOn E
        q.push(&[0x80, 60, 0]); // NoteOff C — should cancel NoteOn C
        let bytes = drain_all_bytes(&mut q);
        let msgs = split_messages(&bytes);
        // No NoteOn for note 60 should survive.
        for m in &msgs {
            if m[0] & 0xF0 == 0x90 {
                assert_ne!(m[1], 60, "stale NoteOn(C) survived NoteOff");
            }
        }
        // NoteOn E and NoteOff C remain (each appears once — retransmits
        // are now done by run_tx, not the queue).
        let e_count = msgs.iter().filter(|m| m[0] == 0x90 && m[1] == 64).count();
        assert_eq!(e_count, 1);
        let off_c_count = msgs.iter().filter(|m| m[0] == 0x80 && m[1] == 60).count();
        assert_eq!(off_c_count, 1);
    }

    #[test]
    fn cc_overrides_pending_cc_same_controller() {
        let mut q = MidiTxQueue::new();
        q.push(&[0xB0, 7, 50]); // Volume 50
        q.push(&[0xB0, 7, 90]); // Volume 90 — should cancel the 50
        q.push(&[0xB0, 64, 127]); // Sustain ON — different ctrl, kept
        let bytes = drain_all_bytes(&mut q);
        let msgs = split_messages(&bytes);
        // Only Volume=90 should survive.
        for m in &msgs {
            if m[0] == 0xB0 && m[1] == 7 {
                assert_eq!(m[2], 90, "stale Volume CC value survived");
            }
        }
        let vol_count = msgs.iter().filter(|m| m[0] == 0xB0 && m[1] == 7).count();
        assert_eq!(vol_count, 1);
        let sus_count = msgs.iter().filter(|m| m[0] == 0xB0 && m[1] == 64).count();
        assert_eq!(sus_count, 1);
    }

    #[test]
    fn pitch_bend_overrides_pending_pitch_bend() {
        let mut q = MidiTxQueue::new();
        q.push(&[0xE0, 0, 64]); // Centre
        q.push(&[0xE0, 0x40, 0x70]); // Bent up — should cancel centre
        let bytes = drain_all_bytes(&mut q);
        let msgs = split_messages(&bytes);
        for m in &msgs {
            if m[0] == 0xE0 {
                assert_eq!((m[1], m[2]), (0x40, 0x70));
            }
        }
        assert_eq!(msgs.iter().filter(|m| m[0] == 0xE0).count(), 1);
    }

    #[test]
    fn note_on_cancels_pending_note_off_same_note() {
        // Rapid release-then-re-press: NoteOff queued, then NoteOn arrives.
        // Without cancellation, a still-queued NoteOff would arrive after
        // the NoteOn and turn the re-struck note off.
        let mut q = MidiTxQueue::new();
        q.push(&[0x80, 60, 0]); // NoteOff C
        q.push(&[0x90, 60, 100]); // NoteOn C — should cancel the NoteOff
        let bytes = drain_all_bytes(&mut q);
        let msgs = split_messages(&bytes);
        for m in &msgs {
            if m[0] == 0x80 {
                assert_ne!(m[1], 60, "stale NoteOff(C) survived NoteOn");
            }
        }
        let on_count = msgs.iter().filter(|m| m[0] == 0x90 && m[1] == 60).count();
        assert_eq!(on_count, 1);
    }

    #[test]
    fn note_on_not_deduped_against_other_note_on() {
        // Same note pressed twice in a row shouldn't have the first cancelled.
        let mut q = MidiTxQueue::new();
        q.push(&[0x90, 60, 100]);
        q.push(&[0x90, 60, 100]);
        let bytes = drain_all_bytes(&mut q);
        let msgs = split_messages(&bytes);
        let n_count = msgs.iter().filter(|m| m[0] == 0x90 && m[1] == 60).count();
        assert_eq!(n_count, 2);
    }

    #[test]
    fn different_channels_dont_interfere() {
        let mut q = MidiTxQueue::new();
        q.push(&[0xB0, 7, 50]); // ch 0, Vol 50
        q.push(&[0xB1, 7, 90]); // ch 1, Vol 90 — different channel, kept
        let bytes = drain_all_bytes(&mut q);
        let msgs = split_messages(&bytes);
        let ch0 = msgs.iter().filter(|m| m[0] == 0xB0).count();
        let ch1 = msgs.iter().filter(|m| m[0] == 0xB1).count();
        assert_eq!(ch0, 1);
        assert_eq!(ch1, 1);
    }

    #[test]
    fn batch_pop_respects_buffer_size() {
        let mut q = MidiTxQueue::new();
        q.push(&[0x90, 60, 100]);
        q.push(&[0x90, 64, 100]);
        q.push(&[0x90, 67, 100]);
        // Buffer fits only 2 messages = 6 bytes.
        let mut out = [0u8; 6];
        let (n, _) = q.pop_send_batch(&mut out).unwrap();
        assert_eq!(n, 6);
        assert_eq!(&out[..6], &[0x90, 60, 100, 0x90, 64, 100]);
        // Third message remains.
        let (n2, _) = q.pop_send_batch(&mut [0u8; 64]).unwrap();
        assert_eq!(n2, 3);
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
