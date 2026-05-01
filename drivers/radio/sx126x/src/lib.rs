// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

//! GFSK-only async wrapper around the upstream `sx1262` crate (v0.3).
//!
//! HAL-agnostic: takes generic `embedded-hal-async` SPI and `Wait`-able DIO1,
//! plus an `embedded-hal` `OutputPin` for RESET. LoRa is intentionally not
//! exposed.
//!
//! Two RF-switch styles are supported via [`RfSwitchControl`]:
//!
//! - [`Dio2RfSwitch`]: SX1262 DIO2 directly drives an external RF-switch IC
//!   (e.g. UPG2179 on Heltec T114). `init` calls `SetDio2AsRfSwitchCtrl` once;
//!   the chip handles tx/rx switching autonomously.
//! - [`PinRfSwitch`]: two MCU GPIOs (TXEN/RXEN) drive the switch (e.g.
//!   DX-LR30). The wrapper toggles them around `set_tx`/`set_rx`. DIO2 stays
//!   free for IRQ mapping.

use embedded_hal::digital::OutputPin;
use embedded_hal_async::{digital::Wait, spi::SpiDevice};

// Re-export the upstream GFSK enums callers need to plumb through. Other GFSK
// fields (header type, CRC type, address filtering) are typed locally below
// because the v0.3 upstream `PacketParams` is a raw byte array.
pub use sx1262::commands::{GfskBandwidth, GfskPulseShape, RampTime};

use sx1262::{
    commands::{
        BufferBaseAddressConfig, Calibrate, CalibrateImage, CalibrationConfig, ClearIrqStatus,
        DeviceSelect, DioIrqConfig, GetIrqStatus, GetPacketStatus, GetRxBufferStatus,
        GfskModParams, ImageCalibConfig, IrqMask, ModulationParams, PaConfig, PacketParams,
        PacketType, RfFrequencyConfig, RfSwitchConfig, RxMode, SetBufferBaseAddress,
        SetDio2AsRfSwitchCtrl, SetDioIrqParams, SetModulationParams, SetPaConfig, SetPacketParams,
        SetPacketType, SetRfFrequency, SetRx, SetStandby, SetTx, SetTxParams, StandbyConfig,
        Timeout, TxParams,
    },
    registers::SyncWord,
    Device,
};

// ---------------------------------------------------------------------------
// Local GFSK config enums (upstream v0.3 only takes a raw [u8; 9])
// ---------------------------------------------------------------------------

/// GFSK preamble-detector length, in bits. Datasheet field name: `PreambleDetectorLength`.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum PreambleDetectorLength {
    Off = 0x00,
    Bits8 = 0x04,
    Bits16 = 0x05,
    Bits24 = 0x06,
    Bits32 = 0x07,
}

/// GFSK address-filtering mode.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum AddressFiltering {
    Disable = 0x00,
    Node = 0x01,
    NodeAndBroadcast = 0x02,
}

/// GFSK packet header type. We only ever use `Variable`.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum GfskPacketHeaderType {
    Fixed = 0x00,
    Variable = 0x01,
}

/// GFSK CRC selection.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum CrcType {
    Off = 0x01,
    Crc1Byte = 0x00,
    Crc2Byte = 0x02,
    Crc1ByteInv = 0x04,
    Crc2ByteInv = 0x06,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors surfaced by the wrapper.
///
/// `SwitchE` is the RF-switch's error type. `ResetE` is the RESET pin's error
/// type. The upstream v0.3 driver erases SPI errors into its own `Error` type,
/// so SPI failures all show up as [`Error::Bus`].
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Error<SwitchE, ResetE> {
    /// SPI/register/command failure originating from the upstream driver.
    /// Typically a stuck BUSY line or a deserialization mismatch.
    Bus,
    /// RF-switch control layer failed (only the `PinRfSwitch` variant can
    /// produce this).
    Switch(SwitchE),
    /// RESET pin failed to toggle.
    Reset(ResetE),
    /// IRQ fired but neither TX_DONE nor RX_DONE was set.
    UnexpectedIrq(u16),
    /// CRC check failed on a received packet.
    CrcMismatch,
    /// Caller-supplied RX buffer is smaller than the received payload.
    BufferTooSmall,
    /// Caller passed a payload that doesn't fit (configured `payload_max_len`
    /// or hardware 256-byte buffer).
    PayloadTooLarge,
    /// Unsupported sync-word length (must be 0..=8 bytes).
    InvalidSyncWord,
}

impl<SwitchE, ResetE> From<sx1262::Error> for Error<SwitchE, ResetE> {
    fn from(_: sx1262::Error) -> Self {
        Error::Bus
    }
}

/// One received GFSK packet.
///
/// `snr_db` is included for symmetry with LoRa-flavoured radios. SX1262's
/// `GetPacketStatus` does not expose SNR for GFSK (datasheet table 11-66 only
/// gives `RxStatus`/`RssiSync`/`RssiAvg`), so it is always 0 here. `rssi_dbm`
/// comes from `RssiSync`, latched at sync-word detection.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct RxPacket {
    pub len: usize,
    pub rssi_dbm: i16,
    pub snr_db: i8,
    pub crc_ok: bool,
}

// ---------------------------------------------------------------------------
// RF switch trait + impls
// ---------------------------------------------------------------------------

/// Pluggable RF-switch driver.
///
/// `init` takes the live `sx1262::Device` so the DIO2 variant can issue
/// `SetDio2AsRfSwitchCtrl` against it. The pin variant just leaves both pins
/// low.
///
/// Single-executor embedded use does not need `Send` bounds on the returned
/// futures, so we tolerate the `async_fn_in_trait` lint here.
#[allow(async_fn_in_trait)]
pub trait RfSwitchControl {
    type Error;

    async fn init<SPI>(&mut self, dev: &mut Device<SPI>) -> Result<(), Self::Error>
    where
        SPI: SpiDevice;

    async fn before_tx(&mut self) -> Result<(), Self::Error>;
    async fn before_rx(&mut self) -> Result<(), Self::Error>;
    async fn to_idle(&mut self) -> Result<(), Self::Error>;
}

/// DIO2 drives an external RF switch IC autonomously.
///
/// `init` enables the chip's automatic DIO2 switching. The runtime methods
/// are no-ops; the chip toggles DIO2 a few microseconds before PA ramp-up/down.
pub struct Dio2RfSwitch;

impl RfSwitchControl for Dio2RfSwitch {
    type Error = sx1262::Error;

    async fn init<SPI>(&mut self, dev: &mut Device<SPI>) -> Result<(), Self::Error>
    where
        SPI: SpiDevice,
    {
        dev.execute_command_async(SetDio2AsRfSwitchCtrl {
            config: RfSwitchConfig { enable: true },
        })
        .await
        .map(|_| ())
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
}

/// Two MCU GPIOs (TXEN/RXEN) drive a discrete RF switch.
///
/// `init` deasserts both pins; switching is done synchronously around
/// `set_tx`/`set_rx`. DIO2 stays available for IRQ mapping.
pub struct PinRfSwitch<Txen, Rxen>
where
    Txen: OutputPin,
    Rxen: OutputPin,
{
    pub txen: Txen,
    pub rxen: Rxen,
}

impl<Txen, Rxen> PinRfSwitch<Txen, Rxen>
where
    Txen: OutputPin,
    Rxen: OutputPin,
{
    pub fn new(txen: Txen, rxen: Rxen) -> Self {
        Self { txen, rxen }
    }
}

/// Error wrapper for the two-pin switch variant. Either pin can fail
/// independently.
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum PinSwitchError<TE, RE> {
    Txen(TE),
    Rxen(RE),
}

impl<Txen, Rxen> RfSwitchControl for PinRfSwitch<Txen, Rxen>
where
    Txen: OutputPin,
    Rxen: OutputPin,
{
    type Error = PinSwitchError<Txen::Error, Rxen::Error>;

    async fn init<SPI>(&mut self, _dev: &mut Device<SPI>) -> Result<(), Self::Error>
    where
        SPI: SpiDevice,
    {
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
}

// ---------------------------------------------------------------------------
// Main driver
// ---------------------------------------------------------------------------

/// GFSK-only async SX1262 driver.
pub struct Sx1262Radio<Spi, Dio1, Reset, Switch>
where
    Spi: SpiDevice,
    Dio1: Wait,
    Reset: OutputPin,
    Switch: RfSwitchControl,
{
    dev: Device<Spi>,
    dio1: Dio1,
    reset: Reset,
    switch: Switch,
    preamble_len: u16,
    sync_word_bits: u8,
    payload_max_len: u8,
    crc_on: bool,
}

/// Convenience alias for the wrapper's error type.
pub type RadioError<Reset, Switch> = Error<
    <Switch as RfSwitchControl>::Error,
    <Reset as embedded_hal::digital::ErrorType>::Error,
>;

impl<Spi, Dio1, Reset, Switch> Sx1262Radio<Spi, Dio1, Reset, Switch>
where
    Spi: SpiDevice,
    Dio1: Wait,
    Reset: OutputPin,
    Switch: RfSwitchControl,
{
    pub fn new(spi: Spi, dio1: Dio1, reset: Reset, switch: Switch) -> Self {
        Self {
            dev: Device::new(spi),
            dio1,
            reset,
            switch,
            preamble_len: 32,
            sync_word_bits: 0,
            payload_max_len: 255,
            crc_on: true,
        }
    }

    /// Release the SPI device and pins.
    pub fn release(self) -> (Spi, Dio1, Reset, Switch) {
        (self.dev.release(), self.dio1, self.reset, self.switch)
    }

    /// Pulse RESET, enter STDBY_RC, set GFSK packet type, calibrate everything,
    /// initialize the RF switch, and configure DIO1 to OR together TX_DONE,
    /// RX_DONE, CRC_ERROR, and TIMEOUT.
    ///
    /// TODO(hardware): the datasheet's >=100 us reset-low pulse and ~10 ms
    /// post-reset wait are *not* enforced by this driver (we don't take a
    /// `DelayNs`). Callers should sequence `init` after a board-level
    /// `Timer::after(Duration::from_millis(10))` or similar. Verify on real
    /// silicon before trusting the first command.
    pub async fn init(&mut self) -> Result<(), RadioError<Reset, Switch>> {
        // Pulse RESET. We don't have a delayer, so the actual low duration is
        // up to the surrounding code.
        self.reset.set_low().map_err(Error::Reset)?;
        self.reset.set_high().map_err(Error::Reset)?;

        self.dev
            .execute_command_async(SetStandby {
                config: StandbyConfig::Rc,
            })
            .await?;

        self.dev
            .execute_command_async(SetPacketType {
                packet_type: PacketType::Gfsk,
            })
            .await?;

        // Calibrate everything. Bitflags has `::all()` since v2.
        self.dev
            .execute_command_async(Calibrate {
                config: CalibrationConfig::all(),
            })
            .await?;

        // Per-board RF switch init. Dio2 variant issues
        // `SetDio2AsRfSwitchCtrl{enable=true}`; pin variant drives both pins low.
        self.switch.init(&mut self.dev).await.map_err(Error::Switch)?;

        // DIO1 OR mask: TX_DONE | RX_DONE | CRC_ERROR | TIMEOUT.
        let mask = IrqMask::TX_DONE | IrqMask::RX_DONE | IrqMask::CRC_ERROR | IrqMask::TIMEOUT;
        self.dev
            .execute_command_async(SetDioIrqParams {
                config: DioIrqConfig {
                    irq_mask: mask,
                    dio1_mask: mask,
                    dio2_mask: IrqMask::empty(),
                    dio3_mask: IrqMask::empty(),
                },
            })
            .await?;

        Ok(())
    }

    /// Set RF frequency in Hz. Also runs `CalibrateImage` for the matching
    /// band (datasheet table 9-2).
    pub async fn set_frequency(
        &mut self,
        hz: u32,
    ) -> Result<(), RadioError<Reset, Switch>> {
        let (f1, f2) = image_cal_band(hz);
        self.dev
            .execute_command_async(CalibrateImage {
                config: ImageCalibConfig { freq1: f1, freq2: f2 },
            })
            .await?;
        self.dev
            .execute_command_async(SetRfFrequency {
                config: RfFrequencyConfig { frequency: hz },
            })
            .await?;
        Ok(())
    }

    /// GFSK modulation parameters.
    pub async fn set_modulation_gfsk(
        &mut self,
        bitrate_bps: u32,
        deviation_hz: u32,
        bandwidth: GfskBandwidth,
        pulse_shape: GfskPulseShape,
    ) -> Result<(), RadioError<Reset, Switch>> {
        self.dev
            .execute_command_async(SetModulationParams {
                params: ModulationParams::Gfsk(GfskModParams {
                    bit_rate: bitrate_bps,
                    pulse_shape,
                    bandwidth,
                    freq_deviation: deviation_hz,
                }),
            })
            .await?;
        Ok(())
    }

    /// Configure variable-length GFSK packet format.
    ///
    /// `sync_word` is at most 8 bytes. The chip's sync-word register at
    /// 0x06C0 is always 8 bytes wide; shorter sync words are zero-padded on
    /// the right and the active length (in bits) is stored in the
    /// `SetPacketParams` byte stream.
    ///
    /// CRC is fixed to 2-byte if enabled; whitening stays off (link-layer
    /// scrambling is the protocol stack's job).
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

        self.dev
            .write_register_async::<SyncWord>(SyncWord { value: sync })
            .await?;

        self.preamble_len = preamble_len;
        self.sync_word_bits = (sync_word.len() as u8) * 8;
        self.payload_max_len = payload_max_len;
        self.crc_on = crc_on;

        // Re-issue SetPacketParams with the configured max payload length.
        // tx() will rewrite payload_length per packet.
        self.write_packet_params(payload_max_len).await
    }

    /// Configure TX power. Uses the SX1262 (high-power) PA config recommended
    /// by the datasheet for +22 dBm; output is clamped to `[-9, +22]` dBm.
    pub async fn set_tx_power(
        &mut self,
        dbm: i8,
    ) -> Result<(), RadioError<Reset, Switch>> {
        let dbm = dbm.clamp(-9, 22);

        // SX1262 +22 dBm PA config.
        self.dev
            .execute_command_async(SetPaConfig {
                config: PaConfig {
                    duty_cycle: 0x04,
                    hp_max: 0x07,
                    device_sel: DeviceSelect::Sx1262,
                    pa_lut: 0x01,
                },
            })
            .await?;

        self.dev
            .execute_command_async(SetTxParams {
                params: TxParams {
                    power: dbm,
                    ramp_time: RampTime::Micros200,
                },
            })
            .await?;
        Ok(())
    }

    /// Transmit a single packet.
    ///
    /// switch.before_tx -> SetBufferBaseAddress -> WriteBuffer ->
    /// SetPacketParams (with this packet's length) -> ClearIrqStatus ->
    /// SetTx -> wait DIO1 -> GetIrqStatus -> ClearIrqStatus ->
    /// switch.to_idle -> SetStandby(RC).
    pub async fn tx(
        &mut self,
        payload: &[u8],
    ) -> Result<(), RadioError<Reset, Switch>> {
        if payload.len() > self.payload_max_len as usize || payload.len() > 255 {
            return Err(Error::PayloadTooLarge);
        }

        self.switch.before_tx().await.map_err(Error::Switch)?;

        self.dev
            .execute_command_async(SetBufferBaseAddress {
                config: BufferBaseAddressConfig {
                    tx_base_addr: 0,
                    rx_base_addr: 0,
                },
            })
            .await?;
        self.dev.write_buffer_async(0, payload).await?;

        // Tell the modem how many bytes to send.
        self.write_packet_params(payload.len() as u8).await?;

        self.dev
            .execute_command_async(ClearIrqStatus {
                irq_mask: IrqMask::all(),
            })
            .await?;

        self.dev
            .execute_command_async(SetTx {
                timeout: Timeout(0),
            })
            .await?;

        self.dio1.wait_for_high().await.map_err(|_| Error::Bus)?;

        let irq = self
            .dev
            .execute_command_async(GetIrqStatus)
            .await?
            .irq_mask;
        self.dev
            .execute_command_async(ClearIrqStatus {
                irq_mask: IrqMask::all(),
            })
            .await?;

        self.switch.to_idle().await.map_err(Error::Switch)?;
        self.dev
            .execute_command_async(SetStandby {
                config: StandbyConfig::Rc,
            })
            .await?;

        if irq.contains(IrqMask::TX_DONE) {
            Ok(())
        } else {
            Err(Error::UnexpectedIrq(irq.bits()))
        }
    }

    /// Receive a single packet in continuous-RX mode.
    ///
    /// Returns on TX_DONE/RX_DONE/CRC_ERROR/TIMEOUT IRQ. CRC errors return
    /// `Error::CrcMismatch` so the caller can decide whether to drop or keep
    /// the frame.
    pub async fn rx_continuous(
        &mut self,
        buf: &mut [u8],
    ) -> Result<RxPacket, RadioError<Reset, Switch>> {
        self.switch.before_rx().await.map_err(Error::Switch)?;

        // Re-issue SetPacketParams with the configured max payload length so
        // the modem accepts up to that many bytes.
        self.write_packet_params(self.payload_max_len).await?;

        self.dev
            .execute_command_async(ClearIrqStatus {
                irq_mask: IrqMask::all(),
            })
            .await?;

        self.dev
            .execute_command_async(SetRx {
                mode: RxMode::Continuous,
            })
            .await?;

        self.dio1.wait_for_high().await.map_err(|_| Error::Bus)?;

        let irq = self
            .dev
            .execute_command_async(GetIrqStatus)
            .await?
            .irq_mask;
        self.dev
            .execute_command_async(ClearIrqStatus {
                irq_mask: IrqMask::all(),
            })
            .await?;

        let crc_err = irq.contains(IrqMask::CRC_ERROR);
        let rx_done = irq.contains(IrqMask::RX_DONE);
        let timeout = irq.contains(IrqMask::TIMEOUT);

        let result = if rx_done {
            let buf_status = self
                .dev
                .execute_command_async(GetRxBufferStatus)
                .await?
                .buffer_status;
            let len = buf_status.payload_length as usize;
            if len > buf.len() {
                Err(Error::BufferTooSmall)
            } else {
                self.dev
                    .read_buffer_async(buf_status.buffer_pointer, &mut buf[..len])
                    .await?;
                let pkt = self
                    .dev
                    .execute_command_async(GetPacketStatus)
                    .await?
                    .packet_status;
                // FSK packet status: status[0]=RxStatus, status[1]=RssiSync,
                // status[2]=RssiAvg. Use RssiSync (latched at sync detect).
                let rssi_dbm = -((pkt.status[1] as i16) >> 1);
                Ok(RxPacket {
                    len,
                    rssi_dbm,
                    snr_db: 0,
                    crc_ok: !crc_err,
                })
            }
        } else if crc_err {
            Err(Error::CrcMismatch)
        } else if timeout {
            Err(Error::UnexpectedIrq(irq.bits()))
        } else {
            Err(Error::UnexpectedIrq(irq.bits()))
        };

        self.switch.to_idle().await.map_err(Error::Switch)?;
        self.dev
            .execute_command_async(SetStandby {
                config: StandbyConfig::Rc,
            })
            .await?;

        result
    }

    // ---- internal helpers ----

    /// Build the 9-byte GFSK `SetPacketParams` payload from current settings
    /// plus a per-call payload length, and send it.
    async fn write_packet_params(
        &mut self,
        payload_len: u8,
    ) -> Result<(), RadioError<Reset, Switch>> {
        // Datasheet 13.4.4: GFSK SetPacketParams byte layout:
        //   [0..2] preamble_length (BE u16, in bits/2 - actually bytes; see ds)
        //   [2]    preamble_detector_length
        //   [3]    sync_word_length (in bits, 0..=64)
        //   [4]    address_filtering
        //   [5]    packet_type (0=Fixed, 1=Variable)
        //   [6]    payload_length
        //   [7]    crc_type
        //   [8]    whitening_enable
        let pl = self.preamble_len.to_be_bytes();
        let bytes = [
            pl[0],
            pl[1],
            PreambleDetectorLength::Bits16 as u8,
            self.sync_word_bits,
            AddressFiltering::Disable as u8,
            GfskPacketHeaderType::Variable as u8,
            payload_len,
            if self.crc_on {
                CrcType::Crc2Byte as u8
            } else {
                CrcType::Off as u8
            },
            0, // whitening disabled
        ];
        self.dev
            .execute_command_async(SetPacketParams {
                params: PacketParams { params: bytes },
            })
            .await?;
        Ok(())
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
        // Default to the 902-928 MHz band - safest for ISM use cases.
        _ => (0xE1, 0xE9),
    }
}
