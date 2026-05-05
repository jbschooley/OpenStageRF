// SPDX-License-Identifier: AGPL-3.0-or-later
#![cfg_attr(not(test), no_std)]

//! OpenStageRF Transport Envelope v1 — wire-format encode/decode.
//!
//! See `SPEC.md` in this directory for the full specification.  This crate
//! implements:
//!
//! * Header encode/decode (`encode`, `decode`).
//! * ChannelVoice and SysExFragment body parsing (`ChannelVoiceIter`,
//!   `parse_sysex_fragment`, `encode_sysex_fragment_body`).
//! * Replay-window types (`PacketReplayWindow32`, `EventReplayWindow16`)
//!   including the modular-arithmetic event window with the
//!   session-reset fallback for `boot_counter` collisions.
//!
//! AEAD/MAC computation lives in `osrf-crypto` (future).  The link
//! layer (queue, watchdog, SysEx reassembly) lives in `osrf-link`.
//! This crate is `no_std` and depends only on `heapless` and (optionally)
//! `defmt`.

// ── Constants ────────────────────────────────────────────────────────────────

/// Transport envelope version byte.  v1 = 0x01.
pub const VER_V1: u8 = 0x01;

/// Total fixed header length:
/// `ver(1) + key_fp(3) + boot_counter(2) + packet_seq(4) + event_type(1)`.
///
/// The full header is the AEAD AAD per the spec.
pub const HEADER_LEN: usize = 11;

/// Sentinel meaning "no encryption, no authentication".
pub const KEY_FP_NONE: KeyFp = [0, 0, 0];

/// Reserved sentinel — must not appear on the wire.
pub const KEY_FP_RESERVED: KeyFp = [0xFF, 0xFF, 0xFF];

/// Three-byte key fingerprint.  See `SPEC.md` § "key_fp".
pub type KeyFp = [u8; 3];

/// Maximum body bytes (after header, before any AEAD tag) that the encoder
/// will write.  Sized to fit the RF payload limit minus the header — for
/// the current 64-byte radio packet, that's `64 - HEADER_LEN = 53`.
/// Crypto-enabled builds need to leave room for the tag too; that's the
/// link layer's concern (it supplies a smaller body buffer).
pub const MAX_BODY_LEN: usize = 53;

/// Maximum SysEx fragment data bytes (excluding the 4-byte fragment
/// header `[sysex_id:2][frag_idx:1][frag_total:1]`).
pub const MAX_FRAG_DATA_BYTES: usize = MAX_BODY_LEN - 4;

/// Backward `packet_seq` jump larger than this triggers the session-reset
/// fallback path in `PacketReplayWindow32`.  Sized to comfortably exceed
/// the peak `packet_seq` advance over one minute of sustained max-rate
/// transmission (~90 000 packets/min at 1500 packets/sec).  See SPEC.md
/// § "Session-reset fallback".
pub const SESSION_RESET_GAP: u32 = 100_000;

/// Numeric event_type discriminators.  Public so callers can match raw values.
pub mod event_type {
    pub const HEARTBEAT: u8 = 0x01;
    pub const CHANNEL_VOICE: u8 = 0x02;
    pub const SYSEX_FRAGMENT: u8 = 0x03;
}

// ── EventType ────────────────────────────────────────────────────────────────

/// Discriminator for the body format.  `Unknown(u8)` preserves any
/// reserved-future value so the receiver can drop it gracefully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum EventType {
    Heartbeat,
    ChannelVoice,
    SysExFragment,
    Unknown(u8),
}

impl EventType {
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Heartbeat => event_type::HEARTBEAT,
            Self::ChannelVoice => event_type::CHANNEL_VOICE,
            Self::SysExFragment => event_type::SYSEX_FRAGMENT,
            Self::Unknown(b) => b,
        }
    }

    pub const fn from_u8(b: u8) -> Self {
        match b {
            event_type::HEARTBEAT => Self::Heartbeat,
            event_type::CHANNEL_VOICE => Self::ChannelVoice,
            event_type::SYSEX_FRAGMENT => Self::SysExFragment,
            _ => Self::Unknown(b),
        }
    }
}

// ── Header ───────────────────────────────────────────────────────────────────

/// Parsed packet header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Header {
    pub ver: u8,
    pub key_fp: KeyFp,
    pub boot_counter: u16,
    pub packet_seq: u32,
    pub event_type: EventType,
}

// ── Errors ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum EncodeError {
    /// `out` doesn't fit `HEADER_LEN + body.len()` bytes.
    BufferTooSmall,
    /// Body exceeds `MAX_BODY_LEN`.
    BodyTooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum DecodeError {
    /// `buf.len() < HEADER_LEN`.
    TooShort,
    /// Header `ver` byte doesn't match a version we recognize.
    UnknownVersion(u8),
    /// `event_type = 0x00` is reserved and MUST NOT appear on the wire.
    ReservedEventType,
}

// ── Encode / Decode ──────────────────────────────────────────────────────────

/// Encode header + body into `out`.  No AEAD tag is written — that's the
/// crypto layer's job.  Returns the number of bytes written.
pub fn encode(out: &mut [u8], header: &Header, body: &[u8]) -> Result<usize, EncodeError> {
    if body.len() > MAX_BODY_LEN {
        return Err(EncodeError::BodyTooLarge);
    }
    let total = HEADER_LEN + body.len();
    if out.len() < total {
        return Err(EncodeError::BufferTooSmall);
    }
    out[0] = header.ver;
    out[1..4].copy_from_slice(&header.key_fp);
    out[4..6].copy_from_slice(&header.boot_counter.to_be_bytes());
    out[6..10].copy_from_slice(&header.packet_seq.to_be_bytes());
    out[10] = header.event_type.as_u8();
    out[HEADER_LEN..total].copy_from_slice(body);
    Ok(total)
}

/// Decode header from `buf`.  Returns the header and a slice over the body
/// bytes (everything after the header).  The caller is responsible for
/// further parsing the body based on `header.event_type`.
pub fn decode(buf: &[u8]) -> Result<(Header, &[u8]), DecodeError> {
    if buf.len() < HEADER_LEN {
        return Err(DecodeError::TooShort);
    }
    let ver = buf[0];
    if ver != VER_V1 {
        return Err(DecodeError::UnknownVersion(ver));
    }
    let mut key_fp: KeyFp = [0; 3];
    key_fp.copy_from_slice(&buf[1..4]);
    let boot_counter = u16::from_be_bytes([buf[4], buf[5]]);
    let packet_seq = u32::from_be_bytes([buf[6], buf[7], buf[8], buf[9]]);
    let event_type_byte = buf[10];
    if event_type_byte == 0x00 {
        return Err(DecodeError::ReservedEventType);
    }
    let event_type = EventType::from_u8(event_type_byte);
    let body = &buf[HEADER_LEN..];
    Ok((
        Header { ver, key_fp, boot_counter, packet_seq, event_type },
        body,
    ))
}

// ── ChannelVoice body parsing ────────────────────────────────────────────────

/// Returns the byte length of a MIDI message starting with `status`, or
/// `None` if `status` is not a valid wire-format status byte.  Running
/// status, F0/F7 (SysEx markers — those use SysExFragment), and System
/// Common reserved bytes (F4, F5) are all rejected.
pub const fn midi_message_length(status: u8) -> Option<usize> {
    match status & 0xF0 {
        0xC0 | 0xD0 => Some(2),
        0x80 | 0x90 | 0xA0 | 0xB0 | 0xE0 => Some(3),
        0xF0 => match status {
            0xF1 | 0xF3 => Some(2),
            0xF2 => Some(3),
            0xF6 => Some(1),
            0xF8..=0xFF => Some(1),
            _ => None,
        },
        _ => None,
    }
}

/// Iterator over a CHANNEL_VOICE body.  Yields `(event_seq, midi)` tuples
/// or an error if the body is malformed.
pub struct ChannelVoiceIter<'a> {
    body: &'a [u8],
}

impl<'a> ChannelVoiceIter<'a> {
    pub fn new(body: &'a [u8]) -> Self {
        Self { body }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum ChannelVoiceParseError {
    /// Ran out of body bytes mid-event.
    Truncated,
    /// Status byte isn't a valid MIDI message start.
    InvalidStatus(u8),
}

impl<'a> Iterator for ChannelVoiceIter<'a> {
    type Item = Result<(u16, &'a [u8]), ChannelVoiceParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.body.is_empty() {
            return None;
        }
        if self.body.len() < 3 {
            // Need at least: 2 seq bytes + 1 status byte.
            self.body = &[];
            return Some(Err(ChannelVoiceParseError::Truncated));
        }
        let event_seq = u16::from_be_bytes([self.body[0], self.body[1]]);
        let status = self.body[2];
        let msg_len = match midi_message_length(status) {
            Some(n) => n,
            None => {
                self.body = &[];
                return Some(Err(ChannelVoiceParseError::InvalidStatus(status)));
            }
        };
        if self.body.len() < 2 + msg_len {
            self.body = &[];
            return Some(Err(ChannelVoiceParseError::Truncated));
        }
        let midi = &self.body[2..2 + msg_len];
        self.body = &self.body[2 + msg_len..];
        Some(Ok((event_seq, midi)))
    }
}

// ── SysExFragment body parsing ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct SysExFragmentParts<'a> {
    pub sysex_id: u16,
    pub frag_idx: u8,
    pub frag_total: u8,
    pub data: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum SysExParseError {
    /// Body too short to contain the 4-byte fragment header.
    Truncated,
    /// `frag_total == 0`.
    InvalidFragTotal,
    /// `frag_idx >= frag_total`.
    InvalidFragIdx { idx: u8, total: u8 },
}

pub fn parse_sysex_fragment(body: &[u8]) -> Result<SysExFragmentParts<'_>, SysExParseError> {
    if body.len() < 4 {
        return Err(SysExParseError::Truncated);
    }
    let sysex_id = u16::from_be_bytes([body[0], body[1]]);
    let frag_idx = body[2];
    let frag_total = body[3];
    let data = &body[4..];
    if frag_total == 0 {
        return Err(SysExParseError::InvalidFragTotal);
    }
    if frag_idx >= frag_total {
        return Err(SysExParseError::InvalidFragIdx { idx: frag_idx, total: frag_total });
    }
    Ok(SysExFragmentParts { sysex_id, frag_idx, frag_total, data })
}

pub fn encode_sysex_fragment_body(
    out: &mut [u8],
    parts: &SysExFragmentParts<'_>,
) -> Result<usize, EncodeError> {
    let n = 4 + parts.data.len();
    if n > MAX_BODY_LEN {
        return Err(EncodeError::BodyTooLarge);
    }
    if out.len() < n {
        return Err(EncodeError::BufferTooSmall);
    }
    out[0..2].copy_from_slice(&parts.sysex_id.to_be_bytes());
    out[2] = parts.frag_idx;
    out[3] = parts.frag_total;
    out[4..n].copy_from_slice(parts.data);
    Ok(n)
}

// ── Replay windows ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum CheckOutcome {
    Accept,
    /// Same as `Accept` but the receiver should also reset upper-layer
    /// state (event window, SysEx reassembly).  Emitted by the packet
    /// replay window when a backward `packet_seq` jump exceeds
    /// `SESSION_RESET_GAP`, indicating a `boot_counter` collision after
    /// a TX restart.
    AcceptSessionReset,
    Replay,
    TooOld,
}

/// 32-bit linear sliding-window replay detector for `packet_seq`.
///
/// Tracks the highest seq seen plus a 64-bit bitmap of which of the 64
/// preceding seqs we've also accepted.  Out-of-order packets within the
/// window are accepted at most once.  Backward jumps beyond
/// `SESSION_RESET_GAP` are treated as a fresh session and reset the
/// window — see SPEC.md § "Session-reset fallback".
#[derive(Debug, Default, Clone)]
pub struct PacketReplayWindow32 {
    high: u32,
    bitmap: u64,
    initialised: bool,
}

impl PacketReplayWindow32 {
    pub const fn new() -> Self {
        Self { high: 0, bitmap: 0, initialised: false }
    }

    pub fn reset(&mut self) {
        self.high = 0;
        self.bitmap = 0;
        self.initialised = false;
    }

    pub fn high(&self) -> Option<u32> {
        if self.initialised {
            Some(self.high)
        } else {
            None
        }
    }

    pub fn check_and_advance(&mut self, seq: u32) -> CheckOutcome {
        if !self.initialised {
            self.high = seq;
            self.bitmap = 1;
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

/// 16-bit modular sliding-window replay detector for `event_seq`.
///
/// Uses RFC 1982-style serial-number arithmetic: a new seq is "forward"
/// if the modular distance is in `[1, 32_767]`, "backward" if in
/// `[32_768, 65_535]`.  Backward by ≤ 64 → check bitmap.  Backward by
/// > 64 but < 32 768 → too old.  Wraparound (e.g., from 65 535 → 0) is
/// invisible to the algorithm because `wrapping_sub` does the right
/// thing.  See SPEC.md § "Event replay window".
#[derive(Debug, Default, Clone)]
pub struct EventReplayWindow16 {
    high: u16,
    bitmap: u64,
    initialised: bool,
}

impl EventReplayWindow16 {
    pub const fn new() -> Self {
        Self { high: 0, bitmap: 0, initialised: false }
    }

    pub fn reset(&mut self) {
        self.high = 0;
        self.bitmap = 0;
        self.initialised = false;
    }

    pub fn high(&self) -> Option<u16> {
        if self.initialised {
            Some(self.high)
        } else {
            None
        }
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
                let shift = d as u32;
                self.bitmap = if shift >= 64 { 0 } else { self.bitmap << shift };
                self.bitmap |= 1;
                self.high = seq;
                CheckOutcome::Accept
            }
            32_768..=65_471 => CheckOutcome::TooOld,
            65_472..=65_535 => {
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

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn h(boot: u16, seq: u32, et: EventType) -> Header {
        Header {
            ver: VER_V1,
            key_fp: KEY_FP_NONE,
            boot_counter: boot,
            packet_seq: seq,
            event_type: et,
        }
    }

    // ── Header round-trips ────────────────────────────────────────────────

    #[test]
    fn round_trip_heartbeat() {
        let hdr = h(7, 42, EventType::Heartbeat);
        let mut buf = [0u8; HEADER_LEN];
        let n = encode(&mut buf, &hdr, &[]).unwrap();
        assert_eq!(n, HEADER_LEN);
        let (parsed, body) = decode(&buf[..n]).unwrap();
        assert_eq!(parsed, hdr);
        assert!(body.is_empty());
    }

    #[test]
    fn round_trip_channel_voice() {
        let hdr = h(0xABCD, 0x1234_5678, EventType::ChannelVoice);
        // Body: (seq=1, NoteOn 60 100), (seq=2, NoteOn 64 100)
        let body = [
            0x00, 0x01, 0x90, 60, 100,
            0x00, 0x02, 0x90, 64, 100,
        ];
        let mut buf = [0u8; 32];
        let n = encode(&mut buf, &hdr, &body).unwrap();
        assert_eq!(n, HEADER_LEN + body.len());
        let (parsed, parsed_body) = decode(&buf[..n]).unwrap();
        assert_eq!(parsed, hdr);
        assert_eq!(parsed_body, &body);
    }

    #[test]
    fn round_trip_sysex_fragment() {
        let hdr = h(1, 9, EventType::SysExFragment);
        let parts = SysExFragmentParts {
            sysex_id: 0x4242,
            frag_idx: 0,
            frag_total: 1,
            data: &[0x7E, 0x7F, 0x06, 0x01],
        };
        let mut body_buf = [0u8; 16];
        let body_n = encode_sysex_fragment_body(&mut body_buf, &parts).unwrap();
        let mut wire = [0u8; 32];
        let n = encode(&mut wire, &hdr, &body_buf[..body_n]).unwrap();
        let (parsed_hdr, parsed_body) = decode(&wire[..n]).unwrap();
        assert_eq!(parsed_hdr, hdr);
        let parsed_parts = parse_sysex_fragment(parsed_body).unwrap();
        assert_eq!(parsed_parts, parts);
    }

    // ── Wire layout ───────────────────────────────────────────────────────

    #[test]
    fn wire_layout_known_bytes() {
        let hdr = Header {
            ver: VER_V1,
            key_fp: [0x12, 0x34, 0x56],
            boot_counter: 0x00AB,
            packet_seq: 0xCDEF_0123,
            event_type: EventType::ChannelVoice,
        };
        let body = [0x00, 0x05, 0x90, 60, 100];
        let mut buf = [0u8; 16];
        let n = encode(&mut buf, &hdr, &body).unwrap();
        assert_eq!(n, 16);
        assert_eq!(
            buf,
            [
                0x01, // ver
                0x12, 0x34, 0x56, // key_fp
                0x00, 0xAB, // boot_counter
                0xCD, 0xEF, 0x01, 0x23, // packet_seq
                0x02, // event_type = ChannelVoice
                0x00, 0x05, 0x90, 60, 100, // body
            ]
        );
    }

    #[test]
    fn aad_is_first_eleven_bytes() {
        // AAD = ver || key_fp || boot_counter || packet_seq || event_type.
        let hdr = h(0xBEEF, 0xDEAD_BEEF, EventType::ChannelVoice);
        let mut buf = [0u8; 32];
        encode(&mut buf, &hdr, &[0, 1, 0x90, 60, 100]).unwrap();
        assert_eq!(buf[0], VER_V1);
        assert_eq!(&buf[1..4], &KEY_FP_NONE);
        assert_eq!(&buf[4..6], &0xBEEFu16.to_be_bytes());
        assert_eq!(&buf[6..10], &0xDEAD_BEEFu32.to_be_bytes());
        assert_eq!(buf[10], event_type::CHANNEL_VOICE);
    }

    // ── ChannelVoice parsing ─────────────────────────────────────────────

    #[test]
    fn channel_voice_iter_yields_each_event() {
        // (seq=10, NoteOn C, NoteOn E, NoteOn G)
        let body = [
            0x00, 0x0A, 0x90, 60, 100,
            0x00, 0x0B, 0x90, 64, 100,
            0x00, 0x0C, 0x90, 67, 100,
        ];
        let events: std::vec::Vec<_> = ChannelVoiceIter::new(&body)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0], (10, &[0x90, 60, 100][..]));
        assert_eq!(events[1], (11, &[0x90, 64, 100][..]));
        assert_eq!(events[2], (12, &[0x90, 67, 100][..]));
    }

    #[test]
    fn channel_voice_iter_handles_mixed_lengths() {
        // ProgramChange (2 bytes) + NoteOn (3 bytes) + TimingClock (1 byte)
        let body = [
            0x00, 0x01, 0xC5, 42,
            0x00, 0x02, 0x90, 60, 100,
            0x00, 0x03, 0xF8,
        ];
        let events: std::vec::Vec<_> = ChannelVoiceIter::new(&body)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0], (1, &[0xC5, 42][..]));
        assert_eq!(events[1], (2, &[0x90, 60, 100][..]));
        assert_eq!(events[2], (3, &[0xF8][..]));
    }

    #[test]
    fn channel_voice_iter_empty_body_yields_nothing() {
        let events: std::vec::Vec<_> = ChannelVoiceIter::new(&[]).collect();
        assert!(events.is_empty());
    }

    #[test]
    fn channel_voice_iter_truncated_returns_error() {
        let body = [0x00, 0x01, 0x90, 60]; // missing velocity byte
        let mut iter = ChannelVoiceIter::new(&body);
        match iter.next() {
            Some(Err(ChannelVoiceParseError::Truncated)) => {}
            other => panic!("expected Truncated, got {other:?}"),
        }
        // Iterator is now exhausted.
        assert!(iter.next().is_none());
    }

    #[test]
    fn channel_voice_iter_invalid_status_returns_error() {
        // 0xF4 is reserved (System Common, not allocated).
        let body = [0x00, 0x01, 0xF4, 0x00];
        let mut iter = ChannelVoiceIter::new(&body);
        match iter.next() {
            Some(Err(ChannelVoiceParseError::InvalidStatus(0xF4))) => {}
            other => panic!("expected InvalidStatus(0xF4), got {other:?}"),
        }
    }

    // ── SysEx parsing ────────────────────────────────────────────────────

    #[test]
    fn parse_sysex_fragment_round_trips() {
        let parts = SysExFragmentParts {
            sysex_id: 0x1234,
            frag_idx: 2,
            frag_total: 5,
            data: &[1, 2, 3, 4, 5],
        };
        let mut buf = [0u8; 16];
        let n = encode_sysex_fragment_body(&mut buf, &parts).unwrap();
        let parsed = parse_sysex_fragment(&buf[..n]).unwrap();
        assert_eq!(parsed, parts);
    }

    #[test]
    fn parse_sysex_fragment_rejects_zero_total() {
        let body = [0x00, 0x01, 0x00, 0x00, 0xAA];
        assert_eq!(
            parse_sysex_fragment(&body),
            Err(SysExParseError::InvalidFragTotal)
        );
    }

    #[test]
    fn parse_sysex_fragment_rejects_idx_ge_total() {
        let body = [0x00, 0x01, 0x05, 0x05, 0xAA]; // idx=5, total=5
        assert_eq!(
            parse_sysex_fragment(&body),
            Err(SysExParseError::InvalidFragIdx { idx: 5, total: 5 })
        );
    }

    #[test]
    fn parse_sysex_fragment_rejects_truncated() {
        assert_eq!(
            parse_sysex_fragment(&[0x00, 0x01, 0x00]),
            Err(SysExParseError::Truncated)
        );
    }

    // ── Decode error paths ───────────────────────────────────────────────

    #[test]
    fn decode_rejects_unknown_version() {
        let mut buf = [0u8; HEADER_LEN];
        buf[0] = 0x99;
        match decode(&buf) {
            Err(DecodeError::UnknownVersion(0x99)) => {}
            other => panic!("expected UnknownVersion(0x99), got {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_truncated() {
        assert_eq!(decode(&[0u8; 5]), Err(DecodeError::TooShort));
        assert_eq!(decode(&[0u8; HEADER_LEN - 1]), Err(DecodeError::TooShort));
    }

    #[test]
    fn decode_rejects_reserved_event_type_zero() {
        let mut buf = [0u8; HEADER_LEN];
        buf[0] = VER_V1;
        // event_type = 0x00 (reserved)
        assert_eq!(decode(&buf), Err(DecodeError::ReservedEventType));
    }

    #[test]
    fn decode_preserves_unknown_event_type() {
        let hdr = h(0, 0, EventType::Unknown(0x05));
        let mut buf = [0u8; HEADER_LEN + 1];
        let n = encode(&mut buf, &hdr, &[0xFF]).unwrap();
        let (parsed, body) = decode(&buf[..n]).unwrap();
        assert!(matches!(parsed.event_type, EventType::Unknown(0x05)));
        assert_eq!(body, &[0xFF]);
    }

    // ── Encode error paths ───────────────────────────────────────────────

    #[test]
    fn encode_rejects_oversize_body() {
        let mut buf = [0u8; HEADER_LEN + MAX_BODY_LEN + 8];
        let body = [0u8; MAX_BODY_LEN + 1];
        let hdr = h(0, 0, EventType::ChannelVoice);
        assert_eq!(
            encode(&mut buf, &hdr, &body),
            Err(EncodeError::BodyTooLarge)
        );
    }

    #[test]
    fn encode_rejects_buffer_too_small() {
        let mut buf = [0u8; 5];
        let hdr = h(0, 0, EventType::Heartbeat);
        assert_eq!(encode(&mut buf, &hdr, &[]), Err(EncodeError::BufferTooSmall));
    }

    // ── PacketReplayWindow32 ─────────────────────────────────────────────

    #[test]
    fn packet_replay_first_packet_accepted() {
        let mut w = PacketReplayWindow32::new();
        assert_eq!(w.check_and_advance(100), CheckOutcome::Accept);
    }

    #[test]
    fn packet_replay_strict_forward_accepted() {
        let mut w = PacketReplayWindow32::new();
        assert_eq!(w.check_and_advance(1), CheckOutcome::Accept);
        assert_eq!(w.check_and_advance(2), CheckOutcome::Accept);
        assert_eq!(w.check_and_advance(3), CheckOutcome::Accept);
    }

    #[test]
    fn packet_replay_same_seq_twice_rejected() {
        let mut w = PacketReplayWindow32::new();
        assert_eq!(w.check_and_advance(5), CheckOutcome::Accept);
        assert_eq!(w.check_and_advance(5), CheckOutcome::Replay);
    }

    #[test]
    fn packet_replay_out_of_order_in_window_accepted_once() {
        let mut w = PacketReplayWindow32::new();
        assert_eq!(w.check_and_advance(10), CheckOutcome::Accept);
        assert_eq!(w.check_and_advance(8), CheckOutcome::Accept);
        assert_eq!(w.check_and_advance(8), CheckOutcome::Replay);
    }

    #[test]
    fn packet_replay_too_old_rejected() {
        let mut w = PacketReplayWindow32::new();
        assert_eq!(w.check_and_advance(1000), CheckOutcome::Accept);
        // 1000 - 935 = 65, just outside 64-deep window.
        assert_eq!(w.check_and_advance(935), CheckOutcome::TooOld);
    }

    #[test]
    fn packet_replay_session_reset_on_huge_backward_jump() {
        let mut w = PacketReplayWindow32::new();
        assert_eq!(w.check_and_advance(200_000), CheckOutcome::Accept);
        // Jump backward by 100_000+ → session reset.
        assert_eq!(
            w.check_and_advance(50),
            CheckOutcome::AcceptSessionReset
        );
        // Window reset to new high.
        assert_eq!(w.check_and_advance(50), CheckOutcome::Replay);
        assert_eq!(w.check_and_advance(51), CheckOutcome::Accept);
    }

    // ── EventReplayWindow16 ──────────────────────────────────────────────

    #[test]
    fn event_replay_first_packet_accepted() {
        let mut w = EventReplayWindow16::new();
        assert_eq!(w.check_and_advance(0), CheckOutcome::Accept);
    }

    #[test]
    fn event_replay_strict_forward_accepted() {
        let mut w = EventReplayWindow16::new();
        for s in 0..10 {
            assert_eq!(w.check_and_advance(s), CheckOutcome::Accept);
        }
    }

    #[test]
    fn event_replay_same_seq_rejected() {
        let mut w = EventReplayWindow16::new();
        assert_eq!(w.check_and_advance(42), CheckOutcome::Accept);
        assert_eq!(w.check_and_advance(42), CheckOutcome::Replay);
    }

    #[test]
    fn event_replay_out_of_order_in_window() {
        let mut w = EventReplayWindow16::new();
        assert_eq!(w.check_and_advance(100), CheckOutcome::Accept);
        assert_eq!(w.check_and_advance(98), CheckOutcome::Accept);
        assert_eq!(w.check_and_advance(98), CheckOutcome::Replay);
    }

    #[test]
    fn event_replay_too_old_rejected() {
        let mut w = EventReplayWindow16::new();
        assert_eq!(w.check_and_advance(1000), CheckOutcome::Accept);
        // 1000 - 935 = 65, just outside 64-deep window.
        assert_eq!(w.check_and_advance(935), CheckOutcome::TooOld);
    }

    #[test]
    fn event_replay_wraparound_accepted_as_forward() {
        let mut w = EventReplayWindow16::new();
        assert_eq!(w.check_and_advance(65530), CheckOutcome::Accept);
        assert_eq!(w.check_and_advance(65535), CheckOutcome::Accept);
        // wrap to 0 — modular distance from 65535 = 1, treated as forward.
        assert_eq!(w.check_and_advance(0), CheckOutcome::Accept);
        assert_eq!(w.check_and_advance(5), CheckOutcome::Accept);
    }

    #[test]
    fn event_replay_out_of_order_across_wraparound() {
        let mut w = EventReplayWindow16::new();
        // Receive seq=5 first.
        assert_eq!(w.check_and_advance(5), CheckOutcome::Accept);
        // Then seq=65535 — modular distance forward = 65530 (treated as
        // backward by 6 in our mapping).
        assert_eq!(w.check_and_advance(65535), CheckOutcome::Accept);
        // Then seq=0.
        assert_eq!(w.check_and_advance(0), CheckOutcome::Accept);
        // Replays of any of those.
        assert_eq!(w.check_and_advance(5), CheckOutcome::Replay);
        assert_eq!(w.check_and_advance(65535), CheckOutcome::Replay);
        assert_eq!(w.check_and_advance(0), CheckOutcome::Replay);
    }

    #[test]
    fn event_replay_no_session_reset_variant_emitted() {
        // The 16-bit window never emits AcceptSessionReset — only the
        // 32-bit packet window does.
        let mut w = EventReplayWindow16::new();
        let _ = w.check_and_advance(40_000);
        // Walk through every backward distance and verify no session-reset.
        for s in [0, 1, 100, 30_000, 35_000, 39_999] {
            let r = w.check_and_advance(s);
            assert!(
                !matches!(r, CheckOutcome::AcceptSessionReset),
                "got AcceptSessionReset for seq={s}"
            );
        }
    }
}
