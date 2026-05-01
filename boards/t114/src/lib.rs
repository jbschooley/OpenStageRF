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

use embassy_nrf::{bind_interrupts, buffered_uarte, peripherals, spim};

pub mod clocks;
#[cfg(feature = "usb-log")]
pub mod usb_log;

// `BufferedUarte` (used for the MIDI UART so we can expose
// `embedded_io_async::Read`) needs UARTE1's interrupt bound to its own
// `buffered_uarte::InterruptHandler` — not the plain `uarte::*` one.
bind_interrupts!(struct Irqs {
    TWISPI0 => spim::InterruptHandler<peripherals::TWISPI0>;
    TWISPI1 => spim::InterruptHandler<peripherals::TWISPI1>;
    UARTE1  => buffered_uarte::InterruptHandler<peripherals::UARTE1>;
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
// Built-in single button on the T114 v2.0 — always present.
pub mod button_user {
    use embassy_nrf::peripherals;
    pub type Pin = peripherals::P1_10;
}

// ── 5-way joystick (deployment-specific add-on, T114 design) ─────────────────
// The Heltec T114 itself only ships with the single user button above; this
// module describes the canonical pin assignments for an externally-wired
// 5-way joystick on the GPIO header pins, matching the input surface
// expected by `docs/ui_design.md` and the DX-LR30 board.
//
// Free GPIO header pins were picked so they don't collide with the default
// `dual_spi_diff_bus_radio1` pinout (which uses P0_28..P0_31 + P1_13/P1_15).
// The GPS-module pins (P1_02/P1_04..P1_07) are repurposable here because
// this project doesn't use the on-board GNSS.
//
// If the deployment wires the joystick differently, override by defining
// a custom `joystick` module in the profile crate.
pub mod joystick {
    use embassy_nrf::peripherals;
    pub type Up     = peripherals::P0_08;
    pub type Down   = peripherals::P0_00;
    pub type Left   = peripherals::P0_01;
    pub type Right  = peripherals::P1_11;
    pub type Center = peripherals::P1_04; // formerly GPS_PPS — GPS unused
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

/// Built-in 1.14" ST7789 TFT (240×135) on TWISPI1, MODE_3 @ 8 MHz.
///
/// The display is fully initialised by the time `resources()` returns:
/// VTFT_CTRL has been raised, the panel reset pulse has been issued, and
/// `mipidsi::Builder::init` has run the ST7789 power-on command sequence.
/// Drawing operations are immediate (no flush needed) and blocking on the
/// SPI bus.  Implements `embedded_graphics::DrawTarget<Color = Rgb565>`.
///
/// `Backlight` (P0_15) is left LOW for first bring-up; UI code must enable
/// it explicitly.  `VEXT_ENABLE` (P0_21) is also left LOW — that rail
/// powers external sensors; the TFT is on its own VTFT_CTRL gate.
pub type Display = mipidsi::Display<
    mipidsi::interface::SpiInterface<
        'static,
        embedded_hal_bus::spi::ExclusiveDevice<
            embassy_nrf::spim::Spim<'static>,
            embassy_nrf::gpio::Output<'static>,
            embassy_time::Delay,
        >,
        embassy_nrf::gpio::Output<'static>,
    >,
    mipidsi::models::ST7789,
    embassy_nrf::gpio::Output<'static>,
>;

/// MIDI UART (UARTE1) configured at 31250 baud 8N1.  Implements
/// `embedded_io_async::Read` and `embedded_io_async::Write` directly so
/// app crates can drive it through HAL-agnostic traits.
///
/// We use `BufferedUarte` rather than the plain `Uarte`: only the
/// buffered driver implements `embedded_io_async::Read` (the plain
/// `Uarte` only implements `Write`), and at MIDI's 31250 baud the
/// extra TIMER+PPI machinery the buffered version uses for idle
/// detection is well within budget on the otherwise-quiet UARTE1.
pub type MidiUart = embassy_nrf::buffered_uarte::BufferedUarte<'static>;

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

    /// DIN MIDI UART on UARTE1 (P0_09 RX, P0_10 TX) at 31250 baud 8N1.
    pub midi_uart: MidiUart,

    /// Built-in ST7789 TFT (240×135) on TWISPI1.  Already initialised:
    /// caller can immediately draw with `embedded_graphics`.  Backlight
    /// (P0_15) is currently held LOW; toggle it from the bin (the pin
    /// is consumed into the panel power chain so we don't expose it
    /// here in v1 — TODO once the UI design picks a backlight policy).
    pub display: Display,
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

    // ── MIDI UART: UARTE1 @ 31250 baud 8N1, P0_09 RX, P0_10 TX ──────────────
    // The `nfc-pins-as-gpio` feature on `embassy-nrf` is what makes P0_09 /
    // P0_10 usable as a UART (T114 wires them to the P1 header).
    //
    // BufferedUarte requires a TIMER, two PPI channels, and a PPI group for
    // its DMA-with-idle-detect machinery.  TIMER1 + PPI_CH0/CH1 + PPI_GROUP0
    // are otherwise unused on this board.
    static mut MIDI_RX_BUF: [u8; 256] = [0; 256];
    static mut MIDI_TX_BUF: [u8; 64] = [0; 64];
    let mut uart_cfg = embassy_nrf::uarte::Config::default();
    uart_cfg.baudrate = embassy_nrf::uarte::Baudrate::BAUD31250;
    let midi_uart = embassy_nrf::buffered_uarte::BufferedUarte::new(
        p.UARTE1,
        p.TIMER1,
        p.PPI_CH0,
        p.PPI_CH1,
        p.PPI_GROUP0,
        p.P0_09, // RX
        p.P0_10, // TX
        Irqs,
        uart_cfg,
        // SAFETY: static storage, single-call build_resources() consumes
        // Peripherals, so these slices are uniquely owned.
        unsafe { &mut *core::ptr::addr_of_mut!(MIDI_RX_BUF) },
        unsafe { &mut *core::ptr::addr_of_mut!(MIDI_TX_BUF) },
    );

    // ── Display: ST7789 240×135 TFT on TWISPI1 ──────────────────────────────
    // Power sequence (per Heltec T114 v2.0 schematic):
    //   1. VTFT_CTRL (P0_03) HIGH → gates the 3.3 V rail to the panel.
    //   2. Wait ≥10 ms for rail to stabilise.
    //   3. RESET pulse (handled by mipidsi::Builder::init via the reset_pin).
    //   4. Backlight (P0_15) — left LOW for now (off); the UI layer turns
    //      it on once the first frame has been drawn so the user never
    //      sees garbage during init.
    //
    // We use blocking `cortex_m::asm::delay` for the pre-init waits because
    // build_resources() is sync.  At 64 MHz, 64_000 cycles ≈ 1 ms.
    let pwr_ctrl = Output::new(p.P0_03, Level::High, OutputDrive::Standard);
    cortex_m::asm::delay(64_000 * 15);                      // ~15 ms

    // Backlight off for now — bin can drive P0_15 itself once the UI
    // layer is ready.  We hold a reference to keep the pin pinned to
    // a known state.
    let _backlight = Output::new(p.P0_15, Level::Low, OutputDrive::Standard);
    let _ = pwr_ctrl;  // hold high; dropping resets the pin to Hi-Z.
    // Leak both pins so they stay asserted for the lifetime of the program.
    // (Output<'static> is Drop-safe; without `forget` Rust would drop them
    // after build_resources returns and tristate the pin.)
    core::mem::forget(_backlight);
    core::mem::forget(pwr_ctrl);

    // SPIM1 @ 8 MHz, MODE_3 (CPOL=1/CPHA=1).  ST7789 datasheet permits
    // either MODE_0 or MODE_3; MODE_3 is the convention in the
    // mipidsi/embedded-graphics ecosystem.
    let mut tft_spi_cfg = SpimConfig::default();
    tft_spi_cfg.frequency = Frequency::M8;
    tft_spi_cfg.mode = embassy_nrf::spim::MODE_3;
    let tft_spi = Spim::new(
        p.TWISPI1,
        Irqs,
        p.P1_08, // SCK
        // The ST7789 is write-only from the MCU's perspective — there's no
        // MISO line to read back.  We pin one of the unused TFT-side pads
        // (P0_11 will become CS below; we still need *some* pin for MISO).
        // Pick P1_11 — unrouted on T114 v2.0 — to avoid clobbering anything.
        p.P1_11,
        p.P1_09, // MOSI
        tft_spi_cfg,
    );
    let tft_cs = Output::new(p.P0_11, Level::High, OutputDrive::Standard);
    let tft_spi_dev = embedded_hal_bus::spi::ExclusiveDevice::new(
        tft_spi,
        tft_cs,
        embassy_time::Delay,
    )
    .expect("CS pin set_high cannot fail (Infallible)");

    let tft_dc = Output::new(p.P0_12, Level::Low, OutputDrive::Standard);
    let tft_reset = Output::new(p.P0_02, Level::High, OutputDrive::Standard);

    // mipidsi's SpiInterface buffer: collects pixel data before flushing
    // it down the SPI bus.  512 bytes ≈ one ST7789 row at 16 bpp; bigger
    // is slightly faster but RAM-hungry.  T114 has 256 KB RAM, so 512 is
    // a comfortable starting point — bump if profiling shows we're
    // bottlenecked on per-batch overhead.
    static mut TFT_BUF: [u8; 512] = [0; 512];
    // SAFETY: build_resources consumes the singleton Peripherals, so this
    // is the only producer of a &'static mut to TFT_BUF.
    let tft_buf: &'static mut [u8] =
        unsafe { &mut *core::ptr::addr_of_mut!(TFT_BUF) };

    let di = mipidsi::interface::SpiInterface::new(tft_spi_dev, tft_dc, tft_buf);
    let mut delay = embassy_time::Delay;
    let display = mipidsi::Builder::new(mipidsi::models::ST7789, di)
        .reset_pin(tft_reset)
        .display_size(240, 135)
        // The 1.14" ST7789 panel is wired such that the 240×135 active area
        // sits offset (52, 40) inside the controller's 240×320 frame buffer
        // when in landscape orientation.  This matches the Heltec T114 v2.0
        // schematic + Meshtastic firmware variant.h.
        .display_offset(40, 53)
        .orientation(
            mipidsi::options::Orientation::new()
                .rotate(mipidsi::options::Rotation::Deg90),
        )
        .invert_colors(mipidsi::options::ColorInversion::Inverted)
        .init(&mut delay)
        .expect("ST7789 init failed (config rejected by mipidsi Builder)");

    (Resources { status_led, radio0, midi_uart, display }, usbd)
}
