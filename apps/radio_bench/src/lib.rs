// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

//! Milestone 2 — SX1262 bench test, now through the [`Phy`] adapter.
//!
//! Two async functions: [`run_tx`] sends a known payload once per second;
//! [`run_rx`] listens forever and logs every packet's bytes + RSSI.  Both are
//! generic over the radio's HAL types and the LED — board crates' `Resources`
//! supply the concrete instances.  The radio is driven only through
//! `osrf_phy_api::Phy` (via `osrf_phy_sx126x`), so a clean run of this bench
//! on two boards is the hardware test of the adapter.
//!
//! Radio config: [`Sx126xConfig::BENCH_915`] — 915 MHz US ISM, 300 kbps GFSK,
//! 50 kHz deviation, 467 kHz receiver bandwidth, BT = 0.5, 4-byte sync word,
//! +14 dBm output.

use embassy_time::Timer;
use embedded_hal::digital::StatefulOutputPin;
use embedded_hal_async::{digital::Wait, spi::SpiDevice};
use osrf_phy_api::Phy;
use osrf_phy_sx126x::{Sx126xConfig, Sx126xPhy, PAYLOAD_MAX};
use osrf_radio_sx126x::{RfSwitchControl, Sx1262Radio};

const CONFIG: Sx126xConfig = Sx126xConfig::BENCH_915;

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
    let mut phy = Sx126xPhy::new(radio);
    if let Err(e) = phy.configure(&CONFIG).await {
        defmt::error!(
            "radio configure failed ({}); halting TX loop",
            defmt::Debug2Format(&e)
        );
        loop {
            Timer::after_millis(1000).await;
        }
    }
    defmt::info!(
        "TX bench: {} Hz / {} bps GFSK / +{} dBm (via Phy)",
        CONFIG.frequency_hz,
        CONFIG.bitrate_bps,
        CONFIG.tx_power_dbm
    );

    loop {
        let slot = phy.next_tx_slot().await;
        let counter = slot.period;
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
        match phy.tx(slot.slot, &payload).await {
            Ok(()) => {
                defmt::info!("TX #{}: sent {} bytes", counter, payload.len());
                let _ = led.toggle();
            }
            Err(e) => defmt::error!("TX #{}: failed ({})", counter, defmt::Debug2Format(&e)),
        }
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
    let mut phy = Sx126xPhy::new(radio);
    if let Err(e) = phy.configure(&CONFIG).await {
        defmt::error!(
            "radio configure failed ({}); halting RX loop",
            defmt::Debug2Format(&e)
        );
        loop {
            Timer::after_millis(1000).await;
        }
    }
    defmt::info!(
        "RX bench: listening on {} Hz / {} bps GFSK (via Phy)",
        CONFIG.frequency_hz,
        CONFIG.bitrate_bps
    );

    let mut buf = [0u8; PAYLOAD_MAX as usize];
    let mut count: u32 = 0;
    loop {
        match phy.rx(&mut buf).await {
            Ok(m) if m.crc_ok => {
                count = count.wrapping_add(1);
                defmt::info!(
                    "RX #{}: len={} rssi={}dBm bytes={=[u8]:#x}",
                    count,
                    m.len,
                    m.rssi_dbm.unwrap_or(0),
                    &buf[..m.len],
                );
                let _ = led.toggle();
            }
            Ok(_) => defmt::warn!("RX: CRC mismatch"),
            Err(e) => defmt::warn!("RX: error ({})", defmt::Debug2Format(&e)),
        }
    }
}
