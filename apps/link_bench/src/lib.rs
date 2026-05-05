// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![allow(async_fn_in_trait)]

//! Link-layer bench, board-agnostic.
//!
//! Exercises the full TX→radio→RX path with `osrf-link`'s `LinkSender`,
//! `LinkReceiver`, `MidiTxQueue`, `HeartbeatTimer`, and `WatchdogTimer`
//! against the hand-rolled `osrf-radio-sx126x` driver.  The MIDI byte
//! source (TX) and sink (RX) are abstracted behind two traits so the
//! bench can run with either:
//!
//! * the synthetic source/sink in [`synthetic`] (proves the link layer
//!   end-to-end without real-MIDI hardware);
//! * a future UART-backed source/sink wrapping `BufferedUarte` + a MIDI
//!   parser (one trait impl swap, no runtime changes here).
//!
//! Each MIDI message is wrapped in a `CHANNEL_VOICE` body with a fresh
//! `event_seq`; heartbeats fill silence; SysEx (when supported by the
//! source) is queued at SysEx priority.  When TX power is cut, the
//! receiver's watchdog fires, [`MidiSink::all_notes_off`] is called, and
//! the receiver is marked link-down so the next packet (post-restart)
//! triggers a session reset.

pub mod synthetic;

use core::task::Poll;
use embassy_futures::poll_once;
use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Instant, Timer};
use embedded_hal::digital::StatefulOutputPin;
use embedded_hal_async::{digital::Wait, spi::SpiDevice};

use osrf_link::{
    EventType, HeartbeatTimer, LinkReceiver, LinkSender, MidiTxQueue, PoppedPacket, QueueKind,
    RxDrop, RxEvent, WatchdogTimer, MAX_BODY_LEN,
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

// ── Link-layer config ───────────────────────────────────────────────────────

/// Receiver watchdog: 200 ms of silence → assume link lost.
pub const WATCHDOG_MS: u64 = 200;
/// Transmitter heartbeat: 10 ms idle → emit a Heartbeat packet.  20× margin
/// against the receiver's 200 ms watchdog.
pub const HEARTBEAT_MS: u64 = 10;

// ── Source / Sink traits ─────────────────────────────────────────────────────

pub trait MidiSource {
    type Error;
    /// Wait for the next MIDI message and write its bytes into `buf`.
    /// Returns the number of bytes written.  May resolve only after an
    /// arbitrarily long delay.
    async fn next_message(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error>;
}

pub trait MidiSink {
    type Error;
    async fn write_message(&mut self, bytes: &[u8]) -> Result<(), Self::Error>;
    /// Emit "all notes off" on every channel.  Called by [`run_rx`] on
    /// watchdog expiry.
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

/// Run the TX side: consume MIDI messages from `source`, queue them with
/// status-aware dedup + per-event seq, transmit packets via the credit-
/// based round-robin queue.  When the queue is empty for `HEARTBEAT_MS`
/// ms, send a `Heartbeat` instead so the receiver's watchdog stays fed.
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
    let mut queue = MidiTxQueue::new();
    let mut midi_buf = [0u8; 4];
    let mut body_buf = [0u8; MAX_BODY_LEN];
    let mut wire_buf = [0u8; RF_PAYLOAD_MAX as usize];
    let mut tx_count: u32 = 0;
    let mut hb_count: u32 = 0;
    let mut overflow_count: u32 = 0;

    loop {
        // 1. Drain any source events into the queue (non-blocking).  Each
        //    push applies MIDI status-aware dedup and assigns a fresh
        //    event_seq.
        loop {
            match poll_once(source.next_message(&mut midi_buf)) {
                Poll::Ready(Ok(n)) => {
                    if !queue.push_channel_voice(&midi_buf[..n]) {
                        overflow_count = overflow_count.wrapping_add(1);
                        defmt::error!(
                            "link_bench TX: queue overflow! dropping (overflows={})",
                            overflow_count
                        );
                    }
                    tx_count = tx_count.wrapping_add(1);
                }
                Poll::Ready(Err(_)) => break,
                Poll::Pending => break,
            }
        }

        // 2. If the queue has anything, pop one packet's worth and TX.
        //    The credit-based queue handles batching, priority, and
        //    round-robin retransmits.  Each pop yields a fresh packet
        //    with a new packet_seq; consumed events are requeued at the
        //    back of their priority class until their credits exhaust.
        if let Some(PoppedPacket { kind, body_len }) = queue.pop_packet(&mut body_buf) {
            let event_type = match kind {
                QueueKind::ChannelVoice => EventType::ChannelVoice,
                QueueKind::SysExFragment => EventType::SysExFragment,
            };
            match sender.encode(event_type, &body_buf[..body_len], &mut wire_buf) {
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
        //    heartbeat deadline.
        match select(source.next_message(&mut midi_buf), hb.wait()).await {
            Either::First(Ok(n)) => {
                if !queue.push_channel_voice(&midi_buf[..n]) {
                    overflow_count = overflow_count.wrapping_add(1);
                    defmt::error!(
                        "link_bench TX: queue overflow! dropping (overflows={})",
                        overflow_count
                    );
                }
                tx_count = tx_count.wrapping_add(1);
                continue;
            }
            Either::First(Err(_)) => {
                defmt::warn!("link_bench TX: source error; sending heartbeat");
            }
            Either::Second(()) => {
                // After heartbeat win, micro-poll source to catch races.
                if let Poll::Ready(Ok(n)) = poll_once(source.next_message(&mut midi_buf)) {
                    if !queue.push_channel_voice(&midi_buf[..n]) {
                        overflow_count = overflow_count.wrapping_add(1);
                    }
                    tx_count = tx_count.wrapping_add(1);
                    continue;
                }
            }
        }

        // Send heartbeat (single copy — next one is ≤ HEARTBEAT_MS away).
        match sender.encode(EventType::Heartbeat, &[], &mut wire_buf) {
            Ok(wire_n) => {
                if let Err(_) = radio.tx(&wire_buf[..wire_n]).await {
                    defmt::error!("link_bench TX: radio.tx() failed");
                }
            }
            Err(_) => defmt::error!("link_bench TX: encode failed"),
        }
        hb.note_send();
        let _ = led.toggle();
        hb_count = hb_count.wrapping_add(1);

        if (tx_count.wrapping_add(hb_count)) % 500 == 0 {
            defmt::info!(
                "link_bench TX: midi_events={} heartbeats={} queue_depth={} overflows={}",
                tx_count,
                hb_count,
                queue.len(),
                overflow_count
            );
        }
    }
}

// ── RX loop ─────────────────────────────────────────────────────────────────

/// One observable RX event ready to be delivered to the sink.  We buffer
/// these inside `process()`'s callback and drain them after the call so
/// the async sink can be awaited without holding the receiver borrow.
enum BufferedEvent {
    Midi(heapless::Vec<u8, 8>),
    SysEx(heapless::Vec<u8, { osrf_link::MAX_SYSEX_BYTES }>),
}

/// Run the RX side: receive packets, dedup at packet + event level,
/// reassemble SysEx, hand each surviving event to the sink.  On
/// watchdog expiry call `sink.all_notes_off` and mark the receiver as
/// link-down so the next packet triggers a session reset.
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
    let mut accepted_sysex: u32 = 0;
    let mut dropped: u32 = 0;
    let mut crc_mismatch: u32 = 0;
    let mut last_stats_log = Instant::now();
    let stats_interval = Duration::from_secs(5);
    let mut prev_midi: u32 = 0;
    let mut prev_hb: u32 = 0;
    let mut prev_dropped: u32 = 0;
    let mut prev_crc: u32 = 0;
    let mut link_up = false;
    let mut events: heapless::Vec<BufferedEvent, 32> = heapless::Vec::new();

    loop {
        events.clear();
        match select(radio.rx_recv(&mut radio_buf), wd.wait()).await {
            Either::First(Ok(pkt)) if pkt.crc_ok => {
                wd.kick();
                let was_down = !link_up;
                link_up = true;
                if was_down {
                    defmt::info!("link_bench RX: link UP");
                }

                let n = pkt.len.min(radio_buf.len());
                let now = Instant::now();
                let result = receiver.process(&radio_buf[..n], now, |ev| {
                    match ev {
                        RxEvent::Heartbeat => {
                            accepted_heartbeats = accepted_heartbeats.wrapping_add(1);
                        }
                        RxEvent::ChannelVoice(midi) => {
                            accepted_midi = accepted_midi.wrapping_add(1);
                            let mut v: heapless::Vec<u8, 8> = heapless::Vec::new();
                            let _ = v.extend_from_slice(midi);
                            let _ = events.push(BufferedEvent::Midi(v));
                        }
                        RxEvent::SysExComplete(body) => {
                            accepted_sysex = accepted_sysex.wrapping_add(1);
                            let mut v: heapless::Vec<u8, { osrf_link::MAX_SYSEX_BYTES }> =
                                heapless::Vec::new();
                            let _ = v.extend_from_slice(body);
                            let _ = events.push(BufferedEvent::SysEx(v));
                        }
                    }
                });
                match result {
                    Ok(Ok(())) => {
                        accepted = accepted.wrapping_add(1);
                        let _ = led.toggle();
                    }
                    Ok(Err(reason)) => {
                        dropped = dropped.wrapping_add(1);
                        if !matches!(reason, RxDrop::PacketReplay(_)) {
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

                // Drain buffered events to the sink.
                for ev in events.iter() {
                    match ev {
                        BufferedEvent::Midi(bytes) => {
                            defmt::info!("RX MIDI: {=[u8]:#x}", bytes.as_slice());
                            if let Err(_) = sink.write_message(bytes).await {
                                defmt::error!("sink write_message failed");
                            }
                        }
                        BufferedEvent::SysEx(bytes) => {
                            defmt::info!("RX SysEx: {} bytes", bytes.len());
                            if let Err(_) = sink.write_message(bytes).await {
                                defmt::error!("sink SysEx write failed");
                            }
                        }
                    }
                }
            }
            Either::First(Ok(_)) => {
                crc_mismatch = crc_mismatch.wrapping_add(1);
                if crc_mismatch % 50 == 0 {
                    defmt::warn!("RX: CRC mismatch count = {}", crc_mismatch);
                }
            }
            Either::First(Err(_)) => {
                defmt::warn!("RX: radio error");
            }
            Either::Second(()) => {
                if link_up {
                    link_up = false;
                    defmt::warn!(
                        "link_bench RX: LINK LOST (no packet for {}ms) → all-notes-off",
                        WATCHDOG_MS
                    );
                    if let Err(_) = sink.all_notes_off().await {
                        defmt::error!("sink all_notes_off failed");
                    }
                    receiver.mark_link_down();
                    let _ = led.toggle();
                }
                wd.kick();
            }
        }

        // Periodic stats — useful for diagnosing whether link is dropping
        // due to interference (low success rate) vs. a firmware bug.
        let now = Instant::now();
        if now.duration_since(last_stats_log) >= stats_interval {
            let d_midi = accepted_midi.wrapping_sub(prev_midi);
            let d_hb = accepted_heartbeats.wrapping_sub(prev_hb);
            let d_dropped = dropped.wrapping_sub(prev_dropped);
            let d_crc = crc_mismatch.wrapping_sub(prev_crc);
            let elapsed_ms = now.duration_since(last_stats_log).as_millis() as u32;
            let expected_hb: u32 = elapsed_ms / (HEARTBEAT_MS as u32);
            let loss_x10 = if expected_hb > 0 {
                let received = d_midi + d_hb;
                let lost = expected_hb.saturating_sub(received);
                (lost * 1000) / expected_hb
            } else {
                0
            };
            defmt::info!(
                "RX last5s: midi={} hb={}/{} loss={}.{}% drop={} crc_err={} | total: midi={} hb={} sysex={} drop={} crc_err={}",
                d_midi,
                d_hb,
                expected_hb,
                loss_x10 / 10,
                loss_x10 % 10,
                d_dropped,
                d_crc,
                accepted_midi,
                accepted_heartbeats,
                accepted_sysex,
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
