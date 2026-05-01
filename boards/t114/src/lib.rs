// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

//! Heltec Mesh Node T114 v2.0 — nRF52840 + SX1262 + 1.14" ST7789 TFT.
//!
//! Pin assignments verified from the v2.0 schematic and Meshtastic firmware
//! variant.h.  Each module corresponds to a piece of board hardware that a
//! profile can opt into via `pub use`.
//!
//! Peripheral allocation:
//!   - TWISPI0 (periph 0, SPI mode) → radio0
//!   - TWISPI1 (periph 1, SPI mode) → display
//!   - UARTE1  (periph 2, UART mode) → midi UART
//!   - SPI3    (periph 3, dedicated) → radio1 (dual_spi_diff_bus)
//! No two modules share the same nRF52840 peripheral instance.

use embassy_nrf::{bind_interrupts, peripherals, spim};

pub mod clocks;
#[cfg(feature = "usb-log")]
pub mod usb_log;

bind_interrupts!(struct Irqs {
    TWISPI0 => spim::InterruptHandler<peripherals::TWISPI0>;
});

// ── Built-in SX1262 radio (TWISPI0 in SPI mode) ──────────────────────────────
// No TXEN/RXEN pins — the SX1262's DIO2 output drives a UPG2179 RF switch IC
// directly.  Set DIO2_AS_RF_SWITCH in the SX126x driver config.
pub mod radio0 {
    use embassy_nrf::peripherals;
    pub type Spi  = peripherals::TWISPI0;
    pub type Sck  = peripherals::P0_19;
    pub type Miso = peripherals::P0_23;
    pub type Mosi = peripherals::P0_22;
    pub type Cs   = peripherals::P0_24;
    pub type Busy = peripherals::P0_17;
    pub type Dio1 = peripherals::P0_20;
    pub type Nrst = peripherals::P0_25;
}

// ── Default radio1 pinout for dual_spi_diff_bus (SPI3, dedicated) ────────────
// Second SX1262 wired to the GPIO header pins (P0_28..P0_31 + P1_xx).
// Profiles can override by defining their own radio1 module.
pub mod dual_spi_diff_bus_radio1 {
    use embassy_nrf::peripherals;
    pub type Spi  = peripherals::SPI3;
    pub type Sck  = peripherals::P0_28;
    pub type Miso = peripherals::P0_29;
    pub type Mosi = peripherals::P0_30;
    pub type Cs   = peripherals::P0_31;
    pub type Busy = peripherals::P1_13;
    pub type Dio1 = peripherals::P1_15;
    pub type Nrst = peripherals::P0_05;
}

// dual_spi_same_bus_radio1 is intentionally absent — T114 has only one SPI
// peripheral wired to the built-in radio module; sharing TWISPI0 with a
// second SX1262 would require external bus expansion that the PCB doesn't
// route.  Profiles that try to import it get a clear "unresolved import"
// compile error.

// ── Built-in 1.14" ST7789 TFT (TWISPI1 in SPI mode) ──────────────────────────
pub mod display {
    use embassy_nrf::peripherals;
    pub type Spi       = peripherals::TWISPI1;
    pub type Sck       = peripherals::P1_08;
    pub type Mosi      = peripherals::P1_09;
    pub type Cs        = peripherals::P0_11;
    pub type Dc        = peripherals::P0_12;  // data/command select
    pub type Reset     = peripherals::P0_02;
    pub type Backlight = peripherals::P0_15;
    pub type PwrCtrl   = peripherals::P0_03;  // VTFT_CTRL — gates display power
}

// ── MIDI UART (UARTE1) ───────────────────────────────────────────────────────
// P0_09 / P0_10 are exposed on the P1 header as a general-purpose UART.
pub mod midi_uart {
    use embassy_nrf::peripherals;
    pub type Uart = peripherals::UARTE1;
    pub type Rx   = peripherals::P0_09;
    pub type Tx   = peripherals::P0_10;
}

// ── User button ──────────────────────────────────────────────────────────────
pub mod button_user {
    use embassy_nrf::peripherals;
    pub type Pin = peripherals::P1_10;
}

// ── Status LED (green, active-high) ──────────────────────────────────────────
pub mod led_status {
    use embassy_nrf::peripherals;
    pub type Pin = peripherals::P1_03;
}

// ── Addressable RGB (single WS2812-style) ────────────────────────────────────
pub mod neopixel {
    use embassy_nrf::peripherals;
    pub type Pin = peripherals::P0_14;
}

// ── External 3.3 V rail enable (controls display + sensors) ──────────────────
pub mod vext_power {
    use embassy_nrf::peripherals;
    pub type Pin = peripherals::P0_21;
}

/// Raw Embassy peripheral tokens.  Use this for fine-grained hardware access
/// in apps that need more than `Resources` provides.
///
/// LFCLK is configured to use the board's 32.768 kHz crystal (LFXO) for
/// accurate Embassy time-driver timestamps; HFCLK stays on HFINT (64 MHz
/// internal RC).  See `clocks.rs` for the rationale.
pub fn init() -> embassy_nrf::Peripherals {
    init_with(clocks::default_config())
}

/// Like [`init()`] but with a caller-supplied clock config.  Use this when
/// the default HFINT/LFXO mix isn't enough — most notably, USB-CDC needs
/// HFXO (see [`clocks::usb_config()`]).
pub fn init_with(config: embassy_nrf::config::Config) -> embassy_nrf::Peripherals {
    embassy_nrf::init(config)
}

// ── Board-level resource API ─────────────────────────────────────────────────
// The fields below are HAL-specific types but each implements an embedded-hal
// trait, letting board-agnostic apps drive them through the trait surface.

/// SX1262 wrapper as it lives on this board: SPIM0 (TWISPI0) + GPIOTE-driven
/// DIO1 + GPIO NRESET + DIO2-driven RF switch (no MCU-side switch pins).
pub type Radio0 = osrf_radio_sx126x::Sx1262Radio<
    embedded_hal_bus::spi::ExclusiveDevice<
        embassy_nrf::spim::Spim<'static>,
        embassy_nrf::gpio::Output<'static>,
        embassy_time::Delay,
    >,
    embassy_nrf::gpio::Input<'static>,
    embassy_nrf::gpio::Output<'static>,
    osrf_radio_sx126x::Dio2RfSwitch,
>;

/// Eagerly-initialised on-board peripherals.  Apps that just want "the LED"
/// or "the user button" call `resources()` and read fields off the result.
pub struct Resources {
    /// Green status LED (P1_03, active-high).  Implements
    /// `embedded_hal::digital::OutputPin`.
    pub status_led: embassy_nrf::gpio::Output<'static>,

    /// Built-in SX1262 radio on TWISPI0 (SPI mode).  RF switch is driven by
    /// the chip's DIO2 line autonomously.  NRESET has already been pulsed;
    /// caller can immediately `radio0.init().await` and proceed to configure
    /// modulation.
    pub radio0: Radio0,
}

/// Initialise hardware with the default clock config and bundle the common
/// peripherals into `Resources`.  Equivalent to `resources_with(clocks::default_config())`.
pub fn resources() -> Resources {
    resources_with(clocks::default_config())
}

/// Like [`resources()`] but with a caller-supplied clock config.  Use this
/// when the default HFINT/LFXO mix isn't enough — most notably, the
/// `usb-log` feature requires HFXO (see [`clocks::usb_config()`]).
///
/// The unused-peripheral tokens needed for USB (`USBD`, `POWER` IRQ binding)
/// remain inside `embassy_nrf::Peripherals` and are not exposed by this
/// resource bundle; profiles that need them must call
/// [`resources_and_usbd_with()`] instead.
pub fn resources_with(config: embassy_nrf::config::Config) -> Resources {
    let p = init_with(config);
    let (r, _usbd) = build_resources(p);
    r
}

/// Like [`resources_with()`] but also returns the still-unused USB
/// peripheral token, so a profile can hand it to [`crate::usb_log::spawn`]
/// alongside its `Resources`.  Available regardless of features so the
/// API surface stays stable; the returned token is unused (and the USB
/// peripheral stays idle) until the caller actually starts a USB driver.
pub fn resources_and_usbd_with(
    config: embassy_nrf::config::Config,
) -> (Resources, embassy_nrf::Peri<'static, embassy_nrf::peripherals::USBD>) {
    let p = init_with(config);
    build_resources(p)
}

/// Internal: take an `embassy_nrf::Peripherals`, peel off USBD, build
/// `Resources` from the rest.  Inlined into both public entry points so
/// we never have to pass a partially-moved `Peripherals` across a
/// function boundary.
fn build_resources(
    p: embassy_nrf::Peripherals,
) -> (Resources, embassy_nrf::Peri<'static, embassy_nrf::peripherals::USBD>) {
    use embassy_nrf::gpio::{Input, Level, Output, OutputDrive, Pull};
    use embassy_nrf::spim::{Config as SpimConfig, Frequency, Spim, MODE_0};

    // Move USBD out first — Rust accepts partial moves of a struct so long
    // as we only access (not move) the rest of the fields below.
    let usbd = p.USBD;

    // ── Status LED (P1_03, active-high) ─────────────────────────────────────
    let status_led = Output::new(p.P1_03, Level::Low, OutputDrive::Standard);

    // ── SX1262 SPI bus: SPIM0 @ 8 MHz, MODE_0 ───────────────────────────────
    let mut spi_cfg = SpimConfig::default();
    spi_cfg.frequency = Frequency::M8;
    spi_cfg.mode = MODE_0;
    let spi = Spim::new(
        p.TWISPI0,
        Irqs,
        p.P0_19, // SCK
        p.P0_23, // MISO
        p.P0_22, // MOSI
        spi_cfg,
    );
    let cs = Output::new(p.P0_24, Level::High, OutputDrive::Standard);
    let spi_dev = embedded_hal_bus::spi::ExclusiveDevice::new(spi, cs, embassy_time::Delay)
        .expect("CS pin set_high cannot fail (Infallible)");

    // ── DIO1 = P0_20 (interrupt-capable Input via GPIOTE) ───────────────────
    let dio1 = Input::new(p.P0_20, Pull::Down);

    // ── NRESET = P0_25 ──────────────────────────────────────────────────────
    let mut reset = Output::new(p.P0_25, Level::High, OutputDrive::Standard);

    // ── Hardware reset pulse: low ≥100 µs, then ≥10 ms post-reset wait ──────
    // SYSCLK on nRF52840 is 64 MHz.  Be generous: ~200 µs and ~15 ms.
    reset.set_low();
    cortex_m::asm::delay(64 * 200);
    reset.set_high();
    cortex_m::asm::delay(64_000 * 15);

    let radio0 = osrf_radio_sx126x::Sx1262Radio::new(
        spi_dev,
        dio1,
        reset,
        osrf_radio_sx126x::Dio2RfSwitch,
    );

    (Resources { status_led, radio0 }, usbd)
}
