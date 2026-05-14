// SPDX-License-Identifier: AGPL-3.0-or-later
#![cfg_attr(not(test), no_std)]

//! OpenStageRF link layer — packet/event sequence numbering, replay-window
//! dedup at two layers, watchdog, heartbeat, SysEx reassembly.
//!
//! Sits between the radio (which moves bytes) and the application (which
//! produces / consumes MIDI events).
//!
//! Current scope: the no-crypto path (`key_fp = KEY_FP_NONE`).  AEAD
//! integration is a future milestone — the link layer here just frames
//! and dedups; AEAD verification will plug in around the receiver's
//! header decode + body decrypt step.

use osrf_protocols_midi_v1 as proto;

pub use osrf_crypto::{self, CipherId, Direction};
pub use proto::{
    ChannelVoiceIter, ChannelVoiceParseError, CheckOutcome, EventReplayWindow16, EventType, Header,
    KeyFp, PacketReplayWindow32, SysExFragmentParts, SysExParseError, HEADER_LEN, KEY_FP_NONE,
    MAX_BODY_LEN, MAX_BODY_LEN_AEAD, MAX_FRAG_DATA_BYTES, SESSION_RESET_GAP, TAG_LEN, VER_V1,
};

/// Bundles the AEAD parameters that don't change packet-to-packet
/// — held by [`LinkSender`] (for encrypt) and [`LinkReceiver`] (for
/// decrypt).
///
/// `device_id` and `direction` are part of the nonce; see
/// [`osrf_crypto::derive_nonce`].  `key` and `cipher` drive the
/// AEAD primitive.  `key_fp` is the 24-bit fingerprint written into
/// the header — derived from `fingerprint(cipher, &key)` by the
/// constructors so TX and RX agree on the wire value without
/// either side having to compute it from the raw key.
#[derive(Clone, Copy)]
pub struct AeadContext {
    pub cipher: CipherId,
    pub key: [u8; osrf_crypto::KEY_LEN],
    pub device_id: u32,
    pub direction: Direction,
}

/// 24-bit fingerprint of a key, packed little-endian into the
/// 3-byte [`KeyFp`] wire field.
fn fp_to_bytes(fp: u32) -> KeyFp {
    [
        (fp & 0xFF) as u8,
        ((fp >> 8) & 0xFF) as u8,
        ((fp >> 16) & 0xFF) as u8,
    ]
}

pub mod midi_tx;
pub mod state;
pub mod sysex;

pub use midi_tx::{
    MidiTxQueue, PoppedPacket, QueueKind, DEFAULT_CREDITS, QUEUE_CAPACITY, REALTIME_PRIORITY,
    REGULAR_PRIORITY, SYSEX_PRIORITY,
};
pub use state::{ChannelNoteCounts, PressedNotes};
pub use sysex::{
    SysExOutcome, SysExReassembler, MAX_CONCURRENT_SYSEX, MAX_FRAGS_PER_SYSEX, MAX_SYSEX_BYTES,
};

// ---------------------------------------------------------------------------
// LinkSender — outbound: pack header + body into a buffer for the radio
// ---------------------------------------------------------------------------

/// Outbound link-layer encoder.
///
/// Owns the local `(boot_counter, packet_seq)` pair.  Each call to
/// [`Self::encode`] consumes one `packet_seq`.  When `packet_seq` would
/// wrap past `u32::MAX`, encoding fails with
/// [`SendError::PacketSeqOverflow`] — caller is expected to bump
/// `boot_counter` (e.g., reboot) before that happens in practice (~50
/// days continuous at peak rates).
#[derive(Clone, Copy)]
pub struct LinkSender {
    boot_counter: u16,
    packet_seq: u32,
    key_fp: KeyFp,
    aead: Option<AeadContext>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum SendError {
    /// Underlying protocol encoder rejected the body / header.
    Encode(proto::EncodeError),
    /// Out of packet sequence numbers.  Reset (bumps boot_counter) required.
    PacketSeqOverflow,
    /// AEAD encryption failed — either the underlying cipher errored
    /// (vanishingly rare; effectively an internal bug) or `out`
    /// didn't have room for the trailing tag.
    Aead,
}

impl From<proto::EncodeError> for SendError {
    fn from(e: proto::EncodeError) -> Self {
        Self::Encode(e)
    }
}

impl LinkSender {
    pub fn new(boot_counter: u16, key_fp: KeyFp) -> Self {
        Self {
            boot_counter,
            packet_seq: 0,
            key_fp,
            aead: None,
        }
    }

    /// Construct with `KEY_FP_NONE` — the no-crypto path.
    pub fn no_crypto(boot_counter: u16) -> Self {
        Self::new(boot_counter, KEY_FP_NONE)
    }

    /// Construct with AEAD enabled.  `key_fp` is computed from
    /// `(cipher, key)` via [`osrf_crypto::fingerprint`] so the
    /// receiver — which holds the same `(cipher, key)` — derives the
    /// same fingerprint without further coordination.
    pub fn with_aead(boot_counter: u16, aead: AeadContext) -> Self {
        let fp = osrf_crypto::fingerprint(aead.cipher, &aead.key);
        Self {
            boot_counter,
            packet_seq: 0,
            key_fp: fp_to_bytes(fp),
            aead: Some(aead),
        }
    }

    pub fn boot_counter(&self) -> u16 {
        self.boot_counter
    }

    pub fn packet_seq(&self) -> u32 {
        self.packet_seq
    }

    pub fn key_fp(&self) -> KeyFp {
        self.key_fp
    }

    /// Encode header + body into `out`, allocating a fresh `packet_seq`.
    /// Returns the number of bytes written.
    ///
    /// **Open mode** (no AEAD context): writes header + plaintext body
    /// only; output is `HEADER_LEN + body.len()` bytes.
    ///
    /// **AEAD mode** (with AEAD context): writes header, encrypts body
    /// in place over the header AAD, and appends the 16-byte auth tag
    /// at the tail.  Output is `HEADER_LEN + body.len() + TAG_LEN`
    /// bytes — `out` must be large enough or [`SendError::Aead`]
    /// fires.  `body.len()` must additionally fit in
    /// [`MAX_BODY_LEN_AEAD`] (= 37 bytes for the 64-byte radio packet).
    pub fn encode(
        &mut self,
        event_type: EventType,
        body: &[u8],
        out: &mut [u8],
    ) -> Result<usize, SendError> {
        let next = self
            .packet_seq
            .checked_add(1)
            .ok_or(SendError::PacketSeqOverflow)?;
        let header = Header {
            ver: VER_V1,
            key_fp: self.key_fp,
            boot_counter: self.boot_counter,
            packet_seq: self.packet_seq,
            event_type,
        };

        // Reject AEAD bodies that don't leave room for the tag inside
        // the wire-format budget.  In open mode the existing
        // `MAX_BODY_LEN` check inside `proto::encode` is sufficient.
        if self.aead.is_some() && body.len() > MAX_BODY_LEN_AEAD {
            return Err(SendError::Encode(proto::EncodeError::BodyTooLarge));
        }

        let n_plain = proto::encode(out, &header, body)?;

        if let Some(aead) = &self.aead {
            // AAD = the just-written header.  Copy it out because the
            // borrow checker won't let us hand `&out[..HEADER_LEN]` as
            // AAD while also taking `&mut out[HEADER_LEN..n_plain]` as
            // the encrypt buffer.  11 bytes — trivial copy.
            let mut aad = [0u8; HEADER_LEN];
            aad.copy_from_slice(&out[..HEADER_LEN]);

            let nonce = osrf_crypto::derive_nonce(
                aead.device_id,
                aead.direction,
                header.packet_seq,
                header.boot_counter,
            );
            let nonce_slice = &nonce[..aead.cipher.nonce_len()];

            let body_buf = &mut out[HEADER_LEN..n_plain];
            let tag = osrf_crypto::encrypt(aead.cipher, &aead.key, nonce_slice, &aad, body_buf)
                .map_err(|_| SendError::Aead)?;

            // Append tag.
            let tag_end = n_plain + TAG_LEN;
            if out.len() < tag_end {
                return Err(SendError::Aead);
            }
            out[n_plain..tag_end].copy_from_slice(&tag);

            self.packet_seq = next;
            return Ok(tag_end);
        }

        self.packet_seq = next;
        Ok(n_plain)
    }
}

// ---------------------------------------------------------------------------
// LinkReceiver — inbound: decode + dedup at two layers
// ---------------------------------------------------------------------------

/// Reason a received packet was dropped after framing-level decode succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum RxDrop {
    /// `key_fp` didn't match this receiver's expected fingerprint.
    KeyFpMismatch(KeyFp),
    /// Packet replay-window rejection (duplicate).
    PacketReplay(u32),
    /// Packet replay-window rejection (too old).
    PacketTooOld(u32),
    /// `event_type` wasn't recognised (reserved-future value).
    UnknownEventType(u8),
    /// Body was malformed for the declared `event_type`.
    MalformedBody,
    /// SysEx fragment processing dropped the fragment (e.g., reassembly
    /// buffer full, invalid frag header).
    SysExDropped,
    /// AEAD authentication failed: tag mismatch, body too short to
    /// contain a tag, or any other cipher-side error.  The packet was
    /// tampered with, was encrypted under a different nonce / key, or
    /// is plaintext on a port that expects encryption.  Drop without
    /// further parsing — the body bytes are untrusted.
    AeadFail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum RxError {
    /// Wire-format decode failed (truncation, version mismatch, etc.).
    Decode(proto::DecodeError),
}

impl From<proto::DecodeError> for RxError {
    fn from(e: proto::DecodeError) -> Self {
        Self::Decode(e)
    }
}

/// One observable event surfaced to the caller's per-packet callback.
#[derive(Debug, PartialEq, Eq)]
pub enum RxEvent<'a> {
    /// Heartbeat packet — no MIDI to deliver, watchdog already kicked
    /// implicitly by accepting the packet.
    ///
    /// The optional u16 is the TX-reported active-channel mask: bit
    /// `i` set ⇔ channel `i` has at least one note pressed at the TX
    /// (see [`ChannelNoteCounts`]).  `None` means the packet had no
    /// state info (legacy 0-byte heartbeat or malformed body).  Used
    /// by the stuck-note recovery path: when TX says a channel is
    /// silent but RX still has notes pressed, the caller fires CC 123
    /// (All Notes Off) for that channel.
    Heartbeat(Option<u16>),
    /// A single channel-voice MIDI message that survived event-level
    /// dedup.  Bytes are the raw MIDI message (1–3 bytes including
    /// status byte).
    ChannelVoice(&'a [u8]),
    /// A complete SysEx, reassembled from fragments.  Includes `0xF0`
    /// start and `0xF7` end markers — caller can hand the slice
    /// directly to a MIDI sink.
    SysExComplete(&'a [u8]),
}

/// Inbound link-layer decoder, replay windows, and SysEx reassembler.
///
/// Tracks `boot_counter` to detect TX restarts.  When the link goes silent
/// for the watchdog interval, the *next* packet triggers a full session
/// reset regardless of `boot_counter` — this catches restarts whose
/// random `boot_counter` happens to collide with the previous session's.
///
/// # Wire-order assumption
///
/// `process()` assumes the caller delivers wire packets in **send
/// order** (lower `packet_seq` first).  Single-antenna setups satisfy
/// this trivially because the wire is serial and air propagation is
/// effectively instantaneous over our distances.
///
/// **For independent-demod diversity** (two SX1262s feeding a shared
/// queue), the merge layer must preserve send order — either by
/// processing packets strictly in `packet_seq` order, or by ensuring
/// per-antenna demod-to-merge latency is uniform.
///
/// If send order is violated, the worst case is a **truncated note**,
/// not a stuck one: the heartbeat-state failsafe (see
/// [`PressedNotes::missing_clear`]) silences any note that the
/// receiver thinks is pressed when TX's heartbeat says the channel is
/// silent.  Recovery happens within ~20 ms (two stable heartbeats),
/// during which the synth plays the misordered note for that brief
/// window.  Audible as a click or short blip but not as a stuck drone.
///
/// To eliminate the truncated-note artifact for diversity setups, wrap
/// `process()` in a 30 ms reorder buffer that sorts events by
/// `event_seq` before delivering to the sink.  Trades 30 ms latency
/// for strict ordering.  Not implemented today — single antenna is the
/// supported deployment.
pub struct LinkReceiver {
    expected_key_fp: KeyFp,
    aead: Option<AeadContext>,
    boot_session: Option<u16>,
    packet_replay: PacketReplayWindow32,
    event_replay: EventReplayWindow16,
    sysex_reasm: SysExReassembler,
    sysex_scratch: [u8; MAX_SYSEX_BYTES],
    /// Set by `mark_link_down()` (called from the watchdog).  The next
    /// `process()` call clears this and forces a full session reset.
    link_down: bool,
}

impl LinkReceiver {
    pub fn new(expected_key_fp: KeyFp) -> Self {
        Self {
            expected_key_fp,
            aead: None,
            boot_session: None,
            packet_replay: PacketReplayWindow32::new(),
            event_replay: EventReplayWindow16::new(),
            sysex_reasm: SysExReassembler::new(),
            sysex_scratch: [0u8; MAX_SYSEX_BYTES],
            link_down: false,
        }
    }

    pub fn no_crypto() -> Self {
        Self::new(KEY_FP_NONE)
    }

    /// Construct with AEAD enabled.  Like [`LinkSender::with_aead`],
    /// the expected `key_fp` is derived from `fingerprint(cipher,
    /// &key)`.  `direction` must match the sender's direction byte
    /// (one-way TX→RX link: both sides use [`Direction::TxToRx`]).
    pub fn with_aead(aead: AeadContext) -> Self {
        let fp = osrf_crypto::fingerprint(aead.cipher, &aead.key);
        Self {
            expected_key_fp: fp_to_bytes(fp),
            aead: Some(aead),
            boot_session: None,
            packet_replay: PacketReplayWindow32::new(),
            event_replay: EventReplayWindow16::new(),
            sysex_reasm: SysExReassembler::new(),
            sysex_scratch: [0u8; MAX_SYSEX_BYTES],
            link_down: false,
        }
    }

    /// Called by the watchdog timer when no packet has been received for
    /// `WATCHDOG_MS`.  Marks the link as down so the next packet
    /// triggers a full session reset (clearing both replay windows and
    /// any in-flight SysEx reassembly state).  See SPEC.md § "Watchdog".
    pub fn mark_link_down(&mut self) {
        self.link_down = true;
    }

    pub fn last_boot_counter(&self) -> Option<u16> {
        self.boot_session
    }

    /// Highest `packet_seq` accepted from the current session, or `None`
    /// if no packet has been processed yet.  Useful for stats: the
    /// difference between two snapshots gives the number of packets TX
    /// actually transmitted in that window (since `packet_seq`
    /// increments per wire transmission, including retransmits).
    pub fn last_packet_seq(&self) -> Option<u32> {
        self.packet_replay.high()
    }

    /// Decode + dedup `wire`, calling `on_event` for each observable
    /// event that survives all checks (heartbeat, individual
    /// channel-voice messages, complete SysEx).  Returns `Ok(())` if
    /// the packet was accepted at the packet level (regardless of how
    /// many events were emitted) or `Err(RxError)` on decode failures.
    /// Returns `Ok(())` with no callbacks fired if the packet was
    /// accepted but its body was empty (e.g., a CHANNEL_VOICE packet
    /// containing only events that all replay-dropped).
    pub fn process<F>(
        &mut self,
        wire: &[u8],
        now: embassy_time::Instant,
        mut on_event: F,
    ) -> Result<Result<(), RxDrop>, RxError>
    where
        F: FnMut(RxEvent<'_>),
    {
        // 1. Parse header.
        let (hdr, body) = proto::decode(wire)?;

        // 2. Key fingerprint check.
        if hdr.key_fp != self.expected_key_fp {
            return Ok(Err(RxDrop::KeyFpMismatch(hdr.key_fp)));
        }

        // 3. Session reset detection.  Trigger reset on either:
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
            }
            CheckOutcome::Replay => return Ok(Err(RxDrop::PacketReplay(hdr.packet_seq))),
            CheckOutcome::TooOld => return Ok(Err(RxDrop::PacketTooOld(hdr.packet_seq))),
        }

        // 5. AEAD verify + decrypt.  Body slice flips from "wire body
        //    bytes" (open mode) to "decrypted plaintext inside a stack
        //    scratch buffer" (AEAD mode).  We allocate the scratch
        //    here on the stack so the borrow stays disjoint from the
        //    rest of `self` for the downstream channel-voice / sysex
        //    dispatch calls that take `&mut self`.
        let mut aead_scratch = [0u8; MAX_BODY_LEN];
        let body = if let Some(aead) = &self.aead {
            if body.len() < TAG_LEN {
                return Ok(Err(RxDrop::AeadFail));
            }
            let plaintext_len = body.len() - TAG_LEN;
            let (ciphertext, tag) = body.split_at(plaintext_len);
            // Copy ciphertext into the mutable scratch — RustCrypto's
            // detached-tag interface decrypts in place.
            aead_scratch[..plaintext_len].copy_from_slice(ciphertext);
            let mut tag_arr = [0u8; TAG_LEN];
            tag_arr.copy_from_slice(tag);

            let nonce = osrf_crypto::derive_nonce(
                aead.device_id,
                aead.direction,
                hdr.packet_seq,
                hdr.boot_counter,
            );
            // AAD = the same 11 header bytes that TX authenticated.
            // Slice `wire` directly — header bytes are unchanged from
            // the original packet so the AAD matches byte-for-byte.
            let aad = &wire[..HEADER_LEN];

            if osrf_crypto::decrypt(
                aead.cipher,
                &aead.key,
                &nonce[..aead.cipher.nonce_len()],
                aad,
                &mut aead_scratch[..plaintext_len],
                &tag_arr,
            )
            .is_err()
            {
                return Ok(Err(RxDrop::AeadFail));
            }
            &aead_scratch[..plaintext_len]
        } else {
            body
        };

        // 6. Dispatch by event_type.
        match hdr.event_type {
            EventType::Heartbeat => {
                // Heartbeat body carries an optional 2-byte active-channel
                // mask (big-endian).  Empty body or any other length is
                // treated as "no info" so we don't fire spurious stuck-
                // note recovery on malformed or legacy packets.
                let mask = if body.len() == 2 {
                    Some(u16::from_be_bytes([body[0], body[1]]))
                } else {
                    None
                };
                on_event(RxEvent::Heartbeat(mask));
                Ok(Ok(()))
            }
            EventType::ChannelVoice => self.process_channel_voice(body, &mut on_event),
            EventType::SysExFragment => self.process_sysex(body, now, &mut on_event),
            EventType::Unknown(t) => Ok(Err(RxDrop::UnknownEventType(t))),
        }
    }

    fn process_channel_voice<F>(
        &mut self,
        body: &[u8],
        on_event: &mut F,
    ) -> Result<Result<(), RxDrop>, RxError>
    where
        F: FnMut(RxEvent<'_>),
    {
        for tuple in ChannelVoiceIter::new(body) {
            match tuple {
                Ok((event_seq, midi)) => {
                    match self.event_replay.check_and_advance(event_seq) {
                        CheckOutcome::Accept | CheckOutcome::AcceptSessionReset => {
                            on_event(RxEvent::ChannelVoice(midi));
                        }
                        CheckOutcome::Replay | CheckOutcome::TooOld => {
                            // Silent dedup — expected for retransmits.
                        }
                    }
                }
                Err(_) => return Ok(Err(RxDrop::MalformedBody)),
            }
        }
        Ok(Ok(()))
    }

    fn process_sysex<F>(
        &mut self,
        body: &[u8],
        now: embassy_time::Instant,
        on_event: &mut F,
    ) -> Result<Result<(), RxDrop>, RxError>
    where
        F: FnMut(RxEvent<'_>),
    {
        let parts = match proto::parse_sysex_fragment(body) {
            Ok(p) => p,
            Err(_) => return Ok(Err(RxDrop::MalformedBody)),
        };
        match self.sysex_reasm.process_fragment(
            parts.sysex_id,
            parts.frag_idx,
            parts.frag_total,
            parts.data,
            now,
            &mut self.sysex_scratch,
        ) {
            SysExOutcome::Pending | SysExOutcome::Replay => Ok(Ok(())),
            SysExOutcome::Dropped => Ok(Err(RxDrop::SysExDropped)),
            SysExOutcome::Complete(body) => {
                on_event(RxEvent::SysExComplete(body));
                Ok(Ok(()))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Watchdog + Heartbeat — timer-based primitives the app composes via select
// ---------------------------------------------------------------------------

use embassy_time::{Duration, Instant, Timer};

/// Receiver-side watchdog: fires after `timeout` elapses with no kick.
///
/// Spec target: 200 ms.  The app composes this with the radio's
/// receive future via `embassy_futures::select`.  On each accepted
/// packet the app calls [`Self::kick`]; on watchdog expiry the app
/// surfaces "link lost" (typically all-notes-off + sustain-off) AND
/// calls `LinkReceiver::mark_link_down()` so the next packet triggers a
/// session reset.
pub struct WatchdogTimer {
    timeout: Duration,
    deadline: Instant,
}

impl WatchdogTimer {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            deadline: Instant::now() + timeout,
        }
    }

    pub fn kick(&mut self) {
        self.deadline = Instant::now() + self.timeout;
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub async fn wait(&self) {
        Timer::at(self.deadline).await;
    }
}

/// Transmitter-side heartbeat: tells the app "you should send something
/// now or the receiver's watchdog will trip".
///
/// Spec target: 10 ms (20× safety margin against the 200 ms watchdog).
pub struct HeartbeatTimer {
    interval: Duration,
    next_due: Instant,
}

impl HeartbeatTimer {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            next_due: Instant::now() + interval,
        }
    }

    /// Note that the app just sent something (heartbeat or otherwise).
    /// The next heartbeat deadline is reset to `interval` from now, so
    /// a heartbeat fires only after `interval` of silence following any
    /// transmission.
    ///
    /// We deliberately do NOT advance from the previous deadline — that
    /// approach (intended to give a fixed-rate cadence) interacts badly
    /// with K=3 retransmit bursts.  Each `note_send` call would push
    /// the deadline `interval` further out, while wall clock barely
    /// advances.  After ~100 ms of bursting at 200 events/sec, the
    /// deadline is seconds in the future; when the burst ends, the RX
    /// watchdog (200 ms) trips before any heartbeat actually goes on
    /// air.  Anchoring to `now + interval` caps how far the deadline
    /// can get ahead and keeps the receiver fed.
    pub fn note_send(&mut self) {
        self.next_due = Instant::now() + self.interval;
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    pub fn next_due(&self) -> Instant {
        self.next_due
    }

    pub async fn wait(&self) {
        Timer::at(self.next_due).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> embassy_time::Instant {
        embassy_time::Instant::from_ticks(0)
    }

    // -------- LinkSender --------

    #[test]
    fn sender_increments_packet_seq() {
        let mut s = LinkSender::no_crypto(7);
        let mut buf = [0u8; 64];
        let _ = s.encode(EventType::Heartbeat, &[], &mut buf).unwrap();
        assert_eq!(s.packet_seq(), 1);
        let _ = s.encode(EventType::Heartbeat, &[], &mut buf).unwrap();
        assert_eq!(s.packet_seq(), 2);
    }

    #[test]
    fn sender_writes_correct_header() {
        let mut s = LinkSender::no_crypto(7);
        let mut buf = [0u8; 64];
        let body = [0x00, 0x05, 0x90, 60, 100];
        let n = s.encode(EventType::ChannelVoice, &body, &mut buf).unwrap();
        let (hdr, decoded_body) = proto::decode(&buf[..n]).unwrap();
        assert_eq!(hdr.ver, VER_V1);
        assert_eq!(hdr.key_fp, KEY_FP_NONE);
        assert_eq!(hdr.boot_counter, 7);
        assert_eq!(hdr.packet_seq, 0);
        assert!(matches!(hdr.event_type, EventType::ChannelVoice));
        assert_eq!(decoded_body, &body);
    }

    #[test]
    fn sender_packet_seq_overflow_returns_err() {
        let mut s = LinkSender::no_crypto(0);
        s.packet_seq = u32::MAX;
        let mut buf = [0u8; 64];
        match s.encode(EventType::Heartbeat, &[], &mut buf) {
            Err(SendError::PacketSeqOverflow) => {}
            other => panic!("expected overflow, got {other:?}"),
        }
    }

    // -------- LinkReceiver: basic accept/dedup --------

    fn encode_cv(s: &mut LinkSender, events: &[(u16, &[u8])]) -> ([u8; 64], usize) {
        let mut body = [0u8; 32];
        let mut off = 0;
        for (seq, midi) in events {
            body[off..off + 2].copy_from_slice(&seq.to_be_bytes());
            off += 2;
            body[off..off + midi.len()].copy_from_slice(midi);
            off += midi.len();
        }
        let mut wire = [0u8; 64];
        let n = s
            .encode(EventType::ChannelVoice, &body[..off], &mut wire)
            .unwrap();
        (wire, n)
    }

    #[test]
    fn receiver_accepts_first_packet() {
        let mut s = LinkSender::no_crypto(0);
        let mut r = LinkReceiver::no_crypto();
        let (wire, n) = encode_cv(&mut s, &[(0, &[0x90, 60, 100])]);
        let mut events: std::vec::Vec<std::vec::Vec<u8>> = std::vec::Vec::new();
        r.process(&wire[..n], now(), |ev| {
            if let RxEvent::ChannelVoice(m) = ev {
                events.push(m.to_vec());
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(events, vec![vec![0x90, 60, 100]]);
    }

    #[test]
    fn receiver_dedup_event_seq_within_packet() {
        let mut s = LinkSender::no_crypto(0);
        let mut r = LinkReceiver::no_crypto();
        // Same event_seq twice in one packet — second must be dropped.
        let (wire, n) = encode_cv(&mut s, &[(5, &[0x90, 60, 100]), (5, &[0x90, 60, 100])]);
        let mut count = 0;
        r.process(&wire[..n], now(), |ev| {
            if matches!(ev, RxEvent::ChannelVoice(_)) {
                count += 1;
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn receiver_dedups_retransmit_packets() {
        // Same event_seq across two distinct packets (retransmit case).
        // Receiver's event_replay window must drop the second.
        let mut s = LinkSender::no_crypto(0);
        let mut r = LinkReceiver::no_crypto();
        let body = [0x00, 0x07, 0x90, 60, 100];
        let mut w1 = [0u8; 64];
        let n1 = s.encode(EventType::ChannelVoice, &body, &mut w1).unwrap();
        let mut w2 = [0u8; 64];
        let n2 = s.encode(EventType::ChannelVoice, &body, &mut w2).unwrap();
        // Packet 1: event fires.
        let mut count = 0;
        r.process(&w1[..n1], now(), |ev| {
            if matches!(ev, RxEvent::ChannelVoice(_)) {
                count += 1;
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(count, 1);
        // Packet 2 (different packet_seq, same event_seq): no event fires.
        r.process(&w2[..n2], now(), |ev| {
            if matches!(ev, RxEvent::ChannelVoice(_)) {
                count += 1;
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(count, 1, "retransmit should not refire event");
    }

    #[test]
    fn receiver_packet_replay_drops_exact_duplicate() {
        let mut s = LinkSender::no_crypto(0);
        let mut r = LinkReceiver::no_crypto();
        let mut buf = [0u8; 64];
        let n = s.encode(EventType::Heartbeat, &[], &mut buf).unwrap();
        // First time accepted.
        let r1 = r.process(&buf[..n], now(), |_| {}).unwrap();
        assert!(r1.is_ok());
        // Replay the exact same wire bytes.
        let r2 = r.process(&buf[..n], now(), |_| {}).unwrap();
        assert!(matches!(r2, Err(RxDrop::PacketReplay(_))));
    }

    #[test]
    fn receiver_drops_wrong_key_fp() {
        let mut s = LinkSender::no_crypto(0);
        let mut r = LinkReceiver::new([0x01, 0x02, 0x03]);
        let mut buf = [0u8; 64];
        let n = s.encode(EventType::Heartbeat, &[], &mut buf).unwrap();
        let r1 = r.process(&buf[..n], now(), |_| {}).unwrap();
        match r1 {
            Err(RxDrop::KeyFpMismatch(seen)) => assert_eq!(seen, KEY_FP_NONE),
            other => panic!("expected KeyFpMismatch, got {other:?}"),
        }
    }

    #[test]
    fn receiver_decode_error_propagates() {
        let mut r = LinkReceiver::no_crypto();
        let truncated = [0x01u8, 0x00, 0x00];
        match r.process(&truncated, now(), |_| {}) {
            Err(RxError::Decode(_)) => {}
            other => panic!("expected Decode error, got {other:?}"),
        }
    }

    // -------- Session reset --------

    #[test]
    fn receiver_resets_on_boot_counter_change() {
        let mut s1 = LinkSender::no_crypto(100);
        let mut r = LinkReceiver::no_crypto();
        let mut buf = [0u8; 64];
        for _ in 0..10 {
            let n = s1.encode(EventType::Heartbeat, &[], &mut buf).unwrap();
            r.process(&buf[..n], now(), |_| {}).unwrap().unwrap();
        }
        // New TX with a fresh boot_counter, packet_seq starts at 0 again.
        let mut s2 = LinkSender::no_crypto(50);
        let n = s2.encode(EventType::Heartbeat, &[], &mut buf).unwrap();
        r.process(&buf[..n], now(), |_| {}).unwrap().unwrap();
        assert_eq!(r.last_boot_counter(), Some(50));
    }

    #[test]
    fn receiver_resets_on_link_down_then_packet() {
        let mut s = LinkSender::no_crypto(7);
        let mut r = LinkReceiver::no_crypto();
        let mut buf = [0u8; 64];
        // Build up some state.
        for _ in 0..10 {
            let n = s.encode(EventType::Heartbeat, &[], &mut buf).unwrap();
            r.process(&buf[..n], now(), |_| {}).unwrap().unwrap();
        }
        // TX restarts with a fresh boot_counter that happens to collide
        // with the old one — and packet_seq starts at 0 again.  Without
        // some session-reset signal, packet_seq=0 against high=10 looks
        // like a too-old packet (distance 10, in window) and then a replay.
        let mut s2 = LinkSender::no_crypto(7);
        let n = s2.encode(EventType::Heartbeat, &[], &mut buf).unwrap();
        // Simulate watchdog firing during the TX outage.
        r.mark_link_down();
        let r1 = r.process(&buf[..n], now(), |_| {}).unwrap();
        assert!(
            r1.is_ok(),
            "post-link-down packet should be accepted: {r1:?}"
        );
    }

    #[test]
    fn receiver_resets_on_huge_backward_packet_seq_jump() {
        // Busy-session boot_counter collision: previous session reached
        // packet_seq=200_000, then TX rebooted with same boot_counter
        // and packet_seq=0.  The 100k threshold catches this.
        let mut s = LinkSender::no_crypto(7);
        s.packet_seq = 200_000;
        let mut r = LinkReceiver::no_crypto();
        let mut buf = [0u8; 64];
        // Establish high.
        let n = s.encode(EventType::Heartbeat, &[], &mut buf).unwrap();
        r.process(&buf[..n], now(), |_| {}).unwrap().unwrap();
        // New TX with same boot_counter, packet_seq=0.
        let mut s2 = LinkSender::no_crypto(7);
        let n = s2.encode(EventType::Heartbeat, &[], &mut buf).unwrap();
        let r1 = r.process(&buf[..n], now(), |_| {}).unwrap();
        assert!(r1.is_ok(), "huge backward jump should reset session");
    }

    // -------- SysEx end-to-end --------

    #[test]
    fn receiver_reassembles_two_fragment_sysex() {
        let mut s = LinkSender::no_crypto(0);
        let mut r = LinkReceiver::no_crypto();
        let mut wire = [0u8; 64];
        let mut body = [0u8; 32];

        // Fragment 0: [sysex_id=42, idx=0, total=2, data="AB"]
        let parts0 = SysExFragmentParts {
            sysex_id: 42,
            frag_idx: 0,
            frag_total: 2,
            data: &[0xAA, 0xBB],
        };
        let body_n = proto::encode_sysex_fragment_body(&mut body, &parts0).unwrap();
        let n = s
            .encode(EventType::SysExFragment, &body[..body_n], &mut wire)
            .unwrap();
        let mut got: std::vec::Vec<std::vec::Vec<u8>> = std::vec::Vec::new();
        r.process(&wire[..n], now(), |ev| {
            if let RxEvent::SysExComplete(b) = ev {
                got.push(b.to_vec());
            }
        })
        .unwrap()
        .unwrap();
        assert!(got.is_empty()); // not yet complete

        // Fragment 1: [sysex_id=42, idx=1, total=2, data="CD"]
        let parts1 = SysExFragmentParts {
            sysex_id: 42,
            frag_idx: 1,
            frag_total: 2,
            data: &[0xCC, 0xDD],
        };
        let body_n = proto::encode_sysex_fragment_body(&mut body, &parts1).unwrap();
        let n = s
            .encode(EventType::SysExFragment, &body[..body_n], &mut wire)
            .unwrap();
        r.process(&wire[..n], now(), |ev| {
            if let RxEvent::SysExComplete(b) = ev {
                got.push(b.to_vec());
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(got, vec![vec![0xF0, 0xAA, 0xBB, 0xCC, 0xDD, 0xF7]]);
    }

    // -------- Heartbeat --------

    #[test]
    fn receiver_emits_heartbeat_event() {
        let mut s = LinkSender::no_crypto(0);
        let mut r = LinkReceiver::no_crypto();
        let mut buf = [0u8; 64];
        // Empty heartbeat body — receiver should report mask=None.
        let n = s.encode(EventType::Heartbeat, &[], &mut buf).unwrap();
        let mut saw = false;
        r.process(&buf[..n], now(), |ev| {
            if let RxEvent::Heartbeat(mask) = ev {
                assert_eq!(mask, None, "empty body should produce None mask");
                saw = true;
            }
        })
        .unwrap()
        .unwrap();
        assert!(saw);
    }

    #[test]
    fn receiver_decodes_heartbeat_active_mask() {
        let mut s = LinkSender::no_crypto(0);
        let mut r = LinkReceiver::no_crypto();
        let mut buf = [0u8; 64];
        // Heartbeat body = 2-byte big-endian active-channel mask.  Bit 0
        // and bit 5 set → channels 0 and 5 are active.
        let mask: u16 = 0b0000_0000_0010_0001;
        let body = mask.to_be_bytes();
        let n = s.encode(EventType::Heartbeat, &body, &mut buf).unwrap();
        let mut decoded: Option<u16> = None;
        r.process(&buf[..n], now(), |ev| {
            if let RxEvent::Heartbeat(m) = ev {
                decoded = m;
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(decoded, Some(mask));
    }

    // -------- AEAD encrypt / decrypt round-trip --------

    /// Helper: build matched TX + RX pair for a given cipher with the
    /// same key, device_id, and boot_counter — i.e. the scenario two
    /// real units form when they share a configured key.
    fn aead_pair(cipher: CipherId) -> (LinkSender, LinkReceiver) {
        let key = [0x42u8; osrf_crypto::KEY_LEN];
        let device_id = 0xDEAD_BEEF;
        let ctx = AeadContext {
            cipher,
            key,
            device_id,
            direction: Direction::TxToRx,
        };
        (LinkSender::with_aead(7, ctx), LinkReceiver::with_aead(ctx))
    }

    /// One channel-voice event survives encrypt → decrypt and arrives
    /// byte-identical at the receiver.  Exercised under both ciphers
    /// to confirm the dispatch table doesn't silently fall through.
    #[test]
    fn aead_roundtrip_channel_voice_chacha() {
        let (mut s, mut r) = aead_pair(CipherId::ChaCha20Poly1305);
        let (wire, n) = encode_cv(&mut s, &[(0, &[0x90, 60, 100])]);
        let mut got: std::vec::Vec<std::vec::Vec<u8>> = std::vec::Vec::new();
        r.process(&wire[..n], now(), |ev| {
            if let RxEvent::ChannelVoice(m) = ev {
                got.push(m.to_vec());
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(got, vec![vec![0x90, 60, 100]]);
    }

    #[test]
    fn aead_roundtrip_channel_voice_aes() {
        let (mut s, mut r) = aead_pair(CipherId::Aes128Ccm);
        let (wire, n) = encode_cv(&mut s, &[(0, &[0x90, 60, 100])]);
        let mut got: std::vec::Vec<std::vec::Vec<u8>> = std::vec::Vec::new();
        r.process(&wire[..n], now(), |ev| {
            if let RxEvent::ChannelVoice(m) = ev {
                got.push(m.to_vec());
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(got, vec![vec![0x90, 60, 100]]);
    }

    /// AEAD packet is 16 bytes longer than the equivalent open-mode
    /// packet (the tag).  Confirms the wire-size accounting is right
    /// — important because the SX1262 driver sizes its TX buffer
    /// from this number.
    #[test]
    fn aead_packet_is_tag_len_bytes_longer_than_open() {
        let mut open = LinkSender::no_crypto(0);
        let mut aead = aead_pair(CipherId::ChaCha20Poly1305).0;
        let body = [0x00, 0x05, 0x90, 60, 100];
        let mut buf = [0u8; 64];
        let n_open = open
            .encode(EventType::ChannelVoice, &body, &mut buf)
            .unwrap();
        let n_aead = aead
            .encode(EventType::ChannelVoice, &body, &mut buf)
            .unwrap();
        assert_eq!(n_aead, n_open + TAG_LEN);
    }

    /// Flipping one bit of the ciphertext fails authentication.  This
    /// is the tamper-detection guarantee — without it AEAD would only
    /// give privacy, not authenticity.
    #[test]
    fn aead_tampered_ciphertext_rejected() {
        let (mut s, mut r) = aead_pair(CipherId::ChaCha20Poly1305);
        let (mut wire, n) = encode_cv(&mut s, &[(0, &[0x90, 60, 100])]);
        // Flip one byte of the ciphertext (somewhere in the body region).
        wire[HEADER_LEN + 2] ^= 0x01;
        let mut got_count = 0;
        let outcome = r.process(&wire[..n], now(), |_| got_count += 1).unwrap();
        assert_eq!(outcome, Err(RxDrop::AeadFail));
        assert_eq!(got_count, 0, "tampered packet must not emit events");
    }

    /// Flipping a header bit (changes the AAD) also fails auth.
    /// The header is not encrypted but it is authenticated.
    #[test]
    fn aead_tampered_header_rejected() {
        let (mut s, mut r) = aead_pair(CipherId::ChaCha20Poly1305);
        let (mut wire, n) = encode_cv(&mut s, &[(0, &[0x90, 60, 100])]);
        // Flip a bit in the boot_counter field — the header decode still
        // succeeds because version + event_type are intact, but the AAD
        // mismatches the one TX signed with, so the tag check fails.
        wire[4] ^= 0x01;
        let outcome = r.process(&wire[..n], now(), |_| {}).unwrap();
        assert_eq!(outcome, Err(RxDrop::AeadFail));
    }

    /// Receiver expecting AEAD rejects a plaintext packet at the
    /// fingerprint check — its expected `key_fp` derives from the
    /// configured key and won't match `KEY_FP_NONE`.
    #[test]
    fn aead_rx_rejects_open_packet() {
        let mut open_tx = LinkSender::no_crypto(0);
        let (_, mut aead_rx) = aead_pair(CipherId::ChaCha20Poly1305);
        let (wire, n) = encode_cv(&mut open_tx, &[(0, &[0x90, 60, 100])]);
        let outcome = aead_rx.process(&wire[..n], now(), |_| {}).unwrap();
        assert!(matches!(outcome, Err(RxDrop::KeyFpMismatch(_))));
    }

    /// Open-mode receiver rejects an AEAD packet for the same reason
    /// — fingerprint mismatch.  Bytes are never even examined as
    /// ciphertext on this path.
    #[test]
    fn open_rx_rejects_aead_packet() {
        let (mut aead_tx, _) = aead_pair(CipherId::ChaCha20Poly1305);
        let mut open_rx = LinkReceiver::no_crypto();
        let (wire, n) = encode_cv(&mut aead_tx, &[(0, &[0x90, 60, 100])]);
        let outcome = open_rx.process(&wire[..n], now(), |_| {}).unwrap();
        assert!(matches!(outcome, Err(RxDrop::KeyFpMismatch(_))));
    }

    /// RX with the wrong key (same cipher, different bytes) rejects
    /// at the fingerprint check — fingerprints differ even if the
    /// attacker knows the cipher.
    #[test]
    fn aead_wrong_key_rejected() {
        let mut s = {
            let key = [0x42u8; osrf_crypto::KEY_LEN];
            let ctx = AeadContext {
                cipher: CipherId::ChaCha20Poly1305,
                key,
                device_id: 0xDEAD_BEEF,
                direction: Direction::TxToRx,
            };
            LinkSender::with_aead(7, ctx)
        };
        let mut r = {
            let key = [0x99u8; osrf_crypto::KEY_LEN];
            let ctx = AeadContext {
                cipher: CipherId::ChaCha20Poly1305,
                key,
                device_id: 0xDEAD_BEEF,
                direction: Direction::TxToRx,
            };
            LinkReceiver::with_aead(ctx)
        };
        let (wire, n) = encode_cv(&mut s, &[(0, &[0x90, 60, 100])]);
        let outcome = r.process(&wire[..n], now(), |_| {}).unwrap();
        assert!(matches!(outcome, Err(RxDrop::KeyFpMismatch(_))));
    }

    /// Heartbeat (2-byte body) round-trips through AEAD too —
    /// the dispatch is body-format agnostic.
    #[test]
    fn aead_roundtrip_heartbeat() {
        let (mut s, mut r) = aead_pair(CipherId::Aes128Ccm);
        let mut buf = [0u8; 80]; // room for header + 2 body + 16 tag
        let mask: u16 = 0b0010_0001;
        let n = s
            .encode(EventType::Heartbeat, &mask.to_be_bytes(), &mut buf)
            .unwrap();
        let mut got: Option<u16> = None;
        r.process(&buf[..n], now(), |ev| {
            if let RxEvent::Heartbeat(m) = ev {
                got = m;
            }
        })
        .unwrap()
        .unwrap();
        assert_eq!(got, Some(mask));
    }
}
