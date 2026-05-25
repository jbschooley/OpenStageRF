// SPDX-License-Identifier: AGPL-3.0-or-later

//! SoftDevice S140 integration for the T114.
//!
//! ## What SD does for us
//!
//! Even before we use BLE (Stage 4), running the SoftDevice gives us
//! Nordic-blessed POWER + CLOCK + sleep management.  That turned out
//! to be unrelated to a different bug we hunted (a misidentified
//! VTFT power-gate pin), but the SD-aware setup we built along the
//! way is the right base posture for nRF52840 + embassy on this
//! board: DC-DC enabled via SD's API, LF clock from the 32 kHz
//! crystal, peripheral interrupts at SD-allowed priorities.  When
//! Stage 4 adds advertising / pairing, expand the [`enable`]
//! `Config` with conn / gap / gatt fields.
//!
//! ## Boot path with SD active
//!
//! `MBR (0x0) → SD reset (0x1000) → app reset (0x26000) → cortex-m-rt
//! pre_init → main → embassy_nrf::init → Softdevice::enable`.
//!
//! Notes on order:
//! - `bootloader_handoff()` short-circuits when the `softdevice`
//!   feature is on (does **not** touch VTOR — SD's reset already
//!   pointed it at SD's vector table at 0x1000, and overriding that
//!   makes the next `sd_softdevice_enable` SVC trap to a panic
//!   handler in the app's table).
//! - `embassy_nrf::init()` must run **before** [`enable`] — SD claims
//!   CLOCK and POWER on activation; embassy's init can no longer
//!   configure those once SD owns them.
//! - [`enable`] internally lowers our peripheral IRQ priorities so
//!   they don't sit at SD-reserved levels (P0, P1, P4) when SD's
//!   enable runs its priority audit.
//!
//! ## SoftDevice version
//!
//! Targets S140 v6.1.1, the version the Heltec stock bootloader
//! ships with.  The SVC surface we use (`sd_softdevice_enable`, LF
//! clock cfg, `sd_power_dcdc_mode_set`, `sd_app_evt_wait`) is
//! stable enough across S140 versions that the same code works on
//! v7.x too if a board's bootloader gets upgraded; the wider BLE
//! API has v6/v7 ABI divergence that Stage 4 may need to handle.
//! RAM origin in `memory_softdevice.x` is tuned for what v6.1.1
//! actually requests (`0x200032d8`).

use embassy_nrf::interrupt::{self, InterruptExt, Priority};
use nrf_softdevice::{raw, Softdevice};

/// Bring up the SoftDevice and return its handle.  Caller must spawn
/// [`run`] right after to service SD's event loop.
///
/// Steps performed:
/// 1. Lower every app-side peripheral IRQ to P2 (SD-allowed).
///    embassy-nrf's per-driver constructors leave IRQ priorities at
///    the chip default (P0), which is SD-reserved — without this
///    step `sd_softdevice_enable` panics with
///    `SdmIncorrectInterruptConfiguration`.
/// 2. Call `Softdevice::enable` with a minimal config (LF clock from
///    the 32.768 kHz crystal, no BLE roles / connections).  The
///    "minimal" part matters: setting any BLE config field forces a
///    `sd_ble_cfg_set` call, which on v6.1.1 with v7-shaped structs
///    can return `InvalidParam` and panic.
/// 3. Enable DC-DC via SD's API (`sd_power_dcdc_mode_set`).  POWER
///    is SD-owned so the direct-register approach embassy-nrf would
///    use is faulted; SD's SVC is the supported path.
///
/// **Must be called after `embassy_nrf::init()`** — see module docs.
pub fn enable() -> &'static mut Softdevice {
    lower_app_interrupt_priorities();

    let config = nrf_softdevice::Config {
        clock: Some(raw::nrf_clock_lf_cfg_t {
            source: raw::NRF_CLOCK_LF_SRC_XTAL as u8,
            rc_ctiv: 0,
            rc_temp_ctiv: 0,
            accuracy: raw::NRF_CLOCK_LF_ACCURACY_20_PPM as u8,
        }),
        ..Default::default()
    };
    let sd = Softdevice::enable(&config);

    let ret = unsafe {
        raw::sd_power_dcdc_mode_set(raw::NRF_POWER_DCDC_MODES_NRF_POWER_DCDC_ENABLE as u8)
    };
    #[cfg(feature = "defmt")]
    if ret != 0 {
        defmt::warn!("sd_power_dcdc_mode_set returned {=u32}", ret);
    }

    sd
}

/// SoftDevice event-loop task.  Spawn once after [`enable`].
#[embassy_executor::task]
pub async fn run(sd: &'static Softdevice) -> ! {
    sd.run().await
}

/// Pull `len` random bytes from the SoftDevice's RNG via the
/// `sd_rand_application_vector_get` SVC.  RNG is SD-reserved when SD
/// is enabled — direct register pokes to the RNG peripheral fault —
/// so anything that needs randomness (boot counters, nonces, …)
/// must come through this path.  Returns the SD error code (0 on
/// success); the caller decides whether to retry, fall back, or
/// panic.
pub fn rand_bytes(buf: &mut [u8]) -> u32 {
    let len = buf.len().min(u8::MAX as usize) as u8;
    unsafe { raw::sd_rand_application_vector_get(buf.as_mut_ptr(), len) }
}

/// Bump every peripheral IRQ this board's `Resources` enables to
/// priority P2 (SD-allowed).  embassy-nrf's `Config.{time,gpiote}_
/// interrupt_priority` only cover RTC1 + GPIOTE; per-peripheral
/// drivers (Spim, BufferedUarte) leave their IRQ priorities at the
/// chip default (P0, SD-reserved).
fn lower_app_interrupt_priorities() {
    interrupt::TWISPI0.set_priority(Priority::P2); // SX1262 radio0 SPIM
    interrupt::TWISPI1.set_priority(Priority::P2); // bound but unused; harmless
    interrupt::SPI2.set_priority(Priority::P2); // ST7789 display SPIM
    interrupt::SPIM3.set_priority(Priority::P2); // SX1262 radio1 (diversity) SPIM
    interrupt::UARTE1.set_priority(Priority::P2); // DIN MIDI UART
    interrupt::SAADC.set_priority(Priority::P2); // Battery ADC
}
