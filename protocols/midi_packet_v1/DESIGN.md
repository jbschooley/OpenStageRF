# OpenStageRF v1 — Implementation Sketch

Companion to `SPEC.md`.  Sketches the TX queue, RX state machine, and replay-window types as Rust pseudocode.  Not committed implementation — review and adjust before turning into actual modules.

Targets: `core/link/src/midi_tx.rs`, `core/link/src/lib.rs`, `protocols/midi_packet_v1/src/lib.rs`.

## Module layout

```
protocols/midi_packet_v1/src/
├── lib.rs              — Header / Body encode + decode (wire format only)
├── replay.rs           — PacketReplayWindow32, EventReplayWindow16
└── sysex_reasm.rs      — SysExReassembler

core/link/src/
├── lib.rs              — LinkSender, LinkReceiver
├── midi_tx.rs          — MidiTxQueue (credit-based round-robin with batching + dedup)
└── timers.rs           — WatchdogTimer, HeartbeatTimer
```

## TX queue — `MidiTxQueue`

```rust
//! Credit-based priority queue with same-type batching and per-event seq
//! assignment.  Pop returns one packet's worth of consecutive same-priority,
//! same-event-type entries; consumed entries are decremented and requeued
//! at the back of their priority class until credits reach zero.

use heapless::Vec;
use osrf_protocols_midi_v1::{EventType, MAX_FRAG_DATA_BYTES};

pub const QUEUE_CAPACITY: usize = 128;
pub const REGULAR_PRIORITY: u8 = 1;
pub const REALTIME_PRIORITY: u8 = u8::MAX;
pub const SYSEX_PRIORITY: u8 = 0;
pub const DEFAULT_CREDITS: u8 = 3;
const MAX_MSG_BYTES: usize = 4;

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
}

impl Entry {
    fn kind(&self) -> EventType {
        match self.payload {
            EntryPayload::ChannelVoice { .. } => EventType::ChannelVoice,
            EntryPayload::SysExFragment { .. } => EventType::SysExFragment,
        }
    }
}

pub struct MidiTxQueue {
    entries: Vec<Entry, QUEUE_CAPACITY>,
    next_event_seq: u16,
    next_sysex_id: u16,
}

#[derive(Debug)]
pub enum PoppedKind {
    ChannelVoice,       // body is a sequence of (event_seq, midi) tuples
    SysExFragment,      // body is a single fragment payload
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

    /// Push a channel-voice MIDI message.  Applies status-aware dedup,
    /// assigns a fresh event_seq, inserts by priority + FIFO.
    /// Returns false if the queue is full.
    pub fn push_channel_voice(&mut self, midi: &[u8]) -> bool {
        if midi.is_empty() || midi.len() > MAX_MSG_BYTES {
            return false;
        }
        let status = midi[0];

        // Real-time messages preempt; no dedup.
        if status >= 0xF8 {
            let event_seq = self.alloc_event_seq();
            let mut bytes = Vec::new();
            let _ = bytes.extend_from_slice(midi);
            return self
                .insert_by_priority(Entry {
                    priority: REALTIME_PRIORITY,
                    credits: DEFAULT_CREDITS,
                    payload: EntryPayload::ChannelVoice { event_seq, midi: bytes },
                })
                .is_ok();
        }

        // Status-aware dedup against existing channel-voice entries.
        self.dedup_for_incoming(midi);

        let event_seq = self.alloc_event_seq();
        let mut bytes = Vec::new();
        let _ = bytes.extend_from_slice(midi);
        self.insert_by_priority(Entry {
            priority: REGULAR_PRIORITY,
            credits: DEFAULT_CREDITS,
            payload: EntryPayload::ChannelVoice { event_seq, midi: bytes },
        })
        .is_ok()
    }

    /// Fragment a complete SysEx body (without F0/F7) and queue all
    /// fragments at SYSEX_PRIORITY.  Returns the assigned sysex_id, or
    /// None if the queue can't fit all fragments.
    pub fn push_sysex(&mut self, sysex_body: &[u8]) -> Option<u16> {
        let sysex_id = self.alloc_sysex_id();
        let frag_data_max = MAX_FRAG_DATA_BYTES;
        let frag_total =
            ((sysex_body.len() + frag_data_max - 1) / frag_data_max).min(255) as u8;
        if frag_total == 0 || self.entries.capacity() - self.entries.len() < frag_total as usize
        {
            return None;
        }
        for (frag_idx, chunk) in sysex_body.chunks(frag_data_max).enumerate() {
            let mut data = Vec::new();
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
            });
        }
        Some(sysex_id)
    }

    /// Pop one packet's worth of front-of-queue entries that share both
    /// `priority` AND `event_type`.  Writes the body bytes to `out` and
    /// returns `(kind, len)`, or None if the queue is empty.  Consumed
    /// entries are decremented; survivors (credits > 0) are requeued at
    /// the back of their priority class.
    ///
    /// Body format (per kind):
    /// - ChannelVoice:   [event_seq:2][midi:1..3] [event_seq:2][midi:1..3] ...
    /// - SysExFragment:  [sysex_id:2][frag_idx:1][frag_total:1][data:N]
    ///                   (always exactly one fragment per packet)
    pub fn pop_packet(&mut self, out: &mut [u8]) -> Option<(PoppedKind, usize)> {
        let front = self.entries.first()?;
        let target_priority = front.priority;
        let target_kind = front.kind();
        let mut written = 0usize;
        let mut to_consume = Vec::<usize, QUEUE_CAPACITY>::new();

        // Walk consecutive entries that match priority + kind, accumulating
        // bytes until the buffer is full or the run ends.
        for (idx, entry) in self.entries.iter().enumerate() {
            if entry.priority != target_priority || entry.kind() != target_kind {
                break;
            }
            let needed = match &entry.payload {
                EntryPayload::ChannelVoice { midi, .. } => 2 + midi.len(),
                EntryPayload::SysExFragment { data, .. } => 4 + data.len(),
            };
            if written + needed > out.len() {
                break;
            }
            // Copy this entry's bytes into the body buffer.
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
            let _ = to_consume.push(idx);

            // SysEx is always one fragment per packet — stop after first.
            if matches!(target_kind, EventType::SysExFragment) {
                break;
            }
        }

        if to_consume.is_empty() {
            return None;
        }

        // Decrement credits in-place; collect entries that still have
        // credits to requeue at the back of their priority class.
        let mut requeue: Vec<Entry, QUEUE_CAPACITY> = Vec::new();
        // Walk indices in reverse so removals don't shift earlier indices.
        for &idx in to_consume.iter().rev() {
            let mut entry = self.entries.remove(idx);
            entry.credits = entry.credits.saturating_sub(1);
            if entry.credits > 0 {
                let _ = requeue.push(entry);
            }
        }
        // Reinsert: insert_by_priority places each at the back of its class.
        for entry in requeue.into_iter().rev() {
            // ignore overflow — couldn't have grown beyond capacity since
            // we just removed at least as many entries
            let _ = self.insert_by_priority(entry);
        }

        let kind = match target_kind {
            EventType::ChannelVoice => PoppedKind::ChannelVoice,
            EventType::SysExFragment => PoppedKind::SysExFragment,
            _ => unreachable!("only CV/SysEx in queue"),
        };
        Some((kind, written))
    }

    fn alloc_event_seq(&mut self) -> u16 {
        let seq = self.next_event_seq;
        self.next_event_seq = self.next_event_seq.wrapping_add(1);
        seq
    }

    fn alloc_sysex_id(&mut self) -> u16 {
        let id = self.next_sysex_id;
        self.next_sysex_id = self.next_sysex_id.wrapping_add(1);
        id
    }

    /// Apply MIDI status-aware dedup rules: scan the queue and remove any
    /// channel-voice entries that the incoming message would supersede.
    fn dedup_for_incoming(&mut self, msg: &[u8]) {
        let status = msg[0];
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
                    0x80 => !(m_high == 0x90 && m_ch == ch && m_d1 == d1), // NoteOff cancels NoteOn
                    0x90 => !(m_high == 0x80 && m_ch == ch && m_d1 == d1), // NoteOn cancels NoteOff
                    0xA0 => !(m_high == 0xA0 && m_ch == ch && m_d1 == d1), // PolyAT
                    0xB0 => !(m_high == 0xB0 && m_ch == ch && m_d1 == d1), // CC
                    0xC0 | 0xD0 | 0xE0 => !(m_high == high && m_ch == ch), // PC/CP/PB
                    _ => true,
                }
            }
            EntryPayload::SysExFragment { .. } => true,
        });
    }

    fn insert_by_priority(&mut self, entry: Entry) -> Result<(), Entry> {
        // Place after all entries with priority >= entry.priority.
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
```

### Notes on the queue design

- **Round-robin via requeue.**  Every pop decrements consumed entries' credits and reinserts at the back of their priority class.  Fresh events (full credits) inserted at the back of their class still pop ahead of older events still cycling through retransmits, because `insert_by_priority` goes to the back of the same-priority run — but the algorithm's natural batching ensures fresh events bundle with retransmit-pending ones in the same packet whenever they share priority + kind.
- **Cancellation lives entirely in `dedup_for_incoming`.**  Because retransmits stay in the queue (with decremented credits) until exhausted, a NoteOff arriving even after the NoteOn's first transmit can still scrub the NoteOn's remaining copies.  This is the structural fix for v1's stuck-note risk.
- **SysEx is one-fragment-per-packet** by construction (`break` after first SysEx entry in `pop_packet`).  Multiple fragments in one packet would complicate parsing and aren't worth the marginal byte savings.
- **The queue does NOT touch packet_seq** — the link sender allocates that per wire transmission.  The queue only owns event_seq and sysex_id, which are per-logical-event identities.

## Replay windows — `replay.rs`

```rust
//! Two replay windows: 32-bit packet-level (linear, sliding) and 16-bit
//! event-level (modular, sliding).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    Accept,
    /// Accepted; receiver should also reset the event replay window and
    /// SysEx reassembly state.  Emitted when the packet replay window
    /// detects a backward jump larger than `SESSION_RESET_GAP`, which
    /// only happens if TX rebooted with a colliding `boot_counter`.
    AcceptSessionReset,
    Replay,
    TooOld,
}

/// Backward `packet_seq` jump larger than this triggers the session-reset
/// fallback (see SPEC.md §"Session-reset fallback").  Sized to comfortably
/// exceed peak `packet_seq` advance over one minute of sustained max-rate
/// transmission (~90 000 packets/min at 1500 packets/sec).
pub const SESSION_RESET_GAP: u32 = 100_000;

#[derive(Debug, Default)]
pub struct PacketReplayWindow32 {
    high: u32,
    bitmap: u64,
    initialised: bool,
}

impl PacketReplayWindow32 {
    pub fn reset(&mut self) {
        self.high = 0;
        self.bitmap = 0;
        self.initialised = false;
    }

    pub fn check_and_advance(&mut self, seq: u32) -> CheckOutcome {
        if !self.initialised {
            self.high = seq;
            self.bitmap = 1; // bit 0 = high seen
            self.initialised = true;
            return CheckOutcome::Accept;
        }
        if seq > self.high {
            let shift = seq - self.high;
            self.bitmap = if shift >= 64 { 0 } else { self.bitmap << shift };
            self.bitmap |= 1;
            self.high = seq;
            CheckOutcome::Accept
        } else if seq == self.high {
            CheckOutcome::Replay
        } else if self.high - seq >= SESSION_RESET_GAP {
            // Backward jump too large to be legitimate — TX must have
            // rebooted with a colliding boot_counter.  Reset and accept.
            self.high = seq;
            self.bitmap = 1;
            CheckOutcome::AcceptSessionReset
        } else if self.high - seq >= 64 {
            CheckOutcome::TooOld
        } else {
            let bit = (self.high - seq) as u64;
            if self.bitmap & (1u64 << bit) != 0 {
                CheckOutcome::Replay
            } else {
                self.bitmap |= 1u64 << bit;
                CheckOutcome::Accept
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct EventReplayWindow16 {
    high: u16,
    bitmap: u64,
    initialised: bool,
}

impl EventReplayWindow16 {
    pub fn reset(&mut self) {
        self.high = 0;
        self.bitmap = 0;
        self.initialised = false;
    }

    pub fn check_and_advance(&mut self, seq: u16) -> CheckOutcome {
        if !self.initialised {
            self.high = seq;
            self.bitmap = 1;
            self.initialised = true;
            return CheckOutcome::Accept;
        }
        let d = seq.wrapping_sub(self.high);
        match d {
            0 => CheckOutcome::Replay,
            1..=32_767 => {
                // forward by d
                self.bitmap = if d >= 64 { 0 } else { self.bitmap << d };
                self.bitmap |= 1;
                self.high = seq;
                CheckOutcome::Accept
            }
            32_768..=65_471 => CheckOutcome::TooOld,
            65_472..=65_535 => {
                // backward by (65_536 - d)
                let bit = (65_536u32 - d as u32) as u64;
                if self.bitmap & (1u64 << bit) != 0 {
                    CheckOutcome::Replay
                } else {
                    self.bitmap |= 1u64 << bit;
                    CheckOutcome::Accept
                }
            }
        }
    }
}
```

## SysEx reassembler — `sysex_reasm.rs`

```rust
use heapless::Vec;
pub const MAX_CONCURRENT_SYSEX: usize = 2;
pub const MAX_FRAGMENTS_PER_SYSEX: usize = 32;
pub const MAX_FRAG_DATA_BYTES: usize = 48;
pub const MAX_SYSEX_BYTES: usize = MAX_FRAGMENTS_PER_SYSEX * MAX_FRAG_DATA_BYTES;

use embassy_time::{Duration, Instant};

const REASSEMBLY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct SysExReassembler {
    buffers: Vec<SysExBuffer, MAX_CONCURRENT_SYSEX>,
}

struct SysExBuffer {
    sysex_id: u16,
    frag_total: u8,
    received_mask: u32,                                                // bit i = frag_idx i received
    frags: [Option<Vec<u8, MAX_FRAG_DATA_BYTES>>; MAX_FRAGMENTS_PER_SYSEX],
    last_seen: Instant,
}

pub enum ReassembleOutcome<'a> {
    /// Fragment accepted; SysEx not yet complete.
    Pending,
    /// Fragment was a duplicate (replay).
    Replay,
    /// Fragment dropped (no buffer available, or invalid frag_idx/total).
    Dropped,
    /// SysEx is complete.  `body` is the reassembled body without F0/F7.
    /// Caller is responsible for prepending F0 and appending F7 before
    /// delivering to the MIDI sink.
    Complete { sysex_id: u16, body: &'a [u8] },
}

impl SysExReassembler {
    pub fn process(
        &mut self,
        sysex_id: u16,
        frag_idx: u8,
        frag_total: u8,
        data: &[u8],
        now: Instant,
    ) -> ReassembleOutcome<'_> {
        // Garbage-collect stale buffers.
        self.buffers.retain(|b| now.duration_since(b.last_seen) < REASSEMBLY_TIMEOUT);

        // Validate.
        if frag_total == 0
            || frag_idx >= frag_total
            || frag_total as usize > MAX_FRAGMENTS_PER_SYSEX
        {
            return ReassembleOutcome::Dropped;
        }

        // Find or allocate buffer.
        let pos = match self.buffers.iter().position(|b| b.sysex_id == sysex_id) {
            Some(i) => i,
            None => {
                if self.buffers.is_full() {
                    return ReassembleOutcome::Dropped;
                }
                let buf = SysExBuffer {
                    sysex_id,
                    frag_total,
                    received_mask: 0,
                    frags: Default::default(),
                    last_seen: now,
                };
                let _ = self.buffers.push(buf);
                self.buffers.len() - 1
            }
        };

        let buf = &mut self.buffers[pos];
        if buf.frag_total != frag_total {
            // Inconsistent — drop and start over.
            self.buffers.swap_remove(pos);
            return ReassembleOutcome::Dropped;
        }

        let bit = 1u32 << frag_idx;
        if buf.received_mask & bit != 0 {
            return ReassembleOutcome::Replay;
        }

        let mut frag_data = Vec::new();
        if frag_data.extend_from_slice(data).is_err() {
            return ReassembleOutcome::Dropped;
        }
        buf.frags[frag_idx as usize] = Some(frag_data);
        buf.received_mask |= bit;
        buf.last_seen = now;

        let complete_mask = if frag_total == 32 { u32::MAX } else { (1u32 << frag_total) - 1 };
        if buf.received_mask == complete_mask {
            // Reassemble in-place into the buffer's first fragment slot,
            // then return a slice.  In real code we'd hand the caller an
            // owned Vec or write into a caller-supplied buffer.
            // (Sketch: see "open question" below.)
            todo!("reassemble + return &[u8] slice")
        } else {
            ReassembleOutcome::Pending
        }
    }
}
```

### Open question on the reassembler

Returning `&'a [u8]` from `process` requires the body bytes to live somewhere stable — either in `self.buffers[pos]` (then we need to keep the buffer around long enough for the caller to consume) or in a caller-supplied scratch buffer.  Cleanest is probably:

```rust
pub fn process(
    &mut self,
    sysex_id: u16, frag_idx: u8, frag_total: u8, data: &[u8],
    now: Instant,
    scratch: &'a mut [u8],
) -> ReassembleOutcome<'a>
```

The scratch buffer is sized to `MAX_SYSEX_BYTES` and lives in the `LinkReceiver`.  On completion, we copy the assembled body into `scratch` and return a slice into it.

## Receiver — `LinkReceiver`

```rust
use osrf_protocols_midi_v1::{Header, EventType, KEY_FP_NONE, VER_V1};

pub struct LinkReceiver {
    key_fp: u32,                    // 24-bit, stored as u32
    boot_session: Option<u16>,      // current TX boot_counter, None until first packet
    packet_replay: PacketReplayWindow32,
    event_replay: EventReplayWindow16,
    sysex_reasm: SysExReassembler,
    sysex_scratch: [u8; MAX_SYSEX_BYTES],
    /// Set by the watchdog (`mark_link_down`) when the link goes silent.
    /// The next `process()` call clears this flag and forces a session
    /// reset before the replay window check.  Catches TX restarts whose
    /// `boot_counter` happens to collide with the previous session's.
    link_down: bool,
}

impl LinkReceiver {
    /// Called by the watchdog timer when no packet has been received for
    /// `WATCHDOG_MS`.  Marks the link as down so the next packet triggers
    /// a full session reset.
    pub fn mark_link_down(&mut self) {
        self.link_down = true;
    }
}

pub enum RxEvent<'a> {
    ChannelVoice(&'a [u8]),  // 1..3 raw MIDI bytes
    SysExComplete(&'a [u8]), // raw SysEx bytes including F0/F7
    Heartbeat,
}

#[derive(Debug)]
pub enum RxDrop {
    UnknownVersion,
    KeyFpMismatch,
    PacketReplay,
    PacketTooOld,
    AeadFailure,
    EventReplay,
    EventTooOld,
    UnknownEventType(u8),
    MalformedBody,
    SysExDropped,
}

impl LinkReceiver {
    /// Process one wire packet.  Calls `on_event` zero or more times for
    /// each MIDI event, complete SysEx, or heartbeat that survives all
    /// dedup checks.  Returns Ok(()) on accepted packet (regardless of
    /// how many events emerged) or Err(RxDrop) on packet-level drop.
    pub fn process<F>(
        &mut self,
        wire: &[u8],
        now: embassy_time::Instant,
        mut on_event: F,
    ) -> Result<(), RxDrop>
    where
        F: FnMut(RxEvent<'_>),
    {
        // 1. Parse header.
        let hdr = Header::decode(wire).map_err(|_| RxDrop::MalformedBody)?;
        if hdr.ver != VER_V1 {
            return Err(RxDrop::UnknownVersion);
        }
        // 2. Key fingerprint check.
        if hdr.key_fp != self.key_fp {
            return Err(RxDrop::KeyFpMismatch);
        }
        // 3. Session reset detection.  Trigger reset on any of:
        //    (a) boot_counter mismatch (primary signal for TX restart)
        //    (b) link_down was set by the watchdog (catches restarts whose
        //        new boot_counter collides with the previous session's)
        let boot_changed = matches!(self.boot_session, Some(bc) if bc != hdr.boot_counter);
        let was_down = self.link_down;
        self.link_down = false;
        if boot_changed || was_down {
            self.packet_replay.reset();
            self.event_replay.reset();
            self.sysex_reasm.reset_all();
        }
        self.boot_session = Some(hdr.boot_counter);
        // 4. Packet replay window.
        match self.packet_replay.check_and_advance(hdr.packet_seq) {
            CheckOutcome::Accept => {}
            CheckOutcome::AcceptSessionReset => {
                // Backward jump > SESSION_RESET_GAP — TX rebooted with a
                // colliding boot_counter.  Reset event-level state too.
                self.event_replay.reset();
                self.sysex_reasm.reset_all();
                // boot_session is already correct (we matched it at step 3).
            }
            CheckOutcome::Replay => return Err(RxDrop::PacketReplay),
            CheckOutcome::TooOld => return Err(RxDrop::PacketTooOld),
        }
        // 5. AEAD verify + decrypt (when enabled).
        let body = decrypt_or_passthrough(&hdr, wire).map_err(|_| RxDrop::AeadFailure)?;

        // 6. Dispatch by event_type.
        match hdr.event_type {
            EventType::Heartbeat => {
                on_event(RxEvent::Heartbeat);
                Ok(())
            }
            EventType::ChannelVoice => {
                self.process_channel_voice(body, &mut on_event)
            }
            EventType::SysExFragment => {
                self.process_sysex(body, now, &mut on_event)
            }
            EventType::Unknown(t) => Err(RxDrop::UnknownEventType(t)),
        }
    }

    fn process_channel_voice<F>(
        &mut self,
        body: &[u8],
        on_event: &mut F,
    ) -> Result<(), RxDrop>
    where
        F: FnMut(RxEvent<'_>),
    {
        let mut i = 0;
        while i < body.len() {
            if body.len() - i < 2 + 1 {
                return Err(RxDrop::MalformedBody);
            }
            let event_seq = u16::from_be_bytes([body[i], body[i + 1]]);
            i += 2;
            let status = body[i];
            let msg_len = midi_message_length(status).ok_or(RxDrop::MalformedBody)?;
            if body.len() - i < msg_len {
                return Err(RxDrop::MalformedBody);
            }
            let midi = &body[i..i + msg_len];
            i += msg_len;

            match self.event_replay.check_and_advance(event_seq) {
                CheckOutcome::Accept => on_event(RxEvent::ChannelVoice(midi)),
                CheckOutcome::AcceptSessionReset => {
                    // Event window doesn't generate this variant; only the
                    // packet window does.  Treat as Accept defensively.
                    on_event(RxEvent::ChannelVoice(midi));
                }
                CheckOutcome::Replay => {} // silent dedup
                CheckOutcome::TooOld => {} // silent
            }
        }
        Ok(())
    }

    fn process_sysex<F>(
        &mut self,
        body: &[u8],
        now: embassy_time::Instant,
        on_event: &mut F,
    ) -> Result<(), RxDrop>
    where
        F: FnMut(RxEvent<'_>),
    {
        if body.len() < 4 {
            return Err(RxDrop::MalformedBody);
        }
        let sysex_id = u16::from_be_bytes([body[0], body[1]]);
        let frag_idx = body[2];
        let frag_total = body[3];
        let data = &body[4..];
        match self.sysex_reasm.process(
            sysex_id, frag_idx, frag_total, data, now, &mut self.sysex_scratch,
        ) {
            ReassembleOutcome::Pending => Ok(()),
            ReassembleOutcome::Replay => Ok(()),
            ReassembleOutcome::Dropped => Err(RxDrop::SysExDropped),
            ReassembleOutcome::Complete { body, .. } => {
                on_event(RxEvent::SysExComplete(body));
                Ok(())
            }
        }
    }
}

fn midi_message_length(status: u8) -> Option<usize> {
    match status & 0xF0 {
        0xC0 | 0xD0 => Some(2),
        0x80 | 0x90 | 0xA0 | 0xB0 | 0xE0 => Some(3),
        0xF0 => match status {
            0xF1 | 0xF3 => Some(2),
            0xF2 => Some(3),
            0xF6 | 0xF8..=0xFF => Some(1),
            _ => None,
        },
        _ => None,
    }
}
```

## Sender — `LinkSender`

```rust
pub struct LinkSender {
    key_fp: u32,
    boot_counter: u16,
    next_packet_seq: u32,
}

#[derive(Debug)]
pub enum SendError {
    PacketSeqOverflow,
    Encrypt,
    BodyTooLarge,
}

impl LinkSender {
    pub fn no_crypto(boot_counter: u16) -> Self {
        Self { key_fp: KEY_FP_NONE, boot_counter, next_packet_seq: 0 }
    }

    /// Encode a packet with the given event_type and body bytes.  Writes
    /// the wire packet (header + body + tag if any) into `wire_out`.
    /// Allocates a fresh packet_seq.
    pub fn encode(
        &mut self,
        event_type: EventType,
        body: &[u8],
        wire_out: &mut [u8],
    ) -> Result<usize, SendError> {
        if self.next_packet_seq == u32::MAX {
            return Err(SendError::PacketSeqOverflow);
        }
        let packet_seq = self.next_packet_seq;
        self.next_packet_seq += 1;

        let hdr = Header {
            ver: VER_V1,
            key_fp: self.key_fp,
            boot_counter: self.boot_counter,
            packet_seq,
            event_type,
        };
        hdr.encode_to(wire_out, body /* + crypto if enabled */)
    }
}
```

## How `run_tx` glues this together

```rust
pub async fn run_tx<...>(
    radio: &mut Sx1262Radio<...>,
    source: &mut Source,
    boot_counter: u16,
) -> ! {
    let mut sender = LinkSender::no_crypto(boot_counter);
    let mut queue = MidiTxQueue::new();
    let mut hb = HeartbeatTimer::new(Duration::from_millis(HEARTBEAT_MS));
    let mut midi_buf = [0u8; 4];
    let mut body_buf = [0u8; RF_PAYLOAD_MAX - HEADER_LEN - TAG_MAX];
    let mut wire_buf = [0u8; RF_PAYLOAD_MAX];

    loop {
        // 1. Drain source non-blocking.
        loop {
            match poll_once(source.next_message(&mut midi_buf)) {
                Poll::Ready(Ok(n)) => { let _ = queue.push_channel_voice(&midi_buf[..n]); }
                Poll::Ready(Err(_)) | Poll::Pending => break,
            }
        }
        // 2. If queue has something, pop a packet's worth and TX.
        if let Some((kind, body_n)) = queue.pop_packet(&mut body_buf) {
            let event_type = match kind {
                PoppedKind::ChannelVoice => EventType::ChannelVoice,
                PoppedKind::SysExFragment => EventType::SysExFragment,
            };
            let wire_n = sender.encode(event_type, &body_buf[..body_n], &mut wire_buf)?;
            let _ = radio.tx(&wire_buf[..wire_n]).await;
            hb.note_send();
            continue;
        }
        // 3. Queue empty — wait for source or heartbeat.
        match select(source.next_message(&mut midi_buf), hb.wait()).await {
            Either::First(Ok(n)) => { let _ = queue.push_channel_voice(&midi_buf[..n]); }
            Either::Second(()) => {
                let wire_n = sender.encode(EventType::Heartbeat, &[], &mut wire_buf)?;
                let _ = radio.tx(&wire_buf[..wire_n]).await;
                hb.note_send();
            }
            _ => {}
        }
    }
}
```

Notice there's no copy-loop, no bail-out logic, no slot ring — the credit-based queue handles everything.  Each `pop_packet` returns one packet to send (with whichever events fit at the front-of-queue priority + kind), then the next iteration handles whatever's next.

## Test plan additions

### Unit tests in `protocols/midi_packet_v1`

- Header round-trip (encode/decode) for all event_types, with and without crypto.
- `EventReplayWindow16`: forward, replay, too-old, wraparound (high=65530, accept seqs through 4 across the boundary).
- `EventReplayWindow16`: out-of-order across wraparound (receive seq=5 then seq=65530 then seq=4).
- `PacketReplayWindow32`: standard sliding-window correctness.
- `PacketReplayWindow32`: backward jump > `SESSION_RESET_GAP` returns `AcceptSessionReset` and the window resets to the new seq.
- `LinkReceiver`: TX restart with colliding `boot_counter` and busy session (huge backward `packet_seq` jump) is detected via the `AcceptSessionReset` outcome — event replay window and SysEx buffers both clear.
- `LinkReceiver`: `mark_link_down()` followed by a packet with matching `boot_counter` and small backward `packet_seq` jump still triggers a session reset.  Covers TX restart in idle/low-traffic sessions.
- `SysExReassembler`: in-order, out-of-order, missing-fragment-then-timeout, two concurrent SysEx.

### Unit tests in `core/link/midi_tx.rs`

- `push_channel_voice` assigns monotonic event_seq, wraps at u16::MAX.
- `dedup_for_incoming`: full table from spec, verify each row.
- `pop_packet` batches same-priority, same-kind front entries.
- `pop_packet` requeues survivors at back of priority class.
- `pop_packet` decrements credits to zero and drops.
- Real-time event preempts pending channel-voice.
- SysEx fragment doesn't bundle with channel-voice.
- Cancellation after pop: NoteOn pushed, NoteOff pushed, only NoteOff remains; NoteOn's already-sent copy goes out exactly once.

### Hardware tests in `apps/link_bench`

- Chord at 1 ms spacing: each note arrives at RX exactly once, each with K=3 redundancy.
- Sustained pitch-bend at 100 Hz: latest value always wins; `event_seq` rolls over without disruption.
- TX reboot during bench: RX detects new `boot_counter`, resets cleanly.
- Multi-antenna diversity (future): two RX with different antennas, dedup via `packet_seq`.

## What's NOT in this sketch

- Crypto (AEAD) integration — placeholder `decrypt_or_passthrough` stands in.  Wire format is ready for it (AAD includes everything except the body).
- Audio bodies (`event_type` 0x10–0x1F) — out of scope for M5.
- `direction = RX→TX` for telemetry — out of scope.
- Persistent `packet_seq` across reboots — out of scope; we accept the 1/65 536 boot_counter collision risk.
