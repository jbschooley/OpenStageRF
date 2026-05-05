// SPDX-License-Identifier: AGPL-3.0-or-later

//! SysEx fragment reassembly at the receiver.
//!
//! A `SysExReassembler` maintains up to `MAX_CONCURRENT_SYSEX` in-progress
//! reassembly buffers, keyed by `sysex_id`.  Fragments arrive in any
//! order; the reassembler stores them by `frag_idx`, dedupping replays
//! and tracking completion via a bitmap.  On the last missing fragment's
//! arrival, it concatenates F0 + every fragment's data + F7 into the
//! caller-supplied output buffer and returns a slice into it.
//!
//! Buffers that go more than `REASSEMBLY_TIMEOUT` without a new fragment
//! are discarded — partial SysEx is lost, which is acceptable since
//! SysEx isn't time-critical and our links are simplex (no retry).

use embassy_time::{Duration, Instant};
use heapless::Vec;
use osrf_protocols_midi_v1::MAX_FRAG_DATA_BYTES;

/// Largest SysEx (in fragments) the reassembler will handle.  At
/// `MAX_FRAG_DATA_BYTES = 49` per fragment, this caps one SysEx at
/// ~1.5 KB — comfortably above GM Reset, MTS tuning, and similar
/// real-world setup messages.
pub const MAX_FRAGS_PER_SYSEX: usize = 32;

/// Total reassembled SysEx body size including F0/F7 markers.  Caller-
/// supplied output buffer must be at least this large.
pub const MAX_SYSEX_BYTES: usize = 2 + MAX_FRAGS_PER_SYSEX * MAX_FRAG_DATA_BYTES;

/// How many SysEx messages can be in-flight simultaneously.
pub const MAX_CONCURRENT_SYSEX: usize = 2;

/// Timeout for reassembly: if a buffer goes this long without receiving
/// a new fragment, it's discarded.
pub const REASSEMBLY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
pub enum SysExOutcome<'a> {
    /// Fragment accepted; not yet complete.
    Pending,
    /// Fragment was a duplicate (same sysex_id + frag_idx as one we
    /// already received).
    Replay,
    /// Fragment dropped (no buffer available, or invalid frag_idx /
    /// frag_total, or output buffer too small to assemble).
    Dropped,
    /// SysEx is complete.  The slice points into the caller's output
    /// buffer and contains the full reassembled SysEx including
    /// `0xF0` start and `0xF7` end markers.
    Complete(&'a [u8]),
}

struct SysExBuffer {
    sysex_id: u16,
    frag_total: u8,
    received_mask: u32, // bit i set iff frag_idx i has been received
    fragments: [Vec<u8, MAX_FRAG_DATA_BYTES>; MAX_FRAGS_PER_SYSEX],
    last_seen: Instant,
}

impl SysExBuffer {
    fn new(sysex_id: u16, frag_total: u8, now: Instant) -> Self {
        Self {
            sysex_id,
            frag_total,
            received_mask: 0,
            fragments: core::array::from_fn(|_| Vec::new()),
            last_seen: now,
        }
    }

    fn complete_mask(&self) -> u32 {
        if self.frag_total == 32 {
            u32::MAX
        } else {
            (1u32 << self.frag_total) - 1
        }
    }

    fn is_complete(&self) -> bool {
        self.received_mask == self.complete_mask()
    }
}

#[derive(Default)]
pub struct SysExReassembler {
    buffers: Vec<SysExBuffer, MAX_CONCURRENT_SYSEX>,
}

impl SysExReassembler {
    pub const fn new() -> Self {
        Self {
            buffers: Vec::new(),
        }
    }

    pub fn reset_all(&mut self) {
        self.buffers.clear();
    }

    /// Process one fragment.  Returns `Complete(&[u8])` on the last
    /// missing fragment with a slice into `output` containing the full
    /// reassembled SysEx (F0..F7 inclusive).
    pub fn process_fragment<'a>(
        &mut self,
        sysex_id: u16,
        frag_idx: u8,
        frag_total: u8,
        data: &[u8],
        now: Instant,
        output: &'a mut [u8],
    ) -> SysExOutcome<'a> {
        // Garbage-collect stale buffers first.
        self.buffers
            .retain(|b| now.duration_since(b.last_seen) < REASSEMBLY_TIMEOUT);

        // Validate.
        if frag_total == 0
            || frag_idx >= frag_total
            || frag_total as usize > MAX_FRAGS_PER_SYSEX
            || data.len() > MAX_FRAG_DATA_BYTES
        {
            return SysExOutcome::Dropped;
        }

        // Find or create buffer.
        let pos = match self.buffers.iter().position(|b| b.sysex_id == sysex_id) {
            Some(i) => {
                // Sanity-check that frag_total matches.
                if self.buffers[i].frag_total != frag_total {
                    self.buffers.swap_remove(i);
                    return SysExOutcome::Dropped;
                }
                i
            }
            None => {
                if self.buffers.is_full() {
                    return SysExOutcome::Dropped;
                }
                let buf = SysExBuffer::new(sysex_id, frag_total, now);
                let _ = self.buffers.push(buf);
                self.buffers.len() - 1
            }
        };

        let bit = 1u32 << frag_idx;
        if self.buffers[pos].received_mask & bit != 0 {
            return SysExOutcome::Replay;
        }

        // Store fragment.
        let mut frag_data: Vec<u8, MAX_FRAG_DATA_BYTES> = Vec::new();
        if frag_data.extend_from_slice(data).is_err() {
            return SysExOutcome::Dropped;
        }
        self.buffers[pos].fragments[frag_idx as usize] = frag_data;
        self.buffers[pos].received_mask |= bit;
        self.buffers[pos].last_seen = now;

        if !self.buffers[pos].is_complete() {
            return SysExOutcome::Pending;
        }

        // Reassemble: F0 + concat(fragments) + F7.
        let buf = &self.buffers[pos];
        let total_data: usize = buf.fragments[..buf.frag_total as usize]
            .iter()
            .map(|f| f.len())
            .sum();
        let total_with_markers = total_data + 2;
        if output.len() < total_with_markers {
            // Caller didn't give us enough room.  Discard the buffer so
            // we don't repeatedly fail.
            self.buffers.swap_remove(pos);
            return SysExOutcome::Dropped;
        }
        output[0] = 0xF0;
        let mut offset = 1usize;
        for i in 0..buf.frag_total as usize {
            let frag = &buf.fragments[i];
            output[offset..offset + frag.len()].copy_from_slice(frag);
            offset += frag.len();
        }
        output[offset] = 0xF7;
        // Drop the buffer now that we're done with it.
        self.buffers.swap_remove(pos);
        SysExOutcome::Complete(&output[..total_with_markers])
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        Instant::from_ticks(0)
    }

    #[test]
    fn single_fragment_completes() {
        let mut r = SysExReassembler::new();
        let mut out = [0u8; MAX_SYSEX_BYTES];
        let r1 = r.process_fragment(0x42, 0, 1, &[0x7E, 0x7F, 0x06], now(), &mut out);
        match r1 {
            SysExOutcome::Complete(body) => {
                assert_eq!(body, &[0xF0, 0x7E, 0x7F, 0x06, 0xF7]);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn three_fragments_in_order() {
        let mut r = SysExReassembler::new();
        let mut out = [0u8; MAX_SYSEX_BYTES];
        assert_eq!(
            r.process_fragment(7, 0, 3, &[0x01, 0x02], now(), &mut out),
            SysExOutcome::Pending
        );
        assert_eq!(
            r.process_fragment(7, 1, 3, &[0x03, 0x04], now(), &mut out),
            SysExOutcome::Pending
        );
        match r.process_fragment(7, 2, 3, &[0x05], now(), &mut out) {
            SysExOutcome::Complete(body) => {
                assert_eq!(body, &[0xF0, 0x01, 0x02, 0x03, 0x04, 0x05, 0xF7]);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn three_fragments_out_of_order() {
        let mut r = SysExReassembler::new();
        let mut out = [0u8; MAX_SYSEX_BYTES];
        assert_eq!(
            r.process_fragment(9, 2, 3, &[0xCC], now(), &mut out),
            SysExOutcome::Pending
        );
        assert_eq!(
            r.process_fragment(9, 0, 3, &[0xAA], now(), &mut out),
            SysExOutcome::Pending
        );
        match r.process_fragment(9, 1, 3, &[0xBB], now(), &mut out) {
            SysExOutcome::Complete(body) => {
                assert_eq!(body, &[0xF0, 0xAA, 0xBB, 0xCC, 0xF7]);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_fragment_returns_replay() {
        let mut r = SysExReassembler::new();
        let mut out = [0u8; MAX_SYSEX_BYTES];
        assert_eq!(
            r.process_fragment(1, 0, 2, &[0xAA], now(), &mut out),
            SysExOutcome::Pending
        );
        assert_eq!(
            r.process_fragment(1, 0, 2, &[0xAA], now(), &mut out),
            SysExOutcome::Replay
        );
    }

    #[test]
    fn invalid_frag_idx_dropped() {
        let mut r = SysExReassembler::new();
        let mut out = [0u8; MAX_SYSEX_BYTES];
        assert_eq!(
            r.process_fragment(1, 5, 3, &[0xAA], now(), &mut out),
            SysExOutcome::Dropped
        );
    }

    #[test]
    fn zero_frag_total_dropped() {
        let mut r = SysExReassembler::new();
        let mut out = [0u8; MAX_SYSEX_BYTES];
        assert_eq!(
            r.process_fragment(1, 0, 0, &[0xAA], now(), &mut out),
            SysExOutcome::Dropped
        );
    }

    #[test]
    fn timeout_discards_buffer() {
        let mut r = SysExReassembler::new();
        let mut out = [0u8; MAX_SYSEX_BYTES];
        let t0 = Instant::from_ticks(0);
        assert_eq!(
            r.process_fragment(1, 0, 2, &[0xAA], t0, &mut out),
            SysExOutcome::Pending
        );
        // Advance past timeout.
        let t1 = t0 + REASSEMBLY_TIMEOUT + Duration::from_millis(1);
        // Process unrelated fragment — that triggers the GC sweep.
        assert_eq!(
            r.process_fragment(2, 0, 1, &[0xBB], t1, &mut out),
            SysExOutcome::Complete(&[0xF0, 0xBB, 0xF7])
        );
        // The original buffer (sysex_id=1) is gone — sending its fragment
        // 1 now creates a fresh buffer rather than completing.
        assert_eq!(
            r.process_fragment(1, 1, 2, &[0xCC], t1, &mut out),
            SysExOutcome::Pending
        );
    }

    #[test]
    fn two_concurrent_sysex() {
        let mut r = SysExReassembler::new();
        let mut out = [0u8; MAX_SYSEX_BYTES];
        assert_eq!(
            r.process_fragment(1, 0, 2, &[0xAA], now(), &mut out),
            SysExOutcome::Pending
        );
        assert_eq!(
            r.process_fragment(2, 0, 2, &[0xBB], now(), &mut out),
            SysExOutcome::Pending
        );
        // Complete sysex 1 first.
        match r.process_fragment(1, 1, 2, &[0xCC], now(), &mut out) {
            SysExOutcome::Complete(body) => assert_eq!(body, &[0xF0, 0xAA, 0xCC, 0xF7]),
            other => panic!("got {other:?}"),
        }
        // Sysex 2 still pending.
        match r.process_fragment(2, 1, 2, &[0xDD], now(), &mut out) {
            SysExOutcome::Complete(body) => assert_eq!(body, &[0xF0, 0xBB, 0xDD, 0xF7]),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn third_concurrent_sysex_dropped() {
        let mut r = SysExReassembler::new();
        let mut out = [0u8; MAX_SYSEX_BYTES];
        assert_eq!(
            r.process_fragment(1, 0, 2, &[0xAA], now(), &mut out),
            SysExOutcome::Pending
        );
        assert_eq!(
            r.process_fragment(2, 0, 2, &[0xBB], now(), &mut out),
            SysExOutcome::Pending
        );
        // Third concurrent SysEx — buffers full, dropped.
        assert_eq!(
            r.process_fragment(3, 0, 2, &[0xCC], now(), &mut out),
            SysExOutcome::Dropped
        );
    }
}
