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

// Re-export so profile binaries can drive HAL-level peripherals without
// depending on these crates directly.
pub use embassy_nrf;
pub use embedded_hal;
pub use embedded_hal_bus;
pub use mipidsi;

pub mod clocks;
#[cfg(feature = "usb-log")]
pub mod usb_log;

// `BufferedUarte` (used for the MIDI UART so we can expose
// `embedded_io_async::Read`) needs UARTE1's interrupt bound to its own
// `buffered_uarte::InterruptHandler` — not the plain `uarte::*` one.
bind_interrupts!(pub struct Irqs {
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

/// Address of the user-application slot in flash, matching `memory.x`'s
/// `FLASH ORIGIN`.  Used by [`relocate_vtor()`] and kept here so a single
/// edit changes both the linker layout and the runtime VTOR write.
pub const FLASH_ORIGIN: u32 = 0x0000_1000;

/// Relocate `SCB->VTOR` to the start of our flash slot, and stop the
/// peripherals the Heltec bootloader leaves running on its way out.
///
/// **Must be called from `#[cortex_m_rt::pre_init]`** in every binary that
/// targets this board, before any interrupt is allowed to fire and before
/// `embassy_nrf::init()` runs.
///
/// What this fixes (one bug per item, all observed empirically on the
/// `ht-n5262 0.9.0` bootloader hand-off):
/// 1. **VTOR**: bootloader leaves it pointing at its own vector table at
///    0xF4000 (no SoftDevice on this unit, so the SD-aware
///    `sd_softdevice_vector_table_base_set` path doesn't run).
///    cortex-m-rt's reset shim does not relocate VTOR for us.
/// 2. **GPIOTE / PPI / TIMERs**: bootloader uses these to drive its LED
///    blink during DFU.  It does not tear them down before jumping to the
///    app, and embassy-nrf's `init()` does not preemptively reset them, so
///    leftover GPIOTE channels keep yanking the LED pin even after the app
///    configures it as a regular GPIO output.
/// 3. **RTC1**: embassy's `time-driver-rtc1` assumes RTC1 starts in reset
///    state.  When the bootloader has left RTC1 running, `Timer::after`
///    awaits never fire and the executor hangs at the first `.await`.
/// 4. **NVIC**: bootloader leaves USBD (and possibly others) enabled in
///    NVIC.  Once embassy unmasks PRIMASK to dispatch the executor, those
///    stray interrupts fire through cortex-m-rt's `DefaultHandler`, which
///    is an infinite loop — symptom is the executor freezing at its first
///    `.await` (e.g. `Timer::after_millis`).
///
/// # Safety
/// Only call from within a `#[pre_init]` function.  The peripheral pokes
/// blow away any in-flight DMA / timer state, which is fine at boot but
/// catastrophic if invoked mid-program.
#[inline(always)]
pub unsafe fn bootloader_handoff() {
    // 1. Disable + clear-pending every NVIC source the bootloader may have
    //    armed.  ICER/ICPR are 8 × u32 at 0xE000_E180 / 0xE000_E280; writing
    //    1s to a bit clears it.  We deliberately do NOT touch PRIMASK —
    //    cortex-m-rt and embassy don't restore it, and a stray PRIMASK = 1
    //    here would deadlock the executor.  With every NVIC source
    //    disabled, no interrupt can fire regardless of PRIMASK state.
    let icer: *mut u32 = 0xE000_E180 as *mut u32;
    let icpr: *mut u32 = 0xE000_E280 as *mut u32;
    for i in 0..8 {
        core::ptr::write_volatile(icer.add(i), 0xFFFF_FFFF);
        core::ptr::write_volatile(icpr.add(i), 0xFFFF_FFFF);
    }

    // 2. VTOR.
    (*cortex_m::peripheral::SCB::PTR).vtor.write(FLASH_ORIGIN);

    // 3. Fully reset all RTCs.  Just stopping isn't enough — embassy's
    //    time driver init does not preemptively clear INTENSET / EVTENSET,
    //    so leftover bits from the bootloader can either fire stray
    //    events into a not-yet-bound handler or mask the COMPARE the
    //    driver actually wants.  Symptom: first `.await` never resolves.
    //    Order: stop → disable all interrupts → disable all events → clear counter.
    for &base in &[0x4000_B000u32, 0x4001_1000, 0x4002_4000] {
        core::ptr::write_volatile((base + 0x004) as *mut u32, 1); // TASKS_STOP
        core::ptr::write_volatile((base + 0x308) as *mut u32, 0xFFFF_FFFF); // INTENCLR
        core::ptr::write_volatile((base + 0x348) as *mut u32, 0xFFFF_FFFF); // EVTENCLR
        core::ptr::write_volatile((base + 0x008) as *mut u32, 1); // TASKS_CLEAR
    }

    // 4. Reset LFCLK to a known-stopped state.  The bootloader leaves
    //    LFCLK running on whatever source it chose (often LFRC for the
    //    DFU timeout timer).  embassy-nrf's `init()` will reconfigure and
    //    restart it; clearing it here avoids any "already running on the
    //    wrong source" ambiguity.  CLOCK base 0x4000_0000.
    core::ptr::write_volatile(0x4000_000C as *mut u32, 1); // TASKS_LFCLKSTOP
    core::ptr::write_volatile(0x4000_0308 as *mut u32, 0xFFFF_FFFF); // INTENCLR (clock events)

    // 5. Stop all TIMERs (TASKS_STOP at +0x004, TASKS_SHUTDOWN at +0x00C).
    for &base in &[
        0x4000_8000u32, // TIMER0
        0x4000_9000,    // TIMER1
        0x4000_A000,    // TIMER2
        0x4001_A000,    // TIMER3
        0x4001_B000,    // TIMER4
    ] {
        core::ptr::write_volatile((base + 0x004) as *mut u32, 1);
        core::ptr::write_volatile((base + 0x00C) as *mut u32, 1);
    }

    // 6. Clear all 8 GPIOTE channel configs (CONFIG[0..7] at 0x4000_6510).
    let gpiote_config: *mut u32 = 0x4000_6510 as *mut u32;
    for i in 0..8 {
        core::ptr::write_volatile(gpiote_config.add(i), 0);
    }

    // 7. Disable all PPI channels (CHENCLR at 0x4001_F508).
    core::ptr::write_volatile(0x4001_F508 as *mut u32, 0xFFFF_FFFF);
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

    // The ST7789 TFT (`Display` type) is intentionally NOT initialised by
    // `resources()`.  `mipidsi::Builder::init()` hangs on this hardware
    // for reasons not yet root-caused — every other path through
    // build_resources (SX1262 SPI, UARTE1, raw SPI write, 120 ms
    // `embassy_time::Delay::delay_ms`) verified working in isolation.
    // Display init is deferred to a future `init_display()` helper that
    // the UI smoke-test profile will exercise once we debug the mipidsi
    // hand-off.  See the t114_blink stepwise diagnostic history.

    /// Single WS2812 RGB LED on P0_14, parked Low.  WS2812 inputs are
    /// edge-sensitive — a floating P0_14 picks up noise and the LED
    /// shows random colors / flicker.  Holding the data line Low keeps
    /// it dark.  Replaced with a real driver if/when the NeoPixel is
    /// actually used.
    pub neopixel_parked: embassy_nrf::gpio::Output<'static>,
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

    // ── Display: deferred ───────────────────────────────────────────────────
    // ST7789 init via mipidsi::Builder::init hangs on this hardware (cause
    // not yet root-caused; SPI write and embassy_time delays both verified
    // working in isolation).  The display, backlight, and pwr_ctrl pins
    // (P0_02, P0_03, P0_11, P0_12, P0_15, P1_08, P1_09, P1_11) along with
    // TWISPI1 stay unowned in `embassy_nrf::Peripherals` and will be
    // claimed by a future `init_display()` helper once the hand-off is
    // debugged in the smoke-test profile.

    // ── NeoPixel (P0_14) parked Low ─────────────────────────────────────────
    // The single WS2812 RGB LED on the T114 is edge-sensitive.  Leaving the
    // pin floating causes the LED to interpret line noise as color data —
    // visible as fast flicker / random colors / "stuck halfway on".  Holding
    // the data line Low keeps the LED dark.  Replace this with a real
    // NeoPixel driver when/if it's ever used.
    let neopixel_parked = Output::new(p.P0_14, Level::Low, OutputDrive::Standard);

    (
        Resources {
            status_led,
            radio0,
            midi_uart,
            neopixel_parked,
        },
        usbd,
    )
}
