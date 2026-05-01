// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

//! Milestone 3 bench: DIN MIDI I/O over a 31250 baud UART.
//!
//! Two halves, mirroring the radio bench's TX/RX split:
//!
//! - [`run_rx`] reads bytes from a MIDI-input UART, feeds them through
//!   [`MidiParser`], and `defmt::info!`s every event.  Useful with a
//!   keyboard wired into the FeatherWing's MIDI IN to verify that
//!   running status, real-time messages, pitch bend, and SysEx parse
//!   correctly.
//! - [`run_tx`] emits a recurring C-major arpeggio (NoteOn/NoteOff) with
//!   a 24 PPQN-style real-time clock interspersed.  Useful to verify
//!   that a synth on the FeatherWing's MIDI OUT receives clean,
//!   running-status-using output.
//!
//! Both functions are generic over `embedded_io_async::{Read, Write}` —
//! board crates supply a concrete UART instance via their `Resources`.
//!
//! The two functions intentionally do not share a single UART handle:
//! a real bench setup uses two separate boards (TX side + RX side),
//! each running one of these loops.

use embedded_io_async::{Read, Write};
use osrf_midi_din::{MidiEvent, MidiParser, ParseResult};

/// RX bench: read bytes from a MIDI input UART, parse, log every event.
/// Loops forever; UART errors are logged and the loop continues after a
/// brief backoff (a one-off framing/parity glitch shouldn't kill the bench).
pub async fn run_rx<U: Read>(mut uart: U) -> ! {
    let mut parser = MidiParser::new();
    let mut sysex_bytes: u32 = 0;

    defmt::info!("MIDI RX bench: reading at 31250 baud, listening for events");

    let mut buf = [0u8; 16];
    loop {
        let n = match uart.read(&mut buf).await {
            Ok(n) => n,
            Err(_) => {
                defmt::warn!("MIDI RX: UART read error; resetting parser, retrying");
                parser.reset();
                embassy_time::Timer::after_millis(10).await;
                continue;
            }
        };
        for &byte in &buf[..n] {
            match parser.feed(byte) {
                ParseResult::None => {}
                ParseResult::Event(MidiEvent::SysExStart) => {
                    sysex_bytes = 0;
                    defmt::info!("SysEx start");
                }
                ParseResult::Event(MidiEvent::SysExEnd) => {
                    defmt::info!("SysEx end ({} bytes)", sysex_bytes);
                }
                ParseResult::Event(e) => defmt::info!("event: {:?}", e),
                ParseResult::SysExByte(_) => sysex_bytes = sysex_bytes.saturating_add(1),
            }
        }
    }
}

/// TX bench: emit a recurring sequence of NoteOn/NoteOff events out the
/// UART, with a periodic real-time `TimingClock` interleaved to stress
/// the receiver's parser.  Loops forever; UART errors are logged and the
/// running-status state machine is re-initialised on recovery.
///
/// The arpeggio uses MIDI's "running status" idiom: after an initial
/// `0x90 ch` status byte we just keep sending pairs of data bytes for
/// each note.  NoteOff is rendered as `NoteOn vel=0` (canonical).
pub async fn run_tx<U: Write>(mut uart: U) -> ! {
    use embassy_time::Timer;

    defmt::info!("MIDI TX bench: arpeggiating C major at ~1 Hz, clock pulse every cycle");
    let notes = [60u8, 64, 67, 72]; // C, E, G, C (C major arpeggio)
    let channel: u8 = 0; // MIDI channel 1
    let velocity: u8 = 100;

    // Track whether the receiver's running-status state has the NoteOn
    // status latched.  If a UART error forces a recovery we resend the
    // status byte before the next pair of data bytes.
    let mut status_armed = false;

    let mut tick: u32 = 0;
    loop {
        if !status_armed {
            if let Err(_) = uart.write_all(&[0x90 | channel]).await {
                defmt::warn!("MIDI TX: UART write error on status byte; backing off");
                Timer::after_millis(50).await;
                continue;
            }
            status_armed = true;
        }

        let n = notes[(tick as usize) % notes.len()];

        // Note on (running status: just two data bytes).
        if let Err(_) = uart.write_all(&[n, velocity]).await {
            defmt::warn!("MIDI TX: UART write error on note-on; resyncing");
            status_armed = false;
            Timer::after_millis(50).await;
            continue;
        }
        Timer::after_millis(200).await;

        // Note off (running status; vel=0 is the canonical NoteOff idiom).
        if let Err(_) = uart.write_all(&[n, 0]).await {
            defmt::warn!("MIDI TX: UART write error on note-off; resyncing");
            status_armed = false;
            Timer::after_millis(50).await;
            continue;
        }
        Timer::after_millis(150).await;

        // Real-time clock pulse — does NOT affect running status, so the
        // next pair of data bytes still parses as NoteOn.
        if let Err(_) = uart.write_all(&[0xF8]).await {
            defmt::warn!("MIDI TX: UART write error on clock pulse; resyncing");
            status_armed = false;
            Timer::after_millis(50).await;
            continue;
        }

        tick = tick.wrapping_add(1);
        if tick % notes.len() as u32 == 0 {
            defmt::info!("MIDI TX: arp cycle {} complete", tick / notes.len() as u32);
        }
    }
}
