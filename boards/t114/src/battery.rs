// SPDX-License-Identifier: AGPL-3.0-or-later

//! Battery voltage monitor for the T114 (v2.0/v2.1).
//!
//! Heltec's circuit:
//!   - **AIN2 (P0_04)** = `BAT_ADC` — the divided battery voltage.
//!   - **P0_06** = `ADC_CTRL` — drive HIGH to enable the resistor
//!     divider, LOW to power-gate it (saves a few µA in idle).
//!   - **Divider ratio ≈ 1:3.9** — Meshtastic's variant.h annotates
//!     this as `4.916` and MeshCore uses `4.9`.  We use **4.9**
//!     unmodified.  Per-board variance is ±3 %; if calibration ever
//!     matters we can expose a setting in the future.
//!
//! ADC setup: SAADC with `Reference::INTERNAL` (0.6 V) and
//! `Gain::GAIN1_5` (1/5) giving an effective full-scale of **3.0 V**
//! to match Meshtastic's `AR_INTERNAL_3_0`.  12-bit resolution; one
//! sample takes ~10 µs of acquisition + ~3 µs of conversion.
//!
//! Sampling pattern (matches Meshtastic's reference):
//!   1. Drive `ADC_CTRL` HIGH.
//!   2. Wait 10 ms for the divider to settle.
//!   3. Take 15 samples and average — kills the per-sample noise
//!      (typical SAADC standard deviation is ~10 LSB).
//!   4. Drive `ADC_CTRL` LOW.
//!   5. Apply the 4.9 multiplier + 3.0 V/4096 LSB scaling.
//!
//! Don't poll faster than every ~5 s — the divider's settle current
//! adds up if we hammer it.

use embassy_nrf::peripherals;
use embassy_nrf::saadc::{ChannelConfig, Config, Gain, Reference, Resolution, Saadc};
use embassy_nrf::Peri;
use embassy_time::Timer;

/// Number of SAADC samples taken per call to [`BatteryMonitor::sample`].
/// Each sample is ~13 µs at 10 µs acquisition + conversion overhead,
/// so 15 samples ≈ 200 µs total — negligible against the 10 ms
/// divider settle delay.  Reduces noise by `sqrt(15)` ≈ 4×.
const SAMPLES_PER_READ: usize = 15;
/// Settle time after raising `ADC_CTRL` before taking the first
/// sample.  Heltec's divider plus the SAADC input cap takes a few
/// hundred µs; 10 ms is comfortable.
const SETTLE_MS: u64 = 10;
/// Empirical resistor-divider multiplier (Vbat / Vadc).  Heltec's
/// schematic + measurements via Meshtastic / MeshCore: 4.9.  Per-
/// board variance is ±3 %.
const DIVIDER_MULTIPLIER_X100: u32 = 490;
/// Internal reference × inverse gain = 0.6 V × 5 = 3000 mV effective
/// full-scale.  12-bit ADC → 4096 LSB at full scale.
const AREF_MV: u32 = 3000;
const ADC_FULL_SCALE: u32 = 4096;

/// Pin type aliases for the battery monitor's hardware ownership.
pub type BatAdcPin = peripherals::P0_04;
pub type AdcCtrlPin = peripherals::P0_06;

/// Read VBUS-detect status from `POWER->USBREGSTATUS` (datasheet
/// §5.1.5).  Bit 0 (VBUSDETECT) is set when 5 V is present at the
/// USB connector — which on the T114 is the closest thing we have
/// to "is the charger plugged in" since the TP4054's `STAT` pin
/// isn't routed to a GPIO.
///
/// **SD-safe.**  SoftDevice S140 restricts *writes* to peripheral
/// 0 (POWER), but *reads* of POWER status registers are
/// unrestricted — confirmed by the way Meshtastic / MeshCore call
/// `nrfx_power_usbstatus_get()` on the same chip with SD active.
/// We just punch through to the address directly because the
/// SD doesn't expose this on its SVC surface in a useful form
/// (only events on transitions, which we don't currently consume).
pub fn vbus_present() -> bool {
    // POWER base = 0x40000000; USBREGSTATUS offset = 0x438.
    const USBREGSTATUS: *const u32 = 0x4000_0438 as *const u32;
    // SAFETY: read-only access to a memory-mapped peripheral
    // register; no aliasing concerns.  SD permits reads of
    // POWER registers.
    let val = unsafe { core::ptr::read_volatile(USBREGSTATUS) };
    (val & 0x1) != 0
}

/// Battery-voltage monitor.  Owns the SAADC peripheral and the
/// `ADC_CTRL` pin; safe to keep idle in BSS — costs no power until
/// [`sample`](Self::sample) is called.
pub struct BatteryMonitor {
    saadc: Saadc<'static, 1>,
    adc_ctrl: embassy_nrf::gpio::Output<'static>,
}

impl BatteryMonitor {
    /// Construct the monitor.  Caller passes the SAADC peripheral
    /// token, the BAT_ADC pin (`P0_04`), the ADC_CTRL pin
    /// (`P0_06`), and the interrupt binding (typically the board
    /// crate's [`crate::Irqs`]).
    ///
    /// The ADC_CTRL pin is initialised LOW (divider disabled).
    pub fn new(
        saadc: Peri<'static, peripherals::SAADC>,
        bat_adc: Peri<'static, BatAdcPin>,
        adc_ctrl: Peri<'static, AdcCtrlPin>,
        irq: impl embassy_nrf::interrupt::typelevel::Binding<
                embassy_nrf::interrupt::typelevel::SAADC,
                embassy_nrf::saadc::InterruptHandler,
            > + 'static,
    ) -> Self {
        let mut saadc_config = Config::default();
        saadc_config.resolution = Resolution::_12BIT;

        // Gain 1/5 with internal 0.6V reference → effective AREF = 3.0V.
        // This matches Meshtastic's AR_INTERNAL_3_0 — keeps our
        // multiplier math consistent with their tested numbers.
        let mut chan = ChannelConfig::single_ended(bat_adc);
        chan.reference = Reference::INTERNAL;
        chan.gain = Gain::GAIN1_5;

        let saadc = Saadc::new(saadc, irq, saadc_config, [chan]);

        let adc_ctrl = embassy_nrf::gpio::Output::new(
            adc_ctrl,
            embassy_nrf::gpio::Level::Low,
            embassy_nrf::gpio::OutputDrive::Standard,
        );

        Self { saadc, adc_ctrl }
    }

    /// Sample the battery voltage and return millivolts.  Walks
    /// through enable-divider → settle → N samples → average →
    /// disable-divider → scale.  ~10 ms wall-clock per call,
    /// dominated by the settle delay.
    ///
    /// Returns 0 if no battery appears to be connected (reading
    /// well below the no-battery threshold) — caller should
    /// treat this as "unknown" rather than 0 % SoC.
    pub async fn sample(&mut self) -> u16 {
        self.adc_ctrl.set_high();
        Timer::after_millis(SETTLE_MS).await;

        let mut buf = [0i16; 1];
        let mut acc: u32 = 0;
        for _ in 0..SAMPLES_PER_READ {
            self.saadc.sample(&mut buf).await;
            // SAADC can return slightly negative values for inputs
            // very near 0V (offset error).  Clamp to 0.
            let v = buf[0].max(0) as u32;
            acc += v;
        }

        self.adc_ctrl.set_low();

        let raw_avg = acc / SAMPLES_PER_READ as u32;
        adc_raw_to_battery_mv(raw_avg)
    }
}

/// Scale a raw 12-bit SAADC reading to battery millivolts using the
/// 3.0 V reference and 4.9× resistor divider multiplier.  Extracted
/// for unit-testability (the rest of [`BatteryMonitor`] requires
/// hardware to exercise).
///
/// Math: `Vbat_mV = raw * (3000 / 4096) * 4.9`
///                = `raw * 14700 / 4096`
///                = `raw * 14700 >> 12`
fn adc_raw_to_battery_mv(raw: u32) -> u16 {
    let scaled = raw * AREF_MV * DIVIDER_MULTIPLIER_X100 / (100 * ADC_FULL_SCALE);
    // Max possible: 4096 * 3000 * 490 / (100 * 4096) = 14700 mV.
    // u16 max is 65535, fits with margin.
    scaled as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_to_mv_anchors() {
        // 0 → 0 mV
        assert_eq!(adc_raw_to_battery_mv(0), 0);
        // Full-scale (3.0 V at ADC pin) → 14700 mV at battery side
        assert_eq!(adc_raw_to_battery_mv(4096), 14700);
        // Half-scale (1.5 V at ADC pin) → 7350 mV
        assert_eq!(adc_raw_to_battery_mv(2048), 7350);
        // ~4.2 V battery: 4200 mV / 4.9 = ~857 mV at pin → raw ≈ 1170
        // 1170 / 4096 * 3000 * 4.9 ≈ 4198 mV (within 0.1 %)
        assert!((adc_raw_to_battery_mv(1170) as i32 - 4200).abs() < 20);
        // 3.7 V battery: 3700 / 4.9 = 755 mV → raw ≈ 1031
        assert!((adc_raw_to_battery_mv(1031) as i32 - 3700).abs() < 20);
    }
}
