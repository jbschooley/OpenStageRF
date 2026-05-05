// SPDX-License-Identifier: AGPL-3.0-or-later
#![cfg_attr(not(test), no_std)]

//! OpenStageRF Transport Envelope v1 — wire-format encode/decode.
//!
//! See `SPEC.md` in this directory for the full specification.  This crate
//! implements only the wire format; AEAD/MAC computation lives in
//! `osrf-crypto`, and the link layer (replay window, watchdog, etc.) lives
//! in `osrf-link`.
//!
//! Milestone 4 scope: the no-crypto path (`key_fp = 0x000000`, no tag).
//! Lower-level header / body helpers are exposed so a future AEAD-aware
//! caller can encode the header + plaintext, hand the resulting AAD + body
//! to a cipher, and append the tag itself — without re-doing the framing.

// ── Constants ────────────────────────────────────────────────────────────────

/// Transport envelope version byte.  v1 = 0x01.
pub const VER_V1: u8 = 0x01;

/// Total fixed header length: ver(1) + key_fp(3) + seq(6) + event_type(1).
///
/// The full header is the AEAD AAD per the spec.
pub const HEADER_LEN: usize = 11;

/// Sentinel meaning "no encryption, no authentication".  When a receiver sees
/// this in `key_fp`, it skips key lookup and AEAD verification entirely.
pub const KEY_FP_NONE: KeyFp = [0, 0, 0];

/// Reserved sentinel — must not appear on the wire.
pub const KEY_FP_RESERVED: KeyFp = [0xFF, 0xFF, 0xFF];

/// Sequence numbers are 6 bytes on the wire; this is the maximum representable.
pub const MAX_SEQ: u64 = (1u64 << 48) - 1;

/// Maximum bytes carried in a `Body::MidiMessage` payload.
///
/// Originally a single MIDI channel-voice message (1–3 bytes), this was
/// relaxed in v1 to allow **batched** raw MIDI bytes — multiple
/// status-delimited messages concatenated.  The receiver decodes the
/// stream by MIDI parsing (status bytes have the high bit set; data
/// bytes don't), so the wire format is unchanged — only the length
/// invariant relaxes.
///
/// 64 bytes is comfortably more than fits in a 64-byte radio packet
/// after the 11-byte header (53 usable), but accommodates future
/// configurations with larger payloads.
pub const MAX_MIDI_MESSAGE_LEN: usize = 64;

/// Three-byte key fingerprint.  See `SPEC.md` § "key_fp".
pub type KeyFp = [u8; 3];

/// Numeric event_type discriminators.  Public so callers can match raw values.
pub mod event_type {
    pub const HEARTBEAT: u8 = 0x01;
    pub const MIDI_MESSAGE: u8 = 0x02;
    pub const MIDI_SYSEX_FRAGMENT: u8 = 0x03;
}

// ── Header ───────────────────────────────────────────────────────────────────

/// Parsed packet header.  `seq` carries the 6-byte wire seq in the low 48
/// bits of a u64 (high 16 bits are always zero on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Header {
    pub ver: u8,
    pub key_fp: KeyFp,
    pub seq: u64,
    pub event_type: u8,
}

impl Header {
    /// Pack `(boot_counter, session_seq)` into a single 48-bit `seq`.
    #[inline]
    pub const fn make_seq(boot_counter: u16, session_seq: u32) -> u64 {
        ((boot_counter as u64) << 32) | (session_seq as u64)
    }

    /// Top 16 bits of `seq`, mirroring the wire layout `[boot_counter:2 || session_seq:4]`.
    #[inline]
    pub const fn boot_counter(&self) -> u16 {
        (self.seq >> 32) as u16
    }

    /// Bottom 32 bits of `seq`.
    #[inline]
    pub const fn session_seq(&self) -> u32 {
        self.seq as u32
    }
}

// ── SysEx fragment state ─────────────────────────────────────────────────────

/// Where in a SysEx message a fragment sits.  See `SPEC.md` § "Body: MIDI_SYSEX_FRAGMENT".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[repr(u8)]
pub enum FragState {
    /// First fragment.  Body begins with `0xF0`.
    First = 0x01,
    /// Continuation fragment.  No `0xF0`/`0xF7` markers within.
    Middle = 0x02,
    /// Final fragment.  Body ends with `0xF7`.
    Last = 0x03,
    /// Whole SysEx in one fragment.  Body is `0xF0..0xF7`.
    Single = 0x04,
}

impl FragState {
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(Self::First),
            0x02 => Some(Self::Middle),
            0x03 => Some(Self::Last),
            0x04 => Some(Self::Single),
            _ => None,
        }
    }
}

// ── Body ─────────────────────────────────────────────────────────────────────

/// Decoded packet body, borrowing from the on-wire byte buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Body<'a> {
    /// `event_type = 0x01`.  Empty body.
    Heartbeat,

    /// `event_type = 0x02`.  1..=3 raw MIDI bytes.
    MidiMessage(&'a [u8]),

    /// `event_type = 0x03`.  Body = `[frag_state:1] + sysex_bytes:1..N`.
    SysExFragment { state: FragState, bytes: &'a [u8] },

    /// Any reserved `event_type` value we don't recognize.  Preserved as-is so
    /// the link layer can drop / forward according to its forward-compat rules.
    Unknown { event_type: u8, data: &'a [u8] },
}

impl<'a> Body<'a> {
    /// The `event_type` byte that goes in the header.
    #[inline]
    pub fn event_type(&self) -> u8 {
        match self {
            Self::Heartbeat => event_type::HEARTBEAT,
            Self::MidiMessage(_) => event_type::MIDI_MESSAGE,
            Self::SysExFragment { .. } => event_type::MIDI_SYSEX_FRAGMENT,
            Self::Unknown { event_type, .. } => *event_type,
        }
    }

    /// Length of the `event_data` slice (does NOT include the `event_type`
    /// byte, which is part of the header).
    #[inline]
    pub fn data_len(&self) -> usize {
        match self {
            Self::Heartbeat => 0,
            Self::MidiMessage(b) => b.len(),
            Self::SysExFragment { bytes, .. } => 1 + bytes.len(),
            Self::Unknown { data, .. } => data.len(),
        }
    }
}

// ── Packet ───────────────────────────────────────────────────────────────────

/// A decoded packet (header + body).  AEAD tag, if any, is stripped before
/// decode and re-attached after encode by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet<'a> {
    pub header: Header,
    pub body: Body<'a>,
}

// ── Errors ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum EncodeError {
    /// `out` doesn't fit `HEADER_LEN + body.data_len()` bytes.
    BufferTooSmall,
    /// `MidiMessage` body must be 1..=3 bytes.
    InvalidMidiLength,
    /// `SysExFragment` body must have ≥1 sysex byte.
    InvalidSysExFragment,
    /// `seq` exceeds 2^48 - 1 — would overflow the 6-byte wire field.
    SeqOutOfRange,
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
    /// `MidiMessage` body wasn't 1..=3 bytes.
    InvalidMidiLength,
    /// `SysExFragment` body is missing the fragstate byte or has zero sysex bytes.
    InvalidSysExFragment,
    /// `frag_state` byte wasn't a known FragState variant.
    InvalidFragState(u8),
}

// ── Encode ───────────────────────────────────────────────────────────────────

/// Total wire bytes required for `body` (excluding any AEAD tag).
///
/// Validates body invariants (MIDI length, non-empty SysEx body) and returns
/// the same `EncodeError`s `encode` would return for those cases.
pub fn wire_len(body: &Body<'_>) -> Result<usize, EncodeError> {
    match body {
        Body::MidiMessage(b) if b.is_empty() || b.len() > MAX_MIDI_MESSAGE_LEN => {
            Err(EncodeError::InvalidMidiLength)
        }
        Body::SysExFragment { bytes, .. } if bytes.is_empty() => {
            Err(EncodeError::InvalidSysExFragment)
        }
        _ => Ok(HEADER_LEN + body.data_len()),
    }
}

/// Encode header + plaintext body into `out`.  No AEAD tag is written.
///
/// Returns the number of bytes written.  An AEAD-using caller computes the
/// tag over `out[..HEADER_LEN]` (AAD) and `out[HEADER_LEN..returned_len]`
/// (plaintext, possibly encrypted in place afterwards) and appends the tag
/// to the slice that follows.
pub fn encode(out: &mut [u8], header: &Header, body: &Body<'_>) -> Result<usize, EncodeError> {
    if header.seq > MAX_SEQ {
        return Err(EncodeError::SeqOutOfRange);
    }
    let n = wire_len(body)?;
    if out.len() < n {
        return Err(EncodeError::BufferTooSmall);
    }

    // Header
    out[0] = header.ver;
    out[1..4].copy_from_slice(&header.key_fp);

    // 6-byte big-endian seq, MSB first.  The top 2 bytes of the u64 are
    // guaranteed zero by the MAX_SEQ check above.
    let seq_be = header.seq.to_be_bytes();
    out[4..10].copy_from_slice(&seq_be[2..8]);

    out[10] = body.event_type();

    // Body
    match body {
        Body::Heartbeat => {}
        Body::MidiMessage(b) => out[HEADER_LEN..HEADER_LEN + b.len()].copy_from_slice(b),
        Body::SysExFragment { state, bytes } => {
            out[HEADER_LEN] = *state as u8;
            out[HEADER_LEN + 1..HEADER_LEN + 1 + bytes.len()].copy_from_slice(bytes);
        }
        Body::Unknown { data, .. } => {
            out[HEADER_LEN..HEADER_LEN + data.len()].copy_from_slice(data)
        }
    }

    Ok(n)
}

// ── Decode ───────────────────────────────────────────────────────────────────

/// Parse the 11-byte header + body from `buf`.
///
/// AEAD-using callers strip the trailing tag bytes before passing the buffer
/// in; this function knows nothing about tags.
pub fn decode<'a>(buf: &'a [u8]) -> Result<Packet<'a>, DecodeError> {
    if buf.len() < HEADER_LEN {
        return Err(DecodeError::TooShort);
    }

    let ver = buf[0];
    if ver != VER_V1 {
        return Err(DecodeError::UnknownVersion(ver));
    }

    let mut key_fp: KeyFp = [0; 3];
    key_fp.copy_from_slice(&buf[1..4]);

    let mut seq_be = [0u8; 8];
    seq_be[2..8].copy_from_slice(&buf[4..10]);
    let seq = u64::from_be_bytes(seq_be);

    let event_type = buf[10];
    let data = &buf[HEADER_LEN..];

    let body = match event_type {
        0x00 => return Err(DecodeError::ReservedEventType),
        event_type::HEARTBEAT => Body::Heartbeat,
        event_type::MIDI_MESSAGE => {
            if data.is_empty() || data.len() > MAX_MIDI_MESSAGE_LEN {
                return Err(DecodeError::InvalidMidiLength);
            }
            Body::MidiMessage(data)
        }
        event_type::MIDI_SYSEX_FRAGMENT => {
            if data.is_empty() {
                return Err(DecodeError::InvalidSysExFragment);
            }
            let fs_byte = data[0];
            let state =
                FragState::from_u8(fs_byte).ok_or(DecodeError::InvalidFragState(fs_byte))?;
            let bytes = &data[1..];
            if bytes.is_empty() {
                return Err(DecodeError::InvalidSysExFragment);
            }
            Body::SysExFragment { state, bytes }
        }
        _ => Body::Unknown { event_type, data },
    };

    Ok(Packet {
        header: Header {
            ver,
            key_fp,
            seq,
            event_type,
        },
        body,
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn header(seq: u64, event_type: u8) -> Header {
        Header {
            ver: VER_V1,
            key_fp: KEY_FP_NONE,
            seq,
            event_type,
        }
    }

    // ── Round-trips per body type ─────────────────────────────────────────

    #[test]
    fn round_trip_heartbeat() {
        let h = header(42, event_type::HEARTBEAT);
        let mut buf = [0u8; HEADER_LEN];
        let n = encode(&mut buf, &h, &Body::Heartbeat).unwrap();
        assert_eq!(n, HEADER_LEN);
        let p = decode(&buf[..n]).unwrap();
        assert_eq!(p.header, h);
        assert_eq!(p.body, Body::Heartbeat);
    }

    #[test]
    fn round_trip_midi_three_byte() {
        let h = header(0x1234_5678, event_type::MIDI_MESSAGE);
        let midi = [0x90, 60, 100]; // Note On ch 0, note 60, vel 100
        let mut buf = [0u8; 32];
        let n = encode(&mut buf, &h, &Body::MidiMessage(&midi)).unwrap();
        assert_eq!(n, HEADER_LEN + 3);
        let p = decode(&buf[..n]).unwrap();
        assert_eq!(p.header.seq, 0x1234_5678);
        match p.body {
            Body::MidiMessage(b) => assert_eq!(b, &midi),
            other => panic!("expected MidiMessage, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_midi_two_byte() {
        let h = header(1, event_type::MIDI_MESSAGE);
        let midi = [0xC5, 42]; // Program Change ch 5, program 42
        let mut buf = [0u8; 32];
        let n = encode(&mut buf, &h, &Body::MidiMessage(&midi)).unwrap();
        assert_eq!(n, HEADER_LEN + 2);
        match decode(&buf[..n]).unwrap().body {
            Body::MidiMessage(b) => assert_eq!(b, &midi),
            other => panic!("expected MidiMessage, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_midi_one_byte_realtime() {
        let h = header(2, event_type::MIDI_MESSAGE);
        let midi = [0xF8]; // System Real-Time: TimingClock
        let mut buf = [0u8; 16];
        let n = encode(&mut buf, &h, &Body::MidiMessage(&midi)).unwrap();
        assert_eq!(n, HEADER_LEN + 1);
        match decode(&buf[..n]).unwrap().body {
            Body::MidiMessage(b) => assert_eq!(b, &[0xF8]),
            other => panic!("expected MidiMessage, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_sysex_fragment_first() {
        let h = header(7, event_type::MIDI_SYSEX_FRAGMENT);
        let sysex = [0xF0, 0x7E, 0x7F, 0x06, 0x01];
        let mut buf = [0u8; 32];
        let n = encode(
            &mut buf,
            &h,
            &Body::SysExFragment {
                state: FragState::First,
                bytes: &sysex,
            },
        )
        .unwrap();
        assert_eq!(n, HEADER_LEN + 1 + sysex.len());
        match decode(&buf[..n]).unwrap().body {
            Body::SysExFragment { state, bytes } => {
                assert_eq!(state, FragState::First);
                assert_eq!(bytes, &sysex);
            }
            other => panic!("expected SysExFragment, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_sysex_fragment_all_states() {
        for &(state, expected_byte) in &[
            (FragState::First, 0x01u8),
            (FragState::Middle, 0x02),
            (FragState::Last, 0x03),
            (FragState::Single, 0x04),
        ] {
            let bytes = [0xAA];
            let h = header(0, event_type::MIDI_SYSEX_FRAGMENT);
            let mut buf = [0u8; 16];
            let n = encode(
                &mut buf,
                &h,
                &Body::SysExFragment {
                    state,
                    bytes: &bytes,
                },
            )
            .unwrap();
            assert_eq!(buf[HEADER_LEN], expected_byte, "fragstate byte for {state:?}");
            match decode(&buf[..n]).unwrap().body {
                Body::SysExFragment { state: s2, .. } => assert_eq!(s2, state),
                other => panic!("expected SysExFragment, got {other:?}"),
            }
        }
    }

    // ── Header / seq layout ────────────────────────────────────────────────

    #[test]
    fn seq_pack_unpack_round_trip() {
        let seq = Header::make_seq(0x1234, 0xDEAD_BEEF);
        assert_eq!(seq, 0x0000_1234_DEAD_BEEFu64);
        let h = Header {
            ver: VER_V1,
            key_fp: KEY_FP_NONE,
            seq,
            event_type: event_type::HEARTBEAT,
        };
        assert_eq!(h.boot_counter(), 0x1234);
        assert_eq!(h.session_seq(), 0xDEAD_BEEF);
    }

    #[test]
    fn wire_layout_is_exactly_per_spec() {
        // Known-value test: this byte sequence is the canonical encoding.
        // Any change here means a wire-format break.
        let h = Header {
            ver: VER_V1,
            key_fp: [0x12, 0x34, 0x56],
            seq: Header::make_seq(0x00AB, 0xCDEF_0123),
            event_type: event_type::MIDI_MESSAGE,
        };
        let midi = [0x90, 60, 100];
        let mut buf = [0u8; 14];
        let n = encode(&mut buf, &h, &Body::MidiMessage(&midi)).unwrap();
        assert_eq!(n, 14);
        assert_eq!(
            buf,
            [
                0x01, // ver
                0x12, 0x34, 0x56, // key_fp
                0x00, 0xAB, 0xCD, 0xEF, 0x01, 0x23, // seq big-endian, 6 bytes
                0x02, // event_type = MIDI_MESSAGE
                0x90, 60, 100, // MIDI bytes
            ]
        );
    }

    #[test]
    fn aad_is_first_eleven_bytes() {
        // Per spec, AAD = ver || key_fp || seq || event_type = first HEADER_LEN bytes.
        let h = Header {
            ver: VER_V1,
            key_fp: [0xAB, 0xCD, 0xEF],
            seq: 0x123456,
            event_type: event_type::MIDI_MESSAGE,
        };
        let mut buf = [0u8; 32];
        encode(&mut buf, &h, &Body::MidiMessage(&[0x90, 60, 100])).unwrap();
        // The first HEADER_LEN bytes are the AAD.
        assert_eq!(buf[0], 0x01);
        assert_eq!(&buf[1..4], &[0xAB, 0xCD, 0xEF]);
        assert_eq!(&buf[4..10], &[0, 0, 0, 0x12, 0x34, 0x56]);
        assert_eq!(buf[10], event_type::MIDI_MESSAGE);
    }

    // ── Forward compatibility ─────────────────────────────────────────────

    #[test]
    fn unknown_event_type_passes_through() {
        // 0x05 is reserved-future-MIDI; older firmware should preserve it.
        let h = header(0, 0x05);
        let data = [1u8, 2, 3];
        let mut buf = [0u8; 32];
        let n = encode(
            &mut buf,
            &h,
            &Body::Unknown {
                event_type: 0x05,
                data: &data,
            },
        )
        .unwrap();
        let p = decode(&buf[..n]).unwrap();
        match p.body {
            Body::Unknown { event_type, data } => {
                assert_eq!(event_type, 0x05);
                assert_eq!(data, &[1, 2, 3]);
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    // ── Errors ────────────────────────────────────────────────────────────

    #[test]
    fn rejects_unknown_version() {
        let mut buf = [0u8; HEADER_LEN];
        encode(
            &mut buf,
            &Header {
                ver: 0x99,
                key_fp: KEY_FP_NONE,
                seq: 0,
                event_type: event_type::HEARTBEAT,
            },
            &Body::Heartbeat,
        )
        .unwrap();
        match decode(&buf) {
            Err(DecodeError::UnknownVersion(0x99)) => {}
            other => panic!("expected UnknownVersion(0x99), got {other:?}"),
        }
    }

    #[test]
    fn rejects_truncated() {
        let buf = [0u8; 5];
        assert_eq!(decode(&buf), Err(DecodeError::TooShort));
        let buf = [0u8; HEADER_LEN - 1];
        assert_eq!(decode(&buf), Err(DecodeError::TooShort));
    }

    #[test]
    fn rejects_reserved_event_type_zero() {
        let h = header(0, event_type::HEARTBEAT);
        let mut buf = [0u8; HEADER_LEN];
        encode(&mut buf, &h, &Body::Heartbeat).unwrap();
        // Manually corrupt event_type to the reserved 0x00.
        buf[10] = 0x00;
        assert_eq!(decode(&buf), Err(DecodeError::ReservedEventType));
    }

    #[test]
    fn rejects_invalid_midi_length_zero() {
        let mut buf = [0u8; 32];
        let h = header(0, event_type::MIDI_MESSAGE);
        assert_eq!(
            encode(&mut buf, &h, &Body::MidiMessage(&[])),
            Err(EncodeError::InvalidMidiLength)
        );
    }

    #[test]
    fn accepts_batched_midi_message() {
        // After the v1 batching relaxation, MidiMessage accepts up to
        // MAX_MIDI_MESSAGE_LEN bytes — multiple status-delimited MIDI
        // messages concatenated into a single packet body.
        let mut buf = [0u8; HEADER_LEN + MAX_MIDI_MESSAGE_LEN];
        let h = header(0, event_type::MIDI_MESSAGE);
        // 3-note chord = 9 bytes, well within the new limit.
        let chord = [0x90, 60, 100, 0x90, 64, 100, 0x90, 67, 100];
        let n = encode(&mut buf, &h, &Body::MidiMessage(&chord)).unwrap();
        assert_eq!(n, HEADER_LEN + chord.len());
        let decoded = decode(&buf[..n]).unwrap();
        match decoded.body {
            Body::MidiMessage(b) => assert_eq!(b, &chord),
            other => panic!("expected MidiMessage, got {other:?}"),
        }
    }

    #[test]
    fn rejects_oversize_midi_message() {
        let mut buf = [0u8; HEADER_LEN + MAX_MIDI_MESSAGE_LEN + 8];
        let h = header(0, event_type::MIDI_MESSAGE);
        let too_long = [0u8; MAX_MIDI_MESSAGE_LEN + 1];
        assert_eq!(
            encode(&mut buf, &h, &Body::MidiMessage(&too_long)),
            Err(EncodeError::InvalidMidiLength)
        );
    }

    #[test]
    fn decode_rejects_oversize_midi_message() {
        let total = HEADER_LEN + MAX_MIDI_MESSAGE_LEN + 1;
        let mut buf = std::vec::Vec::with_capacity(total);
        buf.resize(total, 0);
        buf[0] = VER_V1;
        buf[10] = event_type::MIDI_MESSAGE;
        // Body is now (MAX + 1) bytes — should reject.
        assert_eq!(decode(&buf), Err(DecodeError::InvalidMidiLength));
    }

    #[test]
    fn rejects_buffer_too_small() {
        let mut buf = [0u8; 5];
        let h = header(0, event_type::MIDI_MESSAGE);
        assert_eq!(
            encode(&mut buf, &h, &Body::MidiMessage(&[0x90, 60, 100])),
            Err(EncodeError::BufferTooSmall)
        );
    }

    #[test]
    fn rejects_seq_out_of_range() {
        let mut buf = [0u8; HEADER_LEN];
        let h = Header {
            ver: VER_V1,
            key_fp: KEY_FP_NONE,
            seq: 1u64 << 48,
            event_type: event_type::HEARTBEAT,
        };
        assert_eq!(
            encode(&mut buf, &h, &Body::Heartbeat),
            Err(EncodeError::SeqOutOfRange)
        );
    }

    #[test]
    fn rejects_invalid_fragstate() {
        // Forge a SysEx packet with a bogus fragstate byte.
        let mut buf = [0u8; 14];
        buf[0] = VER_V1;
        buf[10] = event_type::MIDI_SYSEX_FRAGMENT;
        buf[11] = 0x99; // invalid fragstate
        buf[12] = 0xF0;
        buf[13] = 0xF7;
        assert_eq!(decode(&buf), Err(DecodeError::InvalidFragState(0x99)));
    }

    #[test]
    fn rejects_empty_sysex_body_on_encode() {
        let mut buf = [0u8; 32];
        let h = header(0, event_type::MIDI_SYSEX_FRAGMENT);
        assert_eq!(
            encode(
                &mut buf,
                &h,
                &Body::SysExFragment {
                    state: FragState::Single,
                    bytes: &[],
                }
            ),
            Err(EncodeError::InvalidSysExFragment)
        );
    }

    #[test]
    fn rejects_empty_sysex_body_on_decode() {
        // SysEx packet with no fragstate byte at all.
        let mut buf = [0u8; HEADER_LEN];
        buf[0] = VER_V1;
        buf[10] = event_type::MIDI_SYSEX_FRAGMENT;
        assert_eq!(decode(&buf), Err(DecodeError::InvalidSysExFragment));

        // SysEx packet with fragstate but no body bytes.
        let mut buf = [0u8; HEADER_LEN + 1];
        buf[0] = VER_V1;
        buf[10] = event_type::MIDI_SYSEX_FRAGMENT;
        buf[11] = 0x01;
        assert_eq!(decode(&buf), Err(DecodeError::InvalidSysExFragment));
    }

    // ── Sizes — sanity checks against SPEC.md table ────────────────────────

    #[test]
    fn spec_size_table_none() {
        // Per SPEC.md "Sizes and timing" table: a NoteOn in `none` mode is 14 bytes.
        let h = header(1, event_type::MIDI_MESSAGE);
        let mut buf = [0u8; 32];
        let n = encode(&mut buf, &h, &Body::MidiMessage(&[0x90, 60, 100])).unwrap();
        assert_eq!(n, 14);
    }

    #[test]
    fn spec_size_table_heartbeat() {
        // Heartbeat = HEADER_LEN bytes, no body, no tag.
        let h = header(1, event_type::HEARTBEAT);
        let mut buf = [0u8; 32];
        let n = encode(&mut buf, &h, &Body::Heartbeat).unwrap();
        assert_eq!(n, HEADER_LEN);
    }
}
