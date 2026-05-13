// SPDX-License-Identifier: AGPL-3.0-or-later

//! Credit-based priority queue with same-priority + same-kind batching
//! and per-event seq assignment.
//!
//! `pop_packet` returns one packet's worth of consecutive front-of-queue
//! entries that share both priority and event kind.  Consumed entries
//! have their credits decremented and are requeued at the back of their
//! priority class until credits reach zero.  This combines:
//!
//! * **Round-robin redundancy** for free — the next pop picks up where
//!   the previous left off, giving each event multiple shots without
//!   blocking newer events behind older retransmits.
//! * **Same-priority batching** — chord notes that arrive in the same
//!   drain cycle pack into a single packet (zero wire-spread).
//! * **Cancellation across retransmits** — because entries stay in the
//!   queue until credits exhaust, a NoteOff arriving after the NoteOn's
//!   first transmit can still scrub the NoteOn's remaining copies.  The
//!   wire never carries a stale NoteOn after a NoteOff.
//!
//! On `push_channel_voice`, MIDI status semantics cancel stale queued
//! messages so the queue never holds opposing-state ghosts:
//!
//! | Incoming             | Cancels in queue                          |
//! |----------------------|-------------------------------------------|
//! | NoteOff note N       | NoteOns on same channel, same note        |
//! | NoteOn note N        | NoteOffs on same channel, same note       |
//! | PolyAftertouch note N| PolyAT on same channel, same note         |
//! | Control Change ctrl C| CC on same channel, same controller       |
//! | Program Change       | PC on same channel                        |
//! | Channel Pressure     | CP on same channel                        |
//! | Pitch Bend           | PB on same channel                        |
//!
//! Real-time (0xF8–0xFF) is never deduped (each carries unique tempo /
//! transport semantics).  Two NoteOns for the same note (without an
//! intervening NoteOff) also aren't deduped — those are legitimate
//! re-strikes with potentially different velocity.  Same for two NoteOffs.

use embassy_time::{Duration, Instant};
use heapless::Vec;
use osrf_protocols_midi_v1::MAX_FRAG_DATA_BYTES;

/// Maximum queued entries.  Sized for worst-case bursts (chord + PB +
/// pending retransmits + a multi-fragment SysEx in flight).
pub const QUEUE_CAPACITY: usize = 128;

/// Priority value used for SysEx fragments — lowest, never preempts
/// channel-voice traffic.
pub const SYSEX_PRIORITY: u8 = 0;

/// Priority value used for regular channel-voice / system-common messages.
pub const REGULAR_PRIORITY: u8 = 1;

/// Priority value for system real-time messages (0xF8–0xFF).  Preempts
/// everything else.
pub const REALTIME_PRIORITY: u8 = u8::MAX;

/// Default retransmit credits per logical event.  At 0.2 % per-packet RF
/// loss, K=3 → per-event miss rate (0.002)³ ≈ 8 × 10⁻⁹.
pub const DEFAULT_CREDITS: u8 = 3;

/// Maximum bytes per stored MIDI message.  Channel-voice is 1–3 bytes;
/// 4 leaves headroom for short System-Common messages.
pub const MAX_MSG_BYTES: usize = 4;

/// Time-spread retransmit offsets for channel-voice messages.  In
/// addition to the K=3 immediate retransmit through the main entry
/// (default credit-based round-robin delivery in ~3 ms), each push
/// also queues a single extra copy at each of these offsets.  This
/// protects against bursty RF interference up to ~30 ms long taking
/// out all three immediate copies — the +30 ms or +60 ms delayed copy
/// survives.  All copies share the original event's `event_seq`, so
/// the receiver's replay window dedups them; the sink sees each
/// logical event exactly once.
///
/// Applies to **all** channel-voice messages (NoteOn/Off, PolyAT, CC,
/// PC, Channel Pressure, Pitch Bend) — not just note-state.  Real-time
/// messages (status ≥ 0xF8) are excluded: they're frequent and
/// miss-tolerant, and adding 2× retransmits would block channel-voice
/// traffic.
///
/// Cancellation: `dedup_for_incoming` scans the *entire queue*
/// (eligible AND ineligible entries) when a new event is pushed.
/// Status-aware dedup removes superseded entries and their delayed
/// copies before they hit the wire:
/// * NoteOff cancels pending NoteOns for the same (ch, note); NoteOn
///   cancels pending NoteOffs.
/// * A new CC#X (ch, ctrl) cancels pending older CC#X on same channel.
/// * A new PC / CP / PB cancels older same-status on same channel.
///
/// For continuous controllers (mod wheel sweeps, pitch bend), this
/// means delayed copies of intermediate values are cancelled by newer
/// pushes — wire bandwidth in steady sweeps stays at K=3.  Only the
/// resting/final value's delayed copies survive to fire.
///
/// Single-antenna RX always receives wire packets in send order, so a
/// delayed copy can never arrive at RX *after* a superseding event for
/// the same key — either dedup removed it before it left the queue, or
/// it was on the wire before the superseder was pushed.
pub const DELAYED_RETRANSMITS_MS: &[u32] = &[30, 60];

/// Discriminates queue entry kinds.  Pop_packet only batches entries of
/// the same kind (channel-voice events never bundle with SysEx fragments).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueKind {
    ChannelVoice,
    SysExFragment,
}

#[derive(Clone)]
enum EntryPayload {
    ChannelVoice {
        event_seq: u16,
        midi: Vec<u8, MAX_MSG_BYTES>,
    },
    SysExFragment {
        sysex_id: u16,
        frag_idx: u8,
        frag_total: u8,
        data: Vec<u8, MAX_FRAG_DATA_BYTES>,
    },
}

#[derive(Clone)]
struct Entry {
    priority: u8,
    credits: u8,
    payload: EntryPayload,
    /// Earliest time this entry is eligible to be popped.  `None` means
    /// "always eligible".  Used by the time-spread NoteOff retransmits:
    /// the +30 ms and +60 ms copies have `next_eligible` set so they
    /// stay in the queue but don't pop until their time arrives.
    next_eligible: Option<Instant>,
}

impl Entry {
    fn kind(&self) -> QueueKind {
        match &self.payload {
            EntryPayload::ChannelVoice { .. } => QueueKind::ChannelVoice,
            EntryPayload::SysExFragment { .. } => QueueKind::SysExFragment,
        }
    }

    fn is_eligible(&self, now: Instant) -> bool {
        match self.next_eligible {
            None => true,
            Some(due) => now >= due,
        }
    }
}

/// What a successful `pop_packet` returns: the discriminator (so the
/// caller knows which `EventType` to set in the header) plus the number
/// of body bytes written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoppedPacket {
    pub kind: QueueKind,
    pub body_len: usize,
}

/// Priority-ordered MIDI / SysEx transmit queue with status-aware dedup
/// and credit-based round-robin retransmit.
pub struct MidiTxQueue {
    entries: Vec<Entry, QUEUE_CAPACITY>,
    next_event_seq: u16,
    next_sysex_id: u16,
}

impl MidiTxQueue {
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_event_seq: 0,
            next_sysex_id: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Push a channel-voice MIDI message.  Applies status-aware dedup,
    /// assigns a fresh `event_seq`, inserts by priority + FIFO.  Returns
    /// `false` if the queue is full.  Empty / oversize messages are
    /// silently ignored.
    ///
    /// `now` is the current `embassy_time::Instant`, used to schedule
    /// the time-spread retransmit copies (see
    /// [`DELAYED_RETRANSMITS_MS`]).
    pub fn push_channel_voice(&mut self, midi: &[u8], now: Instant) -> bool {
        if midi.is_empty() || midi.len() > MAX_MSG_BYTES {
            return false;
        }
        let status = midi[0];

        let (priority, credits) = if status >= 0xF8 {
            // System real-time: jitter-sensitive, preempts everything.
            // K=1 — these messages are frequent and miss-tolerant
            // (Timing Clock fires 24× per quarter note, Active Sensing
            // every 300 ms; one-off misses don't accumulate).  Sending
            // K=3 would block channel-voice traffic for K extra packets
            // every time a real-time message arrives, hurting chord and
            // pitch-bend latency for marginal redundancy benefit.
            (REALTIME_PRIORITY, 1)
        } else {
            self.dedup_for_incoming(status, midi);
            (REGULAR_PRIORITY, DEFAULT_CREDITS)
        };

        let event_seq = self.alloc_event_seq();
        let mut bytes: Vec<u8, MAX_MSG_BYTES> = Vec::new();
        // SAFETY: midi.len() <= MAX_MSG_BYTES (checked above).
        let _ = bytes.extend_from_slice(midi);
        let main_ok = self
            .insert_by_priority(Entry {
                priority,
                credits,
                payload: EntryPayload::ChannelVoice {
                    event_seq,
                    midi: bytes.clone(),
                },
                next_eligible: None,
            })
            .is_ok();

        // For all channel-voice messages, queue extra delayed copies
        // that fire later in time.  Protects against bursty
        // interference taking out all 3 immediate retransmits.  All
        // copies share the same `event_seq` so the receiver's replay
        // window dedups them and the sink fires each logical event
        // exactly once.
        //
        // Status-aware dedup (`dedup_for_incoming`) cancels delayed
        // copies of superseded events before they hit the wire — a
        // newer NoteOff wipes pending NoteOns, a newer CC#X wipes
        // older CC#X on same channel, etc.  Single-antenna RX
        // processes packets in send order, so a delayed copy can
        // never arrive after a superseder for the same key.
        //
        // For continuous controllers (mod wheel sweeps, pitch bend)
        // the rapid succession of pushes cancels intermediate delayed
        // copies — only the resting/final value's delayed copies
        // survive to fire, keeping steady-sweep wire bandwidth at K=3.
        //
        // If the queue is full when adding a delayed copy, we silently
        // skip it — the main K=3 still ships and is the primary
        // delivery guarantee.
        if main_ok {
            for &delay_ms in DELAYED_RETRANSMITS_MS {
                let due = now + Duration::from_millis(delay_ms as u64);
                let _ = self.insert_by_priority(Entry {
                    priority,
                    credits: 1,
                    payload: EntryPayload::ChannelVoice {
                        event_seq,
                        midi: bytes.clone(),
                    },
                    next_eligible: Some(due),
                });
            }
        }

        main_ok
    }

    /// Fragment a complete SysEx body (without F0/F7) and queue all
    /// fragments at `SYSEX_PRIORITY`.  Returns the assigned `sysex_id`,
    /// or `None` if the queue can't fit all fragments.
    pub fn push_sysex(&mut self, sysex_body: &[u8]) -> Option<u16> {
        if sysex_body.is_empty() {
            return None;
        }
        let frag_total_usize = sysex_body.len().div_ceil(MAX_FRAG_DATA_BYTES);
        if frag_total_usize == 0 || frag_total_usize > 255 {
            return None;
        }
        let frag_total = frag_total_usize as u8;
        if QUEUE_CAPACITY - self.entries.len() < frag_total_usize {
            return None;
        }
        let sysex_id = self.alloc_sysex_id();
        for (frag_idx, chunk) in sysex_body.chunks(MAX_FRAG_DATA_BYTES).enumerate() {
            let mut data: Vec<u8, MAX_FRAG_DATA_BYTES> = Vec::new();
            let _ = data.extend_from_slice(chunk);
            let _ = self.insert_by_priority(Entry {
                priority: SYSEX_PRIORITY,
                credits: DEFAULT_CREDITS,
                payload: EntryPayload::SysExFragment {
                    sysex_id,
                    frag_idx: frag_idx as u8,
                    frag_total,
                    data,
                },
                next_eligible: None,
            });
        }
        Some(sysex_id)
    }

    /// Pop one packet's worth of front-of-queue entries that share both
    /// priority AND kind.  Writes the body bytes to `out` and returns
    /// `(kind, len)`.  Consumed entries are decremented; survivors
    /// (credits > 0) are requeued at the back of their priority class.
    /// Returns `None` if no eligible entries are in the queue at `now`.
    ///
    /// `now` is the current `embassy_time::Instant`; entries with a
    /// future `next_eligible` are skipped (they wait their turn — see
    /// [`DELAYED_RETRANSMITS_MS`]).
    ///
    /// Body format:
    /// * `ChannelVoice`: `[event_seq:2][midi:1..3] [event_seq:2][midi:1..3] ...`
    /// * `SysExFragment`: `[sysex_id:2][frag_idx:1][frag_total:1][data]`
    ///   (always exactly one fragment per packet)
    pub fn pop_packet(&mut self, now: Instant, out: &mut [u8]) -> Option<PoppedPacket> {
        // Find the first eligible entry — that establishes the priority
        // and kind we'll batch in this packet.  Ineligible entries
        // (delayed retransmits not yet due) are silently skipped; they
        // remain in the queue for a later pop.
        let pivot = self.entries.iter().position(|e| e.is_eligible(now))?;
        let target_priority = self.entries[pivot].priority;
        let target_kind = self.entries[pivot].kind();
        let mut written = 0usize;
        let mut consumed_indices: Vec<usize, QUEUE_CAPACITY> = Vec::new();

        // Walk forward from the pivot.  Take eligible entries that share
        // priority + kind.  Skip ineligible same-priority+kind entries
        // (they stay in the queue).  Stop on priority/kind change — that
        // marks the end of the current "batch class".
        for i in pivot..self.entries.len() {
            let entry = &self.entries[i];
            if entry.priority != target_priority || entry.kind() != target_kind {
                break;
            }
            if !entry.is_eligible(now) {
                continue;
            }
            let needed = match &entry.payload {
                EntryPayload::ChannelVoice { midi, .. } => 2 + midi.len(),
                EntryPayload::SysExFragment { data, .. } => 4 + data.len(),
            };
            if written + needed > out.len() {
                break;
            }
            match &entry.payload {
                EntryPayload::ChannelVoice { event_seq, midi } => {
                    out[written..written + 2].copy_from_slice(&event_seq.to_be_bytes());
                    out[written + 2..written + 2 + midi.len()].copy_from_slice(midi);
                }
                EntryPayload::SysExFragment {
                    sysex_id,
                    frag_idx,
                    frag_total,
                    data,
                } => {
                    out[written..written + 2].copy_from_slice(&sysex_id.to_be_bytes());
                    out[written + 2] = *frag_idx;
                    out[written + 3] = *frag_total;
                    out[written + 4..written + 4 + data.len()].copy_from_slice(data);
                }
            }
            written += needed;
            let _ = consumed_indices.push(i);

            // SysEx is always one fragment per packet.
            if matches!(target_kind, QueueKind::SysExFragment) {
                break;
            }
        }

        if consumed_indices.is_empty() {
            return None;
        }

        // Decrement credits and split into "still alive" vs "drained".
        // Remove from highest index to lowest so earlier indices stay
        // valid during the loop.  Then reverse the collected survivors
        // so they're re-inserted in their original consumption order
        // (oldest first), preserving FIFO at the back of the priority
        // class — important for round-robin retransmit behavior.
        let mut requeue: Vec<Entry, QUEUE_CAPACITY> = Vec::new();
        for &idx in consumed_indices.iter().rev() {
            let mut entry = self.entries.remove(idx);
            entry.credits = entry.credits.saturating_sub(1);
            if entry.credits > 0 {
                let _ = requeue.push(entry);
            }
        }
        for entry in requeue.into_iter().rev() {
            let _ = self.insert_by_priority(entry);
        }

        Some(PoppedPacket {
            kind: target_kind,
            body_len: written,
        })
    }

    /// Earliest time at which `pop_packet` could plausibly produce a
    /// packet, based on the next_eligible deadlines of currently-queued
    /// entries.  `Some(deadline)` if some entries are queued but all
    /// ineligible (so a later `pop_packet(now)` will succeed once `now`
    /// reaches the deadline).  `None` if the queue is empty or has at
    /// least one entry already eligible.  Used by `run_tx` to decide
    /// how long to wait before retrying.
    pub fn next_eligibility(&self) -> Option<Instant> {
        let mut earliest: Option<Instant> = None;
        let mut any_eligible = false;
        for e in self.entries.iter() {
            match e.next_eligible {
                None => {
                    any_eligible = true;
                    break;
                }
                Some(due) => {
                    earliest = Some(match earliest {
                        Some(prev) if prev < due => prev,
                        _ => due,
                    });
                }
            }
        }
        if any_eligible {
            None
        } else {
            earliest
        }
    }

    fn alloc_event_seq(&mut self) -> u16 {
        let s = self.next_event_seq;
        self.next_event_seq = self.next_event_seq.wrapping_add(1);
        s
    }

    fn alloc_sysex_id(&mut self) -> u16 {
        let s = self.next_sysex_id;
        self.next_sysex_id = self.next_sysex_id.wrapping_add(1);
        s
    }

    /// Apply MIDI status-aware dedup.  Removes any channel-voice entries
    /// that the incoming message would supersede.
    fn dedup_for_incoming(&mut self, status: u8, msg: &[u8]) {
        let high = status & 0xF0;
        let ch = status & 0x0F;
        let d1 = msg.get(1).copied().unwrap_or(0);

        self.entries.retain(|e| match &e.payload {
            EntryPayload::ChannelVoice { midi, .. } => {
                let s = midi.first().copied().unwrap_or(0);
                let m_high = s & 0xF0;
                let m_ch = s & 0x0F;
                let m_d1 = midi.get(1).copied().unwrap_or(0);
                match high {
                    // NoteOff cancels NoteOn for same note (and vice versa).
                    0x80 => !(m_high == 0x90 && m_ch == ch && m_d1 == d1),
                    0x90 => !(m_high == 0x80 && m_ch == ch && m_d1 == d1),
                    // PolyAT, CC: cancel same channel + same note/ctrl.
                    0xA0 => !(m_high == 0xA0 && m_ch == ch && m_d1 == d1),
                    0xB0 => !(m_high == 0xB0 && m_ch == ch && m_d1 == d1),
                    // PC, CP, PB: cancel same channel.
                    0xC0 | 0xD0 | 0xE0 => !(m_high == high && m_ch == ch),
                    _ => true,
                }
            }
            EntryPayload::SysExFragment { .. } => true,
        });
    }

    /// Insert at the position that keeps the queue sorted by descending
    /// priority, with FIFO within the same level.
    fn insert_by_priority(&mut self, entry: Entry) -> Result<(), Entry> {
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

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use osrf_protocols_midi_v1::{parse_sysex_fragment, ChannelVoiceIter};

    /// Reference time for tests that don't care about the time-spread
    /// NoteOff retransmits.  At `T0`, only the immediate-eligible
    /// (`next_eligible: None`) entries pop — the +30 ms / +60 ms NoteOff
    /// copies stay in the queue.  Tests that need to drain those use
    /// `T_FAR_FUTURE`.
    const T0: Instant = Instant::from_ticks(0);
    /// "Pop everything" sentinel — far enough in the future that every
    /// queued entry is eligible.  Used by the legacy tests via
    /// `drain_at(q, T_FAR_FUTURE)` so the addition of delayed NoteOff
    /// copies doesn't change their packet counts.
    const T_FAR_FUTURE: Instant = Instant::from_ticks(1_000_000_000);

    /// Helper: drain all eligible packets at `now`, returning a Vec of
    /// (kind, body_bytes).
    fn drain_at(
        q: &mut MidiTxQueue,
        now: Instant,
    ) -> std::vec::Vec<(QueueKind, std::vec::Vec<u8>)> {
        let mut out = std::vec::Vec::new();
        let mut buf = [0u8; 64];
        while let Some(pkt) = q.pop_packet(now, &mut buf) {
            out.push((pkt.kind, buf[..pkt.body_len].to_vec()));
        }
        out
    }

    /// Drain at `T_FAR_FUTURE` so all delayed retransmits also fire.
    fn drain(q: &mut MidiTxQueue) -> std::vec::Vec<(QueueKind, std::vec::Vec<u8>)> {
        drain_at(q, T_FAR_FUTURE)
    }

    /// Decode one ChannelVoice packet body into a Vec of (event_seq, midi_bytes).
    fn decode_cv(body: &[u8]) -> std::vec::Vec<(u16, std::vec::Vec<u8>)> {
        ChannelVoiceIter::new(body)
            .map(|r| {
                let (s, m) = r.unwrap();
                (s, m.to_vec())
            })
            .collect()
    }

    // ── Basic batching ───────────────────────────────────────────────────

    #[test]
    fn chord_batches_into_one_packet() {
        let mut q = MidiTxQueue::new();
        assert!(q.push_channel_voice(&[0x90, 60, 100], T0));
        assert!(q.push_channel_voice(&[0x90, 64, 100], T0));
        assert!(q.push_channel_voice(&[0x90, 67, 100], T0));
        // Each NoteOn push creates main(K=3) + d_30(K=1) + d_60(K=1) =
        // 3 entries.  3 NoteOns = 9 queued entries.  At T_FAR_FUTURE
        // every entry is eligible, so:
        //   Pop 1: all 9 entries batch into one packet (9 events).
        //          main → K=2, delayed → K=0 (drained).
        //   Pop 2: 3 main entries (K=2) → K=1.
        //   Pop 3: 3 main entries (K=1) → K=0.
        let packets = drain(&mut q);
        assert_eq!(packets.len(), 3);
        let p0 = decode_cv(&packets[0].1);
        let p1 = decode_cv(&packets[1].1);
        let p2 = decode_cv(&packets[2].1);
        assert_eq!(p0.len(), 9, "first packet bundles main + delayed copies");
        assert_eq!(p1.len(), 3, "second packet is K=2 main retransmit");
        assert_eq!(p2.len(), 3, "third packet is K=1 main retransmit");
        // Verify the chord notes appear in p1 (and p2) in input order.
        assert_eq!(p1[0].1, vec![0x90, 60, 100]);
        assert_eq!(p1[1].1, vec![0x90, 64, 100]);
        assert_eq!(p1[2].1, vec![0x90, 67, 100]);
        // p1 and p2 share event_seqs (K=2 and K=1 of the same logical events).
        assert_eq!(p1[0].0, p2[0].0);
        assert_eq!(p1[1].0, p2[1].0);
        assert_eq!(p1[2].0, p2[2].0);
        // p0 has 3 distinct event_seqs (one per push), each appearing
        // 3 times (main + d_30 + d_60).
        let mut seqs: std::vec::Vec<u16> = p0.iter().map(|(s, _)| *s).collect();
        seqs.sort();
        let unique: std::vec::Vec<u16> = {
            let mut u = seqs.clone();
            u.dedup();
            u
        };
        assert_eq!(unique.len(), 3, "3 distinct event_seqs in batched packet");
        // p1's main-retransmit seqs should be the same 3 distinct values.
        let mut p1_seqs: std::vec::Vec<u16> = p1.iter().map(|(s, _)| *s).collect();
        p1_seqs.sort();
        assert_eq!(p1_seqs, unique);
    }

    #[test]
    fn realtime_preempts_chord() {
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x90, 60, 100], T0);
        q.push_channel_voice(&[0x90, 64, 100], T0);
        q.push_channel_voice(&[0x90, 67, 100], T0);
        q.push_channel_voice(&[0xF8], T0); // Timing Clock — preempts everything
        let mut buf = [0u8; 64];
        // First pop: TC alone (real-time priority is its own batch).
        let pkt = q.pop_packet(T0, &mut buf).unwrap();
        let events = decode_cv(&buf[..pkt.body_len]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1, vec![0xF8]);
        // Second pop: chord at REGULAR_PRIORITY.
        let pkt = q.pop_packet(T0, &mut buf).unwrap();
        let events = decode_cv(&buf[..pkt.body_len]);
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn empty_queue_pops_none() {
        let mut q = MidiTxQueue::new();
        let mut buf = [0u8; 64];
        assert!(q.pop_packet(T0, &mut buf).is_none());
    }

    #[test]
    fn buffer_too_small_takes_partial_batch() {
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x90, 60, 100], T0);
        q.push_channel_voice(&[0x90, 64, 100], T0);
        q.push_channel_voice(&[0x90, 67, 100], T0);
        // 11 bytes fits 2 events (5 + 5 = 10, third would need 5 more).
        let mut buf = [0u8; 11];
        let pkt = q.pop_packet(T0, &mut buf).unwrap();
        let events = decode_cv(&buf[..pkt.body_len]);
        assert_eq!(events.len(), 2);
    }

    // ── Credit-based retransmit ──────────────────────────────────────────

    #[test]
    fn each_event_transmits_default_credits_times() {
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x90, 60, 100], T0);
        let packets = drain(&mut q);
        assert_eq!(packets.len(), DEFAULT_CREDITS as usize);
        // Each retransmit has the same event_seq.
        let seqs: std::vec::Vec<u16> = packets
            .iter()
            .map(|(_, body)| decode_cv(body)[0].0)
            .collect();
        assert!(seqs.iter().all(|&s| s == seqs[0]));
    }

    #[test]
    fn new_event_after_pop_goes_to_back() {
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x90, 60, 100], T0); // C, seq=N
        let mut buf = [0u8; 64];
        let _ = q.pop_packet(T0, &mut buf).unwrap(); // C copy 1, C now at K=2
        q.push_channel_voice(&[0x90, 64, 100], T0); // E, seq=N+1, behind C
                                                    // Next pop batches both C(K=2) and E(K=3) since same priority+kind.
        let pkt = q.pop_packet(T0, &mut buf).unwrap();
        let events = decode_cv(&buf[..pkt.body_len]);
        assert_eq!(events.len(), 2);
        // Order: C first (older), E second.
        assert_eq!(events[0].1, vec![0x90, 60, 100]);
        assert_eq!(events[1].1, vec![0x90, 64, 100]);
    }

    // ── Cancellation ─────────────────────────────────────────────────────

    #[test]
    fn note_off_cancels_pending_note_on_in_queue() {
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x90, 60, 100], T0); // NoteOn C
        q.push_channel_voice(&[0x90, 64, 100], T0); // NoteOn E
        q.push_channel_voice(&[0x80, 60, 0], T0); // NoteOff C — cancels NoteOn C
        let packets = drain(&mut q);
        // Inspect first packet — NoteOn C must be absent.
        let first_events = decode_cv(&packets[0].1);
        assert!(!first_events.iter().any(|(_, m)| m == &vec![0x90, 60, 100]));
        // E and Off-C should both be present.
        assert!(first_events.iter().any(|(_, m)| m == &vec![0x90, 64, 100]));
        assert!(first_events.iter().any(|(_, m)| m == &vec![0x80, 60, 0]));
    }

    #[test]
    fn note_off_cancels_partially_transmitted_note_on() {
        // The key correctness property: cancel works EVEN AFTER the
        // NoteOn has been popped once (it's still in the queue at K=2).
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x90, 60, 100], T0);
        let mut buf = [0u8; 64];
        let _ = q.pop_packet(T0, &mut buf).unwrap(); // first transmit, K=2 left
        q.push_channel_voice(&[0x80, 60, 0], T0); // cancel
        let packets = drain(&mut q);
        // None of the remaining packets should contain NoteOn C.
        for (_, body) in &packets {
            for (_, midi) in decode_cv(body) {
                assert_ne!(
                    midi,
                    vec![0x90, 60, 100],
                    "stale NoteOn(C) survived NoteOff in remaining retransmits"
                );
            }
        }
        // Off-C should appear.
        let mut saw_off = false;
        for (_, body) in &packets {
            for (_, midi) in decode_cv(body) {
                if midi == [0x80, 60, 0] {
                    saw_off = true;
                }
            }
        }
        assert!(saw_off);
    }

    #[test]
    fn note_on_cancels_pending_note_off_same_note() {
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x80, 60, 0], T0); // NoteOff C
        q.push_channel_voice(&[0x90, 60, 100], T0); // NoteOn C — cancels NoteOff
        let packets = drain(&mut q);
        for (_, body) in &packets {
            for (_, midi) in decode_cv(body) {
                assert_ne!(midi, vec![0x80, 60, 0]);
            }
        }
    }

    #[test]
    fn cc_overrides_pending_cc_same_controller() {
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0xB0, 7, 50], T0); // Volume 50
        q.push_channel_voice(&[0xB0, 7, 90], T0); // Volume 90 — cancels 50
        q.push_channel_voice(&[0xB0, 64, 127], T0); // Sustain — different ctrl, kept
        let packets = drain(&mut q);
        for (_, body) in &packets {
            for (_, midi) in decode_cv(body) {
                if midi.starts_with(&[0xB0, 7]) {
                    assert_eq!(midi[2], 90, "stale Volume value survived");
                }
            }
        }
    }

    #[test]
    fn pitch_bend_overrides_pending_pitch_bend() {
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0xE0, 0, 64], T0);
        q.push_channel_voice(&[0xE0, 0x40, 0x70], T0);
        let packets = drain(&mut q);
        for (_, body) in &packets {
            for (_, midi) in decode_cv(body) {
                if midi[0] == 0xE0 {
                    assert_eq!((midi[1], midi[2]), (0x40, 0x70));
                }
            }
        }
    }

    #[test]
    fn note_on_not_deduped_against_other_note_on() {
        // Re-strike: same note pressed twice without intervening NoteOff.
        // Each push creates 3 entries (main K=3 + 2 delayed K=1) sharing
        // the push's event_seq, so 2 pushes = 6 entries with 2 distinct
        // event_seqs.  At T_FAR_FUTURE the first pop batches all 6
        // entries' bytes, so the first packet has 6 NoteOn(60) events
        // total.
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x90, 60, 100], T0);
        q.push_channel_voice(&[0x90, 60, 100], T0);
        let packets = drain(&mut q);
        let events = decode_cv(&packets[0].1);
        let on_count = events
            .iter()
            .filter(|(_, m)| m == &vec![0x90, 60, 100])
            .count();
        assert_eq!(on_count, 6, "both pushes' main + 2 delayed copies = 6");
        // The events should carry exactly 2 distinct event_seqs (one per
        // push), since main and delayed copies share their push's seq.
        let mut seqs: std::vec::Vec<u16> = events.iter().map(|(s, _)| *s).collect();
        seqs.sort();
        seqs.dedup();
        assert_eq!(seqs.len(), 2);
    }

    #[test]
    fn different_channels_dont_interfere() {
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0xB0, 7, 50], T0); // ch 0
        q.push_channel_voice(&[0xB1, 7, 90], T0); // ch 1
        let packets = drain(&mut q);
        let events = decode_cv(&packets[0].1);
        // Each push gets main + 2 delayed copies sharing one event_seq;
        // dedup is per-channel so neither push cancels the other.
        assert_eq!(events.len(), 6, "2 pushes × 3 entries each = 6");
        let mut seqs: std::vec::Vec<u16> = events.iter().map(|(s, _)| *s).collect();
        seqs.sort();
        seqs.dedup();
        assert_eq!(seqs.len(), 2, "distinct event_seqs preserved");
    }

    // ── Real-time messages don't get deduped ─────────────────────────────

    #[test]
    fn timing_clocks_dont_dedup() {
        let mut q = MidiTxQueue::new();
        for _ in 0..3 {
            q.push_channel_voice(&[0xF8], T0);
        }
        let mut buf = [0u8; 64];
        let pkt = q.pop_packet(T0, &mut buf).unwrap();
        let events = decode_cv(&buf[..pkt.body_len]);
        // Three TCs all batched at REALTIME_PRIORITY, distinct event_seqs.
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|(_, m)| m == &vec![0xF8]));
        // Verify event_seqs are distinct.
        let seqs: std::vec::Vec<u16> = events.iter().map(|(s, _)| *s).collect();
        assert_ne!(seqs[0], seqs[1]);
        assert_ne!(seqs[1], seqs[2]);
    }

    // ── SysEx ────────────────────────────────────────────────────────────

    #[test]
    fn small_sysex_fits_in_one_fragment() {
        let mut q = MidiTxQueue::new();
        let body = [0x7E, 0x7F, 0x06, 0x01]; // GM Inquiry
        let id = q.push_sysex(&body).unwrap();
        let mut buf = [0u8; 64];
        let pkt = q.pop_packet(T0, &mut buf).unwrap();
        assert_eq!(pkt.kind, QueueKind::SysExFragment);
        let parts = parse_sysex_fragment(&buf[..pkt.body_len]).unwrap();
        assert_eq!(parts.sysex_id, id);
        assert_eq!(parts.frag_idx, 0);
        assert_eq!(parts.frag_total, 1);
        assert_eq!(parts.data, &body);
    }

    #[test]
    fn large_sysex_splits_into_fragments() {
        let mut q = MidiTxQueue::new();
        // 100 bytes → 3 fragments at 49 bytes/frag.
        let body: std::vec::Vec<u8> = (0..100).map(|i| i as u8).collect();
        let id = q.push_sysex(&body).unwrap();
        let mut buf = [0u8; 64];
        let mut all = std::vec::Vec::new();
        // Drain first round (one fragment per packet).
        let mut seen_idxs = std::vec::Vec::new();
        loop {
            let Some(pkt) = q.pop_packet(T0, &mut buf) else {
                break;
            };
            assert_eq!(pkt.kind, QueueKind::SysExFragment);
            let parts = parse_sysex_fragment(&buf[..pkt.body_len]).unwrap();
            assert_eq!(parts.sysex_id, id);
            assert_eq!(parts.frag_total, 3);
            seen_idxs.push(parts.frag_idx);
            all.push((parts.frag_idx, parts.data.to_vec()));
        }
        // 3 fragments × 3 retransmits = 9 packets.
        assert_eq!(all.len(), 9);
        // First-round indices: 0, 1, 2 in some order.
        let mut first_round_idxs: std::vec::Vec<u8> = seen_idxs[0..3].to_vec();
        first_round_idxs.sort();
        assert_eq!(first_round_idxs, vec![0, 1, 2]);
    }

    #[test]
    fn channel_voice_preempts_sysex() {
        let mut q = MidiTxQueue::new();
        q.push_sysex(&[0x7E, 0x7F]);
        q.push_channel_voice(&[0x90, 60, 100], T0);
        let mut buf = [0u8; 64];
        // First pop: ChannelVoice (REGULAR_PRIORITY > SYSEX_PRIORITY).
        let pkt = q.pop_packet(T0, &mut buf).unwrap();
        assert_eq!(pkt.kind, QueueKind::ChannelVoice);
    }

    #[test]
    fn sysex_doesnt_bundle_with_channel_voice() {
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x90, 60, 100], T0);
        q.push_sysex(&[0x7E, 0x7F]);
        let mut buf = [0u8; 64];
        let pkt = q.pop_packet(T0, &mut buf).unwrap();
        assert_eq!(pkt.kind, QueueKind::ChannelVoice); // CV first
        let pkt = q.pop_packet(T0, &mut buf).unwrap();
        // Eventually CV's K=3 finishes and SysEx pops.
        if pkt.kind == QueueKind::ChannelVoice {
            // Drain remaining CV retransmits.
            let _ = q.pop_packet(T0, &mut buf);
            let pkt = q.pop_packet(T0, &mut buf).unwrap();
            assert_eq!(pkt.kind, QueueKind::SysExFragment);
        }
    }

    // ── Time-spread NoteOff retransmits ──────────────────────────────────

    #[test]
    fn note_off_pushes_main_plus_delayed_copies() {
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x80, 60, 0], T0);
        // 1 main entry + 2 delayed copies = 3 entries in the queue.
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn delayed_note_off_copies_dont_pop_before_their_time() {
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x80, 60, 0], T0);
        // At T0, only the main entry's K=3 retransmits are eligible.
        // Drain those 3 and the queue should return None — the delayed
        // copies are still ineligible.
        let packets = drain_at(&mut q, T0);
        assert_eq!(packets.len(), 3, "expected K=3 main retransmits at T0");
        assert!(!q.is_empty(), "delayed copies should still be queued");
        assert_eq!(q.len(), 2, "exactly 2 delayed copies left");
    }

    #[test]
    fn delayed_note_off_copies_fire_after_their_deadline() {
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x80, 60, 0], T0);
        // Drain main K=3 at T0.
        let _ = drain_at(&mut q, T0);
        assert_eq!(q.len(), 2);

        // At T0 + 30 ms, the +30 ms delayed copy is eligible.  Pop it.
        let t30 = T0 + Duration::from_millis(30);
        let packets = drain_at(&mut q, t30);
        assert_eq!(packets.len(), 1, "+30 ms delayed copy fires");
        assert_eq!(q.len(), 1, "the +60 ms copy still pending");

        // At T0 + 60 ms, the +60 ms copy is eligible too.
        let t60 = T0 + Duration::from_millis(60);
        let packets = drain_at(&mut q, t60);
        assert_eq!(packets.len(), 1, "+60 ms delayed copy fires");
        assert!(q.is_empty(), "all NoteOff copies drained");
    }

    #[test]
    fn delayed_note_off_copies_share_event_seq_with_main() {
        // All 5 wire packets (K=3 main + 2 delayed) carry the SAME
        // event_seq so the receiver's replay window dedups them and the
        // sink fires NoteOff exactly once.
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x80, 60, 0], T0);

        let mut buf = [0u8; 64];
        // First pop: main K=3 → K=2.
        let pkt = q.pop_packet(T0, &mut buf).unwrap();
        let main_seq = decode_cv(&buf[..pkt.body_len])[0].0;

        // Drain rest of main retransmits at T0.
        let _ = drain_at(&mut q, T0); // main K=2, K=1

        // Pop the +30 ms delayed copy in isolation.
        let t30 = T0 + Duration::from_millis(30);
        let pkt30 = q.pop_packet(t30, &mut buf).unwrap();
        let events_30 = decode_cv(&buf[..pkt30.body_len]);
        assert_eq!(events_30.len(), 1);
        assert_eq!(events_30[0].0, main_seq, "+30 ms copy must share event_seq");
        assert_eq!(events_30[0].1, vec![0x80, 60, 0]);

        // Pop the +60 ms delayed copy.
        let t60 = T0 + Duration::from_millis(60);
        let pkt60 = q.pop_packet(t60, &mut buf).unwrap();
        let events_60 = decode_cv(&buf[..pkt60.body_len]);
        assert_eq!(events_60.len(), 1);
        assert_eq!(events_60[0].0, main_seq, "+60 ms copy must share event_seq");
        assert_eq!(events_60[0].1, vec![0x80, 60, 0]);
    }

    #[test]
    fn note_on_cancels_pending_delayed_note_off_copies() {
        // Rapid release-then-restrike: NoteOff queues main + 2 delayed.
        // A NoteOn for the same note before the delayed copies fire
        // must remove them — otherwise a delayed NoteOff would arrive
        // at the receiver AFTER the NoteOn and turn the new strike off.
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x80, 60, 0], T0); // NoteOff C
        assert_eq!(q.len(), 3);

        // Drain main K=3 (the NoteOff packets that already went out).
        let _ = drain_at(&mut q, T0);
        assert_eq!(q.len(), 2, "delayed NoteOff copies still pending");

        // Restrike before the delayed NoteOffs fire.  This NoteOn
        // cancels the pending delayed NoteOffs and itself adds 3
        // entries (main K=3 + 2 delayed copies).
        q.push_channel_voice(&[0x90, 60, 100], T0); // NoteOn C
        assert_eq!(
            q.len(),
            3,
            "pending delayed NoteOffs cancelled; new NoteOn adds 3 entries"
        );

        // Drain everything at far future — no stale NoteOffs survive.
        let packets = drain_at(&mut q, T_FAR_FUTURE);
        for (_, body) in &packets {
            for (_, midi) in decode_cv(body) {
                assert_ne!(
                    midi,
                    vec![0x80, 60, 0],
                    "stale delayed NoteOff survived NoteOn restrike"
                );
            }
        }
    }

    #[test]
    fn note_on_for_different_note_doesnt_cancel_delayed_note_off() {
        // NoteOff C queues delayed copies.  NoteOn for E (different
        // note) must NOT cancel them — they're for a different note.
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x80, 60, 0], T0); // NoteOff C
        let _ = drain_at(&mut q, T0); // main K=3
        assert_eq!(q.len(), 2, "delayed NoteOff C copies still pending");

        // NoteOn E adds main + 2 delayed = 3 entries; doesn't touch
        // the pending NoteOff C copies (different note).
        q.push_channel_voice(&[0x90, 64, 100], T0);
        assert_eq!(
            q.len(),
            5,
            "2 delayed NoteOff C + 3 NoteOn E entries (main + 2 delayed)"
        );

        // The delayed NoteOff C copies still fire at their times.
        let packets = drain_at(&mut q, T_FAR_FUTURE);
        let mut saw_off_c = false;
        for (_, body) in &packets {
            for (_, midi) in decode_cv(body) {
                if midi == [0x80, 60, 0] {
                    saw_off_c = true;
                }
            }
        }
        assert!(saw_off_c, "delayed NoteOff C should still fire");
    }

    #[test]
    fn note_off_chord_release_queues_delayed_copies_per_note() {
        // Releasing a 3-note chord pushes 3 NoteOffs.  Each gets its
        // own main + 2 delayed = 9 entries total.  All 9 stay queued
        // and fire at appropriate times.
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x80, 60, 0], T0);
        q.push_channel_voice(&[0x80, 64, 0], T0);
        q.push_channel_voice(&[0x80, 67, 0], T0);
        assert_eq!(q.len(), 9, "3 NoteOffs × (1 main + 2 delayed)");

        // Drain main batch at T0 — all 3 mains batched together × K=3.
        let main_packets = drain_at(&mut q, T0);
        assert_eq!(main_packets.len(), 3, "K=3 retransmits of the chord-off");
        for (_, body) in &main_packets {
            let events = decode_cv(body);
            assert_eq!(events.len(), 3, "each packet has all 3 NoteOffs");
        }

        // 6 delayed copies still waiting (3 NoteOffs × 2 delays each).
        assert_eq!(q.len(), 6);

        // At +30 ms, the 3 delayed30 copies are eligible — batch into 1
        // packet with all 3 NoteOffs.
        let t30 = T0 + Duration::from_millis(30);
        let mut buf = [0u8; 64];
        let pkt = q.pop_packet(t30, &mut buf).unwrap();
        let events = decode_cv(&buf[..pkt.body_len]);
        assert_eq!(events.len(), 3, "all 3 +30 ms delayed copies batch");

        // At +60 ms, the 3 delayed60 copies fire similarly.
        let t60 = T0 + Duration::from_millis(60);
        let pkt = q.pop_packet(t60, &mut buf).unwrap();
        let events = decode_cv(&buf[..pkt.body_len]);
        assert_eq!(events.len(), 3, "all 3 +60 ms delayed copies batch");

        assert!(q.is_empty());
    }

    #[test]
    fn delayed_copies_dont_block_eligible_new_events() {
        // Queue has only delayed NoteOff copies (ineligible at T0).
        // A new NoteOn arrives at T0 — it must not be blocked behind
        // the delayed copies.
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x80, 60, 0], T0);
        let _ = drain_at(&mut q, T0); // pop main K=3
        assert_eq!(q.len(), 2, "delayed copies remain");

        q.push_channel_voice(&[0x90, 64, 100], T0); // unrelated NoteOn

        // pop_packet at T0 should return the eligible NoteOn, NOT
        // wait for the delayed NoteOff copies.
        let mut buf = [0u8; 64];
        let pkt = q.pop_packet(T0, &mut buf).unwrap();
        let events = decode_cv(&buf[..pkt.body_len]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1, vec![0x90, 64, 100]);
    }

    #[test]
    fn next_eligibility_reports_earliest_ineligible() {
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x80, 60, 0], T0);
        // While main is still in queue (eligible), next_eligibility = None.
        assert!(q.next_eligibility().is_none());

        // After draining main, only delayed copies remain (ineligible).
        let _ = drain_at(&mut q, T0);
        match q.next_eligibility() {
            Some(t) => assert_eq!(t, T0 + Duration::from_millis(30)),
            None => panic!("expected Some(deadline) when only delayed copies left"),
        }

        // After draining the +30 ms copy, the +60 ms is the earliest.
        let t30 = T0 + Duration::from_millis(30);
        let _ = drain_at(&mut q, t30);
        match q.next_eligibility() {
            Some(t) => assert_eq!(t, T0 + Duration::from_millis(60)),
            None => panic!("expected Some(deadline) for +60 ms copy"),
        }
    }

    // ── NoteOn time-spread retransmits ───────────────────────────────────

    #[test]
    fn note_on_pushes_main_plus_delayed_copies() {
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x90, 60, 100], T0);
        // 1 main entry + 2 delayed copies = 3 entries.
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn delayed_note_on_copies_dont_pop_before_their_time() {
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x90, 60, 100], T0);
        // At T0, only the main entry's K=3 retransmits are eligible.
        let packets = drain_at(&mut q, T0);
        assert_eq!(packets.len(), 3, "expected K=3 main retransmits at T0");
        assert_eq!(q.len(), 2, "delayed copies stay queued");
    }

    #[test]
    fn delayed_note_on_copies_fire_at_deadline() {
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x90, 60, 100], T0);
        let _ = drain_at(&mut q, T0); // main K=3
        assert_eq!(q.len(), 2);

        let t30 = T0 + Duration::from_millis(30);
        let pkts30 = drain_at(&mut q, t30);
        assert_eq!(pkts30.len(), 1, "+30 ms delayed NoteOn fires");
        assert_eq!(q.len(), 1);

        let t60 = T0 + Duration::from_millis(60);
        let pkts60 = drain_at(&mut q, t60);
        assert_eq!(pkts60.len(), 1, "+60 ms delayed NoteOn fires");
        assert!(q.is_empty());
    }

    #[test]
    fn delayed_note_on_copies_share_event_seq_with_main() {
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x90, 60, 100], T0);

        let mut buf = [0u8; 64];
        let pkt = q.pop_packet(T0, &mut buf).unwrap();
        let main_seq = decode_cv(&buf[..pkt.body_len])[0].0;

        // Drain the rest of main at T0.
        let _ = drain_at(&mut q, T0);

        // +30 ms delayed copy — same event_seq as main.
        let t30 = T0 + Duration::from_millis(30);
        let pkt30 = q.pop_packet(t30, &mut buf).unwrap();
        let events_30 = decode_cv(&buf[..pkt30.body_len]);
        assert_eq!(events_30.len(), 1);
        assert_eq!(events_30[0].0, main_seq, "+30 ms NoteOn shares event_seq");
        assert_eq!(events_30[0].1, vec![0x90, 60, 100]);
    }

    #[test]
    fn note_off_cancels_pending_delayed_note_on_copies() {
        // The crucial dedup case: after the K=3 main NoteOn copies have
        // gone out, the +30 ms / +60 ms copies are still queued.  A
        // subsequent NoteOff for the same note must remove them so they
        // can never reach the wire after the NoteOff has been sent.
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x90, 60, 100], T0); // NoteOn C
        let _ = drain_at(&mut q, T0); // main K=3
        assert_eq!(q.len(), 2, "2 delayed NoteOn copies pending");

        // NoteOff arrives before delayed NoteOn copies fire.
        q.push_channel_voice(&[0x80, 60, 0], T0);
        // 2 delayed NoteOns removed, then NoteOff adds main + 2 delayed
        // = 3 entries.
        assert_eq!(q.len(), 3, "delayed NoteOns cancelled; NoteOff added");

        // Drain everything — no stale NoteOns survive.
        let packets = drain_at(&mut q, T_FAR_FUTURE);
        for (_, body) in &packets {
            for (_, midi) in decode_cv(body) {
                assert_ne!(
                    midi,
                    vec![0x90, 60, 100],
                    "stale delayed NoteOn survived NoteOff cancellation"
                );
            }
        }
    }

    #[test]
    fn pseudo_note_off_also_gets_delayed_copies() {
        // NoteOn with velocity 0 is the MIDI 1.0 alias for NoteOff.
        // The queue treats it as a note-state event and adds delayed
        // copies the same as a 0x80 NoteOff.
        let mut q = MidiTxQueue::new();
        q.push_channel_voice(&[0x90, 60, 0], T0); // pseudo-NoteOff
        assert_eq!(q.len(), 3, "pseudo-NoteOff also gets delayed copies");

        let _ = drain_at(&mut q, T0); // main K=3
        let t60 = T0 + Duration::from_millis(60);
        let _ = drain_at(&mut q, t60);
        assert!(q.is_empty(), "all copies fired by +60 ms");
    }
}
