// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![allow(async_fn_in_trait)]

//! Milestone 4 link-layer bench, board-agnostic.
//!
//! Exercises the full TX→radio→RX path with `osrf-link`'s `LinkSender`,
//! `LinkReceiver`, `HeartbeatTimer`, and `WatchdogTimer` against the
//! hand-rolled `osrf-radio-sx126x` driver.  The MIDI byte source (TX) and
//! sink (RX) are abstracted behind two traits so the bench can run with
//! either:
//!
//! * the synthetic source/sink in [`synthetic`] (current — proves the link
//!   layer end-to-end without M3 hardware);
//! * a future UART-backed source/sink wrapping `BufferedUarte` + `MidiParser`
//!   for the real-MIDI hand-off (one trait impl swap, no runtime changes
//!   in [`run_tx`] / [`run_rx`]).
//!
//! The bench keeps the radio packet format trivial: each MIDI message is
//! wrapped in a single `Body::MidiMessage` packet with the link-layer
//! sequence number; heartbeats fill silence so the receiver's watchdog
//! stays fed.  When TX power is cut, the receiver's watchdog fires, and
//! [`MidiSink::all_notes_off`] is called — that's the M4 exit-criterion
//! observable.

pub mod synthetic;

use core::task::Poll;
use embassy_futures::poll_once;
use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Timer};
use embedded_hal::digital::StatefulOutputPin;
use embedded_hal_async::{digital::Wait, spi::SpiDevice};

use osrf_link::{
    Body, HeartbeatTimer, LinkReceiver, LinkSender, MidiTxQueue, RxDrop, RxOutcome, WatchdogTimer,
};
use osrf_radio_sx126x::{
    GfskBandwidth, GfskPulseShape, RadioError, RfSwitchControl, Sx1262Radio,
};

// ── Bench radio config (matches radio_bench so packets interop) ─────────────

const RF_FREQUENCY_HZ: u32 = 915_000_000;
const RF_BITRATE_BPS: u32 = 300_000;
const RF_DEVIATION_HZ: u32 = 50_000;
const RF_TX_POWER_DBM: i8 = 14;
const RF_PREAMBLE_BITS: u16 = 16;
const RF_PAYLOAD_MAX: u8 = 64;
const RF_SYNC_WORD: [u8; 4] = [0xC1, 0x94, 0xC1, 0x94];

// ── Link-layer config (M4 spec) ──────────────────────────────────────────────

/// Receiver watchdog: 200 ms of silence → assume link lost.
pub const WATCHDOG_MS: u64 = 200;
/// Transmitter heartbeat: 10 ms idle → emit a Heartbeat packet.  20× margin
/// against the receiver's 200 ms watchdog (i.e., the link survives losing
/// up to 19 consecutive packets before the watchdog fires).
pub const HEARTBEAT_MS: u64 = 10;
/// Each MIDI message is transmitted this many times — but **round-robin**
/// via [`MidiTxQueue`] (chord notes interleave: `C E G C E G C E G`), so
/// the first round delivers the chord with minimal spread (~one
/// message-time gap between notes) and the next two rounds insure
/// against per-packet RF loss.  At 0.2 % per-packet loss, three rounds
/// drop the per-event miss rate to (0.002)³ ≈ 8 × 10⁻⁹.  Heartbeats stay
/// single-send (next one is ≤ 10 ms away).
pub const MIDI_REPEAT_COUNT: u8 = 3;

// ── Source / Sink traits ─────────────────────────────────────────────────────

/// A producer of MIDI byte-sequence messages, one at a time.  The bench TX
/// loop awaits this and wraps each result in a `Body::MidiMessage` packet.
///
/// `next_message` writes the bytes into the caller-supplied scratch buffer
/// (avoiding `&'a [u8]` self-borrow lifetimes that would force a more
/// awkward async-fn-in-trait shape) and returns the populated length.
///
/// Implementations:
/// * [`synthetic::ChordHoldSource`] — pre-baked NoteOn chord then idle.
/// * Future: a `BufferedUarte` + `MidiParser` adapter that emits one
///   message per parser-recognized event.
pub trait MidiSource {
    type Error;

    /// Wait for the next MIDI message and write its bytes into `buf`.
    /// Returns the number of bytes written.  May resolve only after an
    /// arbitrarily long delay (e.g., when no MIDI input is available).
    async fn next_message(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error>;
}

/// A consumer of MIDI byte-sequence messages.  The bench RX loop calls
/// [`Self::write_message`] for every accepted MIDI body, and
/// [`Self::all_notes_off`] when the watchdog fires (link lost).
pub trait MidiSink {
    type Error;

    /// Write a single MIDI message (1..=N bytes).
    async fn write_message(&mut self, bytes: &[u8]) -> Result<(), Self::Error>;

    /// Emit "all notes off" on every channel.  Real-MIDI sinks generate
    /// 16 × `[0xB0+ch, 0x7B, 0x00]`.  The defmt-backed synthetic sink
    /// just logs the event.  Called by [`run_rx`] on watchdog expiry.
    async fn all_notes_off(&mut self) -> Result<(), Self::Error>;
}

// ── Radio configuration shared by both ends ─────────────────────────────────

async fn configure_radio<Spi, Busy, Dio1, Reset, Switch>(
    radio: &mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>,
) -> Result<(), RadioError<Reset, Switch>>
where
    Spi: SpiDevice,
    Busy: Wait,
    Dio1: Wait,
    Reset: embedded_hal::digital::OutputPin,
    Switch: RfSwitchControl,
{
    radio.init().await?;
    radio.set_frequency(RF_FREQUENCY_HZ).await?;
    radio
        .set_modulation_gfsk(
            RF_BITRATE_BPS,
            RF_DEVIATION_HZ,
            GfskBandwidth::Bw4670,
            GfskPulseShape::Bt05,
        )
        .await?;
    radio
        .set_packet_format(RF_PREAMBLE_BITS, &RF_SYNC_WORD, RF_PAYLOAD_MAX, true)
        .await?;
    radio.set_tx_power(RF_TX_POWER_DBM).await?;
    radio.finish_init().await?;
    Ok(())
}

// ── TX loop ─────────────────────────────────────────────────────────────────

/// Run the TX side of the bench: consume MIDI messages from `source`, wrap
/// each in a `Body::MidiMessage`, transmit via `radio`.  When `source`
/// goes idle for [`HEARTBEAT_MS`] ms, send a `Body::Heartbeat` instead so
/// the receiver's watchdog stays fed.
///
/// `boot_counter` should be persisted across resets in production so that
/// the receiver's replay window treats each reset as a fresh forward jump
/// in `seq`.  For the bench, callers can hard-code a fresh value per power
/// cycle — the only consequence of reusing a counter is that previously-
/// transmitted seqs would replay-reject on the receiver, which only
/// matters across same-session-reboot scenarios.
pub async fn run_tx<Spi, Busy, Dio1, Reset, Switch, Led, Source>(
    radio: &mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>,
    led: &mut Led,
    source: &mut Source,
    boot_counter: u16,
) -> !
where
    Spi: SpiDevice,
    Busy: Wait,
    Dio1: Wait,
    Reset: embedded_hal::digital::OutputPin,
    Switch: RfSwitchControl,
    Led: StatefulOutputPin,
    Source: MidiSource,
{
    if let Err(_) = configure_radio(radio).await {
        defmt::error!("link_bench TX: radio configure failed; halting");
        loop {
            Timer::after_millis(1000).await;
        }
    }
    defmt::info!(
        "link_bench TX: {} Hz / {} bps GFSK / +{} dBm, boot_counter={}",
        RF_FREQUENCY_HZ,
        RF_BITRATE_BPS,
        RF_TX_POWER_DBM,
        boot_counter
    );

    let mut sender = LinkSender::no_crypto(boot_counter);
    let mut hb = HeartbeatTimer::new(Duration::from_millis(HEARTBEAT_MS));
    let mut queue = MidiTxQueue::with_repeat_count(MIDI_REPEAT_COUNT);
    let mut midi_buf = [0u8; RF_PAYLOAD_MAX as usize];
    let mut wire_buf = [0u8; RF_PAYLOAD_MAX as usize];
    let mut msg_buf = [0u8; 4]; // single MIDI message
    let mut hb_count: u32 = 0;
    let mut tx_count: u32 = 0;

    loop {
        // 1. Drain any source events into the queue (non-blocking).  Each
        //    push applies MIDI status-aware dedup; new state cancels stale
        //    queued state for the same target (NoteOn↔NoteOff, CC, PB, etc.).
        loop {
            match poll_once(source.next_message(&mut midi_buf)) {
                Poll::Ready(Ok(n)) => {
                    queue.push(&midi_buf[..n]);
                    tx_count = tx_count.wrapping_add(1);
                }
                Poll::Ready(Err(_)) => break,
                Poll::Pending => break,
            }
        }

        // 2. If the queue has anything to send, take the next message and
        //    transmit it.  Round-robin: a chord's notes interleave across
        //    triple-send rounds (C E G C E G C E G), so the first round
        //    delivers the chord with minimal spread.
        if let Some(n) = queue.pop_send(&mut msg_buf) {
            let body = Body::MidiMessage(&msg_buf[..n]);
            match sender.encode(&body, &mut wire_buf) {
                Ok(wire_n) => {
                    if let Err(_) = radio.tx(&wire_buf[..wire_n]).await {
                        defmt::error!("link_bench TX: radio.tx() failed");
                    }
                }
                Err(_) => defmt::error!("link_bench TX: encode failed"),
            }
            hb.note_send();
            let _ = led.toggle();
            continue;
        }

        // 3. Queue empty — wait for either a new source event or the
        //    heartbeat deadline.  MIDI priority: `select` polls source
        //    first, and a heartbeat-win is followed by a non-blocking
        //    re-poll of the source to catch micro-races.
        let body = match select(source.next_message(&mut midi_buf), hb.wait()).await {
            Either::First(Ok(n)) => {
                queue.push(&midi_buf[..n]);
                tx_count = tx_count.wrapping_add(1);
                continue; // go back to step 1, drain + send
            }
            Either::First(Err(_)) => {
                defmt::warn!("link_bench TX: source error; sending heartbeat");
                Body::Heartbeat
            }
            Either::Second(()) => {
                match poll_once(source.next_message(&mut midi_buf)) {
                    Poll::Ready(Ok(n)) => {
                        queue.push(&midi_buf[..n]);
                        tx_count = tx_count.wrapping_add(1);
                        continue;
                    }
                    _ => {
                        hb_count = hb_count.wrapping_add(1);
                        Body::Heartbeat
                    }
                }
            }
        };

        // Send heartbeat (single copy — the next one is ≤ HEARTBEAT_MS away).
        match sender.encode(&body, &mut wire_buf) {
            Ok(wire_n) => {
                if let Err(_) = radio.tx(&wire_buf[..wire_n]).await {
                    defmt::error!("link_bench TX: radio.tx() failed");
                }
            }
            Err(_) => defmt::error!("link_bench TX: encode failed"),
        }
        hb.note_send();
        let _ = led.toggle();

        if (tx_count.wrapping_add(hb_count)) % 500 == 0 {
            defmt::info!(
                "link_bench TX: midi_events={} heartbeats={} (queue_depth={})",
                tx_count,
                hb_count,
                queue.len()
            );
        }
    }
}

// ── RX loop ─────────────────────────────────────────────────────────────────

/// Run the RX side: receive packets, dedup via `LinkReceiver`, hand
/// `Body::MidiMessage` payloads to `sink.write_message`, ignore
/// `Body::Heartbeat` (just keep the watchdog fed), and on watchdog expiry
/// (no packet for [`WATCHDOG_MS`] ms) call `sink.all_notes_off` — the M4
/// exit-criterion observable.
pub async fn run_rx<Spi, Busy, Dio1, Reset, Switch, Led, Sink>(
    radio: &mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>,
    led: &mut Led,
    sink: &mut Sink,
) -> !
where
    Spi: SpiDevice,
    Busy: Wait,
    Dio1: Wait,
    Reset: embedded_hal::digital::OutputPin,
    Switch: RfSwitchControl,
    Led: StatefulOutputPin,
    Sink: MidiSink,
{
    if let Err(_) = configure_radio(radio).await {
        defmt::error!("link_bench RX: radio configure failed; halting");
        loop {
            Timer::after_millis(1000).await;
        }
    }
    if let Err(_) = radio.rx_start().await {
        defmt::error!("link_bench RX: rx_start failed; halting");
        loop {
            Timer::after_millis(1000).await;
        }
    }
    defmt::info!(
        "link_bench RX: listening on {} Hz / {} bps GFSK, watchdog={}ms",
        RF_FREQUENCY_HZ,
        RF_BITRATE_BPS,
        WATCHDOG_MS
    );

    let mut receiver = LinkReceiver::no_crypto();
    let mut wd = WatchdogTimer::new(Duration::from_millis(WATCHDOG_MS));
    let mut radio_buf = [0u8; RF_PAYLOAD_MAX as usize];
    let mut accepted: u32 = 0;
    let mut accepted_heartbeats: u32 = 0;
    let mut accepted_midi: u32 = 0;
    let mut dropped: u32 = 0;
    // Count of CRC-mismatch packets — these are usually false-positive
    // sync detections from noise (the chip's sync-word match fires on
    // random bits, then the body's CRC fails).  Common on a noisy band;
    // not actionable per-packet, so we log a summary every 50 instead.
    let mut crc_mismatch: u32 = 0;
    let mut last_stats_log = embassy_time::Instant::now();
    let stats_interval = Duration::from_secs(5);
    // Snapshot of cumulative counters at last stats log, so we can print
    // per-window deltas alongside totals.
    let mut prev_midi: u32 = 0;
    let mut prev_hb: u32 = 0;
    let mut prev_dropped: u32 = 0;
    let mut prev_crc: u32 = 0;
    // Track whether we believe the link is currently up so we only emit
    // ALL_NOTES_OFF on the *transition* from "fed" to "expired".
    let mut link_up = false;

    loop {
        match select(radio.rx_recv(&mut radio_buf), wd.wait()).await {
            Either::First(Ok(pkt)) if pkt.crc_ok => {
                wd.kick();
                let was_down = !link_up;
                link_up = true;
                if was_down {
                    defmt::info!("link_bench RX: link UP");
                }

                let n = pkt.len.min(radio_buf.len());
                match receiver.process(&radio_buf[..n]) {
                    Ok(RxOutcome::Accept(p)) => {
                        accepted = accepted.wrapping_add(1);
                        let _ = led.toggle();
                        match p.body {
                            Body::MidiMessage(bytes) => {
                                accepted_midi = accepted_midi.wrapping_add(1);
                                defmt::info!(
                                    "RX #{} MIDI: rssi={}dBm bytes={=[u8]:#x}",
                                    accepted,
                                    pkt.rssi_dbm,
                                    bytes
                                );
                                if let Err(_) = sink.write_message(bytes).await {
                                    defmt::error!("sink write_message failed");
                                }
                            }
                            Body::Heartbeat => {
                                accepted_heartbeats = accepted_heartbeats.wrapping_add(1);
                                // Watchdog already kicked above; no-op.
                            }
                            Body::SysExFragment { .. } => {
                                defmt::warn!("RX: SysExFragment not handled in M4 bench");
                            }
                            Body::Unknown { event_type, .. } => {
                                defmt::warn!("RX: Unknown event_type=0x{:02x}", event_type);
                            }
                        }
                    }
                    Ok(RxOutcome::Drop(reason)) => {
                        dropped = dropped.wrapping_add(1);
                        // With MIDI triple-send, every MIDI event produces
                        // 2 expected same-seq replay drops.  Don't log per-
                        // drop — count is visible in the periodic stats.
                        // Only log non-Replay reasons (key mismatch is a
                        // real anomaly worth seeing).
                        if !matches!(reason, RxDrop::Replay(_)) {
                            defmt::warn!(
                                "RX dropped: {:?} (accepted={} dropped={})",
                                reason,
                                accepted,
                                dropped
                            );
                        }
                    }
                    Err(_) => {
                        dropped = dropped.wrapping_add(1);
                        defmt::warn!("RX decode error (accepted={} dropped={})", accepted, dropped);
                    }
                }
            }
            Either::First(Ok(_)) => {
                // Likely a false-positive sync detection on noise — the
                // chip latched on random bits matching our sync word, then
                // the framed body's CRC didn't match.  Common, harmless,
                // throttle the log to every 50 events.
                crc_mismatch = crc_mismatch.wrapping_add(1);
                if crc_mismatch % 50 == 0 {
                    defmt::warn!("RX: CRC mismatch count = {}", crc_mismatch);
                }
            }
            Either::First(Err(_)) => {
                defmt::warn!("RX: radio error");
            }
            Either::Second(()) => {
                // Watchdog fired.  Only emit ALL_NOTES_OFF on the link-down
                // transition; subsequent watchdog firings stay quiet.
                if link_up {
                    link_up = false;
                    defmt::warn!(
                        "link_bench RX: LINK LOST (no packet for {}ms) → all-notes-off",
                        WATCHDOG_MS
                    );
                    if let Err(_) = sink.all_notes_off().await {
                        defmt::error!("sink all_notes_off failed");
                    }
                    let _ = led.toggle();
                }
                // Reset deadline so we don't busy-spin on repeated firings.
                wd.kick();
            }
        }

        // Periodic packet-success stats — useful for diagnosing whether
        // the link is dropping due to interference (low success rate)
        // vs. some firmware bug (success rate fine but link still drops).
        // Shows per-window deltas (last 5 s) AND cumulative-since-boot.
        let now = embassy_time::Instant::now();
        if now.duration_since(last_stats_log) >= stats_interval {
            let d_midi = accepted_midi.wrapping_sub(prev_midi);
            let d_hb = accepted_heartbeats.wrapping_sub(prev_hb);
            let d_dropped = dropped.wrapping_sub(prev_dropped);
            let d_crc = crc_mismatch.wrapping_sub(prev_crc);
            // Compute expected from ACTUAL elapsed window (the check above
            // uses `>=`, so the real elapsed is usually ≥ stats_interval by
            // a few ms of scheduling jitter).  Otherwise we'd see 501/500
            // ≠ 0 % loss when the window happens to span 5.01 s.
            let elapsed_ms = now.duration_since(last_stats_log).as_millis() as u32;
            let expected_hb: u32 = elapsed_ms / (HEARTBEAT_MS as u32);
            // Loss percentage * 10 (so we can print one decimal without floats).
            let loss_x10 = if expected_hb > 0 {
                let received = d_midi + d_hb;
                let lost = expected_hb.saturating_sub(received);
                (lost * 1000) / expected_hb
            } else {
                0
            };
            defmt::info!(
                "RX last5s: midi={} hb={}/{} loss={}.{}% drop={} crc_err={} | total: midi={} hb={} drop={} crc_err={}",
                d_midi,
                d_hb,
                expected_hb,
                loss_x10 / 10,
                loss_x10 % 10,
                d_dropped,
                d_crc,
                accepted_midi,
                accepted_heartbeats,
                dropped,
                crc_mismatch,
            );
            prev_midi = accepted_midi;
            prev_hb = accepted_heartbeats;
            prev_dropped = dropped;
            prev_crc = crc_mismatch;
            last_stats_log = now;
        }
    }
}
