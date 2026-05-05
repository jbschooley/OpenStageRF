// SPDX-License-Identifier: AGPL-3.0-or-later
#![cfg_attr(not(test), no_std)]

//! OpenStageRF link layer — sequence numbering, replay-window dedup,
//! watchdog, heartbeat.  Sits between the radio (which moves bytes) and
//! the application (which produces / consumes `MidiEvent`s).
//!
//! Milestone 4 scope: the no-crypto path (`key_fp = KEY_FP_NONE`).
//! Crypto integration is a future milestone — the link layer here just
//! frames + dedups; AEAD/MAC verification will plug in around the
//! receiver's `decode_and_check` step.

use osrf_protocols_midi_v1 as proto;

pub use proto::{
    Body, FragState, Header, KeyFp, Packet, HEADER_LEN, KEY_FP_NONE, MAX_SEQ, VER_V1,
};

pub mod midi_tx;
pub use midi_tx::{MidiTxQueue, QUEUE_CAPACITY, REALTIME_PRIORITY, REGULAR_PRIORITY};

// ---------------------------------------------------------------------------
// ReplayWindow — 64-entry sliding-window bitmap
// ---------------------------------------------------------------------------

/// Sliding-window replay detector.
///
/// Tracks the highest accepted `seq` and a 64-bit bitmap of which of the
/// 64 immediately-preceding seqs we've also accepted (bit 0 = `latest`,
/// bit i = `latest - i`).  Out-of-order packets within the window are
/// accepted; replays (already-marked) and too-old packets are rejected.
///
/// The window is keyed on the full 48-bit `seq` from the packet header,
/// so a transmitter reset (which bumps `boot_counter` in the high 16
/// bits of `seq`) automatically presents as a forward jump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct ReplayWindow {
    /// Highest seq we've ever accepted.
    latest: u64,
    /// Bit i is set iff we've accepted `latest - i` (bit 0 = latest itself).
    bitmap: u64,
    /// Was any seq ever accepted?  (Distinguishes "fresh" from "latest = 0".)
    primed: bool,
}

impl ReplayWindow {
    pub const fn new() -> Self {
        Self {
            latest: 0,
            bitmap: 0,
            primed: false,
        }
    }

    /// Check whether `seq` is acceptable, and if so, mark it.
    ///
    /// Returns `true` if the packet is new and was accepted; `false` if
    /// it's a replay (already seen) or too old to fit in the window.
    pub fn check_and_mark(&mut self, seq: u64) -> bool {
        if !self.primed {
            self.latest = seq;
            self.bitmap = 0x1;
            self.primed = true;
            return true;
        }

        if seq > self.latest {
            // Forward jump: shift bitmap left by the gap, set bit 0 for `seq`.
            let shift = seq - self.latest;
            self.bitmap = if shift >= 64 {
                0x1 // Everything before `seq` falls out of the window.
            } else {
                (self.bitmap << shift) | 0x1
            };
            self.latest = seq;
            true
        } else {
            // seq <= latest: backward / equal.  Distance from latest:
            let distance = self.latest - seq;
            if distance >= 64 {
                // Too old — outside the window.  Drop.
                false
            } else {
                let mask = 1u64 << distance;
                if self.bitmap & mask != 0 {
                    // Already seen → replay.
                    false
                } else {
                    self.bitmap |= mask;
                    true
                }
            }
        }
    }
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// LinkSender — outbound: pack header + body into a buffer for the radio
// ---------------------------------------------------------------------------

/// Outbound link-layer encoder.
///
/// Owns the local `(boot_counter, session_seq)` pair.  Each call to
/// [`Self::encode`] consumes one session_seq.  When session_seq would
/// wrap past `u32::MAX`, encoding fails with [`SendError::SessionSeqOverflow`]
/// — caller is expected to bump boot_counter (e.g., reboot) before that
/// happens in practice.
///
/// `key_fp` is currently always [`KEY_FP_NONE`] (no-crypto path).  The
/// field is exposed so a future crypto-aware caller can construct a
/// sender with a real key fingerprint without an API break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct LinkSender {
    boot_counter: u16,
    session_seq: u32,
    key_fp: KeyFp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum SendError {
    /// Underlying protocol encoder rejected the body / header.
    Encode(proto::EncodeError),
    /// Out of session sequence numbers.  Reset (bumps boot_counter) required.
    SessionSeqOverflow,
}

impl From<proto::EncodeError> for SendError {
    fn from(e: proto::EncodeError) -> Self {
        Self::Encode(e)
    }
}

impl LinkSender {
    pub fn new(boot_counter: u16, key_fp: KeyFp) -> Self {
        Self {
            boot_counter,
            session_seq: 0,
            key_fp,
        }
    }

    /// Construct with `KEY_FP_NONE` — the M4 no-crypto path.
    pub fn no_crypto(boot_counter: u16) -> Self {
        Self::new(boot_counter, KEY_FP_NONE)
    }

    pub fn boot_counter(&self) -> u16 {
        self.boot_counter
    }

    pub fn session_seq(&self) -> u32 {
        self.session_seq
    }

    pub fn key_fp(&self) -> KeyFp {
        self.key_fp
    }

    /// Encode `body` into `out`, advancing the session seq.  Returns the
    /// number of bytes written.
    pub fn encode(
        &mut self,
        body: &Body<'_>,
        out: &mut [u8],
    ) -> Result<usize, SendError> {
        let next = self
            .session_seq
            .checked_add(1)
            .ok_or(SendError::SessionSeqOverflow)?;
        let header = Header {
            ver: VER_V1,
            key_fp: self.key_fp,
            seq: Header::make_seq(self.boot_counter, self.session_seq),
            event_type: body.event_type(),
        };
        let n = proto::encode(out, &header, body)?;
        self.session_seq = next;
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// LinkReceiver — inbound: decode + replay-check
// ---------------------------------------------------------------------------

/// Reason a received packet was dropped after framing-level decode succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum RxDrop {
    /// `key_fp` didn't match this receiver's expected fingerprint.
    /// Caller passes the wire `key_fp` so consumers can log.
    KeyFpMismatch(KeyFp),
    /// Replay-window rejection (duplicate or too old).  Caller gets the
    /// wire `seq` so it can be logged.
    Replay(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum RxOutcome<'a> {
    /// Packet decoded, key matched, replay window accepted.  Pass to app.
    Accept(Packet<'a>),
    /// Decoded fine but rejected for one of the reasons in [`RxDrop`].
    Drop(RxDrop),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum RxError {
    /// Wire-format decode failed (truncation, version mismatch, etc.).
    Decode(proto::DecodeError),
}

impl From<proto::DecodeError> for RxError {
    fn from(e: proto::DecodeError) -> Self {
        Self::Decode(e)
    }
}

/// Inbound link-layer decoder + replay window.
///
/// Tracks the most recent `boot_counter` seen, and on a change resets
/// the replay window — so a transmitter reboot with any new
/// `boot_counter` (higher *or* lower than the last) is accepted as a
/// fresh session rather than rejected as ancient data.
///
/// **Security note**: in the no-crypto path (M4), this means an
/// attacker who can inject a packet with a fresh `boot_counter` can
/// reset the receiver's replay window.  This is acceptable for the
/// no-crypto bench because the wire format is unauthenticated end-to-
/// end anyway.  Once crypto lands (M5+), the AEAD tag is computed over
/// the full header (including `boot_counter` and `seq`) with the
/// session key, so cross-session replays fail authentication and the
/// session reset is safe to perform unconditionally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct LinkReceiver {
    expected_key_fp: KeyFp,
    window: ReplayWindow,
    /// `boot_counter` of the most recently accepted packet, if any.
    /// Used to detect transmitter resets so we can reset the window.
    last_boot_counter: Option<u16>,
}

impl LinkReceiver {
    pub fn new(expected_key_fp: KeyFp) -> Self {
        Self {
            expected_key_fp,
            window: ReplayWindow::new(),
            last_boot_counter: None,
        }
    }

    /// Construct with `KEY_FP_NONE` — the M4 no-crypto path.
    pub fn no_crypto() -> Self {
        Self::new(KEY_FP_NONE)
    }

    /// Decode + replay-check `buf`.  On accept, returns the borrowed
    /// `Packet`; on drop, returns the reason.  On unrecoverable framing
    /// errors, returns `Err(RxError)` so the caller can decide whether
    /// to log noisily or drop quietly.
    pub fn process<'a>(&mut self, buf: &'a [u8]) -> Result<RxOutcome<'a>, RxError> {
        let pkt = proto::decode(buf)?;
        if pkt.header.key_fp != self.expected_key_fp {
            return Ok(RxOutcome::Drop(RxDrop::KeyFpMismatch(pkt.header.key_fp)));
        }
        // Detect TX-side reboot: any change in boot_counter (high 16 bits
        // of seq) → fresh session, reset the replay window.  Without this,
        // a TX reboot that happens to pick a *lower* random boot_counter
        // would have all its new packets rejected as "too old".
        let boot = pkt.header.boot_counter();
        if self.last_boot_counter != Some(boot) {
            self.window = ReplayWindow::new();
            self.last_boot_counter = Some(boot);
        }
        if !self.window.check_and_mark(pkt.header.seq) {
            return Ok(RxOutcome::Drop(RxDrop::Replay(pkt.header.seq)));
        }
        Ok(RxOutcome::Accept(pkt))
    }

    /// Borrow the underlying replay window — useful for diagnostics.
    pub fn window(&self) -> &ReplayWindow {
        &self.window
    }

    /// `boot_counter` of the most recently accepted packet, if any.
    pub fn last_boot_counter(&self) -> Option<u16> {
        self.last_boot_counter
    }
}

// ---------------------------------------------------------------------------
// Watchdog + Heartbeat — timer-based primitives the app composes via select
// ---------------------------------------------------------------------------

use embassy_time::{Duration, Instant, Timer};

/// Receiver-side watchdog: fires after `timeout` elapses with no [`Self::kick`].
///
/// Spec target (M4): 200 ms.  The app composes this with the radio's
/// `rx_continuous` future via `embassy_futures::select`; on each accepted
/// packet it calls [`Self::kick`], on watchdog expiry it surfaces a
/// `LinkLost` event to the consumer (which on the MIDI side translates
/// to "all-notes-off on every channel").
///
/// Usage pattern:
/// ```ignore
/// let mut wd = WatchdogTimer::new(Duration::from_millis(200));
/// loop {
///     match select(radio.rx_continuous(&mut buf), wd.wait()).await {
///         Either::First(Ok(pkt)) => { wd.kick(); /* process pkt */ }
///         Either::First(Err(e))  => { /* log */ }
///         Either::Second(())     => { /* LinkLost */ wd.kick(); }
///     }
/// }
/// ```
pub struct WatchdogTimer {
    timeout: Duration,
    deadline: Instant,
}

impl WatchdogTimer {
    /// Create with the given timeout.  Deadline is set to `now + timeout`.
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            deadline: Instant::now() + timeout,
        }
    }

    /// Reset the deadline to `now + timeout`.  Call on every accepted
    /// packet to "feed" the watchdog.
    pub fn kick(&mut self) {
        self.deadline = Instant::now() + self.timeout;
    }

    /// The configured timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Current deadline.  Exposed for diagnostics / external composition.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Wait until the deadline.  When this future resolves, the watchdog
    /// has expired; caller should treat that as `LinkLost`.
    ///
    /// If the deadline is updated by [`Self::kick`] *after* this future
    /// is created, the future still resolves at the *original* deadline
    /// — but in the standard `select` pattern the future is dropped on
    /// the first packet, a new wait is created, and the new deadline takes
    /// effect.  No staleness possible.
    pub async fn wait(&self) {
        Timer::at(self.deadline).await;
    }
}

/// Transmitter-side heartbeat: tells the app "you should send something
/// now or the receiver's watchdog will trip".
///
/// Spec target (M4): 20 ms (10× safety margin against the 200 ms watchdog).
/// The app composes this with its inbound MIDI source via `select`; on
/// any send (real MIDI event OR heartbeat), it calls [`Self::note_send`]
/// to defer the next heartbeat.
///
/// Usage pattern:
/// ```ignore
/// let mut hb = HeartbeatTimer::new(Duration::from_millis(20));
/// loop {
///     match select(midi_in.read_event(), hb.wait()).await {
///         Either::First(ev) => { send_event(ev); hb.note_send(); }
///         Either::Second(()) => { send_heartbeat(); hb.note_send(); }
///     }
/// }
/// ```
pub struct HeartbeatTimer {
    interval: Duration,
    next_due: Instant,
}

impl HeartbeatTimer {
    /// Create with the given heartbeat interval.  First heartbeat is due
    /// at `now + interval`.
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            next_due: Instant::now() + interval,
        }
    }

    /// Note that the app just sent something (heartbeat or otherwise).
    /// Advances the next-due deadline by exactly `interval` from the
    /// previous one, giving a true fixed-rate cadence regardless of how
    /// long each `tx()` takes.  If we've fallen behind (because tx()
    /// overshot the next deadline), snap forward to the next future
    /// deadline so we don't burn CPU catching up.
    pub fn note_send(&mut self) {
        let now = Instant::now();
        let mut due = self.next_due + self.interval;
        // If we missed the deadline (e.g., a long TX), advance past `now`.
        while due < now {
            due += self.interval;
        }
        self.next_due = due;
    }

    /// The configured heartbeat interval.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// When the next heartbeat is due.  For diagnostics.
    pub fn next_due(&self) -> Instant {
        self.next_due
    }

    /// Wait until the next heartbeat is due.  When this resolves, the
    /// app should send something (a Heartbeat body if no MIDI event is
    /// queued) and call [`Self::note_send`].
    pub async fn wait(&self) {
        Timer::at(self.next_due).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use osrf_protocols_midi_v1 as proto;

    // -------- ReplayWindow --------

    #[test]
    fn replay_first_packet_accepted() {
        let mut w = ReplayWindow::new();
        assert!(w.check_and_mark(42));
    }

    #[test]
    fn replay_same_seq_twice_rejected() {
        let mut w = ReplayWindow::new();
        assert!(w.check_and_mark(10));
        assert!(!w.check_and_mark(10));
    }

    #[test]
    fn replay_strictly_increasing_seqs_all_accepted() {
        let mut w = ReplayWindow::new();
        for s in 0..100 {
            assert!(w.check_and_mark(s));
        }
    }

    #[test]
    fn replay_out_of_order_within_window_accepted_once() {
        let mut w = ReplayWindow::new();
        // Establish latest=10
        assert!(w.check_and_mark(10));
        // Backwards within window: 5, 8, 3 — all new
        assert!(w.check_and_mark(5));
        assert!(w.check_and_mark(8));
        assert!(w.check_and_mark(3));
        // Re-presenting any of those is a replay
        assert!(!w.check_and_mark(5));
        assert!(!w.check_and_mark(8));
        assert!(!w.check_and_mark(3));
        assert!(!w.check_and_mark(10));
    }

    #[test]
    fn replay_too_old_rejected() {
        let mut w = ReplayWindow::new();
        // Establish latest=100
        assert!(w.check_and_mark(100));
        // 100 - 64 = 36 is the oldest still in window; 35 is too old.
        // But "too old" requires distance >= 64, so 100 - distance.
        // distance=64 → seq=36 → outside.  Let's verify boundary.
        assert!(!w.check_and_mark(36)); // distance 64, just outside
        assert!(w.check_and_mark(37)); // distance 63, just inside (new)
    }

    #[test]
    fn replay_far_forward_jump_resets_window() {
        let mut w = ReplayWindow::new();
        // Establish latest=10 with some history
        assert!(w.check_and_mark(5));
        assert!(w.check_and_mark(10));
        // Jump forward by > 64 — bitmap becomes just {new}
        assert!(w.check_and_mark(1000));
        // 5 and 10 are now WAY out of window — rejected as too old
        assert!(!w.check_and_mark(5));
        assert!(!w.check_and_mark(10));
        // 1000 itself is a replay
        assert!(!w.check_and_mark(1000));
    }

    #[test]
    fn replay_short_forward_jump_keeps_history() {
        let mut w = ReplayWindow::new();
        assert!(w.check_and_mark(0));
        assert!(w.check_and_mark(1));
        assert!(w.check_and_mark(5));
        // 0, 1, 5 should all be replays now
        assert!(!w.check_and_mark(0));
        assert!(!w.check_and_mark(1));
        assert!(!w.check_and_mark(5));
        // 2, 3, 4 are gaps — can be filled in
        assert!(w.check_and_mark(2));
        assert!(w.check_and_mark(3));
        assert!(w.check_and_mark(4));
    }

    #[test]
    fn replay_boot_counter_jump_treated_as_forward() {
        let mut w = ReplayWindow::new();
        // Old session: seq has boot_counter=0
        let old = Header::make_seq(0, 1_000);
        assert!(w.check_and_mark(old));
        // New session: boot_counter=1, session_seq=0 — high bits set, way
        // larger than `old`, so accepted as a fresh forward jump.
        let new = Header::make_seq(1, 0);
        assert!(w.check_and_mark(new));
        // The OLD seq is now far below latest — outside window, rejected.
        assert!(!w.check_and_mark(old));
    }

    // -------- LinkSender --------

    #[test]
    fn sender_increments_session_seq() {
        let mut s = LinkSender::no_crypto(7);
        let mut buf = [0u8; 64];
        let body = Body::Heartbeat;
        let _ = s.encode(&body, &mut buf).unwrap();
        assert_eq!(s.session_seq(), 1);
        let _ = s.encode(&body, &mut buf).unwrap();
        assert_eq!(s.session_seq(), 2);
    }

    #[test]
    fn sender_writes_correct_header() {
        let mut s = LinkSender::no_crypto(7);
        let mut buf = [0u8; 64];
        let body = Body::MidiMessage(&[0x90, 0x40, 0x7F]);
        let n = s.encode(&body, &mut buf).unwrap();
        let pkt = proto::decode(&buf[..n]).unwrap();
        assert_eq!(pkt.header.ver, VER_V1);
        assert_eq!(pkt.header.key_fp, KEY_FP_NONE);
        assert_eq!(pkt.header.boot_counter(), 7);
        assert_eq!(pkt.header.session_seq(), 0);
        assert_eq!(pkt.header.event_type, proto::event_type::MIDI_MESSAGE);
        match pkt.body {
            Body::MidiMessage(b) => assert_eq!(b, &[0x90, 0x40, 0x7F]),
            _ => panic!("wrong body variant"),
        }
    }

    #[test]
    fn sender_session_seq_overflow_returns_err() {
        let mut s = LinkSender::no_crypto(0);
        // Cheat the seq forward by accessing internals via repeated encodes
        // would take too long; instead, simulate near-overflow:
        s.session_seq = u32::MAX;
        let mut buf = [0u8; 64];
        match s.encode(&Body::Heartbeat, &mut buf) {
            Err(SendError::SessionSeqOverflow) => {}
            other => panic!("expected overflow, got {:?}", other),
        }
    }

    // -------- LinkReceiver --------

    #[test]
    fn receiver_accepts_first_packet() {
        let mut s = LinkSender::no_crypto(0);
        let mut r = LinkReceiver::no_crypto();
        let mut buf = [0u8; 64];
        let n = s.encode(&Body::MidiMessage(&[0x90, 0x40, 0x7F]), &mut buf).unwrap();
        match r.process(&buf[..n]).unwrap() {
            RxOutcome::Accept(p) => {
                match p.body {
                    Body::MidiMessage(b) => assert_eq!(b, &[0x90, 0x40, 0x7F]),
                    _ => panic!("wrong body"),
                }
            }
            other => panic!("expected Accept, got {:?}", other),
        }
    }

    #[test]
    fn receiver_dedup_rejects_replay() {
        let mut s = LinkSender::no_crypto(0);
        let mut r = LinkReceiver::no_crypto();
        let mut buf = [0u8; 64];
        let n = s.encode(&Body::Heartbeat, &mut buf).unwrap();
        // First time: accepted.
        assert!(matches!(r.process(&buf[..n]).unwrap(), RxOutcome::Accept(_)));
        // Replay the same wire bytes: dropped.
        match r.process(&buf[..n]).unwrap() {
            RxOutcome::Drop(RxDrop::Replay(_)) => {}
            other => panic!("expected Replay drop, got {:?}", other),
        }
    }

    #[test]
    fn receiver_drops_wrong_key_fp() {
        // Sender signs with KEY_FP_NONE; receiver expects [1,2,3].
        let mut s = LinkSender::no_crypto(0);
        let mut r = LinkReceiver::new([0x01, 0x02, 0x03]);
        let mut buf = [0u8; 64];
        let n = s.encode(&Body::Heartbeat, &mut buf).unwrap();
        match r.process(&buf[..n]).unwrap() {
            RxOutcome::Drop(RxDrop::KeyFpMismatch(seen)) => {
                assert_eq!(seen, KEY_FP_NONE);
            }
            other => panic!("expected KeyFpMismatch, got {:?}", other),
        }
    }

    #[test]
    fn receiver_decode_error_propagates() {
        let mut r = LinkReceiver::no_crypto();
        let truncated = [0x01u8, 0x00, 0x00, 0x00, 0x00, 0x00]; // < HEADER_LEN
        match r.process(&truncated) {
            Err(RxError::Decode(_)) => {}
            other => panic!("expected Decode error, got {:?}", other),
        }
    }

    #[test]
    fn receiver_resets_window_on_lower_boot_counter() {
        // First TX session: boot_counter=100, sends 50 packets.
        let mut s1 = LinkSender::no_crypto(100);
        let mut r = LinkReceiver::no_crypto();
        let mut buf = [0u8; 64];
        for _ in 0..50 {
            let n = s1.encode(&Body::Heartbeat, &mut buf).unwrap();
            assert!(matches!(r.process(&buf[..n]).unwrap(), RxOutcome::Accept(_)));
        }
        // TX reboots with a LOWER boot_counter.  Without the session-reset
        // logic this would all look like ancient replays.
        let mut s2 = LinkSender::no_crypto(50);
        let n = s2.encode(&Body::Heartbeat, &mut buf).unwrap();
        match r.process(&buf[..n]).unwrap() {
            RxOutcome::Accept(_) => {}
            other => panic!("expected Accept after reboot, got {:?}", other),
        }
        // Subsequent packets in the new session keep working normally.
        for _ in 0..10 {
            let n = s2.encode(&Body::Heartbeat, &mut buf).unwrap();
            assert!(matches!(r.process(&buf[..n]).unwrap(), RxOutcome::Accept(_)));
        }
        assert_eq!(r.last_boot_counter(), Some(50));
    }

    #[test]
    fn receiver_resets_window_on_higher_boot_counter_too() {
        // Same as above but new boot_counter is HIGHER — should still work
        // (and would have worked even without the explicit reset, since a
        // higher seq is a forward jump — but we want the tracking state
        // updated either way).
        let mut s1 = LinkSender::no_crypto(50);
        let mut r = LinkReceiver::no_crypto();
        let mut buf = [0u8; 64];
        for _ in 0..50 {
            let n = s1.encode(&Body::Heartbeat, &mut buf).unwrap();
            assert!(matches!(r.process(&buf[..n]).unwrap(), RxOutcome::Accept(_)));
        }
        let mut s2 = LinkSender::no_crypto(100);
        let n = s2.encode(&Body::Heartbeat, &mut buf).unwrap();
        assert!(matches!(r.process(&buf[..n]).unwrap(), RxOutcome::Accept(_)));
        assert_eq!(r.last_boot_counter(), Some(100));
    }

    #[test]
    fn receiver_accepts_out_of_order_within_window() {
        let mut s = LinkSender::no_crypto(0);
        let mut r = LinkReceiver::no_crypto();
        let mut bufs: [[u8; 64]; 5] = [[0; 64]; 5];
        let mut lens = [0usize; 5];
        // Encode 5 packets in order.
        for i in 0..5 {
            lens[i] = s.encode(&Body::Heartbeat, &mut bufs[i]).unwrap();
        }
        // Deliver out of order: 3, 1, 4, 0, 2.
        let order = [3, 1, 4, 0, 2];
        for &i in &order {
            match r.process(&bufs[i][..lens[i]]).unwrap() {
                RxOutcome::Accept(_) => {}
                other => panic!("packet {i} unexpectedly dropped: {other:?}"),
            }
        }
        // Re-deliver any of them: replay.
        match r.process(&bufs[2][..lens[2]]).unwrap() {
            RxOutcome::Drop(RxDrop::Replay(_)) => {}
            other => panic!("expected replay, got {other:?}"),
        }
    }
}
