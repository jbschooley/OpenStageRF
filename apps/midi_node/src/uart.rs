// SPDX-License-Identifier: AGPL-3.0-or-later

//! UART-backed [`MidiSource`] and [`MidiSink`] adapters.
//!
//! These plug a `BufferedUarte` (or any `embedded_io_async::Read`/
//! `Write`) into [`osrf_link_runtime::run_tx`] / [`run_rx`] without
//! the runtime needing to know about MIDI parsing or wire encoding.
//!
//! ## Source side
//!
//! [`UartMidiSource`] reads bytes from the UART via
//! [`embedded_io_async::Read`], feeds them through [`MidiParser`],
//! and re-emits each completed channel-voice event as a single 1–3
//! byte wire message that the link layer queues with status-aware
//! dedup + a fresh `event_seq`.
//!
//! Re-emitting status bytes explicitly (rather than passing through
//! running-status data bytes) is important: the link layer's
//! [`MidiTxQueue::push_channel_voice`] needs the full status byte to
//! perform dedup, and on the wire the receiver doesn't share running
//! state with the transmitter anyway.  Each parsed event becomes one
//! self-contained wire message.
//!
//! Real-time messages (Timing Clock, Active Sensing, etc.) and SysEx
//! are silently dropped in v1.  System Common is also dropped.  Add
//! these by extending [`encode_event`].
//!
//! ## Sink side
//!
//! [`UartMidiSink`] writes bytes verbatim to the UART. The
//! [`MidiSink::all_notes_off`] entry point emits 16 channels worth of
//! `[0xBn, 0x7B, 0x00]` (CC#123 = All Notes Off) — 48 bytes total.
//! At 31250 baud that takes ~15 ms which is fine for a watchdog
//! recovery.
//!
//! ## Wire-byte semantics matching the link layer
//!
//! The link runtime's [`MidiSource::try_next`] contract is "give me a
//! full MIDI message, status byte first, 1–3 bytes."  We meet that
//! by buffering complete events in a [`heapless::Deque`] of byte
//! vectors and popping one per `try_next` call.  At realistic
//! single-keyboard input rates, the queue depth stays well under the
//! [`SOURCE_EVENT_QUEUE_DEPTH`] cap.

use core::convert::Infallible;

use embedded_io_async::{Read, Write};
use heapless::{Deque, Vec};

use osrf_link_runtime::{MidiSink, MidiSource};
use osrf_midi_din::{MidiEvent, MidiParser, ParseResult};

/// Maximum events the source can buffer between `wait_ready` resolves
/// and the link-layer drain loop pops them.  Sized for worst-case key
/// smashes: ~12 NoteOns within one UART read window plus headroom.
pub const SOURCE_EVENT_QUEUE_DEPTH: usize = 32;

/// Maximum bytes per buffered event.  Channel voice is 1–3 bytes; we
/// allocate 4 to align with [`osrf_link::MAX_MSG_BYTES`] (which has
/// the same headroom for short System Common variants).
const EVENT_BYTES_MAX: usize = 4;

/// UART read buffer — one async `read()` call returns up to this many
/// bytes.  Smaller = lower latency per read, larger = fewer await
/// points.  At 31250 baud, 32 bytes ≈ 10 ms of MIDI traffic which is
/// the natural granularity for a chord burst.
const UART_READ_CHUNK: usize = 32;

/// Wraps a UART reader + MIDI parser.  Implements [`MidiSource`] for
/// [`run_tx`].
pub struct UartMidiSource<R> {
    uart: R,
    parser: MidiParser,
    events: Deque<Vec<u8, EVENT_BYTES_MAX>, SOURCE_EVENT_QUEUE_DEPTH>,
    /// Diagnostic counter for events the parser produced but the
    /// queue couldn't fit (back-pressure indicator).
    overflow_count: u32,
    /// Diagnostic counter for parser-emitted events we silently
    /// dropped (SysEx body bytes, system-common, real-time).
    drop_count: u32,
}

impl<R: Read> UartMidiSource<R> {
    /// Wrap the given UART reader.  Both ownership and the parser
    /// state belong to this struct for the lifetime of `run_tx`.
    pub fn new(uart: R) -> Self {
        Self {
            uart,
            parser: MidiParser::new(),
            events: Deque::new(),
            overflow_count: 0,
            drop_count: 0,
        }
    }

    /// Diagnostic — events lost because the internal queue was full.
    pub fn overflow_count(&self) -> u32 {
        self.overflow_count
    }
    /// Diagnostic — events dropped (SysEx / system-common / real-time
    /// in v1).
    pub fn drop_count(&self) -> u32 {
        self.drop_count
    }

    fn ingest(&mut self, byte: u8) {
        match self.parser.feed(byte) {
            ParseResult::None => {}
            ParseResult::SysExByte(_) => {
                // SysEx body bytes — counted as dropped for diagnostics.
                // (The Start/End delimiters are emitted as Events and
                // counted there if we want to add SysEx support later.)
            }
            ParseResult::Event(event) => match encode_event(event) {
                Some(wire) => {
                    if self.events.push_back(wire).is_err() {
                        self.overflow_count = self.overflow_count.wrapping_add(1);
                    }
                }
                None => {
                    self.drop_count = self.drop_count.wrapping_add(1);
                }
            },
        }
    }
}

impl<R: Read> MidiSource for UartMidiSource<R> {
    type Error = Infallible;

    fn try_next(&mut self, buf: &mut [u8]) -> Result<Option<usize>, Self::Error> {
        match self.events.pop_front() {
            Some(ev) => {
                let n = ev.len().min(buf.len());
                buf[..n].copy_from_slice(&ev[..n]);
                Ok(Some(n))
            }
            None => Ok(None),
        }
    }

    async fn wait_ready(&mut self) {
        if !self.events.is_empty() {
            return;
        }
        let mut buf = [0u8; UART_READ_CHUNK];
        loop {
            let n = match self.uart.read(&mut buf).await {
                Ok(0) => return, // EOF — shouldn't happen on a UART
                Ok(n) => n,
                Err(_) => {
                    defmt::warn!("midi_node TX: UART read error; resetting parser");
                    self.parser.reset();
                    return;
                }
            };
            for &byte in &buf[..n] {
                self.ingest(byte);
            }
            if !self.events.is_empty() {
                return;
            }
            // No complete event yet (e.g. only got the status byte) —
            // loop and read more.
        }
    }
}

/// Wraps a UART writer.  Implements [`MidiSink`] for [`run_rx`].
pub struct UartMidiSink<W> {
    uart: W,
}

impl<W: Write> UartMidiSink<W> {
    pub fn new(uart: W) -> Self {
        Self { uart }
    }
}

impl<W: Write> MidiSink for UartMidiSink<W> {
    type Error = Infallible;

    async fn write_message(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        if self.uart.write_all(bytes).await.is_err() {
            defmt::error!("midi_node RX: UART write error");
        }
        Ok(())
    }

    async fn all_notes_off(&mut self) -> Result<(), Self::Error> {
        // 16 channels × CC#123 (All Notes Off) value 0 = 48 bytes.
        let mut buf = [0u8; 48];
        for ch in 0..16usize {
            buf[ch * 3] = 0xB0 | (ch as u8);
            buf[ch * 3 + 1] = 0x7B; // CC#123
            buf[ch * 3 + 2] = 0;
        }
        if self.uart.write_all(&buf).await.is_err() {
            defmt::error!("midi_node RX: UART write error during all_notes_off");
        }
        Ok(())
    }
}

/// Re-encode a parsed [`MidiEvent`] into wire bytes for the link
/// layer.  Returns `None` for events the link layer doesn't handle
/// (SysEx delimiters, system common, real-time) — in v1 these are
/// silently dropped.  Adding any of them is a one-arm change here.
fn encode_event(event: MidiEvent) -> Option<Vec<u8, EVENT_BYTES_MAX>> {
    let mut v: Vec<u8, EVENT_BYTES_MAX> = Vec::new();
    match event {
        // Channel voice — these are what the link layer ships.
        MidiEvent::NoteOff {
            channel,
            note,
            velocity,
        } => {
            v.extend_from_slice(&[0x80 | (channel & 0x0F), note & 0x7F, velocity & 0x7F])
                .ok()?;
        }
        MidiEvent::NoteOn {
            channel,
            note,
            velocity,
        } => {
            v.extend_from_slice(&[0x90 | (channel & 0x0F), note & 0x7F, velocity & 0x7F])
                .ok()?;
        }
        MidiEvent::PolyAftertouch {
            channel,
            note,
            pressure,
        } => {
            v.extend_from_slice(&[0xA0 | (channel & 0x0F), note & 0x7F, pressure & 0x7F])
                .ok()?;
        }
        MidiEvent::ControlChange {
            channel,
            controller,
            value,
        } => {
            v.extend_from_slice(&[0xB0 | (channel & 0x0F), controller & 0x7F, value & 0x7F])
                .ok()?;
        }
        MidiEvent::ProgramChange { channel, program } => {
            v.extend_from_slice(&[0xC0 | (channel & 0x0F), program & 0x7F])
                .ok()?;
        }
        MidiEvent::ChannelAftertouch { channel, pressure } => {
            v.extend_from_slice(&[0xD0 | (channel & 0x0F), pressure & 0x7F])
                .ok()?;
        }
        MidiEvent::PitchBend { channel, value } => {
            // Re-encode signed -8192..=8191 → 14-bit unsigned 0..=16383.
            let u = (value as i32 + 8192) as u16 & 0x3FFF;
            v.extend_from_slice(&[
                0xE0 | (channel & 0x0F),
                (u & 0x7F) as u8,
                ((u >> 7) & 0x7F) as u8,
            ])
            .ok()?;
        }

        // SysEx delimiters — drop in v1; future SysEx work will
        // accumulate the body and call MidiTxQueue::push_sysex.
        MidiEvent::SysExStart | MidiEvent::SysExEnd => return None,

        // System common — not channel voice; not currently routed
        // through the link.
        MidiEvent::TimeCodeQuarterFrame { .. }
        | MidiEvent::SongPosition(_)
        | MidiEvent::SongSelect(_)
        | MidiEvent::TuneRequest => return None,

        // System real-time — frequent and miss-tolerant.  Not yet
        // routed through the link (the runtime's heartbeat fills
        // silence with our own timing reference).  Future feature.
        MidiEvent::TimingClock
        | MidiEvent::Start
        | MidiEvent::Continue
        | MidiEvent::Stop
        | MidiEvent::ActiveSensing
        | MidiEvent::SystemReset => return None,
    }
    Some(v)
}
