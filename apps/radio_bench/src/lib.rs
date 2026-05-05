// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

//! Milestone 2 — SX1262 bench test.
//!
//! Two async functions: [`run_tx`] sends a known payload once per second;
//! [`run_rx`] listens forever and logs every packet's bytes + RSSI.  Both are
//! generic over the radio's HAL types and the LED — board crates' `Resources`
//! supply the concrete instances.
//!
//! Radio config: 915 MHz US ISM, 300 kbps GFSK, 50 kHz deviation, 467 kHz
//! receiver bandwidth (covers 2 × (50 + 150) kHz signal bandwidth with margin),
//! BT = 0.5 Gaussian shaping, 4-byte sync word, +14 dBm output.

use embassy_time::Timer;
use embedded_hal::digital::StatefulOutputPin;
use embedded_hal_async::{digital::Wait, spi::SpiDevice};
use osrf_radio_sx126x::{
    GfskBandwidth, GfskPulseShape, RadioError, RfSwitchControl, Sx1262Radio,
};

const RF_FREQUENCY_HZ: u32 = 915_000_000;
const RF_BITRATE_BPS: u32 = 300_000;
const RF_DEVIATION_HZ: u32 = 50_000;
const RF_TX_POWER_DBM: i8 = 14;
const RF_PREAMBLE_BITS: u16 = 16;
const RF_PAYLOAD_MAX: u8 = 64;
/// 32-bit sync word.  Distinct from any LoRa sync; will be replaced by a
/// per-network value once the link layer lands in Milestone 4.
const RF_SYNC_WORD: [u8; 4] = [0xC1, 0x94, 0xC1, 0x94];

/// Apply the bench-test radio configuration.  Caller-side concrete types are
/// erased through the `Sx1262Radio` generics.
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
            // Signal BW ≈ 2 * (deviation + bitrate/2) = 2 * (50 + 150) = 400 kHz.
            // Bw4670 (467 kHz DSB) is the next variant up — minimum legal choice.
            GfskBandwidth::Bw4670,
            GfskPulseShape::Bt05,
        )
        .await?;
    radio
        .set_packet_format(RF_PREAMBLE_BITS, &RF_SYNC_WORD, RF_PAYLOAD_MAX, true)
        .await?;
    radio.set_tx_power(RF_TX_POWER_DBM).await?;
    // RF switch init must be LAST per SX1262 AN1200.36.
    radio.finish_init().await?;
    Ok(())
}

/// TX loop: send `[0xDE 0xAD 0xBE 0xEF, seq:u32]` once per second forever.
/// Toggles the LED on every successful transmission.
pub async fn run_tx<Spi, Busy, Dio1, Reset, Switch, Led>(
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
    if let Err(_e) = configure_radio(radio).await {
        defmt::error!("radio configure failed; halting TX loop");
        loop {
            Timer::after_millis(1000).await;
        }
    }
    defmt::info!(
        "TX bench: {} Hz / {} bps GFSK / +{} dBm",
        RF_FREQUENCY_HZ,
        RF_BITRATE_BPS,
        RF_TX_POWER_DBM
    );

    let mut counter: u32 = 0;
    loop {
        let payload: [u8; 8] = [
            0xDE,
            0xAD,
            0xBE,
            0xEF,
            (counter >> 24) as u8,
            (counter >> 16) as u8,
            (counter >> 8) as u8,
            counter as u8,
        ];
        match radio.tx(&payload).await {
            Ok(()) => {
                defmt::info!("TX #{}: sent {} bytes", counter, payload.len());
                let _ = led.toggle();
            }
            Err(_) => defmt::error!("TX #{}: failed", counter),
        }
        counter = counter.wrapping_add(1);
        Timer::after_millis(1000).await;
    }
}

/// RX loop: listen continuously, log every received packet's bytes + RSSI,
/// toggle the LED on every CRC-good packet.
pub async fn run_rx<Spi, Busy, Dio1, Reset, Switch, Led>(
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
    if let Err(_e) = configure_radio(radio).await {
        defmt::error!("radio configure failed; halting RX loop");
        loop {
            Timer::after_millis(1000).await;
        }
    }
    defmt::info!(
        "RX bench: listening on {} Hz / {} bps GFSK",
        RF_FREQUENCY_HZ,
        RF_BITRATE_BPS
    );

    let mut buf = [0u8; RF_PAYLOAD_MAX as usize];
    let mut count: u32 = 0;
    loop {
        match radio.rx_continuous(&mut buf).await {
            Ok(pkt) if pkt.crc_ok => {
                count = count.wrapping_add(1);
                let n = pkt.len.min(buf.len());
                defmt::info!(
                    "RX #{}: len={} rssi={}dBm bytes={=[u8]:#x}",
                    count,
                    pkt.len,
                    pkt.rssi_dbm,
                    &buf[..n],
                );
                let _ = led.toggle();
            }
            Ok(_) => defmt::warn!("RX: CRC mismatch"),
            Err(_) => defmt::warn!("RX: error"),
        }
    }
}
