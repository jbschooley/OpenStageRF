// SPDX-License-Identifier: AGPL-3.0-or-later

//! SoftDevice S140 integration for the T114.
//!
//! The Heltec stock bootloader bundles SoftDevice S140 v6.1.1 at flash
//! 0x1000-0x25FFF.  Our app loads at 0x26000 and runs alongside SD;
//! enabling SD via [`enable`] starts the protocol-stack runtime that
//! manages POWER + CLOCK transitions, sleep modes, and (eventually)
//! BLE.  The chip's reset path becomes:
//!
//!   `MBR → SD → app vector table at 0x26000`
//!
//! …with SD owning interrupt forwarding for everything it claims
//! (TIMER0, RTC0, SWI1/2, RADIO, CCM, AAR, ECB, EGU0).  Our peripheral
//! choices stay clear of those — see `boards/t114/src/lib.rs`'s
//! `build_resources` for what we actually use.
//!
//! ## Why we use SD even without BLE yet
//!
//! Empirically, on T114 v2.1 boards (or at least our two units), the
//! display IC is sensitive enough to power-rail transients that direct
//! `wfe()` sleep + manual POWER/CLOCK pokes corrupt SPIM bursts on
//! USB-only power.  Heltec / Meshtastic firmware doesn't see this
//! because all their chip transitions go through SD's vetted code
//! paths (`sd_app_evt_wait`, SD-managed regulators, MPU regions).
//! Running SD with no BLE configured costs us ~8 KB of RAM and a
//! single always-on Embassy task — small price for a known-good
//! hardware-management layer.
//!
//! ## SoftDevice version caveat
//!
//! The `nrf-softdevice` crate's official support matrix is S140 v7.x.x.
//! Heltec's bootloader ships v6.1.1.  v6 / v7 SVC numbers are stable
//! for the surface we touch (`sd_softdevice_enable`, clock cfg,
//! `sd_app_evt_wait`).  If a future BLE feature we use happens to
//! cross a renumbered SVC, it'll show up as a runtime
//! `Result::Err(...)` from a specific call — at which point upgrading
//! the bootloader to a v7.3.0 combo hex (Adafruit's release page) is
//! the fix.

use embassy_nrf::interrupt::{self, InterruptExt, Priority};
use nrf_softdevice::{raw, Softdevice};

/// Bump every peripheral IRQ this board uses to a SoftDevice-compatible
/// priority (P2).  Must be called **before** [`enable`] — SD checks
/// every enabled NVIC source on enable and panics with
/// `SdmIncorrectInterruptConfiguration` if any of them sit at the
/// reserved P0/P1/P4 priorities.
///
/// embassy-nrf's `time_interrupt_priority` + `gpiote_interrupt_priority`
/// in `Config` only cover RTC1 + GPIOTE; the per-peripheral drivers
/// (Spim, BufferedUarte, …) leave their IRQ priorities at the chip
/// reset default (P0) when constructed.  We therefore have to
/// override them explicitly here for every interrupt our
/// `bind_interrupts!` block claims and that gets enabled.
pub fn lower_app_interrupt_priorities() {
    interrupt::TWISPI0.set_priority(Priority::P2); // SX1262 radio SPIM
    interrupt::TWISPI1.set_priority(Priority::P2); // bound but unused; harmless
    interrupt::SPI2.set_priority(Priority::P2);    // ST7789 display SPIM
    interrupt::UARTE1.set_priority(Priority::P2);  // DIN MIDI UART
}

/// Bare SD activation that *only* calls `sd_softdevice_enable`
/// (the basic chip-management SVC) and skips every BLE-related
/// init that comes after.  Hand-rolled because:
///
/// - The current `nrf-softdevice` crate's `Softdevice::enable()`
///   targets S140 v7.x; on the Heltec stock S140 v6.1.1, the
///   `sd_ble_cfg_set` / `sd_ble_enable` calls hit parameter-layout
///   divergence and lock the chip up.
/// - We don't need BLE yet — only SD's POWER/CLOCK handling.
///
/// The single SVC we call (`sd_softdevice_enable`, opcode 0x10) is
/// stable across S140 versions, so this works on both v6.1.1 and
/// v7.3.0.  When we upgrade to v7.3.0 and want BLE, switch the
/// caller to [`enable`] / [`run`] and the rest of the
/// `nrf-softdevice` API.
///
/// **Returns** the SD-API error code (`NRF_SUCCESS = 0` on
/// success).  Caller should panic / log on non-zero return.
pub fn enable_chip_only() -> u32 {
    let clock_cfg = raw::nrf_clock_lf_cfg_t {
        source: raw::NRF_CLOCK_LF_SRC_XTAL as u8,
        rc_ctiv: 0,
        rc_temp_ctiv: 0,
        accuracy: raw::NRF_CLOCK_LF_ACCURACY_20_PPM as u8,
    };
    unsafe { raw::sd_softdevice_enable(&clock_cfg, Some(fault_handler)) }
}

/// SoftDevice fault handler — called by SD on internal asserts /
/// unrecoverable errors.  We log (if defmt is on) and halt; SD
/// already considers the system unusable at this point.
unsafe extern "C" fn fault_handler(id: u32, pc: u32, info: u32) {
    #[cfg(feature = "defmt")]
    defmt::error!("softdevice fault: id={=u32:#x} pc={=u32:#x} info={=u32:#x}", id, pc, info);
    loop {
        cortex_m::asm::wfe();
    }
}

/// Bring up the SoftDevice with a minimal config — LF clock only,
/// no BLE roles or connections.  Suitable for "we just want SD's
/// chip-management" mode (Stage 1-3 of OpenStageRF).  When Stage 4
/// adds BLE config / pairing, expand `Config` with `conn_gap`
/// (`conn_count >= 1`), `gap_role_count` (`periph_role_count >= 1`
/// for advertising), and the GATT-server attr-table size.
///
/// LF clock config: 32.768 kHz crystal (LFXO), ±20 ppm — matches what
/// `clocks::default_config()` configures for embassy-time.
///
/// **Why no BLE config fields** — every BLE config field set to
/// `Some(...)` triggers a corresponding `sd_ble_cfg_set` call inside
/// `Softdevice::enable()`, and SD rejects e.g. `conn_count: 0` as
/// `InvalidParam` (you can't allocate "zero connections worth of
/// state").  The minimal valid config to SD is "just clock", which
/// makes `sd_ble_enable()` use SD's internal defaults — exactly
/// what we want for the no-BLE stage.
pub fn enable() -> &'static mut Softdevice {
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

    // Enable the internal DC-DC regulator via SD's API (POWER is
    // SD-owned, so direct register access faults; this is the
    // SD-blessed equivalent of `c.dcdc.reg1 = true` we'd use without
    // SD).  Reduces current draw on the 3.3 V rail by ~10-15 mA at
    // peak — same optimisation Bluefruit/Heltec/Meshtastic firmware
    // applies right after SD enable.  Hardware requirement is the
    // LC filter on the DCC/DEC pins, populated on T114 v2.1.
    let ret = unsafe {
        raw::sd_power_dcdc_mode_set(raw::NRF_POWER_DCDC_MODES_NRF_POWER_DCDC_ENABLE as u8)
    };
    #[cfg(feature = "defmt")]
    if ret != 0 {
        defmt::warn!("sd_power_dcdc_mode_set returned {=u32}", ret);
    }

    sd
}

/// SD event-loop task.  Spawn this once after [`enable`] so SD can
/// process its internal events.  Without a runner, SD is enabled but
/// won't service its own state machine — fine for very short test
/// programs but never for production.
#[embassy_executor::task]
pub async fn run(sd: &'static Softdevice) -> ! {
    sd.run().await
}
