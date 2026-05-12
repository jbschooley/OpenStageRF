// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
// Trait methods are async on purpose (board-specific switch impls do GPIO
// toggles which await embassy-nrf futures); we don't expose this driver to
// `Send`-required runtimes so the auto-trait warning is just noise.
#![allow(async_fn_in_trait)]

//! Hand-rolled SX1262 driver — raw SPI commands per Semtech datasheet
//! DS_SX1261-2_V2.1, Table 12-1.  Replaces the previous `sx1262 = "0.3"`
//! dependency, which had two crippling issues for our chip variant:
//!   1. `Status::from_bytes(...).unwrap()` panicked on `cmd_status` values
//!      0 (Reserved) and 1 (RFU), both of which the chip returns in normal
//!      operation.
//!   2. No exposure of raw register access, so we couldn't apply the
//!      mandatory TxClampConfig workaround (datasheet §15.2).
//!
//! The driver is generic over `embedded_hal_async::spi::SpiDevice` (so
//! either ExclusiveDevice + ChipSelect or any other CS strategy works),
//! `Wait` for both BUSY and DIO1 (we wait for BUSY low between commands
//! per datasheet §8.3.1, and DIO1 high for IRQ delivery), an `OutputPin`
//! for NRESET, and a per-board `RfSwitchControl` impl.
//!
//! Commands implemented (covers GFSK TX/RX bench-test path):
//!   SetStandby, SetDio3AsTcxoCtrl, SetPacketType, SetDioIrqParams,
//!   CalibrateImage, SetRfFrequency, SetModulationParams (GFSK only),
//!   WriteRegister (sync word, TxClampConfig), SetPacketParams (GFSK),
//!   SetPaConfig, SetTxParams, SetDio2AsRfSwitchCtrl, SetBufferBaseAddress,
//!   WriteBuffer, ClearIrqStatus, SetTx, SetRx, GetStatus, GetIrqStatus,
//!   GetRxBufferStatus, GetPacketStatus, ReadBuffer, SetTxContinuousWave.
//!
//! Not implemented (yet): LoRa modulation, AddressFiltering,
//! Whitening config (we always write whitening=0), CAD, Sleep, Fs.

use embedded_hal::digital::OutputPin;
use embedded_hal_async::{digital::Wait, spi::SpiDevice};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Error<SwitchErr, ResetErr> {
    /// SPI transfer failed.
    Spi,
    /// NRESET pin write failed.
    Reset(ResetErr),
    /// RF-switch op failed.
    Switch(SwitchErr),
    /// Generic bus / pin wait failure.
    Bus,
    /// Caller passed too-long payload.
    PayloadTooLarge,
    /// Caller-supplied buffer too small for received packet.
    BufferTooSmall,
    /// Sync word > 8 bytes.
    InvalidSyncWord,
    /// CRC of received packet didn't match.
    CrcMismatch,
    /// Got an IRQ we weren't expecting.
    UnexpectedIrq(u16),
    /// Timed out waiting for an event.
    Timeout,
}

pub type RadioError<Reset, Switch> = Error<
    <Switch as RfSwitchControl>::Error,
    <Reset as embedded_hal::digital::ErrorType>::Error,
>;

// ---------------------------------------------------------------------------
// RF switch abstraction (board-specific)
// ---------------------------------------------------------------------------

/// Per-board RF switch control.  T114 wires DIO2 to a UPG2179 RF switch and
/// the SX1262 drives it autonomously after `SetDio2AsRfSwitchCtrl(true)`;
/// DX-LR30 has dedicated TXEN / RXEN GPIOs that the host toggles around
/// `set_tx`/`set_rx`.
pub trait RfSwitchControl {
    type Error;

    /// Called once during radio init, after all RF config is in place.
    /// Dio2 variant: SetDio2AsRfSwitchCtrl is sent by the *driver* (since it
    /// needs SPI access), and this hook is a no-op.  Pin variant: drive
    /// both pins low.
    async fn init(&mut self) -> Result<(), Self::Error>;
    async fn before_tx(&mut self) -> Result<(), Self::Error>;
    async fn before_rx(&mut self) -> Result<(), Self::Error>;
    async fn to_idle(&mut self) -> Result<(), Self::Error>;

    /// True if this is the DIO2-driven variant — the driver will issue
    /// `SetDio2AsRfSwitchCtrl` itself after switch.init().
    fn uses_dio2(&self) -> bool;
}

/// DIO2 drives an external RF switch IC autonomously (T114 / Heltec
/// LR1262 module).  Driver issues `SetDio2AsRfSwitchCtrl(true)` once.
pub struct Dio2RfSwitch;

impl RfSwitchControl for Dio2RfSwitch {
    type Error = core::convert::Infallible;
    async fn init(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
    async fn before_tx(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
    async fn before_rx(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
    async fn to_idle(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
    fn uses_dio2(&self) -> bool {
        true
    }
}

/// Two-pin RF switch (DX-LR30 / LR1262-SP module): TXEN and RXEN are
/// regular GPIOs the host drives around tx/rx.
pub struct PinRfSwitch<Txen: OutputPin, Rxen: OutputPin> {
    txen: Txen,
    rxen: Rxen,
}

impl<Txen: OutputPin, Rxen: OutputPin> PinRfSwitch<Txen, Rxen> {
    pub fn new(txen: Txen, rxen: Rxen) -> Self {
        Self { txen, rxen }
    }
}

#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum PinSwitchError<TxenErr, RxenErr> {
    Txen(TxenErr),
    Rxen(RxenErr),
}

impl<Txen: OutputPin, Rxen: OutputPin> RfSwitchControl for PinRfSwitch<Txen, Rxen> {
    type Error = PinSwitchError<
        <Txen as embedded_hal::digital::ErrorType>::Error,
        <Rxen as embedded_hal::digital::ErrorType>::Error,
    >;

    async fn init(&mut self) -> Result<(), Self::Error> {
        self.txen.set_low().map_err(PinSwitchError::Txen)?;
        self.rxen.set_low().map_err(PinSwitchError::Rxen)?;
        Ok(())
    }
    async fn before_tx(&mut self) -> Result<(), Self::Error> {
        self.rxen.set_low().map_err(PinSwitchError::Rxen)?;
        self.txen.set_high().map_err(PinSwitchError::Txen)?;
        Ok(())
    }
    async fn before_rx(&mut self) -> Result<(), Self::Error> {
        self.txen.set_low().map_err(PinSwitchError::Txen)?;
        self.rxen.set_high().map_err(PinSwitchError::Rxen)?;
        Ok(())
    }
    async fn to_idle(&mut self) -> Result<(), Self::Error> {
        self.txen.set_low().map_err(PinSwitchError::Txen)?;
        self.rxen.set_low().map_err(PinSwitchError::Rxen)?;
        Ok(())
    }
    fn uses_dio2(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// GFSK enums (kept for API stability with radio_bench)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum GfskBandwidth {
    Bw4800   = 0x1F,
    Bw5800   = 0x17,
    Bw7300   = 0x0F,
    Bw9700   = 0x1E,
    Bw11700  = 0x16,
    Bw14600  = 0x0E,
    Bw19500  = 0x1D,
    Bw23400  = 0x15,
    Bw29300  = 0x0D,
    Bw39000  = 0x1C,
    Bw46900  = 0x14,
    Bw58600  = 0x0C,
    Bw78200  = 0x1B,
    Bw93800  = 0x13,
    Bw117300 = 0x0B,
    Bw156200 = 0x1A,
    Bw187200 = 0x12,
    Bw234300 = 0x0A,
    Bw312000 = 0x19,
    Bw373600 = 0x11,
    Bw467000 = 0x09,
}

impl GfskBandwidth {
    /// Short alias for `Bw467000`.  The radio_bench config uses this name.
    #[allow(non_upper_case_globals)]
    pub const Bw4670: Self = Self::Bw467000;
}

#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum GfskPulseShape {
    Off  = 0x00,
    Bt03 = 0x08,
    Bt05 = 0x09,
    Bt07 = 0x0A,
    Bt1  = 0x0B,
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

pub struct Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>
where
    Spi: SpiDevice,
    Busy: Wait,
    Dio1: Wait,
    Reset: OutputPin,
    Switch: RfSwitchControl,
{
    spi: Spi,
    busy: Busy,
    dio1: Dio1,
    reset: Reset,
    switch: Switch,
    preamble_len: u16,
    sync_word_bits: u8,
    payload_max_len: u8,
    crc_on: bool,
}

#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct RxPacket {
    pub len: usize,
    pub rssi_dbm: i16,
    pub snr_db: i16,
    pub crc_ok: bool,
}

// ---- Command opcodes (datasheet table 12-1) ----
const CMD_SET_SLEEP: u8 = 0x84;
const CMD_SET_STANDBY: u8 = 0x80;
const CMD_SET_TX: u8 = 0x83;
const CMD_SET_RX: u8 = 0x82;
const CMD_SET_TX_CW: u8 = 0xD1;
const CMD_SET_RF_FREQ: u8 = 0x86;
const CMD_SET_PACKET_TYPE: u8 = 0x8A;
const CMD_SET_TX_PARAMS: u8 = 0x8E;
const CMD_SET_PA_CONFIG: u8 = 0x95;
const CMD_SET_BUFFER_BASE: u8 = 0x8F;
const CMD_SET_MOD_PARAMS: u8 = 0x8B;
const CMD_SET_PACKET_PARAMS: u8 = 0x8C;
const CMD_SET_DIO_IRQ_PARAMS: u8 = 0x08;
const CMD_GET_IRQ_STATUS: u8 = 0x12;
const CMD_CLEAR_IRQ_STATUS: u8 = 0x02;
const CMD_GET_STATUS: u8 = 0xC0;
const CMD_GET_RX_BUFFER_STATUS: u8 = 0x13;
const CMD_GET_PACKET_STATUS: u8 = 0x14;
const CMD_GET_RSSI_INST: u8 = 0x15;
const CMD_CALIBRATE_IMAGE: u8 = 0x98;
const CMD_SET_DIO3_AS_TCXO_CTRL: u8 = 0x97;
const CMD_SET_DIO2_AS_RF_SW: u8 = 0x9D;
const CMD_SET_REGULATOR_MODE: u8 = 0x96;
const CMD_SET_RX_TX_FALLBACK_MODE: u8 = 0x93;
const CMD_WRITE_REGISTER: u8 = 0x0D;
const CMD_READ_REGISTER: u8 = 0x1D;
const CMD_WRITE_BUFFER: u8 = 0x0E;
const CMD_READ_BUFFER: u8 = 0x1E;

// ---- Register addresses we touch ----
const REG_TX_CLAMP_CONFIG: u16 = 0x08D8;
const REG_SYNC_WORD_BASE: u16 = 0x06C0;
/// RX gain control register (datasheet table 13-25).  Default
/// `0x94` (rx_default).  Setting `0x96` enables "rx_boosted" mode
/// — an extra LNA gain stage that improves sensitivity by ~3 dB
/// at a cost of ~0.9 mA additional RX-mode supply current.
const REG_RX_GAIN: u16 = 0x08AC;
const RX_GAIN_DEFAULT: u8 = 0x94;
const RX_GAIN_BOOSTED: u8 = 0x96;

impl<Spi, Busy, Dio1, Reset, Switch> Sx1262Radio<Spi, Busy, Dio1, Reset, Switch>
where
    Spi: SpiDevice,
    Busy: Wait,
    Dio1: Wait,
    Reset: OutputPin,
    Switch: RfSwitchControl,
{
    pub fn new(spi: Spi, busy: Busy, dio1: Dio1, reset: Reset, switch: Switch) -> Self {
        Self {
            spi,
            busy,
            dio1,
            reset,
            switch,
            preamble_len: 32,
            sync_word_bits: 0,
            payload_max_len: 255,
            crc_on: true,
        }
    }

    pub fn release(self) -> (Spi, Busy, Dio1, Reset, Switch) {
        (self.spi, self.busy, self.dio1, self.reset, self.switch)
    }

    // ---- Low-level SPI primitives ----

    /// Wait for BUSY low.  Per datasheet §8.3.1 this must come before every
    /// SPI command.  After a CS-high we add a brief grace delay so BUSY
    /// has time to *rise*; without it, BUSY may still be low at the
    /// moment of the next call and our `wait_for_low` would no-op even
    /// though the chip is about to start processing.
    async fn wait_busy(&mut self) -> Result<(), RadioError<Reset, Switch>> {
        embassy_time::Timer::after_micros(20).await;
        self.busy.wait_for_low().await.map_err(|_| Error::Bus)
    }

    /// Send a write-only command: opcode + parameters.  No response read.
    async fn cmd(&mut self, opcode: u8, params: &[u8]) -> Result<(), RadioError<Reset, Switch>> {
        self.wait_busy().await?;
        // Build a small stack buffer.  Max command is SetPacketParams (9
        // params), plus opcode = 10 bytes.  Allow up to 16 for headroom.
        let mut buf = [0u8; 16];
        buf[0] = opcode;
        let total = 1 + params.len();
        if total > buf.len() {
            return Err(Error::PayloadTooLarge);
        }
        buf[1..total].copy_from_slice(params);
        self.spi.write(&buf[..total]).await.map_err(|_| Error::Spi)
    }

    /// Send a command and read its response.  Caller supplies opcode +
    /// params and a response buffer.  The chip returns the status byte
    /// in place of the response's first byte (we read 1 + N bytes back).
    /// Returns the chip's raw status byte plus fills `response` with the
    /// N response bytes.
    async fn cmd_read(
        &mut self,
        opcode: u8,
        params: &[u8],
        response: &mut [u8],
    ) -> Result<u8, RadioError<Reset, Switch>> {
        self.wait_busy().await?;
        // Single full-duplex transfer: TX = [opcode, params, NOPs...],
        // RX = [garbage, status, response...] (the status byte arrives in
        // the slot AFTER the opcode/params per datasheet).
        let mut tx = [0u8; 16];
        let mut rx = [0u8; 16];
        let prelude = 1 + params.len();
        let total = prelude + 1 + response.len(); // +1 for status byte
        if total > tx.len() {
            return Err(Error::PayloadTooLarge);
        }
        tx[0] = opcode;
        tx[1..prelude].copy_from_slice(params);
        // Remaining tx bytes stay 0 (NOP).
        self.spi
            .transfer(&mut rx[..total], &tx[..total])
            .await
            .map_err(|_| Error::Spi)?;
        let status = rx[prelude];
        response.copy_from_slice(&rx[prelude + 1..total]);
        Ok(status)
    }

    /// WriteRegister opcode: opcode + 16-bit BE address + data.
    async fn write_register(
        &mut self,
        addr: u16,
        data: &[u8],
    ) -> Result<(), RadioError<Reset, Switch>> {
        self.wait_busy().await?;
        let mut buf = [0u8; 16];
        buf[0] = CMD_WRITE_REGISTER;
        buf[1] = (addr >> 8) as u8;
        buf[2] = addr as u8;
        let total = 3 + data.len();
        if total > buf.len() {
            return Err(Error::PayloadTooLarge);
        }
        buf[3..total].copy_from_slice(data);
        self.spi.write(&buf[..total]).await.map_err(|_| Error::Spi)
    }

    /// ReadRegister opcode: opcode + 16-bit BE address + 1 NOP (status) +
    /// N NOPs (data).  Returns the N data bytes (status byte is discarded).
    async fn read_register(
        &mut self,
        addr: u16,
        out: &mut [u8],
    ) -> Result<(), RadioError<Reset, Switch>> {
        self.wait_busy().await?;
        let mut tx = [0u8; 16];
        let mut rx = [0u8; 16];
        tx[0] = CMD_READ_REGISTER;
        tx[1] = (addr >> 8) as u8;
        tx[2] = addr as u8;
        // tx[3] is NOP for the status byte slot; tx[4..] are NOPs for data.
        let total = 4 + out.len();
        if total > tx.len() {
            return Err(Error::PayloadTooLarge);
        }
        self.spi
            .transfer(&mut rx[..total], &tx[..total])
            .await
            .map_err(|_| Error::Spi)?;
        out.copy_from_slice(&rx[4..total]);
        Ok(())
    }

    /// WriteBuffer: opcode + offset + data.  Used for TX payload.
    async fn write_buffer(
        &mut self,
        offset: u8,
        data: &[u8],
    ) -> Result<(), RadioError<Reset, Switch>> {
        self.wait_busy().await?;
        // Use one transaction by chaining on a heapless buffer.  Max
        // payload is bounded by `payload_max_len` (≤ 255 per chip).  For
        // the bench-test path (8-byte payload) a 64-byte stack buffer
        // is plenty.  Accept up to 254 bytes here (256 - 2 = opcode+offset).
        let mut buf = [0u8; 256];
        buf[0] = CMD_WRITE_BUFFER;
        buf[1] = offset;
        let total = 2 + data.len();
        if total > buf.len() {
            return Err(Error::PayloadTooLarge);
        }
        buf[2..total].copy_from_slice(data);
        self.spi.write(&buf[..total]).await.map_err(|_| Error::Spi)
    }

    /// ReadBuffer: opcode + offset + 1 NOP (status) + N NOPs (data).
    async fn read_buffer(
        &mut self,
        offset: u8,
        out: &mut [u8],
    ) -> Result<(), RadioError<Reset, Switch>> {
        self.wait_busy().await?;
        let mut tx = [0u8; 256];
        let mut rx = [0u8; 256];
        tx[0] = CMD_READ_BUFFER;
        tx[1] = offset;
        // tx[2] is NOP for status, tx[3..] are NOPs for data.
        let total = 3 + out.len();
        if total > tx.len() {
            return Err(Error::PayloadTooLarge);
        }
        self.spi
            .transfer(&mut rx[..total], &tx[..total])
            .await
            .map_err(|_| Error::Spi)?;
        out.copy_from_slice(&rx[3..total]);
        Ok(())
    }

    // ---- Public configuration API ----

    /// Reset the chip and run the basic init sequence.  Must be called
    /// FIRST before any other config method.
    ///
    /// Sequence:
    ///   1. Pulse NRESET low ≥100 µs, release, wait BUSY low (~3 ms POR cal).
    ///   2. SetStandby(STBY_RC).
    ///   3. SetDio3AsTcxoCtrl(1.8 V, 5 ms) — Heltec/RAK SX1262 modules need this
    ///      before any RF op or every SetTx fails with cmd_status=5.
    ///   4. SetPacketType(GFSK).
    ///   5. SetDioIrqParams (TX_DONE | RX_DONE | CRC_ERROR | TIMEOUT on DIO1).
    pub async fn init(&mut self) -> Result<(), RadioError<Reset, Switch>> {
        // Reset pulse.
        self.reset.set_low().map_err(Error::Reset)?;
        embassy_time::Timer::after_micros(200).await;
        self.reset.set_high().map_err(Error::Reset)?;
        // POR runs internal calibration; BUSY will go low when complete.
        self.wait_busy().await?;

        // SetStandby(STBY_RC = 0).
        self.cmd(CMD_SET_STANDBY, &[0x00]).await?;

        // SetRegulatorMode(DC-DC + LDO = 0x01).  Default after POR is
        // LDO-only, which can underpower the PA at +14 dBm and above and
        // cause the chip to silently reject SetTx after TX_DONE finishes
        // (every-other-TX pattern observed without this).  Heltec T114
        // hardware supports DC-DC (LR1262 module).
        self.cmd(CMD_SET_REGULATOR_MODE, &[0x01]).await?;

        // SetDio3AsTcxoCtrl: voltage = V1_8 (0x02), delay = 320 (5 ms in
        // 15.625 µs steps) — Heltec T114 wires DIO3 to TCXO power.  MUST
        // come before any RF or calibration command.
        self.cmd(
            CMD_SET_DIO3_AS_TCXO_CTRL,
            &[0x02, 0x00, 0x00, 0x01, 0x40],
        )
        .await?;

        // SetRxTxFallbackMode(FS = 0x40): after TX_DONE / RX_DONE, chip
        // returns to FS (PLL locked) instead of STBY_RC.  Faster restart
        // for the next TX, and avoids the chip entering some sub-state
        // that rejects subsequent SetTx with cmd_status=5.
        self.cmd(CMD_SET_RX_TX_FALLBACK_MODE, &[0x40]).await?;

        // SetPacketType(GFSK = 0).
        self.cmd(CMD_SET_PACKET_TYPE, &[0x00]).await?;

        // SetDioIrqParams: DIO1 fires on TX_DONE (bit 0) | RX_DONE (bit 1) |
        // CRC_ERROR (bit 6) | TIMEOUT (bit 9).  Mask = 0x0243.
        let mask: u16 = 0x0243;
        let mh = (mask >> 8) as u8;
        let ml = mask as u8;
        self.cmd(
            CMD_SET_DIO_IRQ_PARAMS,
            &[mh, ml, mh, ml, 0x00, 0x00, 0x00, 0x00],
        )
        .await?;

        Ok(())
    }

    pub async fn set_frequency(
        &mut self,
        hz: u32,
    ) -> Result<(), RadioError<Reset, Switch>> {
        let (f1, f2) = image_cal_band(hz);
        self.cmd(CMD_CALIBRATE_IMAGE, &[f1, f2]).await?;
        // Calibrate takes a few ms; wait_busy at start of next command will block.

        // SetRfFrequency: register value = (hz * 2^25) / 32_000_000.
        let reg = (((hz as u64) << 25) / 32_000_000) as u32;
        self.cmd(CMD_SET_RF_FREQ, &reg.to_be_bytes()).await
    }

    pub async fn set_modulation_gfsk(
        &mut self,
        bitrate_bps: u32,
        deviation_hz: u32,
        bandwidth: GfskBandwidth,
        pulse_shape: GfskPulseShape,
    ) -> Result<(), RadioError<Reset, Switch>> {
        // SetModulationParams (GFSK), datasheet 13.4.5:
        //   [0..3] BR = (32 * F_XTAL) / bitrate, BE u24
        //   [3]    pulse shape
        //   [4]    bandwidth
        //   [5..8] FDEV = (dev * 2^25) / F_XTAL, BE u24
        let br_reg = ((32u64 * 32_000_000) / bitrate_bps as u64) as u32; // 24-bit
        let fdev_reg = (((deviation_hz as u64) << 25) / 32_000_000) as u32; // 24-bit
        let mut p = [0u8; 8];
        p[0..3].copy_from_slice(&br_reg.to_be_bytes()[1..4]);
        p[3] = pulse_shape as u8;
        p[4] = bandwidth as u8;
        p[5..8].copy_from_slice(&fdev_reg.to_be_bytes()[1..4]);
        self.cmd(CMD_SET_MOD_PARAMS, &p).await
    }

    pub async fn set_packet_format(
        &mut self,
        preamble_len: u16,
        sync_word: &[u8],
        payload_max_len: u8,
        crc_on: bool,
    ) -> Result<(), RadioError<Reset, Switch>> {
        if sync_word.len() > 8 {
            return Err(Error::InvalidSyncWord);
        }
        let mut sync = [0u8; 8];
        sync[..sync_word.len()].copy_from_slice(sync_word);
        self.write_register(REG_SYNC_WORD_BASE, &sync).await?;

        self.preamble_len = preamble_len;
        self.sync_word_bits = (sync_word.len() as u8) * 8;
        self.payload_max_len = payload_max_len;
        self.crc_on = crc_on;

        self.write_packet_params(payload_max_len).await
    }

    pub async fn set_tx_power(
        &mut self,
        dbm: i8,
    ) -> Result<(), RadioError<Reset, Switch>> {
        let dbm = dbm.clamp(-9, 22);
        // PA preset matched to output level (datasheet table 13-21).
        let (duty_cycle, hp_max) = match dbm {
            d if d >= 22 => (0x04, 0x07),
            d if d >= 20 => (0x03, 0x05),
            d if d >= 17 => (0x02, 0x03),
            _ => (0x02, 0x02),
        };
        // SetPaConfig: [duty, hp_max, dev_sel=0x00 (sx1262), pa_lut=0x01].
        self.cmd(CMD_SET_PA_CONFIG, &[duty_cycle, hp_max, 0x00, 0x01])
            .await?;

        // TxClampConfig workaround (datasheet §15.2): RMW reg 0x08D8 |= 0x1E.
        let mut clamp = [0u8; 1];
        self.read_register(REG_TX_CLAMP_CONFIG, &mut clamp).await?;
        clamp[0] |= 0x1E;
        self.write_register(REG_TX_CLAMP_CONFIG, &clamp).await?;

        // SetTxParams: [power, ramp_time].  ramp = Micros200 = 0x04.
        self.cmd(CMD_SET_TX_PARAMS, &[dbm as u8, 0x04]).await
    }

    /// Finish init.  Issue `SetDio2AsRfSwitchCtrl(true)` for the Dio2
    /// variant after all RF config is in place; or call into the pin
    /// switch's idle init for the two-pin variant.
    pub async fn finish_init(&mut self) -> Result<(), RadioError<Reset, Switch>> {
        if self.switch.uses_dio2() {
            self.cmd(CMD_SET_DIO2_AS_RF_SW, &[0x01]).await?;
        }
        self.switch.init().await.map_err(Error::Switch)?;
        Ok(())
    }

    // ---- TX / RX / status ----

    /// Read raw `(mode, cmd_status)` from `GetStatus`.  Mode = bits 6:4,
    /// cmd_status = bits 3:1 of the chip's status byte.
    pub async fn get_status_raw(&mut self) -> Result<(u8, u8), RadioError<Reset, Switch>> {
        // GetStatus returns the status byte itself as the response slot,
        // not after — special case per datasheet 13.5.1.  We just send
        // 0xC0 and read 1 byte.
        self.wait_busy().await?;
        let mut rx = [0u8; 2];
        let tx = [CMD_GET_STATUS, 0x00];
        self.spi
            .transfer(&mut rx, &tx)
            .await
            .map_err(|_| Error::Spi)?;
        let status = rx[1];
        Ok(((status >> 4) & 0x07, (status >> 1) & 0x07))
    }

    /// Read pending IRQ bitmap via `GetIrqStatus`.
    pub async fn get_irq_raw(&mut self) -> Result<u16, RadioError<Reset, Switch>> {
        let mut buf = [0u8; 2];
        self.cmd_read(CMD_GET_IRQ_STATUS, &[], &mut buf).await?;
        Ok(u16::from_be_bytes(buf))
    }

    /// DIAGNOSTIC: enter continuous wave TX mode (no packet, no modulation).
    /// Used to verify the chip's TX path works at all.
    pub async fn dbg_tx_cw(&mut self) -> Result<(), RadioError<Reset, Switch>> {
        self.cmd(CMD_SET_TX_CW, &[]).await
    }

    pub async fn tx(
        &mut self,
        payload: &[u8],
    ) -> Result<(), RadioError<Reset, Switch>> {
        if payload.len() > self.payload_max_len as usize || payload.len() > 255 {
            return Err(Error::PayloadTooLarge);
        }

        self.switch.before_tx().await.map_err(Error::Switch)?;

        // SetBufferBaseAddress(tx=0, rx=0).
        self.cmd(CMD_SET_BUFFER_BASE, &[0x00, 0x00]).await?;
        self.write_buffer(0, payload).await?;

        // Update payload_length in SetPacketParams.
        self.write_packet_params(payload.len() as u8).await?;

        // ClearIrqStatus(all = 0xFFFF).
        self.cmd(CMD_CLEAR_IRQ_STATUS, &[0xFF, 0xFF]).await?;

        // SetTx with timeout = 0 (no timeout).
        self.cmd(CMD_SET_TX, &[0x00, 0x00, 0x00]).await?;

        // Wait for DIO1 to fire (TX_DONE / RX_DONE / CRC_ERROR / TIMEOUT).
        self.dio1.wait_for_high().await.map_err(|_| Error::Bus)?;

        let irq = self.get_irq_raw().await?;
        // ClearIrqStatus(all).
        self.cmd(CMD_CLEAR_IRQ_STATUS, &[0xFF, 0xFF]).await?;

        self.switch.to_idle().await.map_err(Error::Switch)?;
        // NOTE: do NOT call `SetStandby(RC)` here.  With
        // RxTxFallbackMode = FS the chip is already back in FS (PLL locked,
        // PA off) — perfect state for the next TX.  Empirically, calling
        // SetStandby(RC) after TX_DONE leaves the chip in a sub-state
        // that rejects the *next* SetTx with cmd_status=5; only after
        // ~3 seconds of idle does the next SetTx succeed.

        // bit 0 = TX_DONE
        if irq & 0x0001 != 0 {
            Ok(())
        } else {
            Err(Error::UnexpectedIrq(irq))
        }
    }

    /// One-time RX setup.  Puts the chip in continuous RX mode (timeout
    /// 0xFFFFFF) and configures the packet length filter.  Idempotent —
    /// safe to call again from RX mode (acts as a refresh).
    ///
    /// Call ONCE at the start of receive operation, then call
    /// [`Self::rx_recv`] in a loop.  Don't go to standby between packets:
    /// the ~400 µs of TX → RX transition time is enough to miss the next
    /// packet on busy links.
    pub async fn rx_start(&mut self) -> Result<(), RadioError<Reset, Switch>> {
        self.switch.before_rx().await.map_err(Error::Switch)?;
        self.write_packet_params(self.payload_max_len).await?;
        self.cmd(CMD_CLEAR_IRQ_STATUS, &[0xFF, 0xFF]).await?;
        self.cmd(CMD_SET_RX, &[0xFF, 0xFF, 0xFF]).await?;
        Ok(())
    }

    /// Wait for the next packet.  Chip stays in continuous RX after.
    /// Caller is responsible for `rx_start` once before the loop and
    /// (eventually) calling some other state-changing method (`tx`,
    /// `set_standby`, etc.) to leave RX.
    pub async fn rx_recv(
        &mut self,
        buf: &mut [u8],
    ) -> Result<RxPacket, RadioError<Reset, Switch>> {
        self.dio1.wait_for_high().await.map_err(|_| Error::Bus)?;

        let irq = self.get_irq_raw().await?;
        self.cmd(CMD_CLEAR_IRQ_STATUS, &[0xFF, 0xFF]).await?;

        let crc_err = irq & 0x0040 != 0; // bit 6
        let rx_done = irq & 0x0002 != 0; // bit 1

        if rx_done {
            // GetRxBufferStatus: returns [status, payload_len, rx_start_buffer_pointer].
            let mut bs = [0u8; 2];
            self.cmd_read(CMD_GET_RX_BUFFER_STATUS, &[], &mut bs).await?;
            let payload_len = bs[0] as usize;
            let rx_start = bs[1];
            if payload_len > buf.len() {
                return Err(Error::BufferTooSmall);
            }
            self.read_buffer(rx_start, &mut buf[..payload_len]).await?;
            // GetPacketStatus (FSK): [status, RxStatus, RssiSync, RssiAvg].
            let mut ps = [0u8; 3];
            self.cmd_read(CMD_GET_PACKET_STATUS, &[], &mut ps).await?;
            let rssi_sync = ps[1]; // raw
            let rssi_dbm = -((rssi_sync as i16) >> 1);
            Ok(RxPacket {
                len: payload_len,
                rssi_dbm,
                snr_db: 0,
                crc_ok: !crc_err,
            })
        } else if crc_err {
            Err(Error::CrcMismatch)
        } else {
            Err(Error::UnexpectedIrq(irq))
        }
    }

    /// Toggle the SX1262's RX gain register between `rx_default`
    /// (0x94) and `rx_boosted` (0x96).  Boosted adds an extra LNA
    /// gain stage and is worth ~3 dB receiver sensitivity at the
    /// cost of ~0.9 mA additional RX-mode supply current
    /// (datasheet §9.4 "RX Gain Setting" + table 11-7 supply
    /// current figures).  Caveat per datasheet: the boost setting
    /// is wiped by the chip on every wake from sleep, so callers
    /// applying it once at boot are good as long as the chip
    /// stays out of `SLEEP` mode (we never enter it; STBY_RC and
    /// STBY_XOSC preserve the register).
    pub async fn set_rx_boosted(
        &mut self,
        boosted: bool,
    ) -> Result<(), RadioError<Reset, Switch>> {
        let val = if boosted {
            RX_GAIN_BOOSTED
        } else {
            RX_GAIN_DEFAULT
        };
        self.write_register(REG_RX_GAIN, &[val]).await
    }

    /// Move the chip to STDBY_RC.  Required before any of the
    /// `Set*` commands that touch RF parameters (`SetRfFrequency`,
    /// `SetModulationParams`, `SetPacketParams`); the chip silently
    /// drops them otherwise.  Used by the channel-scan path in
    /// `osrf-link-runtime` to walk a band plan via repeated
    /// standby → set_frequency → rx → sample cycles.
    pub async fn set_standby_rc(&mut self) -> Result<(), RadioError<Reset, Switch>> {
        self.switch.to_idle().await.map_err(Error::Switch)?;
        self.cmd(CMD_SET_STANDBY, &[0x00]).await
    }

    /// Move the chip to its lowest-power sleep state (cold start, no
    /// RTC).  Quiescent draw drops from ~600 µA (STDBY_RC) to ~160 nA
    /// per the SX1262 datasheet.  Configuration registers ARE lost —
    /// wake-up via NSS pulse goes through the full init path.  This
    /// is the correct teardown for deep soft-off where the host CPU
    /// is about to System OFF: we won't be needing the radio until
    /// the next cold boot, which re-runs `configure_radio` anyway.
    ///
    /// Sleep config byte: bit 0 = `wakeup_rtc` (0 = no RTC),
    /// bit 2 = `warm_start` (0 = cold start, lose config).  We
    /// choose cold + no-RTC for minimum current.
    pub async fn set_sleep(&mut self) -> Result<(), RadioError<Reset, Switch>> {
        self.switch.to_idle().await.map_err(Error::Switch)?;
        self.cmd(CMD_SET_SLEEP, &[0x00]).await
    }

    /// `SetRfFrequency` only — skips the `CalibrateImage` step that
    /// the full [`Self::set_frequency`] runs.  Within the same band
    /// (per datasheet table 9-2), one calibration during init is
    /// enough; subsequent retunes can use this fast path.  Saves
    /// ~3-4 ms per call versus `set_frequency`, which dominates the
    /// per-channel cost during a band-plan sweep.
    ///
    /// Caller must ensure the chip is in `STDBY_RC` (or
    /// `STDBY_XOSC`) — otherwise the new frequency register write
    /// won't take effect.  Use [`Self::set_standby_rc`] first.
    pub async fn set_frequency_fast(
        &mut self,
        hz: u32,
    ) -> Result<(), RadioError<Reset, Switch>> {
        let reg = (((hz as u64) << 25) / 32_000_000) as u32;
        self.cmd(CMD_SET_RF_FREQ, &reg.to_be_bytes()).await
    }

    /// Read the instantaneous RSSI of the channel currently being
    /// listened on.  Chip must be in RX (continuous or single
    /// reception); reading in standby gives a meaningless value.
    /// Returns dBm.
    ///
    /// SX1262 datasheet §13.5.3: the response is a single byte
    /// `RssiInst`; dBm = `-RssiInst / 2`.  Practical range on the
    /// LR1262's front end is roughly −120 dBm (noise floor) to
    /// −10 dBm (saturation).
    pub async fn get_rssi_inst(&mut self) -> Result<i16, RadioError<Reset, Switch>> {
        let mut buf = [0u8; 1];
        self.cmd_read(CMD_GET_RSSI_INST, &[], &mut buf).await?;
        Ok(-((buf[0] as i16) >> 1))
    }

    /// One-shot convenience: `rx_start` + `rx_recv` + standby.  Useful for
    /// occasional receive-then-do-something flows.  For high-rate continuous
    /// reception, prefer the explicit `rx_start` / `rx_recv` loop, which
    /// avoids the ~400 µs RX↔standby transition between packets.
    pub async fn rx_continuous(
        &mut self,
        buf: &mut [u8],
    ) -> Result<RxPacket, RadioError<Reset, Switch>> {
        self.rx_start().await?;
        let result = self.rx_recv(buf).await;
        self.switch.to_idle().await.map_err(Error::Switch)?;
        self.cmd(CMD_SET_STANDBY, &[0x00]).await?;
        result
    }

    // ---- internal helpers ----

    async fn write_packet_params(
        &mut self,
        payload_len: u8,
    ) -> Result<(), RadioError<Reset, Switch>> {
        // GFSK SetPacketParams (datasheet 13.4.4):
        //   [0..2] preamble length (bits, BE u16)
        //   [2]    preamble detector length (Bits16 = 0x05)
        //   [3]    sync word length (bits, 0..=64)
        //   [4]    address filtering (Disable = 0x00)
        //   [5]    packet header type (Variable = 0x01)
        //   [6]    payload length
        //   [7]    crc type (Crc2Byte = 0x02; Off = 0x01)
        //   [8]    whitening enable (0 = off)
        let pl = self.preamble_len.to_be_bytes();
        let crc_type = if self.crc_on { 0x02 } else { 0x01 };
        let p = [
            pl[0],
            pl[1],
            0x05, // PreambleDetectorLength::Bits16
            self.sync_word_bits,
            0x00,
            0x01,
            payload_len,
            crc_type,
            0x00,
        ];
        self.cmd(CMD_SET_PACKET_PARAMS, &p).await
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map an RF frequency in Hz to the `(freq1, freq2)` pair for the
/// `CalibrateImage` command, per datasheet table 9-2.
fn image_cal_band(hz: u32) -> (u8, u8) {
    match hz {
        430_000_000..=440_000_000 => (0x6B, 0x6F),
        470_000_000..=510_000_000 => (0x75, 0x81),
        779_000_000..=787_000_000 => (0xC1, 0xC5),
        863_000_000..=870_000_000 => (0xD7, 0xDB),
        902_000_000..=928_000_000 => (0xE1, 0xE9),
        _ => (0xE1, 0xE9),
    }
}