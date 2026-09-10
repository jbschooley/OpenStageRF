// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![allow(async_fn_in_trait)]

//! [`Phy`] over the hand-rolled SX1262 driver.
//!
//! A packet radio: on-demand TX slots of up to [`PAYLOAD_MAX`] bytes, chip
//! CRC, no FEC, no sync tick.  RSSI comes back with every packet.  Two
//! instances with different [`Sx126xPhy::with_branch`] ids are what a
//! diversity receiver merges.
//!
//! The adapter borrows the driver so a board's `Resources` keeps owning
//! it; [`Sx126xPhy::release`] hands the borrow back for code that still
//! drives the chip directly (the MIDI runtime, until step A.4).

use embedded_hal::digital::OutputPin;
use embedded_hal_async::{digital::Wait, spi::SpiDevice};
use osrf_phy_api::{
    CrcCaps, Dir, FecCaps, MetaCaps, Phy, PhyCaps, PhyError, PhyKind, RxMeta, SlotDesc, TxSlot,
};
use osrf_radio_sx126x::{
    Error, GfskBandwidth, GfskPulseShape, RadioError, RfSwitchControl, Sx1262Radio,
};

/// Longest payload the adapter accepts (the driver's packet-length field
/// is a byte; the runtimes size their wire buffers to this).
pub const PAYLOAD_MAX: u8 = 64;

static SLOTS: [SlotDesc; 1] = [SlotDesc {
    id: 0,
    bytes: PAYLOAD_MAX as u16,
    dir: Dir::Down,
}];

/// What every SX1262 module offers, independent of configuration.
pub static CAPS: PhyCaps = PhyCaps {
    kind: PhyKind::PacketRadio,
    slot_period_us: 0,
    tx_slots: &SLOTS,
    rx_slots: &SLOTS,
    fec: FecCaps::None,
    crc: CrcCaps::Internal,
    sync_tick: false,
    meta: MetaCaps {
        rssi: true,
        snr: false,
        antenna: false,
    },
    freq_min_khz: 150_000,
    freq_max_khz: 960_000,
    freq_step_khz: 1,
    tx_power_min_dbm: -9,
    tx_power_max_dbm: 22,
};

/// GFSK link parameters.  Must match between transmitter and receiver.
#[derive(Clone, Copy, Debug)]
pub struct Sx126xConfig {
    pub frequency_hz: u32,
    pub bitrate_bps: u32,
    pub deviation_hz: u32,
    pub gfsk_bandwidth: GfskBandwidth,
    pub pulse_shape: GfskPulseShape,
    pub preamble_bits: u16,
    pub sync_word: [u8; 4],
    /// ≤ [`PAYLOAD_MAX`].
    pub payload_max: u8,
    pub tx_power_dbm: i8,
    /// ~3 dB receiver sensitivity for ~0.9 mA; on for anything that listens.
    pub rx_boosted: bool,
}

impl Sx126xConfig {
    /// The Milestone 2 bench setting: 915 MHz, 300 kb/s GFSK, 50 kHz
    /// deviation, 467 kHz receiver bandwidth, BT 0.5, +14 dBm.
    pub const BENCH_915: Sx126xConfig = Sx126xConfig {
        frequency_hz: 915_000_000,
        bitrate_bps: 300_000,
        deviation_hz: 50_000,
        gfsk_bandwidth: GfskBandwidth::Bw4670,
        pulse_shape: GfskPulseShape::Bt05,
        preamble_bits: 16,
        sync_word: [0xC1, 0x94, 0xC1, 0x94],
        payload_max: PAYLOAD_MAX,
        tx_power_dbm: 14,
        rx_boosted: true,
    };
}

pub struct Sx126xPhy<'a, Spi, Busy, Dio1, Reset, Switch>
where
    Spi: SpiDevice,
    Busy: Wait,
    Dio1: Wait,
    Reset: OutputPin,
    Switch: RfSwitchControl,
{
    radio: &'a mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>,
    payload_max: u8,
    tx_period: u32,
    rx_period: u32,
    /// The chip is in continuous RX; `rx` need not re-arm it.
    in_rx: bool,
    branch: u8,
}

fn map_err<S, R>(e: Error<S, R>) -> PhyError {
    match e {
        Error::PayloadTooLarge | Error::BufferTooSmall { .. } | Error::InvalidSyncWord => {
            PhyError::Frame
        }
        Error::Timeout => PhyError::Timeout,
        _ => PhyError::Io,
    }
}

impl<'a, Spi, Busy, Dio1, Reset, Switch> Sx126xPhy<'a, Spi, Busy, Dio1, Reset, Switch>
where
    Spi: SpiDevice,
    Busy: Wait,
    Dio1: Wait,
    Reset: OutputPin,
    Switch: RfSwitchControl,
{
    pub fn new(radio: &'a mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>) -> Self {
        Sx126xPhy {
            radio,
            payload_max: PAYLOAD_MAX,
            tx_period: 0,
            rx_period: 0,
            in_rx: false,
            branch: 0,
        }
    }
    /// Diversity branch id reported in `RxMeta::antenna` (0 = first radio).
    pub fn with_branch(mut self, branch: u8) -> Self {
        self.branch = branch;
        self
    }
    pub fn branch(&self) -> u8 {
        self.branch
    }
    /// Give the driver back.
    pub fn release(self) -> &'a mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch> {
        self.radio
    }
    /// Direct access for code that mixes adapter and driver calls during
    /// the transition (a caller doing so must treat the chip as no longer
    /// in RX: call [`Self::leave_rx`]).
    pub fn radio(&mut self) -> &mut Sx1262Radio<Spi, Busy, Dio1, Reset, Switch> {
        &mut self.radio
    }
    pub fn leave_rx(&mut self) {
        self.in_rx = false;
    }

    async fn apply(&mut self, cfg: &Sx126xConfig) -> Result<(), RadioError<Reset, Switch>> {
        self.radio.init().await?;
        self.radio.set_frequency(cfg.frequency_hz).await?;
        self.radio
            .set_modulation_gfsk(
                cfg.bitrate_bps,
                cfg.deviation_hz,
                cfg.gfsk_bandwidth,
                cfg.pulse_shape,
            )
            .await?;
        self.radio
            .set_packet_format(cfg.preamble_bits, &cfg.sync_word, cfg.payload_max, true)
            .await?;
        self.radio.set_tx_power(cfg.tx_power_dbm).await?;
        if cfg.rx_boosted {
            self.radio.set_rx_boosted(true).await?;
        }
        // RF switch init must be LAST per SX1262 AN1200.36.
        self.radio.finish_init().await?;
        Ok(())
    }
}

impl<'a, Spi, Busy, Dio1, Reset, Switch> Phy for Sx126xPhy<'a, Spi, Busy, Dio1, Reset, Switch>
where
    Spi: SpiDevice,
    Busy: Wait,
    Dio1: Wait,
    Reset: OutputPin,
    Switch: RfSwitchControl,
{
    type Config = Sx126xConfig;

    fn caps(&self) -> &PhyCaps {
        &CAPS
    }

    async fn configure(&mut self, cfg: &Sx126xConfig) -> Result<(), PhyError> {
        if cfg.payload_max == 0 || cfg.payload_max > PAYLOAD_MAX {
            return Err(PhyError::Config);
        }
        if cfg.frequency_hz / 1000 < CAPS.freq_min_khz
            || cfg.frequency_hz / 1000 > CAPS.freq_max_khz
        {
            return Err(PhyError::Config);
        }
        if cfg.tx_power_dbm < CAPS.tx_power_min_dbm || cfg.tx_power_dbm > CAPS.tx_power_max_dbm {
            return Err(PhyError::Config);
        }
        self.payload_max = cfg.payload_max;
        self.in_rx = false;
        self.apply(cfg).await.map_err(|_| PhyError::Io)
    }

    async fn set_frequency_khz(&mut self, khz: u32) -> Result<(), PhyError> {
        if khz < CAPS.freq_min_khz || khz > CAPS.freq_max_khz {
            return Err(PhyError::Config);
        }
        self.in_rx = false;
        self.radio.set_frequency(khz * 1000).await.map_err(map_err)
    }

    async fn set_tx_power_dbm(&mut self, dbm: i8) -> Result<(), PhyError> {
        if dbm < CAPS.tx_power_min_dbm || dbm > CAPS.tx_power_max_dbm {
            return Err(PhyError::Config);
        }
        self.in_rx = false;
        self.radio.set_tx_power(dbm).await.map_err(map_err)
    }

    /// On demand: every call is a new period.
    async fn next_tx_slot(&mut self) -> TxSlot {
        let p = self.tx_period;
        self.tx_period = self.tx_period.wrapping_add(1);
        TxSlot { slot: 0, period: p }
    }

    async fn tx(&mut self, slot: u8, bytes: &[u8]) -> Result<(), PhyError> {
        if slot != 0 || bytes.is_empty() || bytes.len() > self.payload_max as usize {
            return Err(PhyError::Frame);
        }
        self.in_rx = false;
        self.radio.tx(bytes).await.map_err(map_err)
    }

    /// Receive the next packet.  Arms continuous RX on first use and after
    /// any TX or reconfiguration; stays in RX between packets.  A packet
    /// that failed the chip's CRC comes back as `Ok` with `len == 0` and
    /// `crc_ok == false`, so the caller can count it without losing RX.
    async fn rx(&mut self, buf: &mut [u8]) -> Result<RxMeta, PhyError> {
        if !self.in_rx {
            self.radio.rx_start().await.map_err(map_err)?;
            self.in_rx = true;
        }
        let period = self.rx_period;
        self.rx_period = self.rx_period.wrapping_add(1);
        let base = RxMeta {
            slot: 0,
            len: 0,
            period,
            rssi_dbm: None,
            snr_db: None,
            antenna: self.branch,
            crc_ok: true,
            sync_tick: false,
        };
        match self.radio.rx_recv(buf).await {
            Ok(pkt) => Ok(RxMeta {
                len: pkt.len.min(buf.len()),
                rssi_dbm: Some(pkt.rssi_dbm),
                crc_ok: pkt.crc_ok,
                ..base
            }),
            Err(Error::CrcMismatch) => Ok(RxMeta {
                crc_ok: false,
                ..base
            }),
            Err(e) => {
                // The chip may have left RX (unexpected IRQ, bus trouble);
                // re-arm on the next call.
                self.in_rx = false;
                Err(map_err(e))
            }
        }
    }

    async fn rssi_inst(&mut self) -> Result<i16, PhyError> {
        self.radio.get_rssi_inst().await.map_err(map_err)
    }

    fn max_frame_len(&self, slot: u8) -> usize {
        if slot == 0 {
            self.payload_max as usize
        } else {
            0
        }
    }
}
