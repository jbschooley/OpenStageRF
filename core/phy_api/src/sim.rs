// SPDX-License-Identifier: AGPL-3.0-or-later
//! Simulator PHY (host only).
//!
//! An [`Air`] is shared by one transmitter and any number of receivers.
//! Time is the transmitter's period counter: every [`Phy::next_tx_slot`]
//! that wraps past the last sub-slot advances it, and a receiver's
//! [`Phy::rx`] hands out the frames that have "arrived" by the receiver's
//! own period, which the receiver advances by calling [`SimPhy::tick`].
//! Nothing here pends, so [`crate::block_on`] drives it.
//!
//! Impairments, all per receiver branch so diversity can be modelled:
//!
//! * `loss`: probability that a loss event starts at a period; `burst`
//!   periods are then dropped (`--loss 0.000167 --burst 60` in codec-lab
//!   terms is a 5 ms fade every ~0.5 s at 83 µs groups; here the unit is
//!   the slot period).
//! * `ber`: independent bit flips on delivered frames.  With
//!   `CrcCaps::Internal` a flipped frame is delivered with `crc_ok =
//!   false`; with `CrcCaps::None` it is delivered as is and software must
//!   notice.
//! * `delay`: periods between transmission and delivery; `jitter` adds a
//!   random 0..=jitter on top, which also reorders.

use crate::{
    CrcCaps, Dir, FecCaps, MetaCaps, Phy, PhyCaps, PhyError, PhyKind, RxMeta, SlotDesc, TxSlot,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Shapes the simulator can take on.
pub mod shapes {
    use super::*;

    /// 1 Mb/s × 333.3 µs = 333 bits; 24 go to the adapter's sync word and
    /// sequence, 38 bytes (304 bits) are payload.
    const ONE_DOWN_38: [SlotDesc; 1] = [SlotDesc {
        id: 0,
        bytes: 38,
        dir: Dir::Down,
    }];
    const ONE_DOWN_64: [SlotDesc; 1] = [SlotDesc {
        id: 0,
        bytes: 64,
        dir: Dir::Down,
    }];
    const EIGHT_DOWN_ONE_UP: [SlotDesc; 9] = [
        SlotDesc {
            id: 0,
            bytes: 48,
            dir: Dir::Down,
        },
        SlotDesc {
            id: 1,
            bytes: 48,
            dir: Dir::Down,
        },
        SlotDesc {
            id: 2,
            bytes: 48,
            dir: Dir::Down,
        },
        SlotDesc {
            id: 3,
            bytes: 48,
            dir: Dir::Down,
        },
        SlotDesc {
            id: 4,
            bytes: 48,
            dir: Dir::Down,
        },
        SlotDesc {
            id: 5,
            bytes: 48,
            dir: Dir::Down,
        },
        SlotDesc {
            id: 6,
            bytes: 48,
            dir: Dir::Down,
        },
        SlotDesc {
            id: 7,
            bytes: 48,
            dir: Dir::Down,
        },
        SlotDesc {
            id: 8,
            bytes: 16,
            dir: Dir::Up,
        },
    ];

    /// The Si4463 direct-mode shape: one 38-byte block every 333 µs (four
    /// codec groups), no FEC or CRC in the PHY, sync tick.
    pub const STREAM_1: PhyCaps = PhyCaps {
        kind: PhyKind::ContinuousStream,
        slot_period_us: 333,
        tx_slots: &ONE_DOWN_38,
        rx_slots: &ONE_DOWN_38,
        fec: FecCaps::None,
        crc: CrcCaps::None,
        sync_tick: true,
        meta: MetaCaps {
            rssi: true,
            snr: false,
            antenna: false,
        },
        freq_min_khz: 470_000,
        freq_max_khz: 608_000,
        freq_step_khz: 25,
        tx_power_min_dbm: -20,
        tx_power_max_dbm: 20,
    };

    /// A packet radio (SX1262 / Si4463 packet mode): on demand, ≤64 B,
    /// chip CRC, no FEC, no tick.
    pub const PACKET: PhyCaps = PhyCaps {
        kind: PhyKind::PacketRadio,
        slot_period_us: 0,
        tx_slots: &ONE_DOWN_64,
        rx_slots: &ONE_DOWN_64,
        fec: FecCaps::None,
        crc: CrcCaps::Internal,
        sync_tick: false,
        meta: MetaCaps {
            rssi: true,
            snr: true,
            antenna: false,
        },
        freq_min_khz: 470_000,
        freq_max_khz: 928_000,
        freq_step_khz: 1,
        tx_power_min_dbm: -9,
        tx_power_max_dbm: 22,
    };

    /// The wideband modem shape: 1 ms superframe, 8 downlink stream slots
    /// + 1 uplink, internal LDPC and CRC, internal diversity, tick.
    pub const SUPERFRAME_8: PhyCaps = PhyCaps {
        kind: PhyKind::Superframe,
        slot_period_us: 1000,
        tx_slots: &EIGHT_DOWN_ONE_UP,
        rx_slots: &EIGHT_DOWN_ONE_UP,
        fec: FecCaps::Internal { rate: (2, 3) },
        crc: CrcCaps::Internal,
        sync_tick: true,
        meta: MetaCaps {
            rssi: true,
            snr: true,
            antenna: true,
        },
        freq_min_khz: 470_000,
        freq_max_khz: 608_000,
        freq_step_khz: 1000,
        tx_power_min_dbm: -10,
        tx_power_max_dbm: 20,
    };
}

/// Frames the air remembers for receivers that have not read them yet.
pub const LOG_DEPTH: usize = 16_384;

/// Impairments for one receiver branch.
#[derive(Clone, Copy, Debug)]
pub struct Impairments {
    pub loss: f64,
    pub burst: u32,
    pub ber: f64,
    pub delay: u32,
    pub jitter: u32,
    /// Reported RSSI for delivered frames.
    pub rssi_dbm: i16,
}

impl Impairments {
    pub const CLEAN: Impairments = Impairments {
        loss: 0.0,
        burst: 1,
        ber: 0.0,
        delay: 0,
        jitter: 0,
        rssi_dbm: -60,
    };
}

#[derive(Clone)]
struct Frame {
    slot: u8,
    period: u32,
    bytes: Vec<u8>,
}

struct AirState {
    caps: PhyCaps,
    tx_period: u32,
    tx_next_slot: usize, // index into caps.tx_slots
    /// Frames transmitted, oldest first, each tagged with its period.
    sent: VecDeque<Frame>,
    /// Number of frames ever pushed to `sent` (so branches can index).
    sent_base: u64,
    branches: usize,
}

/// The shared medium: one transmitter, `branches` receivers.
#[derive(Clone)]
pub struct Air(Arc<Mutex<AirState>>);

impl Air {
    pub fn new(caps: PhyCaps) -> Self {
        Air(Arc::new(Mutex::new(AirState {
            caps,
            tx_period: 0,
            tx_next_slot: 0,
            sent: VecDeque::new(),
            sent_base: 0,
            branches: 0,
        })))
    }
    pub fn caps(&self) -> PhyCaps {
        self.0.lock().unwrap().caps
    }
    /// The transmitter end.
    pub fn transmitter(&self) -> SimPhy {
        SimPhy {
            air: self.clone(),
            role: Role::Tx,
            caps: self.caps(),
            imp: Impairments::CLEAN,
            rx_period: 0,
            next_index: 0,
            burst_left: 0,
            in_burst: false,
            prng: Lcg(1),
            branch: 0,
            pending: VecDeque::new(),
            delivered: 0,
            dropped: 0,
        }
    }
    /// A receiver branch with its own impairments and random stream.
    pub fn receiver(&self, imp: Impairments, seed: u64) -> SimPhy {
        let branch = {
            let mut a = self.0.lock().unwrap();
            a.branches += 1;
            a.branches as u8 - 1
        };
        SimPhy {
            air: self.clone(),
            role: Role::Rx,
            caps: self.caps(),
            imp,
            rx_period: 0,
            next_index: 0,
            burst_left: 0,
            in_burst: false,
            prng: Lcg(seed
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(branch as u64 + 1)),
            branch,
            pending: VecDeque::new(),
            delivered: 0,
            dropped: 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Tx,
    Rx,
}

struct Lcg(u64);
impl Lcg {
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn below(&mut self, n: u32) -> u32 {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as u32
        }
    }
}

/// A delivered-but-not-yet-read frame at a receiver.
struct Arrival {
    deliver_at: u32,
    frame: Frame,
    crc_ok: bool,
}

/// One end of the simulated link.
pub struct SimPhy {
    air: Air,
    role: Role,
    caps: PhyCaps,
    imp: Impairments,
    /// Receiver's own period counter (advance with [`SimPhy::tick`]).
    rx_period: u32,
    /// Next index into the air's `sent` log this receiver has processed.
    next_index: u64,
    burst_left: u32,
    in_burst: bool,
    prng: Lcg,
    branch: u8,
    pending: VecDeque<Arrival>,
    /// Frames handed out by `rx`.
    pub delivered: u64,
    /// Frames lost to the loss model at this branch.
    pub dropped: u64,
}

impl SimPhy {
    /// Advance the receiver's clock by one period.  The transmitter's clock
    /// advances by itself in `next_tx_slot`.
    pub fn tick(&mut self) {
        self.rx_period = self.rx_period.wrapping_add(1);
    }
    pub fn period(&self) -> u32 {
        match self.role {
            Role::Tx => self.air.0.lock().unwrap().tx_period,
            Role::Rx => self.rx_period,
        }
    }
    pub fn branch(&self) -> u8 {
        self.branch
    }

    /// Pull newly transmitted frames through this branch's impairments.
    fn ingest(&mut self) {
        let a = self.air.0.lock().unwrap();
        let start = self.next_index.saturating_sub(a.sent_base) as usize;
        let mut new = Vec::new();
        for f in a.sent.iter().skip(start) {
            new.push(f.clone());
        }
        self.next_index = a.sent_base + a.sent.len() as u64;
        drop(a);
        let mut last_period = None;
        for f in new {
            // One loss decision per period, not per sub-slot.
            if last_period != Some(f.period) {
                last_period = Some(f.period);
                if self.burst_left == 0
                    && self.imp.loss > 0.0
                    && self.prng.next_f64() < self.imp.loss
                {
                    self.burst_left = self.imp.burst.max(1);
                }
                // `in_burst` covers this whole period's sub-slots; the
                // counter steps once per period.
                self.in_burst = self.burst_left > 0;
                if self.burst_left > 0 {
                    self.burst_left -= 1;
                }
            }
            if self.in_burst {
                self.dropped += 1;
                continue;
            }
            let mut bytes = f.bytes.clone();
            let mut flipped = false;
            if self.imp.ber > 0.0 {
                for b in bytes.iter_mut() {
                    for bit in 0..8 {
                        if self.prng.next_f64() < self.imp.ber {
                            *b ^= 1 << bit;
                            flipped = true;
                        }
                    }
                }
            }
            let crc_ok = match self.caps.crc {
                CrcCaps::Internal => !flipped,
                CrcCaps::None => true,
            };
            let deliver_at = f
                .period
                .wrapping_add(self.imp.delay)
                .wrapping_add(self.prng.below(self.imp.jitter + 1));
            self.pending.push_back(Arrival {
                deliver_at,
                frame: Frame { bytes, ..f },
                crc_ok,
            });
        }
        // Deliver in arrival order.
        let mut v: Vec<Arrival> = self.pending.drain(..).collect();
        v.sort_by_key(|x| (x.deliver_at, x.frame.period, x.frame.slot));
        self.pending = v.into_iter().collect();
    }
}

impl Phy for SimPhy {
    type Config = Impairments;

    fn caps(&self) -> &PhyCaps {
        &self.caps
    }
    async fn configure(&mut self, cfg: &Impairments) -> Result<(), PhyError> {
        self.imp = *cfg;
        Ok(())
    }
    async fn set_frequency_khz(&mut self, khz: u32) -> Result<(), PhyError> {
        if khz < self.caps.freq_min_khz
            || khz > self.caps.freq_max_khz
            || (khz - self.caps.freq_min_khz) % self.caps.freq_step_khz != 0
        {
            return Err(PhyError::Config);
        }
        Ok(())
    }
    async fn set_tx_power_dbm(&mut self, dbm: i8) -> Result<(), PhyError> {
        if dbm < self.caps.tx_power_min_dbm || dbm > self.caps.tx_power_max_dbm {
            return Err(PhyError::Config);
        }
        Ok(())
    }
    async fn next_tx_slot(&mut self) -> TxSlot {
        let mut a = self.air.0.lock().unwrap();
        if a.caps.kind == PhyKind::PacketRadio {
            // On demand: every frame is its own period.
            let p = a.tx_period;
            a.tx_period = a.tx_period.wrapping_add(1);
            return TxSlot { slot: 0, period: p };
        }
        let idx = a.tx_next_slot;
        let slot = a.caps.tx_slots[idx].id;
        let period = a.tx_period;
        a.tx_next_slot += 1;
        if a.tx_next_slot >= a.caps.tx_slots.len() {
            a.tx_next_slot = 0;
            a.tx_period = a.tx_period.wrapping_add(1);
        }
        TxSlot { slot, period }
    }
    async fn tx(&mut self, slot: u8, bytes: &[u8]) -> Result<(), PhyError> {
        if self.role != Role::Tx {
            return Err(PhyError::Unsupported);
        }
        let max = self.max_frame_len(slot);
        if max == 0 || bytes.len() > max {
            return Err(PhyError::Frame);
        }
        let mut a = self.air.0.lock().unwrap();
        // The period this frame belongs to is the one `next_tx_slot` handed out.
        let period = if a.caps.kind == PhyKind::PacketRadio || a.tx_next_slot != 0 {
            a.tx_period
        } else {
            a.tx_period.wrapping_sub(1)
        };
        a.sent.push_back(Frame {
            slot,
            period,
            bytes: bytes.to_vec(),
        });
        // Keep the log bounded: a receiver that falls this far behind the
        // transmitter loses the oldest frames (a real radio would too).
        while a.sent.len() > LOG_DEPTH {
            a.sent.pop_front();
            a.sent_base += 1;
        }
        Ok(())
    }
    async fn rx(&mut self, buf: &mut [u8]) -> Result<RxMeta, PhyError> {
        if self.role != Role::Rx {
            return Err(PhyError::Unsupported);
        }
        self.ingest();
        let due = match self.pending.front() {
            Some(a) if self.caps.kind == PhyKind::PacketRadio || a.deliver_at <= self.rx_period => {
                true
            }
            _ => false,
        };
        if !due {
            return Err(PhyError::Timeout);
        }
        let a = self.pending.pop_front().unwrap();
        let n = a.frame.bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&a.frame.bytes[..n]);
        self.delivered += 1;
        Ok(RxMeta {
            slot: a.frame.slot,
            len: n,
            period: a.frame.period,
            rssi_dbm: if self.caps.meta.rssi {
                Some(self.imp.rssi_dbm)
            } else {
                None
            },
            snr_db: if self.caps.meta.snr { Some(20) } else { None },
            antenna: if self.caps.meta.antenna {
                self.branch
            } else {
                0
            },
            crc_ok: a.crc_ok,
            sync_tick: self.caps.sync_tick && a.frame.slot == self.caps.rx_slots[0].id,
        })
    }
    async fn rssi_inst(&mut self) -> Result<i16, PhyError> {
        Ok(self.imp.rssi_dbm)
    }
    fn max_frame_len(&self, slot: u8) -> usize {
        self.caps
            .tx_slot(slot)
            .map(|s| s.bytes as usize)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_on;

    fn send_periods(tx: &mut SimPhy, periods: u32, fill: impl Fn(u32, u8) -> Vec<u8>) {
        let slots = tx.caps().tx_slots.len() as u32;
        for _ in 0..periods * slots {
            let s = block_on(tx.next_tx_slot());
            block_on(tx.tx(s.slot, &fill(s.period, s.slot))).unwrap();
        }
    }

    fn drain(rx: &mut SimPhy, periods: u32) -> Vec<(RxMeta, Vec<u8>)> {
        let mut out = Vec::new();
        let mut buf = [0u8; 128];
        for _ in 0..periods {
            loop {
                match block_on(rx.rx(&mut buf)) {
                    Ok(m) => out.push((m, buf[..m.len].to_vec())),
                    Err(PhyError::Timeout) => break,
                    Err(e) => panic!("{e:?}"),
                }
            }
            rx.tick();
        }
        out
    }

    #[test]
    fn stream_round_trips_every_frame_when_clean() {
        let air = Air::new(shapes::STREAM_1);
        let mut tx = air.transmitter();
        let mut rx = air.receiver(Impairments::CLEAN, 1);
        send_periods(&mut tx, 1000, |p, _| p.to_le_bytes().to_vec());
        let got = drain(&mut rx, 1001);
        assert_eq!(got.len(), 1000);
        for (i, (m, b)) in got.iter().enumerate() {
            assert_eq!(m.period, i as u32);
            assert_eq!(b, &(i as u32).to_le_bytes());
            assert!(m.sync_tick && m.crc_ok && m.rssi_dbm == Some(-60));
        }
    }

    #[test]
    fn loss_model_hits_the_configured_rate_in_bursts() {
        let air = Air::new(shapes::STREAM_1);
        let mut tx = air.transmitter();
        let imp = Impairments {
            loss: 0.002,
            burst: 10,
            ..Impairments::CLEAN
        };
        let mut rx = air.receiver(imp, 7);
        // Interleave like a real link: one period sent, one period received.
        let mut got = Vec::new();
        for _ in 0..100_000 {
            send_periods(&mut tx, 1, |p, _| p.to_le_bytes().to_vec());
            got.extend(drain(&mut rx, 1));
        }
        let lost = 100_000 - got.len();
        // ~0.002 events/period × 10 periods each = ~2 % expected.
        assert!(lost > 1_500 && lost < 2_500, "lost {lost}");
        // Bursts are contiguous: count gaps in the period sequence.
        let mut gaps = 0;
        let mut prev = None;
        for (m, _) in &got {
            if let Some(p) = prev {
                if m.period != p + 1 {
                    gaps += 1;
                }
            }
            prev = Some(m.period);
        }
        assert!(
            gaps * 10 <= lost + 200 && gaps * 10 + 200 >= lost,
            "gaps {gaps} for lost {lost}"
        );
    }

    #[test]
    fn superframe_delivers_per_sub_slot_with_internal_fec_and_antenna() {
        let air = Air::new(shapes::SUPERFRAME_8);
        assert_eq!(air.caps().bytes_per_second(Dir::Down), 8 * 48 * 1000);
        let mut tx = air.transmitter();
        let mut rx = air.receiver(Impairments::CLEAN, 3);
        send_periods(&mut tx, 50, |p, s| vec![s, p as u8]);
        let got = drain(&mut rx, 51);
        assert_eq!(got.len(), 50 * 9);
        for (m, b) in &got {
            assert_eq!(b[0], m.slot);
            assert_eq!(b[1], m.period as u8);
            assert_eq!(m.sync_tick, m.slot == 0);
            assert_eq!(m.antenna, 0);
        }
        assert!(matches!(rx.caps().fec, FecCaps::Internal { .. }));
        // A second branch reports its own antenna id.
        let mut rx2 = air.receiver(Impairments::CLEAN, 4);
        send_periods(&mut tx, 1, |_, s| vec![s]);
        let g2 = drain(&mut rx2, 52);
        assert!(g2.iter().all(|(m, _)| m.antenna == 1));
    }

    #[test]
    fn bit_errors_fail_the_internal_crc_but_pass_through_without_one() {
        for (caps, expect_flag) in [(shapes::PACKET, true), (shapes::STREAM_1, false)] {
            let air = Air::new(caps);
            let mut tx = air.transmitter();
            let mut rx = air.receiver(
                Impairments {
                    ber: 0.01,
                    ..Impairments::CLEAN
                },
                9,
            );
            send_periods(&mut tx, 500, |_, _| vec![0u8; 32]);
            let got = drain(&mut rx, 501);
            assert_eq!(got.len(), 500);
            let corrupted = got
                .iter()
                .filter(|(_, b)| b.iter().any(|&x| x != 0))
                .count();
            assert!(corrupted > 400, "corrupted {corrupted}");
            let flagged = got.iter().filter(|(m, _)| !m.crc_ok).count();
            if expect_flag {
                assert_eq!(flagged, corrupted);
            } else {
                assert_eq!(flagged, 0);
            }
        }
    }

    #[test]
    fn delay_and_jitter_hold_frames_until_due_and_reorder() {
        let air = Air::new(shapes::STREAM_1);
        let mut tx = air.transmitter();
        let mut rx = air.receiver(
            Impairments {
                delay: 3,
                jitter: 2,
                ..Impairments::CLEAN
            },
            5,
        );
        send_periods(&mut tx, 200, |p, _| p.to_le_bytes().to_vec());
        let mut buf = [0u8; 64];
        // Nothing is due before period 3.
        for _ in 0..3 {
            assert_eq!(block_on(rx.rx(&mut buf)).err(), Some(PhyError::Timeout));
            rx.tick();
        }
        let got = drain(&mut rx, 210);
        assert_eq!(got.len(), 200);
        let periods: Vec<u32> = got.iter().map(|(m, _)| m.period).collect();
        let mut sorted = periods.clone();
        sorted.sort();
        assert_ne!(periods, sorted, "jitter should reorder some frames");
    }

    #[test]
    fn packet_radio_is_on_demand_and_rejects_oversize() {
        let air = Air::new(shapes::PACKET);
        let mut tx = air.transmitter();
        let mut rx = air.receiver(Impairments::CLEAN, 2);
        assert_eq!(block_on(tx.tx(0, &[0u8; 65])).err(), Some(PhyError::Frame));
        assert_eq!(block_on(tx.tx(1, &[0u8; 8])).err(), Some(PhyError::Frame));
        assert_eq!(
            block_on(tx.set_frequency_khz(469_999)).err(),
            Some(PhyError::Config)
        );
        assert!(block_on(tx.set_frequency_khz(915_000)).is_ok());
        for i in 0..5u8 {
            let s = block_on(tx.next_tx_slot());
            assert_eq!(s.period, i as u32);
            block_on(tx.tx(0, &[i; 10])).unwrap();
        }
        let mut buf = [0u8; 64];
        for i in 0..5u8 {
            let m = block_on(rx.rx(&mut buf)).unwrap();
            assert_eq!((m.len, buf[0], m.sync_tick), (10, i, false));
        }
        assert_eq!(block_on(rx.rx(&mut buf)).err(), Some(PhyError::Timeout));
    }
}
