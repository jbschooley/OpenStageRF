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
// Maximum TX power on the SX1262 is +22 dBm (HP PA mode).  At stage
// distances (>1 m) path loss eats most of the link-budget headroom, so
// we run flat-out for maximum range and interference robustness.  At
// ~6 in benchtop with this power the RX front end is mildly saturated
// (~−1 dBm at the antenna), causing ~3–6 % loss and occasional 200 ms
// demod lockups; for benchtop testing drop to −9 dBm temporarily.
const RF_TX_POWER_DBM: i8 = 22;
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
    /// Synchronously try to read the next MIDI message.  Returns
    /// `Ok(Some(n))` if an event is ready *right now*, `Ok(None)` if
    /// no event is ready (e.g., scheduled-event source's deadline
    /// hasn't passed, UART buffer is empty, etc.).
    ///
    /// Implementations MUST NOT call into `embassy_time::Timer` here —
    /// this is invoked from `poll_once`-equivalent contexts where the
    /// waker may not support timers.
    fn try_next(&mut self, buf: &mut [u8]) -> Result<Option<usize>, Self::Error>;

    /// Wait until `try_next` would return `Ok(Some(_))`.  May resolve
    /// immediately if an event is already ready, or after a timer if
    /// the next event's deadline is in the future.  Called only inside
    /// `select` where the waker supports `embassy_time::Timer`.
    async fn wait_ready(&mut self);
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
        // 1. Drain any source events into the queue (non-blocking).
        //    `try_next` is sync and safe to call repeatedly; each event
        //    that's "due" right now goes into the queue with status-aware
        //    dedup + a fresh event_seq.
        loop {
            match source.try_next(&mut midi_buf) {
                Ok(Some(n)) => {
                    if !queue.push_channel_voice(&midi_buf[..n]) {
                        overflow_count = overflow_count.wrapping_add(1);
                        defmt::error!(
                            "link_bench TX: queue overflow! dropping (overflows={})",
                            overflow_count
                        );
                    }
                    tx_count = tx_count.wrapping_add(1);
                }
                Ok(None) => break,
                Err(_) => break,
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

        // 3. Queue empty — wait for source-ready OR heartbeat deadline.
        //    `wait_ready` may use `embassy_time::Timer` internally; that's
        //    safe inside `select` because the executor's waker is real.
        match select(source.wait_ready(), hb.wait()).await {
            Either::First(()) => {
                // Source has an event ready; loop to drain.
                continue;
            }
            Either::Second(()) => {
                // Heartbeat fired — fall through to send one.
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
    let mut prev_accepted: u32 = 0;
    // None until RX sees its first packet.  Initialised to whatever
    // packet_seq TX is at when we first hear it, so the first window's
    // loss isn't skewed by the boot-up gap (TX may have already been
    // running for many seconds before RX powered on).
    let mut prev_packet_seq: Option<u32> = None;
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

        // Periodic stats.  The denominator is "packets TX actually
        // transmitted in this window" derived from `packet_seq`
        // advancement (each TX increments it by 1, including K=3
        // retransmits).  That's the only honest "expected" count — it
        // accounts for whether the scenario was bursty (PB/Mod sweeps,
        // K=3 chord copies) or sparse (silent window with heartbeats
        // every 10 ms).  Loss is then real RF loss, not a counting
        // artifact.
        //
        // On a session reset (boot_counter change or huge packet_seq
        // backward jump), the new packet_seq starts low — we clamp the
        // diff to zero in that window so loss reads 0 % rather than a
        // garbage "negative".
        let now = Instant::now();
        if now.duration_since(last_stats_log) >= stats_interval {
            let d_midi = accepted_midi.wrapping_sub(prev_midi);
            let d_hb = accepted_heartbeats.wrapping_sub(prev_hb);
            let d_accepted = accepted.wrapping_sub(prev_accepted);
            let d_dropped = dropped.wrapping_sub(prev_dropped);
            let d_crc = crc_mismatch.wrapping_sub(prev_crc);
            let cur_packet_seq = receiver.last_packet_seq();
            let (tx_count, loss_x10) = match (prev_packet_seq, cur_packet_seq) {
                (Some(prev), Some(cur)) => {
                    let n = cur.saturating_sub(prev);
                    let l = if n > 0 {
                        n.saturating_sub(d_accepted) * 1000 / n
                    } else {
                        0
                    };
                    (n, l)
                }
                // First-ever observation, or session-reset between
                // windows — show 0/0 rather than a skewed first number.
                _ => (0, 0),
            };
            defmt::info!(
                "RX last5s: pkts={}/{} loss={}.{}% midi_ev={} hb={} drop={} crc_err={} | total: pkts={} midi_ev={} hb={} sysex={} drop={} crc_err={}",
                d_accepted,
                tx_count,
                loss_x10 / 10,
                loss_x10 % 10,
                d_midi,
                d_hb,
                d_dropped,
                d_crc,
                accepted,
                accepted_midi,
                accepted_heartbeats,
                accepted_sysex,
                dropped,
                crc_mismatch,
            );
            prev_midi = accepted_midi;
            prev_hb = accepted_heartbeats;
            prev_accepted = accepted;
            prev_packet_seq = cur_packet_seq;
            prev_dropped = dropped;
            prev_crc = crc_mismatch;
            last_stats_log = now;
            // Note: prev_packet_seq is now Some() once we've seen any
            // packet; the next window will compute real loss.
        }
    }
}
