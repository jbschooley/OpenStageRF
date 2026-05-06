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
    ChannelNoteCounts, EventType, HeartbeatTimer, LinkReceiver, LinkSender, MidiTxQueue,
    PoppedPacket, PressedNotes, QueueKind, RxDrop, RxEvent, WatchdogTimer, MAX_BODY_LEN,
};
use osrf_radio_sx126x::{
    GfskBandwidth, GfskPulseShape, RadioError, RfSwitchControl, Sx1262Radio,
};

// ── Bench config ────────────────────────────────────────────────────────────

/// Compile-time maximum radio packet length.  Used to size the static
/// wire / radio buffers.  The runtime payload length is set by
/// [`LinkBenchConfig::payload_max`] and MUST be ≤ this.
pub const RF_PAYLOAD_MAX: u8 = 64;

/// All link-bench tunables in one struct.  RF parameters (frequency,
/// modulation, sync word, TX power) and link-layer timing
/// (watchdog/heartbeat) live here so they can come from a UI / flash
/// store later without a function-signature break.
///
/// `RF_PAYLOAD_MAX` is intentionally NOT in this struct — it sizes
/// compile-time-static buffers and changing it requires a recompile.
/// The runtime `payload_max` field can be ≤ `RF_PAYLOAD_MAX` to use
/// shorter framing if a future radio config requires it.
#[derive(Debug, Clone, Copy)]
pub struct LinkBenchConfig {
    // ── RF (must match between TX and RX) ──
    pub frequency_hz: u32,
    pub bitrate_bps: u32,
    pub deviation_hz: u32,
    pub gfsk_bandwidth: GfskBandwidth,
    pub pulse_shape: GfskPulseShape,
    pub preamble_bits: u16,
    pub payload_max: u8,
    pub sync_word: [u8; 4],

    // ── TX-only knobs ──
    /// SX1262 supports −9..=+22 dBm.  Use +22 for stage range; drop to
    /// ~−9 for benchtop testing inside ~1 m or the receiver front end
    /// gets saturated (~3–6 % loss + occasional demod lockups).
    pub tx_power_dbm: i8,

    // ── Link layer ──
    /// Receiver watchdog timeout.  Default 200 ms — fires `all_notes_off`
    /// on this much silence.
    pub watchdog_ms: u64,
    /// Idle-fill heartbeat interval.  Default 10 ms.  20× safety margin
    /// against the 200 ms watchdog.
    pub heartbeat_ms: u64,
    // Reserved for future: KeyFp, AEAD cipher_id, frequency-hop table,
    // device_id, paired-peer device id, etc.
}

impl LinkBenchConfig {
    /// Default 915 MHz / 300 kbps GFSK / +22 dBm config — what every
    /// existing T114 deployment uses today.  Use as the starting point
    /// before passing into `configure_radio` / `run_tx` / `run_rx`.
    pub const fn default_915() -> Self {
        Self {
            frequency_hz: 915_000_000,
            bitrate_bps: 300_000,
            deviation_hz: 50_000,
            gfsk_bandwidth: GfskBandwidth::Bw4670,
            pulse_shape: GfskPulseShape::Bt05,
            preamble_bits: 16,
            payload_max: RF_PAYLOAD_MAX,
            sync_word: [0xC1, 0x94, 0xC1, 0x94],
            tx_power_dbm: 22,
            watchdog_ms: 200,
            heartbeat_ms: 10,
        }
    }
}

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

/// Apply a `LinkBenchConfig` to the radio.  Idempotent — safe to call
/// again to update RF parameters at runtime (the chip enters and leaves
/// standby per `set_*` call).  Caller must ensure no `radio.tx()` /
/// `radio.rx_recv()` is in flight while this runs.
pub async fn configure_radio<Spi, Busy, Dio1, Reset, Switch>(
    radio: &mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>,
    config: &LinkBenchConfig,
) -> Result<(), RadioError<Reset, Switch>>
where
    Spi: SpiDevice,
    Busy: Wait,
    Dio1: Wait,
    Reset: embedded_hal::digital::OutputPin,
    Switch: RfSwitchControl,
{
    radio.init().await?;
    radio.set_frequency(config.frequency_hz).await?;
    radio
        .set_modulation_gfsk(
            config.bitrate_bps,
            config.deviation_hz,
            config.gfsk_bandwidth,
            config.pulse_shape,
        )
        .await?;
    radio
        .set_packet_format(
            config.preamble_bits,
            &config.sync_word,
            config.payload_max,
            true,
        )
        .await?;
    radio.set_tx_power(config.tx_power_dbm).await?;
    radio.finish_init().await?;
    Ok(())
}

// ── TX loop ─────────────────────────────────────────────────────────────────

/// Run the TX side: consume MIDI messages from `source`, queue them with
/// status-aware dedup + per-event seq, transmit packets via the credit-
/// based round-robin queue.  When the queue is empty for
/// `config.heartbeat_ms`, send a `Heartbeat` instead so the receiver's
/// watchdog stays fed.
pub async fn run_tx<Spi, Busy, Dio1, Reset, Switch, Led, Source>(
    radio: &mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>,
    led: &mut Led,
    source: &mut Source,
    boot_counter: u16,
    config: &LinkBenchConfig,
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
    if let Err(_) = configure_radio(radio, config).await {
        defmt::error!("link_bench TX: radio configure failed; halting");
        loop {
            Timer::after_millis(1000).await;
        }
    }
    defmt::info!(
        "link_bench TX: {} Hz / {} bps GFSK / +{} dBm, boot_counter={}",
        config.frequency_hz,
        config.bitrate_bps,
        config.tx_power_dbm,
        boot_counter
    );

    let mut sender = LinkSender::no_crypto(boot_counter);
    let mut hb = HeartbeatTimer::new(Duration::from_millis(config.heartbeat_ms));
    let mut queue = MidiTxQueue::new();
    // Per-channel pressed-note counts for the heartbeat active-channel
    // mask.  Updated on every successful push_channel_voice; encoded
    // into the body of every heartbeat sent below.
    let mut tx_state = ChannelNoteCounts::new();
    let mut midi_buf = [0u8; 4];
    let mut body_buf = [0u8; MAX_BODY_LEN];
    let mut wire_buf = [0u8; RF_PAYLOAD_MAX as usize];
    let mut tx_count: u32 = 0;
    let mut hb_count: u32 = 0;
    let mut overflow_count: u32 = 0;

    loop {
        let now = Instant::now();

        // 1. Drain any source events into the queue (non-blocking).
        //    `try_next` is sync and safe to call repeatedly; each event
        //    that's "due" right now goes into the queue with status-aware
        //    dedup + a fresh event_seq.  NoteOff pushes also queue
        //    delayed retransmit copies based on `now`.
        loop {
            match source.try_next(&mut midi_buf) {
                Ok(Some(n)) => {
                    let msg = &midi_buf[..n];
                    if queue.push_channel_voice(msg, now) {
                        // Track the note-count change so the next
                        // heartbeat carries an accurate active mask.
                        tx_state.observe(msg);
                    } else {
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

        // 2. If the queue has anything eligible, pop one packet's worth
        //    and TX.  The credit-based queue handles batching, priority,
        //    round-robin retransmits, and time-spread NoteOff redundancy
        //    (delayed copies stay queued until their `next_eligible`).
        if let Some(PoppedPacket { kind, body_len }) = queue.pop_packet(now, &mut body_buf) {
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

        // Send heartbeat (single copy — next one is ≤ heartbeat_ms
        // away).  The body is a 2-byte big-endian active-channel mask
        // — the receiver uses it to detect channels with stuck notes
        // and fire CC 123 (All Notes Off) for any that need recovery.
        let mask_body = tx_state.active_mask().to_be_bytes();
        match sender.encode(EventType::Heartbeat, &mask_body, &mut wire_buf) {
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
    config: &LinkBenchConfig,
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
    if let Err(_) = configure_radio(radio, config).await {
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
        config.frequency_hz,
        config.bitrate_bps,
        config.watchdog_ms
    );

    let mut receiver = LinkReceiver::no_crypto();
    let mut wd = WatchdogTimer::new(Duration::from_millis(config.watchdog_ms));
    let mut radio_buf = [0u8; RF_PAYLOAD_MAX as usize];
    let mut accepted: u32 = 0;
    let mut accepted_heartbeats: u32 = 0;
    let mut accepted_midi: u32 = 0;
    let mut accepted_sysex: u32 = 0;
    let mut dropped: u32 = 0;
    let mut crc_mismatch: u32 = 0;
    let mut last_stats_log = Instant::now();
    let stats_interval = Duration::from_secs(1);
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
    // Local pressed-notes tracker for the heartbeat-state failsafe.
    // Updated on every accepted ChannelVoice; checked on each
    // Heartbeat carrying an active-channel mask.
    let mut rx_state = PressedNotes::new();
    // Per-channel "divergence first observed" timestamp.  When a
    // heartbeat mask says ch X is silent but RX still has notes
    // pressed in ch X, we record `now`.  The divergence must persist
    // for at least `STUCK_NOTE_MIN_DIVERGENCE_MS` before we fire
    // recovery — this protects against the legitimate race where TX
    // updates its mask the instant a NoteOff is *pushed* but the
    // NoteOff packet (or its main K=3 / +30 / +60 ms delayed copies)
    // hasn't reached RX yet.  The +60 ms delayed copy is the latest a
    // legitimate NoteOff can arrive, so the threshold must comfortably
    // exceed it.  When the divergence clears (mask now reflects RX's
    // pressed state, or RX cleared the channel) the timestamp resets.
    let mut divergence_since: [Option<Instant>; 16] = [None; 16];
    /// Minimum continuous divergence before stuck-note recovery fires.
    /// Must exceed the +60 ms delayed-copy ceiling with slack for
    /// heartbeat-cadence jitter; 100 ms gives 40 ms of head-room.
    const STUCK_NOTE_MIN_DIVERGENCE_MS: u64 = 100;
    // Counter of stuck-channel recoveries fired (diagnostic).
    let mut stuck_recoveries: u32 = 0;

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
                // Snapshot what we may need outside the closure.
                let mut heartbeat_mask: Option<Option<u16>> = None;
                let result = receiver.process(&radio_buf[..n], now, |ev| {
                    match ev {
                        RxEvent::Heartbeat(mask) => {
                            accepted_heartbeats = accepted_heartbeats.wrapping_add(1);
                            heartbeat_mask = Some(mask);
                        }
                        RxEvent::ChannelVoice(midi) => {
                            accepted_midi = accepted_midi.wrapping_add(1);
                            // Track local pressed-notes state so we can
                            // detect divergence from TX's heartbeat mask.
                            rx_state.observe(midi);
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

                // Stuck-note recovery: if this packet was a heartbeat
                // carrying a mask, check each channel where TX says
                // silent but RX has notes pressed.  Only fire recovery
                // for channels where that divergence has persisted
                // continuously for ≥ `STUCK_NOTE_MIN_DIVERGENCE_MS` —
                // shorter divergences are almost certainly the
                // legitimate race between a NoteOff being pushed (TX
                // mask flips to 0) and that NoteOff actually reaching
                // RX (up to +60 ms via delayed-copy retransmits).
                //
                // For each stuck channel, send SELECTIVE NoteOffs
                // (status 0x80, vel 0) for the notes RX believes are
                // still down — NOT a blanket CC 123.  This preserves
                // release tails on unrelated notes while still
                // clearing the genuinely-stuck ones.
                if let Some(mask_opt) = heartbeat_mask {
                    match mask_opt {
                        Some(mask) => {
                            let needed = rx_state.missing_clear(mask);
                            for ch in 0..16u8 {
                                let bit = 1u16 << ch;
                                if needed & bit == 0 {
                                    // No divergence on this channel —
                                    // reset its timer.
                                    divergence_since[ch as usize] = None;
                                    continue;
                                }
                                // Divergence present.  Start the timer
                                // if this is the first observation.
                                let started = divergence_since[ch as usize]
                                    .get_or_insert(now);
                                if now.duration_since(*started)
                                    < Duration::from_millis(STUCK_NOTE_MIN_DIVERGENCE_MS)
                                {
                                    continue;
                                }
                                // Persisted long enough — recover.
                                let pressed = rx_state.pressed_on(ch);
                                let mut count = 0u32;
                                for note in 0..128u8 {
                                    if pressed & (1u128 << note) != 0 {
                                        let mut noteoff: heapless::Vec<u8, 8> =
                                            heapless::Vec::new();
                                        let _ = noteoff.extend_from_slice(&[
                                            0x80 | ch,
                                            note,
                                            0,
                                        ]);
                                        let _ = events
                                            .push(BufferedEvent::Midi(noteoff));
                                        count += 1;
                                    }
                                }
                                rx_state.clear_channel(ch);
                                divergence_since[ch as usize] = None;
                                stuck_recoveries =
                                    stuck_recoveries.wrapping_add(1);
                                defmt::warn!(
                                    "RX stuck-note recovery: ch {} → {} selective NoteOff(s) (total recoveries={})",
                                    ch,
                                    count,
                                    stuck_recoveries
                                );
                            }
                        }
                        None => {
                            // Legacy 0-byte heartbeat — reset all
                            // divergence timers so a stale mask
                            // doesn't pollute recovery.
                            divergence_since = [None; 16];
                        }
                    }
                }
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
                        config.watchdog_ms
                    );
                    if let Err(_) = sink.all_notes_off().await {
                        defmt::error!("sink all_notes_off failed");
                    }
                    receiver.mark_link_down();
                    // Watchdog all-notes-off clears local pressed-notes
                    // state; also reset the per-channel divergence
                    // timers so a stale mask from before the link drop
                    // doesn't pollute recovery when the link comes
                    // back.
                    rx_state.reset();
                    divergence_since = [None; 16];
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
                "RX last1s: pkts={}/{} loss={}.{}% midi_ev={} hb={} drop={} crc_err={} | total: pkts={} midi_ev={} hb={} sysex={} drop={} crc_err={}",
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
