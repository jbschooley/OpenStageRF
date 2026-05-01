// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

//! DX-LR30-900M22SP board — STM32F103C8T6 + DX-LR30-900M22S (SX1262).
//!
//! Each module below corresponds to one piece of board hardware (a connector,
//! a chip, a wired-up sub-circuit).  Profiles `pub use` whichever modules
//! they need.  Trying to import a module that doesn't exist on this board
//! produces a clear "unresolved import" compile error.
//!
//! Pin assignments verified against the LR30-SP PCBA schematic.

pub mod clocks;

// ── Built-in SX1262 radio on SPI1 ────────────────────────────────────────────
pub mod radio0 {
    use embassy_stm32::peripherals;
    pub type Spi  = peripherals::SPI1;
    pub type Sck  = peripherals::PA5;
    pub type Miso = peripherals::PA6;
    pub type Mosi = peripherals::PA7;
    pub type Cs   = peripherals::PA4;
    pub type Busy = peripherals::PA2;
    pub type Dio1 = peripherals::PC15;  // OSC32_OUT repurposed as GPIO
    pub type Nrst = peripherals::PA3;
    pub type Txen = peripherals::PA0;   // RF-switch TX path
    pub type Rxen = peripherals::PA1;   // RF-switch RX path
}

// ── Default radio1 pinout for dual_spi_diff_bus ──────────────────────────────
// Second SX1262 wired to the SPI2 expansion header.  Profiles can override
// by defining their own radio1 module if a different pin set is needed.
pub mod dual_spi_diff_bus_radio1 {
    use embassy_stm32::peripherals;
    pub type Spi  = peripherals::SPI2;
    pub type Sck  = peripherals::PB13;
    pub type Miso = peripherals::PB14;
    pub type Mosi = peripherals::PB15;
    pub type Cs   = peripherals::PB12;  // SPI2_NSS
    pub type Busy = peripherals::PB5;
    pub type Dio1 = peripherals::PA8;
    pub type Nrst = peripherals::PC14;  // OSC32_IN repurposed as GPIO
}

// Note: `dual_spi_same_bus_radio1` would be possible (radio1 sharing SPI1
// with a different CS), but no canonical default pinout is blessed yet.

// ── MIDI UART (USART3) ───────────────────────────────────────────────────────
// USART1 (PA9/PA10) is wired to the CH340C USB-serial bridge — unavailable.
// USART2 (PA2/PA3) clashes with radio BUSY/NRST — unavailable.
pub mod midi_uart {
    use embassy_stm32::peripherals;
    pub type Uart = peripherals::USART3;
    pub type Tx   = peripherals::PB10;
    pub type Rx   = peripherals::PB11;
}

// ── Debug UART (CH340C bridge on USART1) ─────────────────────────────────────
pub mod debug_uart {
    use embassy_stm32::peripherals;
    pub type Uart = peripherals::USART1;
    pub type Tx   = peripherals::PA9;
    pub type Rx   = peripherals::PA10;
}

// ── Status LED (active-low) ──────────────────────────────────────────────────
pub mod led_status {
    use embassy_stm32::peripherals;
    pub type Pin = peripherals::PC13;
}

// ── I²C OLED add-on ──────────────────────────────────────────────────────────
pub mod oled_i2c {
    use embassy_stm32::peripherals;
    pub type I2c = peripherals::I2C1;
    pub type Scl = peripherals::PB6;
    pub type Sda = peripherals::PB7;
}

// ── 5-way joystick add-on ────────────────────────────────────────────────────
pub mod joystick {
    use embassy_stm32::peripherals;
    pub type Up     = peripherals::PA8;
    pub type Down   = peripherals::PB8;
    pub type Left   = peripherals::PB9;
    pub type Right  = peripherals::PB3;
    pub type Center = peripherals::PB4;
}

/// Raw Embassy peripheral tokens.  Use this for fine-grained hardware access
/// in apps that need more than `Resources` provides.
///
/// Default: HSI + PLL at 64 MHz (no external crystal required).
/// Override with `--features hsi` to drop to bare 8 MHz HSI.
pub fn init() -> embassy_stm32::Peripherals {
    #[cfg(feature = "hsi")]
    return embassy_stm32::init(clocks::hsi_config());
    #[cfg(not(feature = "hsi"))]
    return embassy_stm32::init(clocks::hsi_64mhz_config());
}

// ── Board-level resource API ─────────────────────────────────────────────────
// The fields below are HAL-specific types but each implements an embedded-hal
// trait, letting board-agnostic apps drive them through the trait surface.

/// Eagerly-initialised on-board peripherals.  Apps that just want "the LED"
/// or "the MIDI UART" call `resources()` and read fields off the result.
pub struct Resources {
    /// Onboard status LED (PC13, active-low).  Implements
    /// `embedded_hal::digital::OutputPin`.
    pub status_led: embassy_stm32::gpio::Output<'static>,
}

/// Initialise hardware and bundle the common peripherals into `Resources`.
pub fn resources() -> Resources {
    let p = init();
    Resources {
        status_led: embassy_stm32::gpio::Output::new(
            p.PC13,
            embassy_stm32::gpio::Level::High, // active-low — start LED off
            embassy_stm32::gpio::Speed::Low,
        ),
    }
}
