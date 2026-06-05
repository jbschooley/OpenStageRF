// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![allow(async_fn_in_trait)]

//! Link-layer node runtime, board-agnostic.
//!
//! Exercises the full TX→radio→RX path with `osrf-link`'s `LinkSender`,
//! `LinkReceiver`, `MidiTxQueue`, `HeartbeatTimer`, and `WatchdogTimer`
//! against the hand-rolled `osrf-radio-sx126x` driver.  The MIDI byte
//! source (TX) and sink (RX) are abstracted behind two traits so the
//! same runtime can drive both:
//!
//! * the synthetic source/sink in `osrf-app-link-bench` (proves the link
//!   layer end-to-end without real-MIDI hardware);
//! * the UART-backed source/sink in `osrf-app-midi-node` wrapping
//!   `BufferedUarte` + `MidiParser`.
//!
//! Each MIDI message is wrapped in a `CHANNEL_VOICE` body with a fresh
//! `event_seq`; heartbeats fill silence with a 2-byte big-endian
//! active-channel mask; SysEx (when supported by the source) is queued
//! at SysEx priority.  When TX power is cut, the receiver's watchdog
//! fires, [`MidiSink::all_notes_off`] is called, and the receiver is
//! marked link-down so the next packet (post-restart) triggers a
//! session reset.

use embassy_futures::select::{select, select5, Either, Either5};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Channel, Receiver, Sender};
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use embedded_hal::digital::StatefulOutputPin;
use embedded_hal_async::{digital::Wait, spi::SpiDevice};

use osrf_link::{
    AeadContext, ChannelNoteCounts, EventType, HeartbeatTimer, LinkReceiver, LinkSender,
    MidiTxQueue, PoppedPacket, PressedNotes, QueueKind, RxDrop, RxEvent, WatchdogTimer,
    MAX_BODY_LEN,
};
// Re-export the AEAD types so callers configure encryption without
// depending on `osrf-link` directly.
pub use osrf_link::{osrf_crypto, AeadContext as AeadConfig, CipherId, Direction};
use osrf_radio_sx126x::{
    Error as RadioErrorKind, GfskBandwidth, GfskPulseShape, RadioError, RfSwitchControl, RxPacket,
    Sx1262Radio,
};

// ── Runtime config ──────────────────────────────────────────────────────────

/// Compile-time maximum radio packet length.  Used to size the static
/// wire / radio buffers.  The runtime payload length is set by
/// [`LinkConfig::payload_max`] and MUST be ≤ this.
pub const RF_PAYLOAD_MAX: u8 = 64;

/// Depth of the diversity-radio handoff channel.  A handful of slots
/// absorbs the consumer being briefly busy (sink writes, stuck-note
/// recovery) without the producer ever blocking — the producer uses
/// `try_send` and drops on full, so a backed-up channel never stalls the
/// secondary radio's drain loop (which is what must keep running to avoid
/// a stuck DIO1 IRQ).
const DIVERSITY_CH_DEPTH: usize = 4;

/// One received frame handed from the secondary (diversity) radio's drain
/// task to the primary receive loop.  Owns its bytes (fixed array, no
/// borrow) so it can live in the channel independent of the radio buffer.
#[derive(Clone)]
pub struct DiversityRxFrame {
    /// Raw wire bytes, valid for `len`.
    pub buf: [u8; RF_PAYLOAD_MAX as usize],
    pub len: usize,
    pub rssi_dbm: i16,
}

/// Channel carrying [`DiversityRxFrame`]s from [`run_rx_secondary`] (the
/// diversity radio's drain task) to [`run_rx_diversity`] (the primary
/// receive loop).  A profile declares one as a `static` and hands a
/// `.receiver()` to the consumer and a `.sender()` to the producer.
pub type DiversityRxChannel = Channel<CriticalSectionRawMutex, DiversityRxFrame, DIVERSITY_CH_DEPTH>;
/// Receiver half of [`DiversityRxChannel`] (consumed by the primary loop).
pub type DiversityRxReceiver =
    Receiver<'static, CriticalSectionRawMutex, DiversityRxFrame, DIVERSITY_CH_DEPTH>;
/// Sender half of [`DiversityRxChannel`] (used by the secondary drain task).
pub type DiversityRxSender =
    Sender<'static, CriticalSectionRawMutex, DiversityRxFrame, DIVERSITY_CH_DEPTH>;

/// All link-runtime tunables in one struct.  RF parameters (frequency,
/// modulation, sync word, TX power) and link-layer timing
/// (watchdog/heartbeat) live here so they can come from a UI / flash
/// store later without a function-signature break.
///
/// `RF_PAYLOAD_MAX` is intentionally NOT in this struct — it sizes
/// compile-time-static buffers and changing it requires a recompile.
/// The runtime `payload_max` field can be ≤ `RF_PAYLOAD_MAX` to use
/// shorter framing if a future radio config requires it.
#[derive(Debug, Clone, Copy)]
pub struct LinkConfig {
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

impl LinkConfig {
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

// ── Live config-update signalling ───────────────────────────────────────────

/// Latest-wins slot for live `LinkConfig` updates.  The UI (or any
/// future producer — flash-restore, BLE config write, telemetry-driven
/// retune) calls [`signal`](Self::signal) with the new config; the
/// runtime's `run_tx` / `run_rx` loop checks for a pending update at
/// each iteration's blocking point and, if one is present, walks the
/// radio through `init` → re-`configure_radio` → resume (`rx_start`
/// for RX; just continue for TX).
///
/// "Latest-wins" is the right semantics here — only the most recent
/// config matters; if two updates land before the runtime polls, the
/// older one is obsolete.  Embassy's [`Signal`] is exactly that.
///
/// Profiles that don't need live config (everything except
/// `t114_ui`) pass `None` for `config_updates` and the runtime skips
/// the update arm of its `select` entirely — zero cost.
pub struct LinkConfigSignal {
    inner: Signal<CriticalSectionRawMutex, LinkConfig>,
}

impl LinkConfigSignal {
    pub const fn new() -> Self {
        Self {
            inner: Signal::new(),
        }
    }
    /// Publish a new config.  Latest-wins: a previously signalled
    /// (but not yet consumed) config is dropped.
    pub fn signal(&self, c: LinkConfig) {
        self.inner.signal(c);
    }
    /// Async wait for the next update.  Used by the runtime inside
    /// `select` so it pre-empts whatever blocking await is in flight.
    pub async fn wait(&self) -> LinkConfig {
        self.inner.wait().await
    }
    /// Non-blocking poll.  Returns `Some` if a config has been
    /// signalled and not yet consumed (clears the slot in the same
    /// call).  Used at the top of each runtime loop iteration so a
    /// config that landed during the prior fast-path (TX burst, RX
    /// packet processing) gets applied before the next blocking
    /// await.
    pub fn try_take(&self) -> Option<LinkConfig> {
        self.inner.try_take()
    }
}

impl Default for LinkConfigSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// Live AEAD update sent from the UI when the operator picks a
/// different key (or Open) in the Key menu.  Both `run_tx` and
/// `run_rx` consume the same signal — each takes the parts it cares
/// about.
///
/// * **TX:** uses `aead` only.  `Some(ctx)` → encrypt subsequent
///   packets with this key; `None` → drop to plaintext.  `allow_open`
///   is meaningless on the TX side.
/// * **RX:** uses both.  `aead` configures which key the receiver
///   can decrypt with (single-key keyring for now); `allow_open`
///   gates whether plaintext packets also pass through.  Typical
///   profile mapping:
///   * operator selects Open / Auto → `aead = Some(known_key)`,
///     `allow_open = true` (permissive — accept the known key OR
///     plaintext);
///   * operator selects a specific key → `aead = Some(that_key)`,
///     `allow_open = false` (strict — only that key, plaintext
///     rejected);
///   * operator selects a key the receiver has no material for →
///     `aead = None`, `allow_open = false` (refuse everything; a
///     warning indicator on the UI is the right follow-up).
#[derive(Clone, Copy)]
pub struct AeadUpdate {
    pub aead: Option<AeadContext>,
    pub allow_open: bool,
}

/// Embassy [`Signal`] wrapper for [`AeadUpdate`].  Same shape as
/// [`LinkConfigSignal`] — latest-wins; consumed by the runtime
/// inside its top-of-loop `try_take` poll and inside its `select`
/// arm so updates that land mid-blocking-await still apply.
pub struct AeadUpdateSignal {
    inner: Signal<CriticalSectionRawMutex, AeadUpdate>,
}

impl AeadUpdateSignal {
    pub const fn new() -> Self {
        Self {
            inner: Signal::new(),
        }
    }
    pub fn signal(&self, u: AeadUpdate) {
        self.inner.signal(u);
    }
    pub async fn wait(&self) -> AeadUpdate {
        self.inner.wait().await
    }
    pub fn try_take(&self) -> Option<AeadUpdate> {
        self.inner.try_take()
    }
}

impl Default for AeadUpdateSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// Shutdown request from outside the link runtime.  Used by the
/// profile's battery monitor to ask `run_rx` / `run_tx` to wrap up
/// gracefully on low-battery before the cell finishes dying:
///
///   1. `sink.all_notes_off()` (RX-side only — silences the synth
///      so it doesn't hang notes when TX vanishes).
///   2. `radio.set_standby_rc()` — stops transmitting and cuts
///      most of the radio current.
///   3. LED blink pattern as visual confirmation of the shutdown.
///   4. Permanent idle (`Timer::after_secs(60)` loop) — keeps the
///      task alive at zero CPU until the cell finishes draining.
///
/// Profiles that don't need shutdown coordination pass `None` for
/// the `shutdown` parameter and the runtime never checks it.
pub struct ShutdownSignal {
    inner: Signal<CriticalSectionRawMutex, ()>,
}

impl ShutdownSignal {
    pub const fn new() -> Self {
        Self {
            inner: Signal::new(),
        }
    }
    pub fn signal(&self) {
        self.inner.signal(());
    }
    /// Block until the signal fires.  Exposed `pub` (rather than
    /// crate-private) so a profile can include a `select` arm in its
    /// own UI loop — e.g. to paint a "shutting down" screen, persist
    /// last-state, and halt — alongside the radio-park dance the
    /// link runtime handles internally.  Multiple awaiters are
    /// supported: the underlying `Signal` re-arms on take, so a
    /// single `signal()` wakes the first awaiter only.  In practice
    /// profiles signal exactly once and rely on `Signal::signaled`
    /// remaining `true` until each waiter has observed it.
    pub async fn wait(&self) {
        self.inner.wait().await;
    }
}

impl Default for ShutdownSignal {
    fn default() -> Self {
        Self::new()
    }
}

// ── Channel-scan control ────────────────────────────────────────────────────

/// Maximum channel count a scan request can address.  Sized to match
/// `osrf_ui::MAX_SCAN_CHANNELS` so a UI-driven scan never gets clipped
/// regardless of band plan (the densest plan in the UI core, "Wide",
/// has 131 channels at 200 kHz spacing).
pub const SCAN_MAX_CHANNELS: usize = 144;

/// Sentinel RSSI for "no measurement yet" — written into the results
/// array on `start()` and overwritten as the runtime sweeps.  Renderers
/// can detect this and draw a placeholder bar.
pub const SCAN_RSSI_NONE: i16 = i16::MIN;

/// Receiver settle time after entering RX on a fresh channel,
/// before the first RSSI sample.  Typical SX1262 RX-on-to-RxReady
/// is ~70 µs; 1 ms is plenty of headroom for the front-end and
/// AGC to stabilise even with the narrow scan-time IF filter.
const SCAN_SETTLE_MS: u64 = 1;
/// How many `get_rssi_inst` samples to take per channel, spaced
/// [`SCAN_SAMPLE_INTERVAL_MS`] apart.  We report the **peak** value
/// observed across the window — `get_rssi_inst` is a single
/// instantaneous read, so at a fixed offset against a TX that's
/// on-air ~1 ms / 10 ms (heartbeat) we'd catch the carrier only
/// ~10 % of the time.  6 × 1 ms = 6 ms window catches the carrier
/// ~60 % of the time per pass; the UI's `peak_dbm` accumulator
/// covers the remaining gaps within ~3 passes (~3 s on Wide).
const SCAN_SAMPLES_PER_CHANNEL: u8 = 6;
/// Time between RSSI samples within a single channel's dwell.
const SCAN_SAMPLE_INTERVAL_MS: u64 = 1;

/// Shared scan-mode controller.  The UI signals "begin scanning these
/// frequencies" by calling [`start`](Self::start); the runtime's
/// `run_rx` loop notices, walks the chip out of continuous RX, sweeps
/// the channel list ([`scan_one_channel`] per slot — peak RSSI over
/// a >10 ms window so heartbeat-cadence carriers are reliably
/// caught), and continues looping until [`stop`](Self::stop) is
/// called, at which point it re-applies the operating `LinkConfig`
/// and resumes normal RX.
///
/// While a scan is active the link is effectively down: RX isn't
/// listening on the operating channel.  The receiver state is reset
/// on scan exit so the next packet from TX triggers a fresh session.
///
/// `run_tx` ignores the controller — TX has nothing to do during a
/// scan and continues transmitting (heartbeats + any queued MIDI) on
/// the operating channel.  The TX→RX bridge "looks broken" only from
/// the RX side's perspective, which is the correct semantic.
pub struct ScanController {
    inner: critical_section::Mutex<core::cell::RefCell<ScanInner>>,
    /// Fires on enable/disable transitions and on `start()` channel-list
    /// changes.  `run_rx` includes `wait_change().await` in its `select`
    /// so it preempts `rx_recv` immediately rather than waiting for the
    /// next packet (which may never arrive on a quiet channel).
    state_change: Signal<CriticalSectionRawMutex, ()>,
}

struct ScanInner {
    enabled: bool,
    channel_count: u8,
    frequencies: [u32; SCAN_MAX_CHANNELS],
    results: [i16; SCAN_MAX_CHANNELS],
    /// Increments each time the runtime completes a full sweep
    /// through all channels.  UI consumers can read this to know
    /// whether the displayed bars represent "still filling in" vs
    /// "complete coverage of the band plan."
    completed_passes: u32,
}

impl ScanInner {
    const fn empty() -> Self {
        Self {
            enabled: false,
            channel_count: 0,
            frequencies: [0; SCAN_MAX_CHANNELS],
            results: [SCAN_RSSI_NONE; SCAN_MAX_CHANNELS],
            completed_passes: 0,
        }
    }
}

impl ScanController {
    pub const fn new() -> Self {
        Self {
            inner: critical_section::Mutex::new(core::cell::RefCell::new(ScanInner::empty())),
            state_change: Signal::new(),
        }
    }

    /// Begin (or update) a scan.  `frequencies` is the band-plan
    /// channel list in any order — the runtime sweeps it index-0
    /// upward and wraps.  Latest call wins; switching band plans
    /// while scanning is just another `start()` with the new list.
    /// All previous results are cleared to [`SCAN_RSSI_NONE`].
    pub fn start(&self, frequencies: &[u32]) {
        critical_section::with(|cs| {
            let mut s = self.inner.borrow(cs).borrow_mut();
            let n = frequencies.len().min(SCAN_MAX_CHANNELS);
            s.channel_count = n as u8;
            s.frequencies[..n].copy_from_slice(&frequencies[..n]);
            for r in s.results.iter_mut() {
                *r = SCAN_RSSI_NONE;
            }
            s.completed_passes = 0;
            s.enabled = true;
        });
        self.state_change.signal(());
    }

    /// Stop scanning.  Runtime re-applies the operating `LinkConfig`
    /// and resumes normal RX on the next loop iteration.
    pub fn stop(&self) {
        critical_section::with(|cs| {
            self.inner.borrow(cs).borrow_mut().enabled = false;
        });
        self.state_change.signal(());
    }

    /// Snapshot the latest RSSI per channel into `out`.  Returns the
    /// number of slots written (capped by both the controller's
    /// `channel_count` and `out.len()`).  Called by the UI render path
    /// each tick to feed `osrf_ui::UiState::apply_scan_pass`.
    /// Channels that haven't been sampled yet hold [`SCAN_RSSI_NONE`].
    pub fn read_results(&self, out: &mut [i16]) -> usize {
        critical_section::with(|cs| {
            let s = self.inner.borrow(cs).borrow();
            let n = (s.channel_count as usize).min(out.len());
            out[..n].copy_from_slice(&s.results[..n]);
            n
        })
    }

    /// Number of full sweeps the runtime has completed since the most
    /// recent `start()`.  0 = first pass still in progress.
    pub fn completed_passes(&self) -> u32 {
        critical_section::with(|cs| self.inner.borrow(cs).borrow().completed_passes)
    }

    // ── runtime-side accessors ───────────────────────────────────

    fn enabled(&self) -> bool {
        critical_section::with(|cs| self.inner.borrow(cs).borrow().enabled)
    }

    fn channel_count(&self) -> u8 {
        critical_section::with(|cs| self.inner.borrow(cs).borrow().channel_count)
    }

    fn nth_frequency(&self, idx: u8) -> Option<u32> {
        critical_section::with(|cs| {
            let s = self.inner.borrow(cs).borrow();
            if (idx as usize) < s.channel_count as usize {
                Some(s.frequencies[idx as usize])
            } else {
                None
            }
        })
    }

    fn write_rssi(&self, idx: u8, rssi: i16) {
        critical_section::with(|cs| {
            let mut s = self.inner.borrow(cs).borrow_mut();
            if (idx as usize) < s.channel_count as usize {
                s.results[idx as usize] = rssi;
            }
        });
    }

    fn note_pass_complete(&self) {
        critical_section::with(|cs| {
            let mut s = self.inner.borrow(cs).borrow_mut();
            s.completed_passes = s.completed_passes.wrapping_add(1);
        });
    }

    /// Async wait for any state-change (start / stop / channel-list
    /// replacement).  Used inside `run_rx`'s `select` so a UI scan
    /// request preempts the in-flight `rx_recv` immediately.
    async fn wait_change(&self) {
        self.state_change.wait().await;
    }
}

impl Default for ScanController {
    fn default() -> Self {
        Self::new()
    }
}

/// Drop an inter-arrival gap into the right histogram bucket.
/// Bucket edges (in ms): `<2`, `<12`, `<25`, `<50`, `<100`, `<250`,
/// `≥250`.  See the `RX prof:` log line in [`run_rx`] for how to
/// read the resulting numbers.
fn bucket_rx_gap(buckets: &mut [u32; 7], gap_ms: u64) {
    let idx = if gap_ms < 2 {
        0
    } else if gap_ms < 12 {
        1
    } else if gap_ms < 25 {
        2
    } else if gap_ms < 50 {
        3
    } else if gap_ms < 100 {
        4
    } else if gap_ms < 250 {
        5
    } else {
        6
    };
    buckets[idx] = buckets[idx].wrapping_add(1);
}

/// Sample one scan channel: retune, settle, take
/// [`SCAN_SAMPLES_PER_CHANNEL`] RSSI readings spaced
/// [`SCAN_SAMPLE_INTERVAL_MS`] apart, return the peak (least-
/// negative) reading.  Leaves the chip in `STDBY_RC` so the
/// caller can either retune to the next channel or reconfigure
/// for normal operation.
///
/// Returns [`SCAN_RSSI_NONE`] if any radio command failed — the
/// renderer treats that as "no measurement," which is more honest
/// than reporting a stale or fabricated value.
async fn scan_one_channel<Spi, Busy, Dio1, Reset, Switch>(
    radio: &mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>,
    freq_hz: u32,
) -> i16
where
    Spi: SpiDevice,
    Busy: Wait,
    Dio1: Wait,
    Reset: embedded_hal::digital::OutputPin,
    Switch: RfSwitchControl,
{
    if radio.set_frequency_fast(freq_hz).await.is_err() {
        return SCAN_RSSI_NONE;
    }
    if radio.rx_start().await.is_err() {
        return SCAN_RSSI_NONE;
    }
    Timer::after_millis(SCAN_SETTLE_MS).await;

    let mut peak = SCAN_RSSI_NONE;
    for _ in 0..SCAN_SAMPLES_PER_CHANNEL {
        if let Ok(r) = radio.get_rssi_inst().await {
            if r > peak {
                peak = r;
            }
        }
        Timer::after_millis(SCAN_SAMPLE_INTERVAL_MS).await;
    }

    let _ = radio.set_standby_rc().await;
    peak
}

// ── Live stats observable by the UI / telemetry ─────────────────────────────

/// Snapshot of link-runtime state that the UI (or any other consumer)
/// can read at any time.  Updated by `run_rx` on every accepted
/// packet, dropped packet, link transition, and stuck-recovery fire.
/// `run_tx` updates a smaller subset (primarily `total_sent` /
/// `heartbeats_sent`) — TX has no link-up notion since v1 has no ACK.
#[derive(Debug, Clone, Copy)]
pub struct LinkStats {
    /// Receiver watchdog says the peer is alive.  RX-side only.
    pub link_up: bool,
    /// RSSI (dBm) of the most recent accepted packet.  `None` until
    /// the first packet is received.  RX-side only.  `i16` to match
    /// what the SX1262 driver reports — practical values are -120 to
    /// -10 so an `i8` fits, but consumers can clamp at display time.
    pub last_rssi_dbm: Option<i16>,
    /// RSSI (dBm) of the most recent crc-ok packet caught by the
    /// *primary* radio (radio0), and by the *secondary* diversity
    /// radio (radio1), tracked separately so an antenna / front-end
    /// imbalance between the two shows up directly.  `None` until that
    /// radio has caught its first packet.  On a single-radio build
    /// `last_rssi_dbm_rx1` stays `None`.  RX-side only.
    pub last_rssi_dbm_rx0: Option<i16>,
    pub last_rssi_dbm_rx1: Option<i16>,
    /// Total accepted packets since boot (heartbeats + MIDI + sysex).
    /// RX-side.
    pub total_accepted: u32,
    /// Subset: accepted heartbeat packets.  RX-side.
    pub accepted_heartbeats: u32,
    /// Subset: accepted MIDI channel-voice packets.  RX-side.
    pub accepted_midi: u32,
    /// Packets that decoded but failed link-layer validation
    /// (key-fingerprint mismatch, replay window, etc).  RX-side.
    pub dropped: u32,
    /// Packets the radio reported with a CRC error.  RX-side.
    pub crc_mismatch: u32,
    /// Stuck-channel recoveries fired (heartbeat-state failsafe).
    /// RX-side.
    pub stuck_recoveries: u32,
    /// Packet loss (%) over the last 1-second window — `None` until
    /// at least two consecutive windows have been observed (the
    /// runtime needs a baseline `packet_seq` to compute against).
    /// Updated once per second by `run_rx`.  Computed from
    /// `(tx_packets - accepted_packets) / tx_packets`, so it
    /// includes any cause the radio missed a packet (CRC error,
    /// link-layer drop, decoder error).  RX-side.
    pub recent_loss_pct: Option<u8>,
    /// Total transmitted packets (heartbeats + MIDI).  TX-side.
    pub total_sent: u32,
    /// Subset: heartbeat packets sent.  TX-side.
    pub heartbeats_sent: u32,
}

impl LinkStats {
    pub const EMPTY: Self = Self {
        link_up: false,
        last_rssi_dbm: None,
        last_rssi_dbm_rx0: None,
        last_rssi_dbm_rx1: None,
        total_accepted: 0,
        accepted_heartbeats: 0,
        accepted_midi: 0,
        dropped: 0,
        crc_mismatch: 0,
        stuck_recoveries: 0,
        recent_loss_pct: None,
        total_sent: 0,
        heartbeats_sent: 0,
    };
}

impl Default for LinkStats {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// Cross-task shared cell for [`LinkStats`].  Single producer
/// (`run_rx` or `run_tx` running in one task) updates via
/// [`Self::update`]; any number of consumers read snapshots via
/// [`Self::get`].  `const fn new` so a profile can declare a `static
/// STATS: LinkStatsCell = LinkStatsCell::new();` and pass `&STATS` to
/// the runtime + the UI render path.
pub struct LinkStatsCell {
    inner: critical_section::Mutex<core::cell::Cell<LinkStats>>,
}

impl LinkStatsCell {
    pub const fn new() -> Self {
        Self {
            inner: critical_section::Mutex::new(core::cell::Cell::new(LinkStats::EMPTY)),
        }
    }

    /// Copy out the current snapshot.  Safe to call from any task /
    /// IRQ context — internally takes a critical section briefly.
    pub fn get(&self) -> LinkStats {
        critical_section::with(|cs| self.inner.borrow(cs).get())
    }

    /// Mutate the stored stats in place.  Closure-style so callers
    /// don't have to round-trip through `get` / `set` and risk a
    /// concurrent overwrite.
    pub fn update<F: FnOnce(&mut LinkStats)>(&self, f: F) {
        critical_section::with(|cs| {
            let cell = self.inner.borrow(cs);
            let mut s = cell.get();
            f(&mut s);
            cell.set(s);
        });
    }
}

impl Default for LinkStatsCell {
    fn default() -> Self {
        Self::new()
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

    /// Discard any input that accumulated before the link came up.
    ///
    /// `run_tx` calls this once, after the radio is configured but
    /// before the first packet ships.  A keyboardist mashing keys while
    /// the TX board boots piles up NoteOns whose matching NoteOffs were
    /// sent before the source was listening (or arrive only after the
    /// key is released post-boot) — forwarding those strands notes on
    /// the RX synth.  Dropping the boot-window input avoids it without
    /// the RX side ever emitting a speculative all-notes-off (which
    /// would disturb a second player on a split/other channel).
    ///
    /// Default is a no-op: synthetic sources have no stale backlog.
    async fn drain_startup(&mut self) {}
}

pub trait MidiSink {
    type Error;
    async fn write_message(&mut self, bytes: &[u8]) -> Result<(), Self::Error>;
    /// Emit "all notes off" on every channel.  Called by [`run_rx`] on
    /// watchdog expiry.
    async fn all_notes_off(&mut self) -> Result<(), Self::Error>;
}

// ── Radio configuration shared by both ends ─────────────────────────────────

/// Apply a `LinkConfig` to the radio.  Idempotent — safe to call
/// again to update RF parameters at runtime (the chip enters and leaves
/// standby per `set_*` call).  Caller must ensure no `radio.tx()` /
/// `radio.rx_recv()` is in flight while this runs.
pub async fn configure_radio<Spi, Busy, Dio1, Reset, Switch>(
    radio: &mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>,
    config: &LinkConfig,
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
    // Enable RX boosted mode — ~3 dB sensitivity gain at the cost
    // of ~0.9 mA extra in continuous-RX supply current.  For this
    // project the receiver is always-on while listening, so the
    // sensitivity-vs-battery trade is dominated by sensitivity.
    // Boost survives standby transitions but is wiped on `SLEEP`;
    // we never enter SLEEP, so applying it once here is sufficient.
    radio.set_rx_boosted(true).await?;
    radio.finish_init().await?;
    Ok(())
}

// ── Low-battery shutdown helpers ───────────────────────────────────────────

/// Park the radio in standby_rc, blink a recognisable LED pattern, then
/// idle forever.  Called from both `run_tx` and `run_rx` when the
/// profile fires its [`ShutdownSignal`].  Returns `!` so the calling
/// `select` arm can be treated as terminal — the only way out is reset.
async fn handle_tx_shutdown<Spi, Busy, Dio1, Reset, Switch, Led>(
    radio: &mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>,
    led: &mut Led,
) -> !
where
    Spi: SpiDevice,
    Busy: Wait,
    Dio1: Wait,
    Reset: embedded_hal::digital::OutputPin,
    Switch: RfSwitchControl,
    Led: StatefulOutputPin,
{
    defmt::warn!("link TX: shutdown signal received — parking radio");
    let _ = radio.set_standby_rc().await;
    for _ in 0..6 {
        let _ = led.toggle();
        Timer::after_millis(120).await;
    }
    let _ = led.set_low();
    // After the visual confirmation drop the chip the rest of the way:
    // SLEEP collapses STDBY_RC's ~600 µA quiescent down to ~160 nA.
    // Config is lost on SLEEP wake-up, but the only path out of here
    // is a full chip reset (deep soft-off → System OFF wake → boot →
    // `configure_radio` again), so the loss is harmless.
    let _ = radio.set_sleep().await;
    loop {
        Timer::after_secs(60).await;
    }
}

/// RX-side counterpart to [`handle_tx_shutdown`].  Adds a final
/// `sink.all_notes_off()` so a connected synth doesn't hang notes
/// when the receiver goes dark.
async fn handle_rx_shutdown<Spi, Busy, Dio1, Reset, Switch, Led, Sink>(
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
    defmt::warn!("link RX: shutdown signal received — silencing sink + parking radio");
    let _ = sink.all_notes_off().await;
    let _ = radio.set_standby_rc().await;
    for _ in 0..6 {
        let _ = led.toggle();
        Timer::after_millis(120).await;
    }
    let _ = led.set_low();
    // SLEEP after the blink — matches the TX handler.  See
    // `handle_tx_shutdown` for the rationale.
    let _ = radio.set_sleep().await;
    loop {
        Timer::after_secs(60).await;
    }
}

// ── TX loop ─────────────────────────────────────────────────────────────────

/// Apply a new `LinkConfig` to the radio mid-flight (TX side):
/// re-run [`configure_radio`] (which walks the chip back through
/// `init` → set_*) and update the heartbeat-timer cadence if it
/// changed.  No RX-restart needed — `run_tx` doesn't keep the radio
/// in continuous-receive mode.
///
/// On reconfigure failure the runtime logs and keeps the previous
/// config — better than wedging into a halt-on-failure loop, since
/// a misconfigured tune attempt should be recoverable on the next
/// update.
async fn apply_tx_reconfig<Spi, Busy, Dio1, Reset, Switch>(
    radio: &mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>,
    current: &mut LinkConfig,
    new_cfg: &LinkConfig,
    hb: &mut HeartbeatTimer,
) where
    Spi: SpiDevice,
    Busy: Wait,
    Dio1: Wait,
    Reset: embedded_hal::digital::OutputPin,
    Switch: RfSwitchControl,
{
    if configure_radio(radio, new_cfg).await.is_err() {
        defmt::error!("link TX: live reconfigure failed; keeping previous config");
        return;
    }
    if new_cfg.heartbeat_ms != current.heartbeat_ms {
        *hb = HeartbeatTimer::new(Duration::from_millis(new_cfg.heartbeat_ms));
    }
    *current = *new_cfg;
    defmt::info!(
        "link TX: live reconfigure → {} Hz / {} bps / +{} dBm",
        current.frequency_hz,
        current.bitrate_bps,
        current.tx_power_dbm,
    );
}

/// Run the TX side: consume MIDI messages from `source`, queue them with
/// status-aware dedup + per-event seq, transmit packets via the credit-
/// based round-robin queue.  When the queue is empty for
/// `config.heartbeat_ms`, send a `Heartbeat` instead so the receiver's
/// watchdog stays fed.
///
/// `config_updates` (optional): a [`LinkConfigSignal`] for live
/// reconfig.  When `Some`, the loop polls it at each iteration and
/// includes its `wait()` in the queue-empty `select`, so a UI-driven
/// channel/power change applies between packets without a restart.
/// Profiles with no UI pass `None` and the polling path is a single
/// `Option::is_some` check per loop iteration.
///
/// `scan` (optional): a [`ScanController`] for UI-driven channel
/// scanning on the TX side.  Same shape as RX — when enabled the
/// runtime puts the chip in standby and walks the controller's
/// frequency list, sampling `get_rssi_inst` per channel.  TX is
/// silent during a scan (no MIDI, no heartbeats) so the receiver's
/// watchdog will fire and the link drops; on scan exit the operating
/// `LinkConfig` is re-applied and TX resumes — receiver session
/// resets on the next packet.  Source events that arrive during the
/// scan stay in the UART's hardware buffer and drain naturally
/// once TX resumes (subject to the buffer's depth).
#[allow(clippy::too_many_arguments)] // Top-level orchestrator: HAL handles + channels are inherent.
pub async fn run_tx<Spi, Busy, Dio1, Reset, Switch, Led, Source>(
    radio: &mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>,
    led: &mut Led,
    source: &mut Source,
    boot_counter: u16,
    config: &LinkConfig,
    stats: &LinkStatsCell,
    config_updates: Option<&LinkConfigSignal>,
    scan: Option<&ScanController>,
    shutdown: Option<&ShutdownSignal>,
    aead: Option<AeadContext>,
    aead_updates: Option<&'static AeadUpdateSignal>,
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
    let mut current = *config;
    if configure_radio(radio, &current).await.is_err() {
        defmt::error!("link TX: radio configure failed; halting");
        loop {
            Timer::after_millis(1000).await;
        }
    }
    defmt::info!(
        "link TX: {} Hz / {} bps GFSK / +{} dBm, boot_counter={}",
        current.frequency_hz,
        current.bitrate_bps,
        current.tx_power_dbm,
        boot_counter
    );

    let mut sender = match aead {
        Some(ctx) => LinkSender::with_aead(boot_counter, ctx),
        None => LinkSender::no_crypto(boot_counter),
    };
    // The plaintext budget that `pop_packet` can pack into a packet
    // body — `MAX_BODY_LEN` in Open mode, `MAX_BODY_LEN_AEAD` (= 37)
    // with AEAD active.  Caps how much `pop_packet` may consume from
    // the queue per call so the cipher's tag can fit in the wire
    // packet; otherwise `encode()` rejects the body and the events
    // it had already taken from the queue are silently lost.
    let body_budget = if aead.is_some() {
        osrf_link::MAX_BODY_LEN_AEAD
    } else {
        MAX_BODY_LEN
    };
    let mut hb = HeartbeatTimer::new(Duration::from_millis(current.heartbeat_ms));
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

    // Scan-mode tracking — same semantics as the RX side.  When the
    // controller's `enabled` flips on, we walk the chip into standby
    // and start sweeping; flips off, we re-`configure_radio` and
    // resume the normal TX path.
    let mut scanning = false;
    let mut scan_idx: u8 = 0;

    // Throw away any MIDI that arrived while the radio was initialising.
    // If someone is mashing keys as the TX board powers up, the NoteOns
    // pile up in the source's buffers but their NoteOffs may never have
    // been captured — forwarding them strands notes on the RX synth.
    // We discard rather than emit a speculative all-notes-off so a
    // second player on a split / another channel is never disturbed.
    source.drain_startup().await;

    loop {
        let now = Instant::now();

        // Apply any pending live AEAD update from the UI.  Latest-wins;
        // poll at loop-top so a key change that landed while we were
        // blocked in `select` gets applied before the next packet
        // ships.  Logged once per change so the RTT trail shows
        // when the operator toggled.
        if let Some(sig) = aead_updates {
            if let Some(update) = sig.try_take() {
                sender.set_aead(update.aead);
                if update.aead.is_some() {
                    let fp = sender.key_fp();
                    defmt::info!(
                        "link TX: key changed → AEAD on (key_fp={=u8:02x}{=u8:02x}{=u8:02x})",
                        fp[2],
                        fp[1],
                        fp[0],
                    );
                } else {
                    defmt::info!("link TX: key changed → Open (plaintext)");
                }
            }
        }

        // Reconcile scan mode.  Single point where the chip transitions
        // between TX/heartbeat duty and channel-sweep duty.
        let scan_wanted = scan.is_some_and(|s| s.enabled());
        match (scanning, scan_wanted) {
            (false, true) => {
                if radio.set_standby_rc().await.is_err() {
                    defmt::error!("link TX: set_standby_rc failed entering scan");
                }
                // Scan reuses the operating IF bandwidth so the
                // user sees what an actual link on each channel
                // would experience — the TX signal occupies
                // ~400 kHz, so a wide IF picking up adjacent-
                // channel energy *is* the safety picture for
                // dense plans.
                // Drop any pre-scan in-flight state so we don't flush
                // it onto the receiver's fresh post-scan session:
                //   - Pending MIDI events (chord copies, delayed
                //     NoteOff retransmits) get discarded.
                //   - `tx_state` is reset so the first heartbeat
                //     after scan exit advertises an empty active-
                //     channel mask, matching the RX side's post-
                //     watchdog cleared state and avoiding a phantom
                //     stuck-note-recovery cycle on resume.
                queue = MidiTxQueue::new();
                tx_state = ChannelNoteCounts::new();
                scan_idx = 0;
                scanning = true;
                defmt::info!(
                    "link TX: scan mode ON (channels={})",
                    scan.map_or(0, |s| s.channel_count())
                );
            }
            (true, false) => {
                let _ = configure_radio(radio, &current).await;
                // HeartbeatTimer carries a "last send" timestamp; reset
                // it so the resume burst doesn't immediately fire a
                // heartbeat on top of any queued events.
                hb = HeartbeatTimer::new(Duration::from_millis(current.heartbeat_ms));
                scanning = false;
                defmt::info!(
                    "link TX: scan mode OFF, resuming on {} Hz",
                    current.frequency_hz
                );
            }
            _ => {}
        }

        // 0. Apply any pending live-config update before the next
        //    blocking await.  Catches updates that landed during the
        //    prior burst (between `radio.tx()` calls) when there's no
        //    `select` arm watching the signal.  Skipped during scan —
        //    only the local `current` is updated so the right config
        //    is restored on scan exit.
        if let Some(sig) = config_updates {
            if let Some(new_cfg) = sig.try_take() {
                if scanning {
                    current = new_cfg;
                    defmt::info!(
                        "link TX: deferred reconfigure (scanning); will apply on scan exit"
                    );
                } else {
                    apply_tx_reconfig(radio, &mut current, &new_cfg, &mut hb).await;
                }
            }
        }

        // ── Scan mode: sample one channel, advance, loop ─────────
        if scanning {
            // Drain and discard any MIDI bytes that arrived during
            // the scan.  Premise: the user isn't playing while
            // scanning (it's a setup-time activity) — and the
            // alternative, queueing events for a post-scan flush,
            // dumps a chord onto a fresh RX session that has
            // already watchdog'd and cleared its pressed-notes
            // state, producing audible artifacts.  Pulling bytes
            // out of the source also stops its hardware UART RX
            // buffer from filling and back-pressuring the
            // FeatherWing on long sweeps.
            while let Ok(Some(_)) = source.try_next(&mut midi_buf) {}

            let s = scan.unwrap();
            let count = s.channel_count();
            if count == 0 {
                let _ = select(Timer::after_millis(50), s.wait_change()).await;
                continue;
            }
            let i = scan_idx % count;
            if let Some(freq) = s.nth_frequency(i) {
                let rssi = scan_one_channel(radio, freq).await;
                s.write_rssi(i, rssi);
            }
            scan_idx = scan_idx.wrapping_add(1);
            if scan_idx.is_multiple_of(count) {
                s.note_pass_complete();
            }
            continue;
        }

        // 1. Drain any source events into the queue (non-blocking).
        //    `try_next` is sync and safe to call repeatedly; each event
        //    that's "due" right now goes into the queue with status-aware
        //    dedup + a fresh event_seq.  NoteOff pushes also queue
        //    delayed retransmit copies based on `now`.
        loop {
            match source.try_next(&mut midi_buf) {
                Ok(Some(n)) => {
                    let msg = &midi_buf[..n];
                    // Always reflect the operator's intent in
                    // `tx_state` — even when the queue refuses the
                    // event.  If we skip the observe on overflow,
                    // `tx_state` keeps a NoteOff'd key marked as
                    // pressed, the heartbeat mask lies, and RX's
                    // divergence-based stuck-note recovery never
                    // fires — note rings until the next watchdog
                    // wipes everything via all-notes-off.  Observing
                    // unconditionally means `tx_state` agrees with
                    // the controller; RX recovery now does its job
                    // for queue-dropped NoteOffs in ~20 ms instead of
                    // hanging the note out for the full watchdog
                    // window.  No new wire traffic on the happy
                    // path; same code path on the overflow path.
                    tx_state.observe(msg);
                    if !queue.push_channel_voice(msg, now) {
                        overflow_count = overflow_count.wrapping_add(1);
                        defmt::error!(
                            "link TX: queue overflow! dropping (overflows={})",
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
        if let Some(PoppedPacket { kind, body_len }) =
            queue.pop_packet(now, &mut body_buf[..body_budget])
        {
            let event_type = match kind {
                QueueKind::ChannelVoice => EventType::ChannelVoice,
                QueueKind::SysExFragment => EventType::SysExFragment,
            };
            match sender.encode(event_type, &body_buf[..body_len], &mut wire_buf) {
                Ok(wire_n) => {
                    if radio.tx(&wire_buf[..wire_n]).await.is_err() {
                        defmt::error!("link TX: radio.tx() failed (wire_n={})", wire_n);
                    }
                }
                Err(e) => defmt::error!(
                    "link TX: encode failed (body_len={}, err={:?})",
                    body_len,
                    e
                ),
            }
            hb.note_send();
            let _ = led.toggle();
            continue;
        }

        // 3. Queue empty — wait for source-ready OR heartbeat deadline,
        //    *or* a config update / scan request if either is wired
        //    in.  Adding the update arms here means a UI-driven
        //    transition at idle applies immediately rather than after
        //    the next heartbeat fires.
        let cfg_wait = async {
            match config_updates {
                Some(s) => s.wait().await,
                None => core::future::pending::<LinkConfig>().await,
            }
        };
        let scan_wait = async {
            match scan {
                Some(s) => s.wait_change().await,
                None => core::future::pending::<()>().await,
            }
        };
        let shutdown_wait = async {
            match shutdown {
                Some(s) => s.wait().await,
                None => core::future::pending::<()>().await,
            }
        };
        match select5(
            source.wait_ready(),
            hb.wait(),
            cfg_wait,
            scan_wait,
            shutdown_wait,
        )
        .await
        {
            Either5::First(()) => {
                // Source has an event ready; loop to drain.
                continue;
            }
            Either5::Second(()) => {
                // Heartbeat fired — fall through to send one.
            }
            Either5::Third(new_cfg) => {
                apply_tx_reconfig(radio, &mut current, &new_cfg, &mut hb).await;
                continue;
            }
            Either5::Fourth(()) => {
                // Scan-controller state change — top-of-loop reconcile
                // picks it up.
                continue;
            }
            Either5::Fifth(()) => {
                handle_tx_shutdown(radio, led).await;
            }
        }

        // Send heartbeat (single copy — next one is ≤ heartbeat_ms
        // away).  The body is a 2-byte big-endian active-channel mask
        // — the receiver uses it to detect channels with stuck notes
        // and fire CC 123 (All Notes Off) for any that need recovery.
        let mask_body = tx_state.active_mask().to_be_bytes();
        match sender.encode(EventType::Heartbeat, &mask_body, &mut wire_buf) {
            Ok(wire_n) => {
                if radio.tx(&wire_buf[..wire_n]).await.is_err() {
                    defmt::error!("link TX: radio.tx() failed");
                }
            }
            Err(_) => defmt::error!("link TX: encode failed"),
        }
        hb.note_send();
        let _ = led.toggle();
        hb_count = hb_count.wrapping_add(1);

        if tx_count.wrapping_add(hb_count).is_multiple_of(500) {
            defmt::info!(
                "link TX: midi_events={} heartbeats={} queue_depth={} overflows={}",
                tx_count,
                hb_count,
                queue.len(),
                overflow_count
            );
        }

        // Push counters to the shared cell so consumers (UI / telemetry)
        // can render them.  TX has no link-up signal in v1 (no ACK
        // channel), so RX-side fields stay at their defaults.
        stats.update(|s| {
            s.total_sent = tx_count.wrapping_add(hb_count);
            s.heartbeats_sent = hb_count;
        });
    }
}

// ── RX loop ─────────────────────────────────────────────────────────────────

/// One observable RX event ready to be delivered to the sink.  We buffer
/// these and drain them after the call so the async sink can be
/// awaited without holding the receiver borrow.
///
/// We keep MIDI and SysEx buffers separate — earlier this was a
/// single `Vec<BufferedEvent, 32>` enum, but Rust sized every slot
/// to the largest variant (`SysEx`, 1572 B) and the buffer alone
/// burned ~50 KB of RAM for a vec that almost always holds 1-5
/// small MIDI events.  Per-iteration we only ever produce events
/// of *one* kind (a packet body is either ChannelVoice OR
/// SysExFragment; recovery only fires from Heartbeat → MIDI
/// NoteOffs), so the two vecs never carry events that need
/// preserving in interleave order.
type MidiBuf = heapless::Vec<u8, 8>;
type SysExBuf = heapless::Vec<u8, { osrf_link::MAX_SYSEX_BYTES }>;
/// Max queued MIDI events per RX loop iteration.  Has to cover the
/// stuck-note-recovery worst case: a sustain-heavy passage across
/// multiple MIDI channels where every channel diverges
/// simultaneously and each has many pressed notes.  Realistic max
/// is ~60-100 events (sustain pedal + chord-heavy playing on 2-3
/// layered channels); 128 leaves comfortable margin.  Cost is
/// 128 × 12 B ≈ 1.5 KB BSS, well worth the robustness — overflow
/// here would mean silently-dropped NoteOffs and permanently stuck
/// notes on the synth side.  Recovery loop also handles overflow
/// defensively (see `try_recovery_for_channel` below) so even at
/// max capacity an over-budget recovery is split across heartbeats
/// rather than lost.
const MIDI_EVENTS_CAPACITY: usize = 128;
/// Max queued SysEx events per iteration.  One packet body
/// produces at most one SysEx completion event; sized to
/// `MAX_CONCURRENT_SYSEX` so a future protocol revision that
/// allows interleaved bodies wouldn't need to revisit this.
const SYSEX_EVENTS_CAPACITY: usize = osrf_link::MAX_CONCURRENT_SYSEX;

/// Apply a new `LinkConfig` to the radio mid-flight (RX side):
/// re-`configure_radio` and re-`rx_start`, then reset the watchdog
/// to the new ms.  Cancels any in-flight `rx_recv` (caller must
/// have already lost the race in `select4`).
///
/// On hard failure the runtime logs and keeps listening on the
/// previous config — same recoverability stance as TX.
#[allow(clippy::too_many_arguments)] // Mutable cursors threaded through for reset on reconfig.
async fn apply_rx_reconfig<Spi, Busy, Dio1, Reset, Switch>(
    radio: &mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>,
    current: &mut LinkConfig,
    new_cfg: &LinkConfig,
    wd: &mut WatchdogTimer,
    receiver: &mut LinkReceiver,
    rx_state: &mut PressedNotes,
    divergence_since: &mut [Option<Instant>; 16],
    link_up: &mut bool,
) where
    Spi: SpiDevice,
    Busy: Wait,
    Dio1: Wait,
    Reset: embedded_hal::digital::OutputPin,
    Switch: RfSwitchControl,
{
    if configure_radio(radio, new_cfg).await.is_err() {
        defmt::error!("link RX: live reconfigure failed; keeping previous config");
        // Try to resume RX on the *previous* config so we don't
        // get stuck in standby after a partially-applied set_*.
        let _ = radio.rx_start().await;
        return;
    }
    if radio.rx_start().await.is_err() {
        defmt::error!("link RX: rx_start failed after reconfigure");
        return;
    }
    *wd = WatchdogTimer::new(Duration::from_millis(new_cfg.watchdog_ms));
    // The new config implies a new peer (or at least new RF
    // parameters) — anything we knew about the prior session's
    // packet_seq / pressed-notes is no longer trustworthy.  Force
    // a session-reset on the next received packet and clear local
    // pressed-state so a stale mask doesn't trigger recovery.
    receiver.mark_link_down();
    *link_up = false;
    rx_state.reset();
    *divergence_since = [None; 16];
    *current = *new_cfg;
    defmt::info!(
        "link RX: live reconfigure → {} Hz / {} bps / watchdog={}ms",
        current.frequency_hz,
        current.bitrate_bps,
        current.watchdog_ms,
    );
}

/// Drain loop for the **secondary (diversity) radio**, run as its own task.
///
/// This is the heart of the storm-free diversity design: the secondary
/// radio's `rx_recv` runs to completion in a dedicated task and is **never
/// cancelled** during steady-state, so its DIO1 IRQ is always cleared.
/// (The earlier race-and-cancel design left the loser radio's level-
/// sensitive DIO1 stuck high, which storms embassy-nrf's shared GPIOTE PORT
/// interrupt and wrecks the primary's reception — exactly the 50%-loss +
/// `unexpected IRQ 0x0000` failure.)
///
/// Each crc-ok packet is pushed to the primary loop via `tx.try_send`
/// (dropping on a full channel rather than blocking — the drain must keep
/// running no matter what the consumer is doing).  The primary's
/// `LinkReceiver` replay window dedups the copies the primary also caught.
///
/// `cfg_updates` is a signal the **primary** forwards live `LinkConfig`
/// changes through, so the secondary retunes to the same channel.  The only
/// `rx_recv` cancellation happens on such a change, and it's immediately
/// followed by `configure_radio` (a full reset), which clears any
/// transiently-stuck state — so no sustained storm.
pub async fn run_rx_secondary<Spi, Busy, Dio1, Reset, Switch>(
    radio: &mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>,
    config: &LinkConfig,
    cfg_updates: Option<&'static LinkConfigSignal>,
    tx: DiversityRxSender,
) -> !
where
    Spi: SpiDevice,
    Busy: Wait,
    Dio1: Wait,
    Reset: embedded_hal::digital::OutputPin,
    Switch: RfSwitchControl,
{
    if configure_radio(radio, config).await.is_err() || radio.rx_start().await.is_err() {
        defmt::error!("diversity RX (secondary): init failed; this radio will not contribute");
        loop {
            Timer::after_millis(1000).await;
        }
    }
    // SPI presence probe — same honesty as the primary's init.
    match radio.verify_present().await {
        Ok(true) => defmt::info!("diversity RX (secondary): present + in RX"),
        Ok(false) => {
            defmt::error!("diversity RX (secondary): NOT responding on SPI — check wiring/power")
        }
        Err(_) => defmt::error!("diversity RX (secondary): SPI error during presence check"),
    }
    let mut buf = [0u8; RF_PAYLOAD_MAX as usize];
    loop {
        let cfg_wait = async {
            match cfg_updates {
                Some(s) => s.wait().await,
                None => core::future::pending::<LinkConfig>().await,
            }
        };
        match select(radio.rx_recv(&mut buf), cfg_wait).await {
            Either::First(Ok(pkt)) if pkt.crc_ok => {
                let len = pkt.len.min(buf.len());
                let mut frame = DiversityRxFrame {
                    buf: [0u8; RF_PAYLOAD_MAX as usize],
                    len,
                    rssi_dbm: pkt.rssi_dbm,
                };
                frame.buf[..len].copy_from_slice(&buf[..len]);
                // Drop on full — never block the drain loop (a blocked drain
                // would stop servicing DIO1 → stuck IRQ → the storm we're
                // avoiding).  A dropped diversity copy is harmless: the
                // primary likely caught it too, or it's one missed packet.
                let _ = tx.try_send(frame);
            }
            // crc-fail / radio error: rx_recv completed (IRQ cleared); just
            // re-arm on the next loop.  Not forwarded — the primary counts
            // its own crc/error stats.
            Either::First(_) => {}
            Either::Second(new_cfg) => {
                if configure_radio(radio, &new_cfg).await.is_err()
                    || radio.rx_start().await.is_err()
                {
                    defmt::error!("diversity RX (secondary): reconfigure failed; off-channel");
                }
            }
        }
    }
}

/// Run the RX side: receive packets, dedup at packet + event level,
/// reassemble SysEx, hand each surviving event to the sink.  On
/// watchdog expiry call `sink.all_notes_off` and mark the receiver as
/// link-down so the next packet triggers a session reset.
///
/// `config_updates` (optional): see [`run_tx`] — same semantics.
///
/// `scan` (optional): a [`ScanController`] for UI-driven channel-scan
/// mode.  When `Some` and the controller is `enabled`, the runtime
/// puts the chip in standby and walks its frequency list, sampling
/// `get_rssi_inst` per channel and writing back results.  `None`
/// (or "not enabled") keeps the chip in continuous RX as before.
#[allow(clippy::too_many_arguments)] // Top-level orchestrator: HAL handles + channels are inherent.
async fn run_rx_inner<Spi, Busy, Dio1, Reset, Switch, Led, Sink>(
    radio: &mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>,
    // Optional diversity (secondary) radio's handoff channel.  When `Some`,
    // a separately-spawned [`run_rx_secondary`] task drains the second
    // SX1262 and pushes crc-ok frames here; this loop consumes them
    // alongside the primary's own receives and feeds both into the shared
    // `LinkReceiver` (replay window dedups the duplicate copies).  The
    // secondary lives in its own task — never cancelled — so its DIO1 IRQ
    // is always cleared (avoids the GPIOTE-PORT spurious-wake storm that
    // racing two `rx_recv` futures in one task caused).  When `None`, this
    // is the verified single-radio path: the second select future is
    // `pending()` and behaviour is byte-for-byte unchanged.
    secondary: Option<DiversityRxReceiver>,
    // When `Some`, live `LinkConfig` reconfigures are forwarded here so the
    // secondary radio's task retunes to the same channel.  Paired with
    // `secondary`; `None` on the single-radio path.
    secondary_cfg: Option<&'static LinkConfigSignal>,
    led: &mut Led,
    sink: &mut Sink,
    config: &LinkConfig,
    stats: &LinkStatsCell,
    config_updates: Option<&LinkConfigSignal>,
    scan: Option<&ScanController>,
    shutdown: Option<&ShutdownSignal>,
    aead: Option<AeadContext>,
    allow_open: bool,
    aead_updates: Option<&'static AeadUpdateSignal>,
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
    let mut current = *config;
    if configure_radio(radio, &current).await.is_err() {
        defmt::error!("link RX: radio configure failed; halting");
        loop {
            Timer::after_millis(1000).await;
        }
    }
    // Presence check on the primary — non-fatal (a dead primary just means
    // a dead link, which the stats make obvious) but logged so a wiring /
    // power fault reads as one explicit line instead of silent zeros.
    match radio.verify_present().await {
        Ok(true) => defmt::info!("link RX: primary radio responding on SPI"),
        Ok(false) => {
            defmt::error!("link RX: primary radio NOT responding on SPI — check NSS/SCK/MOSI/MISO/BUSY wiring + 3V3")
        }
        Err(_) => defmt::error!("link RX: primary radio SPI error during presence check"),
    }
    if radio.rx_start().await.is_err() {
        defmt::error!("link RX: rx_start failed; halting");
        loop {
            Timer::after_millis(1000).await;
        }
    }
    // The diversity (secondary) radio, if any, is brought up and drained by
    // its own `run_rx_secondary` task (spawned by the profile) — see the
    // `secondary` channel arm below.  Nothing to init here.
    if secondary.is_some() {
        defmt::info!("link RX: diversity enabled (consuming secondary radio channel)");
    }
    defmt::info!(
        "link RX: listening on {} Hz / {} bps GFSK, watchdog={}ms",
        current.frequency_hz,
        current.bitrate_bps,
        current.watchdog_ms
    );

    let mut receiver = match aead {
        Some(ctx) => LinkReceiver::with_aead(ctx, allow_open),
        None => LinkReceiver::no_crypto(),
    };
    let mut wd = WatchdogTimer::new(Duration::from_millis(current.watchdog_ms));
    let mut radio_buf = [0u8; RF_PAYLOAD_MAX as usize];
    // Tracks the last `key_fp` we observed on a successfully-decoded
    // packet so we log once on each Open↔AEAD transition rather than
    // on every packet.  `None` = haven't heard anything yet; the
    // first accepted packet always logs.
    let mut last_logged_key_fp: Option<osrf_link::KeyFp> = None;
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
    let mut midi_events: heapless::Vec<MidiBuf, MIDI_EVENTS_CAPACITY> = heapless::Vec::new();
    let mut sysex_events: heapless::Vec<SysExBuf, SYSEX_EVENTS_CAPACITY> = heapless::Vec::new();
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
    // RSSI of the most recent accepted packet, exposed via `stats`.
    let mut last_rssi: Option<i16> = None;
    // Same, split per radio (diversity diagnostics): most recent crc-ok
    // RSSI from the primary (radio0) and secondary (radio1) radios.
    // Sticky — pair with the per-window `rx0`/`rx1` counts to tell
    // whether the reading is current or stale for that radio.
    let mut last_rssi_rx0: Option<i16> = None;
    let mut last_rssi_rx1: Option<i16> = None;

    // ── Per-radio receive attribution (diversity diagnostics) ────
    // Counts crc-ok packets *delivered* by each radio — i.e. the
    // `select` winner that actually reached processing (the loser's
    // duplicate copy is discarded inside `recv_any` and never counted).
    // On the single-radio path `rx1_caught` stays 0.  With a diversity
    // radio present, watching r0 vs r1 across an antenna-removal test
    // proves which radio is carrying the link: unscrew radio0's antenna
    // and its per-window count should fall toward 0 while radio1's
    // count picks up the slack.
    let mut rx0_caught: u32 = 0;
    let mut rx1_caught: u32 = 0;
    let mut prev_rx0: u32 = 0;
    let mut prev_rx1: u32 = 0;
    // Loss % over the most recent 1-second window — computed inside
    // the periodic-stats block below and exposed via `stats`.  None
    // until we've seen two consecutive windows.
    let mut last_loss_pct: Option<u8> = None;

    // ── RX profile counters ──────────────────────────────────────
    //
    // Goal: distinguish the *flavour* of packet loss when it
    // happens.  A CRC error is a different beast from an
    // `UnexpectedIrq` (chip in a state we didn't expect — usually
    // a sign that an IRQ got serviced late and we're reading a
    // stale status), and both are different from an inter-packet
    // gap that's much longer than the configured TX cadence
    // (preemption / lost-but-no-error).
    //
    // Per-error-variant counts plus an inter-arrival-gap
    // histogram are dumped alongside the existing 1 s stats line.
    let mut err_crc_mismatch: u32 = 0;
    let mut err_unexpected_irq: u32 = 0;
    let mut err_spi: u32 = 0;
    let mut err_bus: u32 = 0;
    let mut err_other: u32 = 0;
    // Last `Either4::First(Ok(_))` arrival — used to compute the
    // inter-arrival gap.  Initialised to `now` at boot so the very
    // first received packet doesn't show as a 100 s gap.
    let mut last_rx_at = Instant::now();
    // Histogram buckets for inter-arrival gaps (in ms).  Bucket
    // edges chosen for visibility into the regimes we care about:
    //   < 2 ms      : burst / back-to-back packets
    //   < 12 ms     : one heartbeat interval (default 10 ms + slack)
    //   < 25 ms     : ~2 heartbeats — first sign of trouble
    //   < 50 ms     : noticeable jitter
    //   < 100 ms    : significant SD-style preemption
    //   < 250 ms    : RX-FIFO-overrun territory
    //   ≥ 250 ms    : link is effectively interrupted
    let mut rx_gap_buckets: [u32; 7] = [0; 7];

    // Local mirror of `scan.enabled()`.  Tracks whether *we* (the
    // runtime) currently have the chip in scan mode (standby +
    // per-channel sweep) versus normal continuous RX.  Reconciled
    // at the top of every loop iteration against the controller's
    // public flag — when they disagree, we walk the chip through
    // the appropriate transition.
    let mut scanning = false;
    // Index into the scan controller's frequency list.  Wraps at
    // `channel_count`; on each wrap we bump the controller's
    // `completed_passes` so UI consumers know when a full sweep
    // has concluded.
    let mut scan_idx: u8 = 0;

    loop {
        // Clear both event buffers at the top of every iteration —
        // we never want events from a prior packet (e.g. yesterday's
        // stuck-note recovery NoteOffs) to replay.
        midi_events.clear();
        sysex_events.clear();
        debug_assert!(midi_events.is_empty() && sysex_events.is_empty());

        // Apply any pending live AEAD update from the UI.  Same
        // latest-wins try_take pattern as the TX side.  The receiver's
        // `set_aead` resets the link-down flag so the next accepted
        // packet drives a clean session reset under the new key.
        if let Some(sig) = aead_updates {
            if let Some(update) = sig.try_take() {
                receiver.set_aead(update.aead, update.allow_open);
                last_logged_key_fp = None;
                match (update.aead.is_some(), update.allow_open) {
                    (true, true) => defmt::info!(
                        "link RX: key changed → auto (key + plaintext both accepted)",
                    ),
                    (true, false) => defmt::info!(
                        "link RX: key changed → strict (specific key only, plaintext rejected)",
                    ),
                    (false, true) => defmt::info!("link RX: key changed → Open (plaintext only)"),
                    (false, false) => defmt::info!(
                        "link RX: key changed → refusing all (no key configured, plaintext rejected)",
                    ),
                }
            }
        }

        // Reconcile our local `scanning` flag against the controller's
        // public `enabled`.  This is the single point where we walk the
        // chip between "continuous RX on operating channel" and
        // "standby + per-channel sweep" — keeps the transition logic in
        // one place rather than scattered across event arms.
        let scan_wanted = scan.is_some_and(|s| s.enabled());
        match (scanning, scan_wanted) {
            (false, true) => {
                // Normal → Scanning: leave continuous RX, drop into
                // standby so the upcoming `set_frequency_fast` calls
                // take effect.  Walk-back of `link_up`/`receiver` is
                // deferred until we exit (the receiver may stay up if
                // the user pops back to the same channel quickly).
                if radio.set_standby_rc().await.is_err() {
                    defmt::error!("link RX: set_standby_rc failed entering scan");
                }
                // The secondary radio stays on the operating channel in its
                // own task during scan; we just stop draining its channel
                // (the `continue` below skips the consume), so its frames
                // age out via the bounded channel's drop-on-full.  Scan
                // doesn't change the operating frequency, so it's still
                // correctly tuned when we resume.
                // Scan keeps the operating IF bandwidth — we want
                // each channel's RSSI to reflect what a real link
                // there would see, including the TX's full ~400 kHz
                // occupied bandwidth bleeding into neighbours.
                scan_idx = 0;
                scanning = true;
                defmt::info!(
                    "link RX: scan mode ON (channels={})",
                    scan.map_or(0, |s| s.channel_count())
                );
            }
            (true, false) => {
                // Scanning → Normal: re-apply the operating LinkConfig
                // (which puts the chip back in standby with the operating
                // RF parameters) and resume continuous RX.  The link
                // looks fresh from RX's side: clear pressed-notes,
                // divergence timers, link-up flag — the next packet
                // from TX will trigger a full session reset.
                let _ = configure_radio(radio, &current).await;
                let _ = radio.rx_start().await;
                // Operating channel is unchanged across a scan, so the
                // secondary (still tuned + receiving in its task) needs no
                // retune here.  A channel *change* made during scan is
                // deferred and applied via the config block below, which
                // forwards it to the secondary.
                receiver.mark_link_down();
                link_up = false;
                rx_state.reset();
                divergence_since = [None; 16];
                wd.kick();
                scanning = false;
                defmt::info!(
                    "link RX: scan mode OFF, resuming on {} Hz",
                    current.frequency_hz
                );
            }
            _ => {}
        }

        // Apply any pending live-config update before re-entering
        // `rx_recv`.  Catches updates that landed during the prior
        // packet's processing (sink writes, stuck-note recovery)
        // when nothing was watching the signal.  Skipped during
        // scan mode — config changes there only update the local
        // `current` so the right config is restored on scan exit.
        if let Some(sig) = config_updates {
            if let Some(new_cfg) = sig.try_take() {
                if scanning {
                    current = new_cfg;
                    defmt::info!(
                        "link RX: deferred reconfigure (scanning); will apply on scan exit"
                    );
                } else {
                    apply_rx_reconfig(
                        radio,
                        &mut current,
                        &new_cfg,
                        &mut wd,
                        &mut receiver,
                        &mut rx_state,
                        &mut divergence_since,
                        &mut link_up,
                    )
                    .await;
                    // Forward to the secondary radio's task so it retunes.
                    if let Some(s) = secondary_cfg {
                        s.signal(current);
                    }
                }
            }
        }

        // ── Scan mode: sample one channel per loop iteration ─────
        if scanning {
            // unwrap safe: `scanning == true` only reached when
            // `scan.is_some()` (see reconcile above).
            let s = scan.unwrap();
            let count = s.channel_count();
            if count == 0 {
                // No channels configured — wait briefly for a
                // start() with a non-empty list rather than busy-
                // looping.  state_change wakes us early.
                let _ = select(Timer::after_millis(50), s.wait_change()).await;
                continue;
            }
            let i = scan_idx % count;
            if let Some(freq) = s.nth_frequency(i) {
                let rssi = scan_one_channel(radio, freq).await;
                s.write_rssi(i, rssi);
            }
            scan_idx = scan_idx.wrapping_add(1);
            if scan_idx.is_multiple_of(count) {
                s.note_pass_complete();
            }
            // No `stats` push during scan — RX-side counters don't
            // advance and pushing zeros every iteration would just
            // be churn on the stats cell.
            continue;
        }

        // ── Normal mode: continuous RX with watchdog + signal arms ─
        let cfg_wait = async {
            match config_updates {
                Some(s) => s.wait().await,
                None => core::future::pending::<LinkConfig>().await,
            }
        };
        let scan_wait = async {
            match scan {
                Some(s) => s.wait_change().await,
                None => core::future::pending::<()>().await,
            }
        };
        let shutdown_wait = async {
            match shutdown {
                Some(s) => s.wait().await,
                None => core::future::pending::<()>().await,
            }
        };
        // Receive from radio0, or from whichever of radio0/radio1 fires
        // first when a diversity radio is present.  On a secondary win we
        // copy its payload into `radio_buf` and return the same
        // `Result<RxPacket, _>` the primary would, so every downstream
        // arm (CRC-ok, early-CRC-fail, radio-error) is reached
        // identically regardless of source.  `select` is biased toward
        // its first future, so the single-radio (`None`) path resolves
        // exactly as before.
        // Which radio won this receive — read in the crc-ok arm to bump
        // the per-radio attribution counters.  Reset each iteration;
        // only meaningful when a packet (not a timer/signal) wins.
        let mut from_radio1 = false;
        let recv_any = async {
            match secondary.as_ref() {
                // Race the primary's own receive against a frame handed over
                // from the secondary radio's drain task.  `select` polls the
                // primary first, so the channel only wins when the primary is
                // *pending* (no packet of its own) — cancelling a pending
                // `rx_recv` (DIO1 still low) is harmless, so the primary's
                // IRQ is never left stuck.  The secondary never gets cancelled
                // at all (it lives in its own task).
                Some(rx) => match select(radio.rx_recv(&mut radio_buf), rx.receive()).await {
                    Either::First(res) => res,
                    Either::Second(frame) => {
                        from_radio1 = true;
                        let n = frame.len.min(radio_buf.len());
                        radio_buf[..n].copy_from_slice(&frame.buf[..n]);
                        // Synthesize the same packet metadata `rx_recv` would
                        // return; the producer only forwards crc-ok frames.
                        Ok(RxPacket {
                            len: n,
                            rssi_dbm: frame.rssi_dbm,
                            snr_db: 0,
                            crc_ok: true,
                        })
                    }
                },
                None => radio.rx_recv(&mut radio_buf).await,
            }
        };
        match select5(recv_any, wd.wait(), cfg_wait, scan_wait, shutdown_wait).await {
            Either5::Fifth(()) => {
                handle_rx_shutdown(radio, led, sink).await;
            }
            Either5::First(Ok(pkt)) if pkt.crc_ok => {
                let arrived = Instant::now();
                bucket_rx_gap(
                    &mut rx_gap_buckets,
                    arrived.duration_since(last_rx_at).as_millis(),
                );
                last_rx_at = arrived;
                // RSSI updates on any radio receive (signal-strength
                // panel should keep ticking even on rejected packets
                // — the operator is still pulling RF in).  But the
                // watchdog and the link-UP flag stay gated on
                // `process()` actually *accepting* the packet (see
                // the `Ok(Ok(()))` arm below) — a wrong-key TX is
                // an effectively-dead link from the application's
                // POV, even though radio packets keep arriving.
                last_rssi = Some(pkt.rssi_dbm);

                // Attribute this delivered packet to the radio that
                // caught it (the `recv_any` select winner).
                if from_radio1 {
                    rx1_caught = rx1_caught.wrapping_add(1);
                    last_rssi_rx1 = Some(pkt.rssi_dbm);
                } else {
                    rx0_caught = rx0_caught.wrapping_add(1);
                    last_rssi_rx0 = Some(pkt.rssi_dbm);
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
                            let mut v: MidiBuf = heapless::Vec::new();
                            let _ = v.extend_from_slice(midi);
                            let _ = midi_events.push(v);
                        }
                        RxEvent::SysExComplete(body) => {
                            accepted_sysex = accepted_sysex.wrapping_add(1);
                            let mut v: SysExBuf = heapless::Vec::new();
                            let _ = v.extend_from_slice(body);
                            let _ = sysex_events.push(v);
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
                                let started = divergence_since[ch as usize].get_or_insert(now);
                                if now.duration_since(*started)
                                    < Duration::from_millis(STUCK_NOTE_MIN_DIVERGENCE_MS)
                                {
                                    continue;
                                }
                                // Persisted long enough — recover.
                                // Push selective NoteOffs for every
                                // pressed note in this channel.  If
                                // `midi_events` runs out of capacity
                                // mid-loop, *don't* clear `rx_state`
                                // for this channel — leave the
                                // divergence timer set so the next
                                // heartbeat retries the leftover
                                // notes.  Sending duplicate NoteOffs
                                // for notes the synth has already
                                // silenced is a no-op; missing
                                // NoteOffs leave permanently stuck
                                // notes.  We optimise for the latter.
                                let pressed = rx_state.pressed_on(ch);
                                let mut count = 0u32;
                                let mut overflowed = false;
                                for note in 0..128u8 {
                                    if pressed & (1u128 << note) != 0 {
                                        let mut noteoff: MidiBuf = heapless::Vec::new();
                                        let _ = noteoff.extend_from_slice(&[0x80 | ch, note, 0]);
                                        if midi_events.push(noteoff).is_err() {
                                            overflowed = true;
                                            break;
                                        }
                                        count += 1;
                                    }
                                }
                                if overflowed {
                                    // Partial recovery — leave divergence
                                    // timer set, retry next heartbeat.
                                    // We don't touch `rx_state` so the
                                    // still-stuck notes get retried.
                                    defmt::warn!(
                                        "RX stuck-note recovery: ch {} → midi_events full after {} NoteOff(s); retrying next heartbeat",
                                        ch,
                                        count
                                    );
                                } else {
                                    rx_state.clear_channel(ch);
                                    divergence_since[ch as usize] = None;
                                    stuck_recoveries = stuck_recoveries.wrapping_add(1);
                                    defmt::warn!(
                                        "RX stuck-note recovery: ch {} → {} selective NoteOff(s) (total recoveries={})",
                                        ch,
                                        count,
                                        stuck_recoveries
                                    );
                                }
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
                        // Mark the link as alive only on acceptance —
                        // a wrong-key TX produces a steady stream of
                        // KeyFpMismatch drops that should *not* keep
                        // the link "UP", because no MIDI is making it
                        // through.  Watchdog kick lives here too for
                        // the same reason: it should fire after
                        // `watchdog_ms` of no *accepted* traffic, not
                        // of no radio activity.
                        wd.kick();
                        let was_down = !link_up;
                        link_up = true;
                        if was_down {
                            defmt::info!("link RX: link UP");
                        }
                        // Log Open↔AEAD transitions exactly once per
                        // crossover — quiet during steady state, but
                        // gives a clear audit trail when the operator
                        // flips the Key menu on the TX side.
                        let observed = receiver.last_accepted_key_fp();
                        if observed != last_logged_key_fp {
                            match observed {
                                None => {}
                                Some(fp) if fp == osrf_link::KEY_FP_NONE => {
                                    defmt::info!("link RX: accepting plaintext (key_fp=000000)",)
                                }
                                Some(fp) => defmt::info!(
                                    "link RX: accepting AEAD key_fp={=u8:02x}{=u8:02x}{=u8:02x}",
                                    fp[2],
                                    fp[1],
                                    fp[0],
                                ),
                            }
                            last_logged_key_fp = observed;
                        }
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
                        defmt::warn!(
                            "RX decode error (accepted={} dropped={})",
                            accepted,
                            dropped
                        );
                    }
                }

                // Drain buffered events to the sink.  MIDI events
                // first, then SysEx — within any single iteration a
                // packet produces events of one kind only (see the
                // `BufferedEvent` comment above), so the order
                // between the two vecs is never observable.
                for bytes in midi_events.iter() {
                    defmt::info!("RX MIDI: {=[u8]:#x}", bytes.as_slice());
                    if sink.write_message(bytes).await.is_err() {
                        defmt::error!("sink write_message failed");
                    }
                }
                for bytes in sysex_events.iter() {
                    defmt::info!("RX SysEx: {} bytes", bytes.len());
                    if sink.write_message(bytes).await.is_err() {
                        defmt::error!("sink SysEx write failed");
                    }
                }
            }
            Either5::First(Ok(_)) => {
                // Chip set both `rx_done` and `crc_err` in the IRQ
                // bitmap — a complete frame arrived but failed CRC.
                // Counted in the existing `crc_mismatch` field so
                // `recent_loss_pct` doesn't double-count it as
                // "lost without trace."
                crc_mismatch = crc_mismatch.wrapping_add(1);
                if crc_mismatch.is_multiple_of(50) {
                    defmt::warn!("RX: CRC mismatch count = {}", crc_mismatch);
                }
            }
            Either5::First(Err(e)) => {
                // Bucket by variant — different fingerprints suggest
                // different root causes.  Throttle the per-variant
                // log lines so a sustained error stream doesn't
                // flood the RTT buffer.
                match e {
                    RadioErrorKind::CrcMismatch => {
                        err_crc_mismatch = err_crc_mismatch.wrapping_add(1);
                        if err_crc_mismatch.is_multiple_of(50) {
                            defmt::warn!("RX: early-CRC-fail count = {}", err_crc_mismatch);
                        }
                    }
                    RadioErrorKind::UnexpectedIrq(irq) => {
                        err_unexpected_irq = err_unexpected_irq.wrapping_add(1);
                        if err_unexpected_irq <= 5 || err_unexpected_irq.is_multiple_of(20) {
                            defmt::warn!(
                                "RX: unexpected IRQ {=u16:#06x} (count {})",
                                irq,
                                err_unexpected_irq
                            );
                        }
                    }
                    RadioErrorKind::Spi => {
                        err_spi = err_spi.wrapping_add(1);
                        defmt::warn!("RX: SPI error (count {})", err_spi);
                    }
                    RadioErrorKind::Bus => {
                        err_bus = err_bus.wrapping_add(1);
                        defmt::warn!("RX: bus / pin-wait error (count {})", err_bus);
                    }
                    _ => {
                        // Reset / Switch / PayloadTooLarge /
                        // BufferTooSmall / InvalidSyncWord / Timeout.
                        // These shouldn't fire on a healthy RX path
                        // — `Reset`/`Switch` have generic payloads
                        // that don't impl `defmt::Format` so we
                        // can't print the variant directly.  The
                        // count alone is enough to flag "something
                        // unusual is going wrong; investigate."
                        err_other = err_other.wrapping_add(1);
                        defmt::warn!("RX: other radio error variant (count {})", err_other);
                    }
                }
            }
            Either5::Second(()) => {
                if link_up {
                    link_up = false;
                    defmt::warn!(
                        "link RX: LINK LOST (no packet for {}ms) → all-notes-off",
                        current.watchdog_ms
                    );
                    if sink.all_notes_off().await.is_err() {
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
            Either5::Third(new_cfg) => {
                apply_rx_reconfig(
                    radio,
                    &mut current,
                    &new_cfg,
                    &mut wd,
                    &mut receiver,
                    &mut rx_state,
                    &mut divergence_since,
                    &mut link_up,
                )
                .await;
                // Forward to the secondary radio's task so it retunes.
                if let Some(s) = secondary_cfg {
                    s.signal(current);
                }
                continue;
            }
            Either5::Fourth(()) => {
                // Scan controller fired (start / stop / channel-list
                // change).  Top-of-loop reconcile picks it up — just
                // restart the iteration so it sees the fresh state
                // before re-entering `rx_recv`.
                continue;
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
            let d_rx0 = rx0_caught.wrapping_sub(prev_rx0);
            let d_rx1 = rx1_caught.wrapping_sub(prev_rx1);
            let cur_packet_seq = receiver.last_packet_seq();
            let (tx_count, loss_x10) = match (prev_packet_seq, cur_packet_seq) {
                (Some(prev), Some(cur)) => {
                    let n = cur.saturating_sub(prev);
                    let l = n
                        .saturating_sub(d_accepted)
                        .saturating_mul(1000)
                        .checked_div(n)
                        .unwrap_or(0);
                    (n, l)
                }
                // First-ever observation, or session-reset between
                // windows — show 0/0 rather than a skewed first number.
                _ => (0, 0),
            };
            // Per-radio RSSI (dBm), most-recent crc-ok packet on each.
            // `0` = that radio hasn't caught anything yet — read it
            // alongside `rx0`/`rx1` to know if it's a current reading.
            let rssi0 = last_rssi_rx0.unwrap_or(0);
            let rssi1 = last_rssi_rx1.unwrap_or(0);
            defmt::info!(
                "RX last1s: pkts={}/{} loss={}.{}% midi_ev={} hb={} drop={} crc_err={} rx0={} rx1={} rssi0={=i16}dBm rssi1={=i16}dBm | total: pkts={} midi_ev={} hb={} sysex={} drop={} crc_err={} rx0={} rx1={}",
                d_accepted,
                tx_count,
                loss_x10 / 10,
                loss_x10 % 10,
                d_midi,
                d_hb,
                d_dropped,
                d_crc,
                d_rx0,
                d_rx1,
                rssi0,
                rssi1,
                accepted,
                accepted_midi,
                accepted_heartbeats,
                accepted_sysex,
                dropped,
                crc_mismatch,
                rx0_caught,
                rx1_caught,
            );
            // RX profile dump — inter-arrival gap histogram and
            // per-variant error counts.  Three buckets matter most
            // for diagnosis:
            //   `<2`  large = fine (burst MIDI, healthy link)
            //   `<12` is the heartbeat-cadence happy path
            //   `<25..<250` are jitter regimes
            //   `>=250` means we're missing entire heartbeats
            // `early-CRC` vs `unexpected-IRQ` distinguishes RF
            // problems from chip-state-management problems.
            defmt::info!(
                "RX prof: gap_ms <2={} <12={} <25={} <50={} <100={} <250={} >=250={} | err crc={} crc-early={} unex-irq={} spi={} bus={} other={}",
                rx_gap_buckets[0],
                rx_gap_buckets[1],
                rx_gap_buckets[2],
                rx_gap_buckets[3],
                rx_gap_buckets[4],
                rx_gap_buckets[5],
                rx_gap_buckets[6],
                crc_mismatch,
                err_crc_mismatch,
                err_unexpected_irq,
                err_spi,
                err_bus,
                err_other,
            );
            // Reset histogram each window so we see *current*
            // jitter, not a smear of all history.  Error counters
            // accumulate so trends are visible.
            rx_gap_buckets = [0; 7];

            prev_midi = accepted_midi;
            prev_hb = accepted_heartbeats;
            prev_accepted = accepted;
            prev_packet_seq = cur_packet_seq;
            prev_dropped = dropped;
            prev_crc = crc_mismatch;
            prev_rx0 = rx0_caught;
            prev_rx1 = rx1_caught;
            last_stats_log = now;
            // Stash this window's loss for export via `stats`.
            // Only meaningful when `tx_count > 0` (i.e. we actually
            // observed packet_seq advancing); on a session reset or
            // pre-first-packet window, leave the previous reading
            // alone rather than reporting a misleading 0%.
            if tx_count > 0 {
                last_loss_pct = Some(((loss_x10 / 10) as u8).min(100));
            }
            // Note: prev_packet_seq is now Some() once we've seen any
            // packet; the next window will compute real loss.
        }

        // Push the latest counters into the shared cell so consumers
        // (UI render path, telemetry exporters) see fresh values.
        // One critical-section per loop iteration is cheap relative
        // to the radio.rx_recv await we're about to block in.
        stats.update(|s| {
            s.link_up = link_up;
            s.last_rssi_dbm = last_rssi;
            s.last_rssi_dbm_rx0 = last_rssi_rx0;
            s.last_rssi_dbm_rx1 = last_rssi_rx1;
            s.total_accepted = accepted;
            s.accepted_heartbeats = accepted_heartbeats;
            s.accepted_midi = accepted_midi;
            s.dropped = dropped;
            s.crc_mismatch = crc_mismatch;
            s.stuck_recoveries = stuck_recoveries;
            s.recent_loss_pct = last_loss_pct;
        });
    }
}

/// Single-radio receive loop.  Configures `radio`, listens continuously,
/// dedups/decodes/decrypts via [`osrf_link::LinkReceiver`], and drives MIDI
/// out to `sink` plus the heartbeat-state stuck-note failsafe.  Supports
/// live config updates, channel-scan, AEAD key changes, and graceful
/// shutdown through the optional signal arguments.
///
/// This is the hardware-verified path (ROADMAP Stage 3: 6913 events, 0
/// missed notes).  It is a thin wrapper over [`run_rx_inner`] with no
/// diversity radio — the second receive future is `pending()`, so the
/// biased `select` always resolves to the one radio and behaviour is
/// identical to the pre-diversity implementation.
#[allow(clippy::too_many_arguments)]
pub async fn run_rx<Spi, Busy, Dio1, Reset, Switch, Led, Sink>(
    radio: &mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>,
    led: &mut Led,
    sink: &mut Sink,
    config: &LinkConfig,
    stats: &LinkStatsCell,
    config_updates: Option<&LinkConfigSignal>,
    scan: Option<&ScanController>,
    shutdown: Option<&ShutdownSignal>,
    aead: Option<AeadContext>,
    allow_open: bool,
    aead_updates: Option<&'static AeadUpdateSignal>,
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
    run_rx_inner(
        radio,
        None,
        None,
        led,
        sink,
        config,
        stats,
        config_updates,
        scan,
        shutdown,
        aead,
        allow_open,
        aead_updates,
    )
    .await
}

/// Primary half of two-radio diversity receive (ROADMAP Stage 2, dual-SPI).
///
/// Drives the **primary** radio and consumes frames the **secondary** radio's
/// [`run_rx_secondary`] task pushes over `secondary` (a [`DiversityRxChannel`]
/// receiver).  Both sources feed the single [`osrf_link::LinkReceiver`], whose
/// packet-`seq` replay window discards the duplicate copies — that *is* the
/// diversity arbitration.  All decode / decrypt / stuck-note-failsafe / stats
/// machinery is shared with [`run_rx`] (one code path in `run_rx_inner`).
///
/// **Topology:** the profile spawns two tasks — this one (owns radio0) and a
/// [`run_rx_secondary`] task (owns radio1) — plus a `static`
/// [`DiversityRxChannel`].  Each radio's `rx_recv` runs to completion in its
/// own task and is never cancelled mid-wait, so neither DIO1 IRQ is ever left
/// asserted.  (Racing both `rx_recv`s in one task with `select` left the loser's
/// level-sensitive DIO1 stuck high → embassy-nrf GPIOTE-PORT spurious-wake
/// storm → 50% loss + `unexpected IRQ 0x0000`.  This split structurally avoids
/// it.)
///
/// Full feature parity with [`run_rx`]: live `config_updates`, `scan`,
/// `shutdown`, `aead_updates`.  A live `LinkConfig` change is forwarded to the
/// secondary task via `secondary_cfg` so it retunes to the same channel.
/// During a channel scan only the primary samples RSSI; the secondary stays on
/// the (unchanged) operating channel and its frames simply age out of the
/// bounded channel until the scan ends.
#[allow(clippy::too_many_arguments)]
pub async fn run_rx_diversity<Spi, Busy, Dio1, Reset, Switch, Led, Sink>(
    radio0: &mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>,
    secondary: DiversityRxReceiver,
    secondary_cfg: &'static LinkConfigSignal,
    led: &mut Led,
    sink: &mut Sink,
    config: &LinkConfig,
    stats: &LinkStatsCell,
    config_updates: Option<&LinkConfigSignal>,
    scan: Option<&ScanController>,
    shutdown: Option<&ShutdownSignal>,
    aead: Option<AeadContext>,
    allow_open: bool,
    aead_updates: Option<&'static AeadUpdateSignal>,
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
    run_rx_inner(
        radio0,
        Some(secondary),
        Some(secondary_cfg),
        led,
        sink,
        config,
        stats,
        config_updates,
        scan,
        shutdown,
        aead,
        allow_open,
        aead_updates,
    )
    .await
}
