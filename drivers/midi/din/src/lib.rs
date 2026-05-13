// SPDX-License-Identifier: AGPL-3.0-or-later
#![cfg_attr(not(test), no_std)]

//! DIN MIDI byte parser.
//!
//! Feed bytes one at a time from a 31250 baud UART; the parser emits
//! complete [`MidiEvent`] values.  SysEx is streamed: the parser emits
//! `SysExStart`, then each body byte as `SysExByte(u8)`, then `SysExEnd`
//! — the consumer is responsible for accumulating the body if it cares.
//!
//! Standards corner cases handled:
//! - **Running status**: a data byte arriving without a preceding status
//!   byte reuses the last channel-voice status (0x80..=0xEF).
//! - **System real-time** (0xF8..=0xFF): can interrupt any other message
//!   (including SysEx) without disturbing the surrounding parser state.
//! - **Malformed SysEx**: a non-real-time status byte arriving mid-SysEx
//!   ends the (malformed) SysEx without an `SysExEnd` event and the new
//!   status byte starts a fresh message.  An `0xF7` ends the SysEx
//!   normally and emits `SysExEnd`.
//! - **Undefined system messages** (`0xF4`, `0xF5`, `0xFD`): silently dropped.

/// One full MIDI event (or a SysEx delimiter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum MidiEvent {
    // ── Channel voice ────────────────────────────────────────────────────
    NoteOff {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    NoteOn {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    PolyAftertouch {
        channel: u8,
        note: u8,
        pressure: u8,
    },
    ControlChange {
        channel: u8,
        controller: u8,
        value: u8,
    },
    ProgramChange {
        channel: u8,
        program: u8,
    },
    ChannelAftertouch {
        channel: u8,
        pressure: u8,
    },
    /// Pitch bend, signed `-8192..=8191`, centered at 0.  On-wire is
    /// LSB-first 14-bit unsigned `0..=16383`; we re-center on parse.
    PitchBend {
        channel: u8,
        value: i16,
    },

    // ── System common ────────────────────────────────────────────────────
    TimeCodeQuarterFrame {
        msg_type: u8,
        value: u8,
    },
    SongPosition(u16),
    SongSelect(u8),
    TuneRequest,

    // ── System real-time (can interrupt other messages mid-stream) ───────
    TimingClock,
    Start,
    Continue,
    Stop,
    ActiveSensing,
    SystemReset,

    // ── SysEx delimiters ─────────────────────────────────────────────────
    SysExStart, // 0xF0
    SysExEnd,   // 0xF7
}

/// Result of feeding one byte into the parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum ParseResult {
    /// Need more bytes; nothing complete yet.
    None,
    /// A complete event was assembled (or a SysEx start/end delimiter).
    Event(MidiEvent),
    /// We are inside a SysEx body and `byte` is the next data byte.
    /// Consumer accumulates these into its own buffer.
    SysExByte(u8),
}

/// Status byte that selects which channel-voice message we're assembling.
///
/// Stored separately from `running_status` so we can distinguish "no
/// channel-voice status remembered yet" from "running status active".
#[derive(Clone, Copy)]
struct InProgress {
    /// Status byte 0x80..=0xEF (high nibble selects message kind).
    status: u8,
    /// First data byte already received? (only meaningful for 2-data-byte
    /// channel-voice messages: NoteOn/Off, PolyAT, CC, PitchBend).
    first_data: Option<u8>,
}

/// System-common in-progress assembly.  Distinct from channel voice
/// because system common does *not* arm running status.
#[derive(Clone, Copy)]
enum SystemCommon {
    /// 0xF1 TimeCodeQuarterFrame: takes 1 data byte.
    TcqfWaiting,
    /// 0xF2 SongPosition: takes LSB then MSB.
    SppWaitingLsb,
    SppWaitingMsb {
        lsb: u8,
    },
    /// 0xF3 SongSelect: takes 1 data byte.
    SongSelectWaiting,
}

#[derive(Clone, Copy)]
enum State {
    /// No partial message in progress.  A data byte here may be consumed
    /// via running status (`running_status` non-zero).
    Idle,
    /// Channel-voice message being assembled.
    Channel(InProgress),
    /// System-common message being assembled.
    SystemCommon(SystemCommon),
    /// Inside a SysEx body — bytes < 0x80 stream as `SysExByte`.
    SysEx,
}

/// Streaming DIN MIDI parser.  Feed it one byte at a time via
/// [`MidiParser::feed`].
pub struct MidiParser {
    state: State,
    /// Last channel-voice status byte (0x80..=0xEF) seen; 0 = none.
    /// Real-time messages and 0xF7 (SysEx end) do *not* clear this;
    /// system-common (0xF0..=0xF6) and any malformed-SysEx termination do.
    running_status: u8,
}

impl Default for MidiParser {
    fn default() -> Self {
        Self::new()
    }
}

impl MidiParser {
    /// Create a parser in the neutral state.
    pub const fn new() -> Self {
        Self {
            state: State::Idle,
            running_status: 0,
        }
    }

    /// Reset to neutral state — call on link reset or after a parse error.
    pub fn reset(&mut self) {
        self.state = State::Idle;
        self.running_status = 0;
    }

    /// Feed one byte from the MIDI input stream.  See [`ParseResult`].
    pub fn feed(&mut self, byte: u8) -> ParseResult {
        // ── Real-time messages: dispatch immediately, don't touch state. ──
        if byte >= 0xF8 {
            return match byte {
                0xF8 => ParseResult::Event(MidiEvent::TimingClock),
                0xFA => ParseResult::Event(MidiEvent::Start),
                0xFB => ParseResult::Event(MidiEvent::Continue),
                0xFC => ParseResult::Event(MidiEvent::Stop),
                0xFE => ParseResult::Event(MidiEvent::ActiveSensing),
                0xFF => ParseResult::Event(MidiEvent::SystemReset),
                // 0xF9 and 0xFD are undefined real-time bytes — silently drop.
                _ => ParseResult::None,
            };
        }

        // ── Status byte (non-real-time, 0x80..=0xF7) ───────────────────
        if byte >= 0x80 {
            return self.handle_status_byte(byte);
        }

        // ── Data byte (0x00..=0x7F) ────────────────────────────────────
        self.handle_data_byte(byte)
    }

    fn handle_status_byte(&mut self, byte: u8) -> ParseResult {
        // Inside a SysEx?  Special handling.
        if matches!(self.state, State::SysEx) {
            if byte == 0xF7 {
                // Normal end of SysEx.
                self.state = State::Idle;
                // 0xF7 does not arm running status.
                return ParseResult::Event(MidiEvent::SysExEnd);
            }
            // Any other status byte terminates the SysEx as malformed.
            // Drop without emitting SysExEnd, then fall through to handle
            // `byte` as the start of a new message.
            self.state = State::Idle;
            self.running_status = 0;
            // Fall through.
        }

        match byte {
            // Channel voice messages (0x80..=0xEF).
            0x80..=0xEF => {
                self.running_status = byte;
                self.state = State::Channel(InProgress {
                    status: byte,
                    first_data: None,
                });
                // ProgramChange (Cn) and ChannelAftertouch (Dn) take only
                // 1 data byte — but we still need that byte before we can
                // emit, so don't shortcut.
                ParseResult::None
            }
            // SysEx start.
            0xF0 => {
                self.state = State::SysEx;
                self.running_status = 0; // sysex breaks running status
                ParseResult::Event(MidiEvent::SysExStart)
            }
            // 0xF1 MIDI Time Code Quarter Frame.
            0xF1 => {
                self.state = State::SystemCommon(SystemCommon::TcqfWaiting);
                self.running_status = 0;
                ParseResult::None
            }
            // 0xF2 Song Position Pointer.
            0xF2 => {
                self.state = State::SystemCommon(SystemCommon::SppWaitingLsb);
                self.running_status = 0;
                ParseResult::None
            }
            // 0xF3 Song Select.
            0xF3 => {
                self.state = State::SystemCommon(SystemCommon::SongSelectWaiting);
                self.running_status = 0;
                ParseResult::None
            }
            // 0xF6 Tune Request — zero data bytes.
            0xF6 => {
                self.state = State::Idle;
                self.running_status = 0;
                ParseResult::Event(MidiEvent::TuneRequest)
            }
            // 0xF7 outside of SysEx is meaningless — drop.
            0xF7 => {
                // Already handled in-SysEx case above; here we're
                // standalone-F7.  Don't disturb running status (some
                // streams emit a stray F7 after SysEx as a sentinel).
                ParseResult::None
            }
            // 0xF4, 0xF5: reserved/undefined system common — drop.
            0xF4 | 0xF5 => {
                self.state = State::Idle;
                self.running_status = 0;
                ParseResult::None
            }
            // unreachable: 0x00..=0x7F handled by caller; 0xF8..=0xFF
            // handled by caller; we covered 0x80..=0xF7 above.
            _ => ParseResult::None,
        }
    }

    fn handle_data_byte(&mut self, byte: u8) -> ParseResult {
        match self.state {
            // ── Inside SysEx: stream every < 0x80 byte. ─────────────────
            State::SysEx => ParseResult::SysExByte(byte),

            // ── Assembling a channel-voice message. ─────────────────────
            State::Channel(InProgress { status, first_data }) => {
                let kind = status & 0xF0;
                let chan = status & 0x0F;
                match kind {
                    // 1-data-byte messages.
                    0xC0 => {
                        // ProgramChange.  After completion, running_status
                        // is still `status` — running status holds.
                        self.state = State::Channel(InProgress {
                            status,
                            first_data: None,
                        });
                        ParseResult::Event(MidiEvent::ProgramChange {
                            channel: chan,
                            program: byte,
                        })
                    }
                    0xD0 => {
                        // ChannelAftertouch.
                        self.state = State::Channel(InProgress {
                            status,
                            first_data: None,
                        });
                        ParseResult::Event(MidiEvent::ChannelAftertouch {
                            channel: chan,
                            pressure: byte,
                        })
                    }
                    // 2-data-byte messages.
                    _ => match first_data {
                        None => {
                            self.state = State::Channel(InProgress {
                                status,
                                first_data: Some(byte),
                            });
                            ParseResult::None
                        }
                        Some(d1) => {
                            // Reset to "waiting for next message of same status"
                            // so running status keeps working.
                            self.state = State::Channel(InProgress {
                                status,
                                first_data: None,
                            });
                            ParseResult::Event(decode_two_byte_channel(status, d1, byte))
                        }
                    },
                }
            }

            // ── Assembling a system-common message. ─────────────────────
            State::SystemCommon(sc) => match sc {
                SystemCommon::TcqfWaiting => {
                    self.state = State::Idle;
                    let msg_type = (byte >> 4) & 0x07;
                    let value = byte & 0x0F;
                    ParseResult::Event(MidiEvent::TimeCodeQuarterFrame { msg_type, value })
                }
                SystemCommon::SppWaitingLsb => {
                    self.state = State::SystemCommon(SystemCommon::SppWaitingMsb { lsb: byte });
                    ParseResult::None
                }
                SystemCommon::SppWaitingMsb { lsb } => {
                    self.state = State::Idle;
                    let pos = ((byte as u16) << 7) | (lsb as u16);
                    ParseResult::Event(MidiEvent::SongPosition(pos))
                }
                SystemCommon::SongSelectWaiting => {
                    self.state = State::Idle;
                    ParseResult::Event(MidiEvent::SongSelect(byte))
                }
            },

            // ── Idle: maybe running status applies. ─────────────────────
            State::Idle => {
                if self.running_status >= 0x80 && self.running_status <= 0xEF {
                    // Re-enter channel state with the cached status and
                    // treat `byte` as the first data byte.
                    let status = self.running_status;
                    let kind = status & 0xF0;
                    let chan = status & 0x0F;
                    match kind {
                        0xC0 => {
                            // PC: 1 data byte — done already.
                            // (state stays Idle; running_status unchanged)
                            ParseResult::Event(MidiEvent::ProgramChange {
                                channel: chan,
                                program: byte,
                            })
                        }
                        0xD0 => {
                            // CAT: 1 data byte — done already.
                            ParseResult::Event(MidiEvent::ChannelAftertouch {
                                channel: chan,
                                pressure: byte,
                            })
                        }
                        _ => {
                            self.state = State::Channel(InProgress {
                                status,
                                first_data: Some(byte),
                            });
                            ParseResult::None
                        }
                    }
                } else {
                    // No status, no running status — orphan data byte.
                    ParseResult::None
                }
            }
        }
    }
}

fn decode_two_byte_channel(status: u8, d1: u8, d2: u8) -> MidiEvent {
    let kind = status & 0xF0;
    let chan = status & 0x0F;
    match kind {
        0x80 => MidiEvent::NoteOff {
            channel: chan,
            note: d1,
            velocity: d2,
        },
        0x90 => MidiEvent::NoteOn {
            channel: chan,
            note: d1,
            velocity: d2,
        },
        0xA0 => MidiEvent::PolyAftertouch {
            channel: chan,
            note: d1,
            pressure: d2,
        },
        0xB0 => MidiEvent::ControlChange {
            channel: chan,
            controller: d1,
            value: d2,
        },
        0xE0 => {
            // PitchBend: LSB first, MSB second; 14-bit unsigned, recenter at 0x2000.
            let raw = ((d2 as u16) << 7) | (d1 as u16);
            let value = (raw as i16) - 0x2000;
            MidiEvent::PitchBend {
                channel: chan,
                value,
            }
        }
        // ProgramChange / ChannelAftertouch don't reach here — they're
        // 1-data-byte and handled before the second byte arrives.
        _ => unreachable!("decode_two_byte_channel called with 1-data-byte status"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests — host only (gated by cfg(test), see Cargo.toml `test = true`).
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    /// Drain one byte and expect a particular result.
    fn feed_one(p: &mut MidiParser, b: u8) -> ParseResult {
        p.feed(b)
    }

    /// Feed a slice; collect all non-None results.
    fn feed_all(p: &mut MidiParser, bytes: &[u8]) -> std::vec::Vec<ParseResult> {
        let mut out = std::vec::Vec::new();
        for &b in bytes {
            match p.feed(b) {
                ParseResult::None => {}
                r => out.push(r),
            }
        }
        out
    }

    #[test]
    fn note_on_basic() {
        let mut p = MidiParser::new();
        // Channel 1 (=0), note 60, velocity 100.
        assert_eq!(feed_one(&mut p, 0x90), ParseResult::None);
        assert_eq!(feed_one(&mut p, 60), ParseResult::None);
        assert_eq!(
            feed_one(&mut p, 100),
            ParseResult::Event(MidiEvent::NoteOn {
                channel: 0,
                note: 60,
                velocity: 100
            })
        );
    }

    #[test]
    fn all_channel_voice_messages() {
        for ch in 0u8..16 {
            let mut p = MidiParser::new();
            // NoteOff
            assert_eq!(
                feed_all(&mut p, &[0x80 | ch, 60, 64]),
                vec![ParseResult::Event(MidiEvent::NoteOff {
                    channel: ch,
                    note: 60,
                    velocity: 64
                })]
            );
            // NoteOn
            assert_eq!(
                feed_all(&mut p, &[0x90 | ch, 60, 100]),
                vec![ParseResult::Event(MidiEvent::NoteOn {
                    channel: ch,
                    note: 60,
                    velocity: 100
                })]
            );
            // PolyAftertouch
            assert_eq!(
                feed_all(&mut p, &[0xA0 | ch, 60, 50]),
                vec![ParseResult::Event(MidiEvent::PolyAftertouch {
                    channel: ch,
                    note: 60,
                    pressure: 50
                })]
            );
            // CC
            assert_eq!(
                feed_all(&mut p, &[0xB0 | ch, 7, 100]),
                vec![ParseResult::Event(MidiEvent::ControlChange {
                    channel: ch,
                    controller: 7,
                    value: 100
                })]
            );
            // PC
            assert_eq!(
                feed_all(&mut p, &[0xC0 | ch, 42]),
                vec![ParseResult::Event(MidiEvent::ProgramChange {
                    channel: ch,
                    program: 42
                })]
            );
            // CAT
            assert_eq!(
                feed_all(&mut p, &[0xD0 | ch, 80]),
                vec![ParseResult::Event(MidiEvent::ChannelAftertouch {
                    channel: ch,
                    pressure: 80
                })]
            );
        }
    }

    #[test]
    fn pitch_bend_center_is_zero() {
        let mut p = MidiParser::new();
        // 0xE0 status, LSB=0x00, MSB=0x40 → raw 0x2000 → centered = 0.
        assert_eq!(
            feed_all(&mut p, &[0xE0, 0x00, 0x40]),
            vec![ParseResult::Event(MidiEvent::PitchBend {
                channel: 0,
                value: 0
            })]
        );
    }

    #[test]
    fn pitch_bend_extremes() {
        let mut p = MidiParser::new();
        // Min: LSB=0, MSB=0 → raw 0 → -8192.
        assert_eq!(
            feed_all(&mut p, &[0xE0, 0x00, 0x00]),
            vec![ParseResult::Event(MidiEvent::PitchBend {
                channel: 0,
                value: -8192
            })]
        );
        // Max: LSB=0x7F, MSB=0x7F → raw 0x3FFF → +8191.
        assert_eq!(
            feed_all(&mut p, &[0xE0, 0x7F, 0x7F]),
            vec![ParseResult::Event(MidiEvent::PitchBend {
                channel: 0,
                value: 8191
            })]
        );
    }

    #[test]
    fn running_status_two_note_ons() {
        let mut p = MidiParser::new();
        // 0x90 (NoteOn ch 0), then 4 data bytes — expect 2 events.
        let events = feed_all(&mut p, &[0x90, 60, 100, 64, 90]);
        assert_eq!(
            events,
            vec![
                ParseResult::Event(MidiEvent::NoteOn {
                    channel: 0,
                    note: 60,
                    velocity: 100
                }),
                ParseResult::Event(MidiEvent::NoteOn {
                    channel: 0,
                    note: 64,
                    velocity: 90
                }),
            ]
        );
    }

    #[test]
    fn running_status_program_change() {
        let mut p = MidiParser::new();
        // Two PCs back to back via running status.
        let events = feed_all(&mut p, &[0xC0, 1, 2]);
        assert_eq!(
            events,
            vec![
                ParseResult::Event(MidiEvent::ProgramChange {
                    channel: 0,
                    program: 1
                }),
                ParseResult::Event(MidiEvent::ProgramChange {
                    channel: 0,
                    program: 2
                }),
            ]
        );
    }

    #[test]
    fn realtime_in_middle_of_channel_message() {
        let mut p = MidiParser::new();
        // 0x90 (NoteOn) then note=60, but a TimingClock (0xF8) interrupts
        // before the velocity arrives.  Expect: TimingClock, then NoteOn.
        assert_eq!(p.feed(0x90), ParseResult::None);
        assert_eq!(p.feed(60), ParseResult::None);
        assert_eq!(p.feed(0xF8), ParseResult::Event(MidiEvent::TimingClock));
        // Now feed the velocity — original NoteOn must complete.
        assert_eq!(
            p.feed(100),
            ParseResult::Event(MidiEvent::NoteOn {
                channel: 0,
                note: 60,
                velocity: 100
            })
        );
    }

    #[test]
    fn sysex_basic_round_trip() {
        let mut p = MidiParser::new();
        let events = feed_all(&mut p, &[0xF0, 0x7E, 0x7F, 0x06, 0x01, 0xF7]);
        assert_eq!(
            events,
            vec![
                ParseResult::Event(MidiEvent::SysExStart),
                ParseResult::SysExByte(0x7E),
                ParseResult::SysExByte(0x7F),
                ParseResult::SysExByte(0x06),
                ParseResult::SysExByte(0x01),
                ParseResult::Event(MidiEvent::SysExEnd),
            ]
        );
    }

    #[test]
    fn realtime_inside_sysex_does_not_break_it() {
        let mut p = MidiParser::new();
        let events = feed_all(&mut p, &[0xF0, 0x11, 0xF8, 0x22, 0xF7]);
        assert_eq!(
            events,
            vec![
                ParseResult::Event(MidiEvent::SysExStart),
                ParseResult::SysExByte(0x11),
                ParseResult::Event(MidiEvent::TimingClock),
                ParseResult::SysExByte(0x22),
                ParseResult::Event(MidiEvent::SysExEnd),
            ]
        );
    }

    #[test]
    fn channel_voice_status_mid_sysex_drops_sysex() {
        let mut p = MidiParser::new();
        // Start sysex, send a body byte, then a NoteOn status arrives — the
        // SysEx is malformed, drop it (no SysExEnd) and start the new
        // NoteOn.
        let events = feed_all(&mut p, &[0xF0, 0x11, 0x90, 60, 100]);
        assert_eq!(
            events,
            vec![
                ParseResult::Event(MidiEvent::SysExStart),
                ParseResult::SysExByte(0x11),
                ParseResult::Event(MidiEvent::NoteOn {
                    channel: 0,
                    note: 60,
                    velocity: 100
                }),
            ]
        );
    }

    #[test]
    fn undefined_status_bytes_dropped() {
        let mut p = MidiParser::new();
        for byte in [0xF4u8, 0xF5, 0xF9, 0xFD] {
            assert_eq!(
                p.feed(byte),
                ParseResult::None,
                "byte {:#x} should drop",
                byte
            );
        }
    }

    #[test]
    fn system_common_breaks_running_status() {
        let mut p = MidiParser::new();
        // Establish running status with a NoteOn.
        assert_eq!(
            feed_all(&mut p, &[0x90, 60, 100]),
            vec![ParseResult::Event(MidiEvent::NoteOn {
                channel: 0,
                note: 60,
                velocity: 100
            })]
        );
        // Send a TuneRequest (0xF6), which breaks running status.
        assert_eq!(p.feed(0xF6), ParseResult::Event(MidiEvent::TuneRequest));
        // A bare data byte now should not emit anything.
        assert_eq!(p.feed(64), ParseResult::None);
    }

    #[test]
    fn realtime_does_not_break_running_status() {
        let mut p = MidiParser::new();
        // NoteOn establishes running status.
        feed_all(&mut p, &[0x90, 60, 100]);
        // Real-time clock pulse.
        assert_eq!(p.feed(0xF8), ParseResult::Event(MidiEvent::TimingClock));
        // Now two data bytes — running status reapplies.
        assert_eq!(p.feed(64), ParseResult::None);
        assert_eq!(
            p.feed(90),
            ParseResult::Event(MidiEvent::NoteOn {
                channel: 0,
                note: 64,
                velocity: 90
            })
        );
    }

    #[test]
    fn song_position_two_data_bytes() {
        let mut p = MidiParser::new();
        // 0xF2, LSB=0x40, MSB=0x01 → 0x0040 | 0x80 = 0x00C0.
        assert_eq!(
            feed_all(&mut p, &[0xF2, 0x40, 0x01]),
            vec![ParseResult::Event(MidiEvent::SongPosition(0x00C0))]
        );
    }

    #[test]
    fn song_select_one_data_byte() {
        let mut p = MidiParser::new();
        assert_eq!(
            feed_all(&mut p, &[0xF3, 5]),
            vec![ParseResult::Event(MidiEvent::SongSelect(5))]
        );
    }

    #[test]
    fn time_code_quarter_frame() {
        let mut p = MidiParser::new();
        // 0xF1 status, value byte 0x35 → msg_type = 3, value = 5.
        assert_eq!(
            feed_all(&mut p, &[0xF1, 0x35]),
            vec![ParseResult::Event(MidiEvent::TimeCodeQuarterFrame {
                msg_type: 3,
                value: 5
            })]
        );
    }

    #[test]
    fn reset_returns_to_neutral() {
        let mut p = MidiParser::new();
        feed_all(&mut p, &[0x90, 60]); // partial NoteOn
        p.reset();
        // A bare data byte after reset emits nothing (running status cleared).
        assert_eq!(p.feed(100), ParseResult::None);
    }
}
