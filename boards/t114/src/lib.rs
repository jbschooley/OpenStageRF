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

use embassy_nrf::{bind_interrupts, buffered_uarte, peripherals, saadc, spim};

// Re-export so profile binaries can drive HAL-level peripherals without
// depending on these crates directly.
pub use embassy_nrf;
pub use embedded_hal;
pub use embedded_hal_bus;

/// Short git commit hash for this build, with a trailing `*` when the
/// working tree had uncommitted changes at build time.  Set by
/// `build.rs`; falls back to `"unknown"` when git isn't available
/// (release tarballs, CI without `.git`).  Surfaced on the About
/// screen so a field-debug session can correlate symptoms with the
/// exact source revision.
pub const GIT_HASH: &str = env!("OSRF_GIT_HASH");

pub mod battery;
pub mod clocks;
pub mod display;
pub mod framebuffer;
pub mod panic_record;
pub mod softdevice;
pub mod storage;
#[cfg(feature = "usb-log")]
pub mod usb_log;

// `BufferedUarte` (used for the MIDI UART so we can expose
// `embedded_io_async::Read`) needs UARTE1's interrupt bound to its own
// `buffered_uarte::InterruptHandler` — not the plain `uarte::*` one.
bind_interrupts!(pub struct Irqs {
    TWISPI0 => spim::InterruptHandler<peripherals::TWISPI0>;
    TWISPI1 => spim::InterruptHandler<peripherals::TWISPI1>;
    SPI2    => spim::InterruptHandler<peripherals::SPI2>;
    UARTE1  => buffered_uarte::InterruptHandler<peripherals::UARTE1>;
    SAADC   => saadc::InterruptHandler;
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

// Built-in 1.14" ST7789 TFT (TWISPI1 in SPI mode) — driver and pin
// type aliases live in `src/display.rs`.

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
    // Pinout for the integrated MIDI-RX + diversity + display deployment.
    // Up/Down/Left/Right cluster on free P1 header pins (top row);
    // Center sits on the lower row.  All five are interrupt-capable
    // GPIOs, none collide with `dual_spi_diff_bus_radio1` (which
    // claims P0_28..P0_31 + P1_13/P1_15 + P0_05) or with the display
    // (which uses P0_02/P0_03/P0_11/P0_12/P0_15/P1_08/P1_09) or with
    // MIDI UART (P0_09/P0_10).
    pub type Up     = peripherals::P1_14;
    pub type Right  = peripherals::P1_12;
    pub type Left   = peripherals::P0_07;
    pub type Down   = peripherals::P0_08;
    pub type Center = peripherals::P0_13;
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

/// No-op pre-init shim.  Kept as a public stable entry point so
/// existing profile binaries can call `board::bootloader_handoff()`
/// from their `#[pre_init]` blocks without rewriting; with the
/// SoftDevice always enabled, SD's reset handler runs before our
/// pre_init and has already configured VTOR + interrupt forwarding
/// + POWER/CLOCK ownership.  There is nothing useful for us to do
/// here — touching the peripherals SD just configured (or
/// overriding the VTOR SD set) would break SD's setup and lock up
/// `sd_softdevice_enable`.
///
/// Profile binaries can drop their `#[pre_init]` block entirely —
/// keeping this function around just so we don't have to rewrite
/// every t114 main.rs in the same change as removing it.
///
/// # Safety
/// No-op, but kept `unsafe` for back-compat with the previous
/// signature.
#[inline(always)]
pub unsafe fn bootloader_handoff() {
    // intentionally empty — SD owns chip setup
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
    embassy_nrf::gpio::Input<'static>, // BUSY (P0_17)
    embassy_nrf::gpio::Input<'static>, // DIO1 (P0_20)
    embassy_nrf::gpio::Output<'static>,
    osrf_radio_sx126x::Dio2RfSwitch,
>;

/// Built-in 1.14" ST7789 TFT (240×135) on TWISPI1 @ 8 MHz.
///
/// Hand-rolled driver in [`display::St7789Display`].  The init sequence
/// (hardware reset → SWRESET → SLPOUT → COLMOD → MADCTL → INVON →
/// NORON → DISPON) runs inside [`Resources::display`] before
/// `resources()` returns, so the panel is ready for `draw_*` calls
/// immediately.  Backlight (P0_15) is enabled at the end of init, after
/// a clear-to-black, so users don't see junk pixels during power-on.
/// Implements `embedded_graphics_core::DrawTarget<Color = Rgb565>`.
pub type Display = display::St7789Display;

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

    /// Built-in 1.14″ ST7789 TFT, **constructed but not yet initialised**.
    /// VTFT_CTRL (the panel's power gate, P0_03) is raised inside
    /// `build_resources`, but the controller's command sequence
    /// (SWRESET → SLPOUT → COLMOD → MADCTL → INVON → NORON → DISPON)
    /// uses millisecond-scale `Timer::after` delays and therefore can't
    /// run from sync code.  The first `await`-context user must call
    /// `display.init().await` before any drawing.  This matches the
    /// `radio0.init().await` pattern.
    pub display: Display,

    /// TFT backlight enable on P0_15.  **Active LOW** — drive low to
    /// turn the backlight on, high to turn it off.  Verified against
    /// the Meshtastic T114 variant.h: `#define TFT_BACKLIGHT_ON LOW`.
    /// Owned separately from [`Display`] so apps can clear-to-
    /// background **before** turning the backlight on (avoids
    /// flashing junk pixels at boot).  Initialised HIGH (off) by
    /// `build_resources`; pull it low after your first frame paints.
    pub display_backlight: embassy_nrf::gpio::Output<'static>,

    /// Single WS2812 RGB LED on P0_14, parked Low.  WS2812 inputs are
    /// edge-sensitive — a floating P0_14 picks up noise and the LED
    /// shows random colors / flicker.  Holding the data line Low keeps
    /// it dark.  Replaced with a real driver if/when the NeoPixel is
    /// actually used.
    pub neopixel_parked: embassy_nrf::gpio::Output<'static>,

    /// Battery voltage monitor (SAADC on P0_04 / AIN2, divider
    /// enable on P0_06 / ADC_CTRL).  See [`battery::BatteryMonitor`].
    /// Profiles that want a battery indicator spawn a periodic
    /// sampling task using this; profiles that don't care can drop
    /// it (no power cost — `ADC_CTRL` is held low so the divider is
    /// off until the first sample).
    pub battery: battery::BatteryMonitor,

    /// Hardware watchdog peripheral token.  Profiles that want
    /// hang-detection turn this into an [`embassy_nrf::wdt::Watchdog`]
    /// with 1..=8 [`embassy_nrf::wdt::WatchdogHandle`]s — one per
    /// monitored task.  Once started the WDT can't be stopped
    /// without a reset, so profiles that don't care can simply
    /// drop the token (the peripheral stays unconfigured /
    /// dormant).
    pub wdt: embassy_nrf::Peri<'static, embassy_nrf::peripherals::WDT>,
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

    // ── BUSY = P0_17 (high while chip is processing a command) ─────────────
    // Used by `Sx1262Radio::wait_busy()` to hold off SPI commands until the
    // chip is idle.  Without this, back-to-back commands after `Calibrate` /
    // `CalibrateImage` are silently dropped → SetTx returns cmd_status=5.
    let busy = Input::new(p.P0_17, Pull::None);

    // ── DIO1 = P0_20 (interrupt-capable Input via GPIOTE) ───────────────────
    let dio1 = Input::new(p.P0_20, Pull::Down);

    // ── NRESET = P0_25 ──────────────────────────────────────────────────────
    let reset = Output::new(p.P0_25, Level::High, OutputDrive::Standard);

    // Hardware reset pulse is now done inside `Sx1262Radio::init()` (which
    // also waits for BUSY low afterward), so we no longer pulse here.

    let radio0 = osrf_radio_sx126x::Sx1262Radio::new(
        spi_dev,
        busy,
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
    // Pin assignments per Heltec's official Heltec_nRF52 BSP variant.h
    // (HT-n5262):
    //   SCK       = P1_08
    //   MOSI      = P1_09
    //   CS        = P0_11
    //   DC        = P0_12
    //   RESET     = P0_02
    //   VTFT_CTRL = P0_03  — gates the TFT VDD rail.  ACTIVE LOW
    //                        (drive LOW to enable LCM power).
    //   VEXT      = P0_21  — gates the GPS / peripheral 3.3 V rail.
    //                        Active HIGH.  *Not* the TFT power gate
    //                        despite a previous (wrong) comment in
    //                        this file claiming it was.  Empirically
    //                        confirmed by Meshtastic + MeshCore source
    //                        ([heltec_mesh_node_t114/variant.h] in
    //                        both projects), and by the symptom an
    //                        SD-aware OpenStageRF firmware spent a day
    //                        debugging — display only worked when an
    //                        SWD probe's 3.3 V wire back-fed the LCM
    //                        rail through P0_03's leakage path.
    //   Backlight = P0_15  — TFT_LEDA_CTL.  ACTIVE LOW.
    //
    // Hand-rolled driver in `display.rs`; replaces the prior mipidsi
    // path which hung during init on this hardware.  Build SPIM1 in
    // MODE_3 @ 8 MHz (ST7789 spec — clock idle high, sample rising
    // edge), construct the driver un-initialised, and let the user
    // call `display.init().await` from their async main.
    //
    // VEXT is raised here (and the pin leaked) so the panel + sensors
    // are powered when the user calls init.  Backlight stays HIGH so
    // the panel can be cleared before any pixels are visible (avoids
    // junk-on-boot).
    let mut display_spi_cfg = SpimConfig::default();
    // 8 MHz, MODE_0 — matches what Adafruit's nRF52 BSP / the
    // Heltec bootloader use for this panel.  ST7789 datasheet
    // permits both MODE_0 and MODE_3; the working reference uses 0.
    display_spi_cfg.frequency = Frequency::M8;
    display_spi_cfg.mode = embassy_nrf::spim::MODE_0;
    // The Heltec bootloader and Adafruit's nRF52 BSP both drive this
    // panel from SPIM2 (= Arduino's `SPI1` object on this board).
    // SPIM1 also works in principle, but SPIM2 matches the working
    // reference and avoids any chance of bootloader/peripheral state
    // confusion.
    let display_spi = Spim::new_txonly(
        p.SPI2,
        Irqs,
        p.P1_08, // SCK
        p.P1_09, // MOSI
        display_spi_cfg,
    );

    // ── nRF52840 SPIM-on-SCK fixup ──────────────────────────────────────
    // The SPIM peripheral internally reads back its own SCK signal for
    // edge timing.  If the SCK pin's GPIO input buffer is "Disconnect"
    // (the default for an Output<>), SPIM clocks **but produces no
    // observable output**.  Symptom on this v2.1 hardware was a
    // perfectly-working CS / DC / MOSI but a permanently-stuck panel
    // showing only backlight.
    //
    // Adafruit's nRF52 BSP sidesteps this by calling `nrf_gpio_cfg`
    // explicitly to set SCK's INPUT bit to *Connect*; embassy-nrf's
    // `Spim::new` does not do this — it routes PSEL but doesn't touch
    // the per-pin `PIN_CNF.INPUT` field.  We poke it manually.
    //
    // PIN_CNF[8] for P1.08 is at: P1 base 0x5000_0300 + 0x700 + 4*8
    //                            = 0x5000_0A20.
    // Bits we set: DIR=1 (Output), INPUT=0 (Connect), PULL=0 (None),
    //              DRIVE=3 (H0H1), SENSE=0 (Disabled).  Encoded value
    //              = 0x301 = (3 << 8) | 1.
    unsafe {
        const PIN_CNF_P1_08: *mut u32 = 0x5000_0A20 as *mut u32;
        core::ptr::write_volatile(PIN_CNF_P1_08, 0x301);
    }
    // CS / DC use HighDrive (H0H1) so the edges settle quickly even
    // under the panel's input capacitance.  Standard drive (S0S1) is
    // 2 mA per pin which can be slow on long traces.
    let display_cs = Output::new(p.P0_11, Level::High, OutputDrive::HighDrive);
    let display_dc = Output::new(p.P0_12, Level::Low, OutputDrive::HighDrive);
    let display_reset = Output::new(p.P0_02, Level::High, OutputDrive::HighDrive);
    // VTFT_CTRL (P0_03) — gates the TFT VDD rail.  ACTIVE LOW: drive
    // LOW to enable LCM power, HIGH to disable.  Owned by Display so
    // `init()` can apply the rail + the post-rail-up settling delay
    // before any SPI traffic, matching what Meshtastic / MeshCore do.
    let vtft = Output::new(p.P0_03, Level::High, OutputDrive::Standard);
    let display =
        display::St7789Display::new(display_spi, display_cs, display_dc, display_reset, vtft);
    // VEXT (P0_21) — peripheral / GPS rail, active HIGH.  *Not* the
    // TFT power gate (a previous comment claimed it was; that was
    // wrong — verified against Meshtastic + MeshCore variant.h on
    // 2026-05-09 after a bug where the display only worked with an
    // SWD probe attached because the probe's 3.3 V wire was back-
    // feeding the LCM rail through whatever leakage path P0_03 left
    // open).  Raised here and leaked so the GPS and any external
    // sensors stay powered; OpenStageRF doesn't use them yet but
    // profiles that do (future) will need this on.
    let vext_peripheral = Output::new(p.P0_21, Level::High, OutputDrive::Standard);
    core::mem::forget(vext_peripheral);
    // Backlight: active LOW per the panel's wiring (Meshtastic
    // variant.h: TFT_BACKLIGHT_ON LOW).  Init HIGH so the backlight
    // is OFF at boot — UI code drives it low after the first clear.
    let display_backlight = Output::new(p.P0_15, Level::High, OutputDrive::Standard);

    // ── NeoPixel (P0_14) parked Low ─────────────────────────────────────────
    // The single WS2812 RGB LED on the T114 is edge-sensitive.  Leaving the
    // pin floating causes the LED to interpret line noise as color data —
    // visible as fast flicker / random colors / "stuck halfway on".  Holding
    // the data line Low keeps the LED dark.  Replace this with a real
    // NeoPixel driver when/if it's ever used.
    let neopixel_parked = Output::new(p.P0_14, Level::Low, OutputDrive::Standard);

    // ── Battery monitor (SAADC on P0_04, divider enable on P0_06) ─────────
    // ADC_CTRL is initialised LOW (divider disabled) — no current drain
    // until the profile actually samples.  Heltec's BAT_ADC → AIN2 routes
    // a 1:3.9 divided Vbat to the SAADC; the driver applies the 4.9×
    // empirical multiplier on read.
    let battery = battery::BatteryMonitor::new(p.SAADC, p.P0_04, p.P0_06, Irqs);

    // ── Watchdog peripheral token ──────────────────────────────────────────
    // Passed through; profile decides whether to arm it and how many slots.
    let wdt = p.WDT;

    (
        Resources {
            status_led,
            radio0,
            midi_uart,
            display,
            display_backlight,
            neopixel_parked,
            battery,
            wdt,
        },
        usbd,
    )
}
