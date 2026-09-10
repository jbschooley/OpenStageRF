// SPDX-License-Identifier: AGPL-3.0-or-later
#![cfg_attr(not(feature = "std"), no_std)]
#![allow(async_fn_in_trait)]

//! The radio boundary (`PLATFORM_PLAN.md` §4.2, D4).
//!
//! Everything above this crate (transport, audio pump, MIDI runtime,
//! diversity merge) talks to a radio only through [`Phy`] and reads what
//! the radio can do from [`PhyCaps`] once at start-up.  A radio adapter in
//! `drivers/phy/*` implements [`Phy`]; the four planned adapters map onto
//! it like this:
//!
//! | | SX1262 (MIDI) | Si4463 direct mode | Si4463 packet mode | wideband FPGA |
//! |---|---|---|---|---|
//! | [`PhyKind`] | `PacketRadio` | `ContinuousStream` | `PacketRadio` | `Superframe` |
//! | slot period | on demand (0) | 500 µs | 500–1000 µs | ~1 ms superframe |
//! | tx slots | 1 × ≤64 B | 1 × ~40 B | 1 × ≤64 B | 8 down + N up |
//! | FEC | none, software adds | none, software adds | none | internal LDPC |
//! | CRC | internal | none, software adds | internal | internal |
//! | sync tick | no | yes | no | yes |
//!
//! Two things the trait must already express for the wideband PHY to be
//! "just another radio layer" (§4.4): **several sub-slots per period** and
//! **PHY-internal FEC**.  Both are in [`PhyCaps`] and both have simulator
//! tests so they cannot rot.
//!
//! The simulator ([`sim`], feature `sim`, host only) is the PHY the desktop
//! transport tests run against: configurable caps, loss and burst models,
//! bit errors, delay and reorder.

#[cfg(feature = "sim")]
pub mod sim;

/// How the radio presents air time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhyKind {
    /// Send a frame whenever asked; receive whole frames as they arrive.
    PacketRadio,
    /// A continuous bitstream the adapter frames itself; a TX slot opens
    /// every `slot_period_us` and RX delivers one frame per period.
    ContinuousStream,
    /// A fixed superframe with several sub-slots per period.
    Superframe,
}

/// Direction of a slot as seen from the transmitter of the audio (rack / MIDI TX).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// Rack → pack (audio), or MIDI TX → RX.
    Down,
    /// Pack → rack (telemetry, later microphone audio).
    Up,
}

/// One sub-slot of the period.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotDesc {
    pub id: u8,
    /// Payload bytes the slot carries (after the PHY's own framing/FEC).
    pub bytes: u16,
    pub dir: Dir,
}

/// Whether the PHY corrects errors itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FecCaps {
    /// Software (`core/fec`) must add FEC if the profile wants it.
    None,
    /// The PHY corrects errors; software FEC is skipped.  `rate` is the
    /// code rate as (numerator, denominator), informational.
    Internal { rate: (u8, u8) },
}

/// Whether the PHY checks frame integrity itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrcCaps {
    /// Software must add a check; `RxMeta::crc_ok` is always `true`.
    None,
    /// `RxMeta::crc_ok` is meaningful.
    Internal,
}

/// Which measurements come back with a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct MetaCaps {
    pub rssi: bool,
    pub snr: bool,
    /// The PHY has internal diversity and reports the branch in `RxMeta::antenna`.
    pub antenna: bool,
}

/// What a PHY can do.  Read once at init by the slot mapper and runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhyCaps {
    pub kind: PhyKind,
    /// How often a TX slot opens; 0 = on demand (packet radios).
    pub slot_period_us: u32,
    pub tx_slots: &'static [SlotDesc],
    pub rx_slots: &'static [SlotDesc],
    pub fec: FecCaps,
    pub crc: CrcCaps,
    /// Delivers an air-frame tick for clock recovery (`RxMeta::sync_tick`).
    pub sync_tick: bool,
    pub meta: MetaCaps,
    pub freq_min_khz: u32,
    pub freq_max_khz: u32,
    pub freq_step_khz: u32,
    pub tx_power_min_dbm: i8,
    pub tx_power_max_dbm: i8,
}

impl PhyCaps {
    pub fn tx_slot(&self, id: u8) -> Option<&SlotDesc> {
        self.tx_slots.iter().find(|s| s.id == id)
    }
    pub fn rx_slot(&self, id: u8) -> Option<&SlotDesc> {
        self.rx_slots.iter().find(|s| s.id == id)
    }
    /// Payload bytes per second in one direction, summed over the TX
    /// slots (0 for on-demand packet radios, whose rate depends on use).
    pub fn bytes_per_second(&self, dir: Dir) -> u32 {
        if self.slot_period_us == 0 {
            return 0;
        }
        let per_period: u32 = self
            .tx_slots
            .iter()
            .filter(|s| s.dir == dir)
            .map(|s| s.bytes as u32)
            .sum();
        (per_period as u64 * 1_000_000 / self.slot_period_us as u64) as u32
    }
}

/// A TX opportunity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxSlot {
    /// Sub-slot to fill.  Packet radios and single-slot streams use 0.
    pub slot: u8,
    /// Period counter since `configure`; wraps.  Superframe and stream PHYs
    /// open every sub-slot of a period in ascending id before the counter
    /// advances.
    pub period: u32,
}

/// What came with a received frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxMeta {
    pub slot: u8,
    pub len: usize,
    /// Period counter at the receiver; on PHYs with a sync tick it is the
    /// air-frame count and `sync_tick` says this frame carried the tick.
    pub period: u32,
    pub rssi_dbm: Option<i16>,
    pub snr_db: Option<i8>,
    /// Diversity branch that produced the frame (0 when the PHY has none).
    pub antenna: u8,
    /// `false` only when `CrcCaps::Internal` and the check failed; the
    /// bytes are still delivered so software can decide.
    pub crc_ok: bool,
    pub sync_tick: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhyError {
    /// Bad `Config`, frequency or power for this PHY.
    Config,
    /// Bus, pin or chip failure.
    Io,
    /// The slot closed before the frame was handed over, or no frame arrived.
    Timeout,
    /// Frame longer than `max_frame_len(slot)`, or unknown slot id.
    Frame,
    /// Operation not supported by this PHY kind (e.g. `rx` on a TX-only slot).
    Unsupported,
}

/// A radio, as the transport sees it.
pub trait Phy {
    /// Radio-specific settings: GFSK parameters, a PHY id + MCS, or a
    /// superframe layout.  Baked from the profile config.
    type Config;

    fn caps(&self) -> &PhyCaps;
    async fn configure(&mut self, cfg: &Self::Config) -> Result<(), PhyError>;
    async fn set_frequency_khz(&mut self, khz: u32) -> Result<(), PhyError>;
    async fn set_tx_power_dbm(&mut self, dbm: i8) -> Result<(), PhyError>;
    /// Wait for the next TX opportunity.  Packet radios return at once;
    /// stream and superframe PHYs return when the slot opens.
    async fn next_tx_slot(&mut self) -> TxSlot;
    /// Hand `bytes` to the PHY for `slot`.  Must be called before the slot's
    /// deadline; the PHY frames, checks and (if internal) codes them.
    async fn tx(&mut self, slot: u8, bytes: &[u8]) -> Result<(), PhyError>;
    /// Receive the next frame into `buf`.
    async fn rx(&mut self, buf: &mut [u8]) -> Result<RxMeta, PhyError>;
    async fn rssi_inst(&mut self) -> Result<i16, PhyError>;
    /// Longest payload `tx(slot, …)` accepts.
    fn max_frame_len(&self, slot: u8) -> usize;
}

/// Drive a future that never actually waits (the simulator, unit tests).
/// Panics if the future pends: a real PHY must run on an executor.
#[cfg(feature = "std")]
pub fn block_on<F: core::future::Future>(f: F) -> F::Output {
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    fn noop(_: *const ()) {}
    fn clone(p: *const ()) -> RawWaker {
        RawWaker::new(p, &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    let mut f = core::pin::pin!(f);
    match f.as_mut().poll(&mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("block_on: future pended; this PHY needs an executor"),
    }
}
