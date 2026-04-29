// SPDX-License-Identifier: AGPL-3.0-or-later

//! DX-LR30-900M22SP pin assignments — verified against the LR30-SP PCBA
//! schematic diagram (DX-Smart Technology, 2025).
//!
//! Source: `06 Hardware Information/LR30-SP PCBA schematic diagram.pdf`
//!
//! Notable design choices from the schematic:
//!  - No 8 MHz HSE crystal (X1 footprint absent); MCU runs on HSI.
//!  - DIO1 is wired to PC15 (OSC32_OUT net), which doubles as a GPIO
//!    when the 32.768 kHz RTC crystal (X2) is not driving the RTC.
//!  - BUSY and NRST share PA2/PA3 with USART2 — MIDI must use USART3.
//!  - PA0 (TXEN) and PA1 (RXEN) control the module's internal RF switch;
//!    the SX1262 driver must toggle these to select TX/RX paths.
//!  - USART1 (PA9/PA10) is wired to the CH340C USB-serial bridge for
//!    firmware download and serial debugging — do not use for MIDI.

use embassy_stm32::peripherals;

// ── SX1262 radio ──────────────────────────────────────────────────────────
// SPI1 bus
pub type RadioSpi  = peripherals::SPI1;
pub type RadioSck  = peripherals::PA5;  // SPI1_SCK
pub type RadioMiso = peripherals::PA6;  // SPI1_MISO
pub type RadioMosi = peripherals::PA7;  // SPI1_MOSI

pub type RadioCs    = peripherals::PA4;  // NSS  — chip select (active-low)
pub type RadioBusy  = peripherals::PA2;  // BUSY — high while SX1262 is busy
pub type RadioReset = peripherals::PA3;  // NRESET — module reset (active-low)
pub type RadioDio1  = peripherals::PC15; // DIO1 — TX-done/RX-done IRQ (OSC32_OUT repurposed)

// RF-switch control — driven by the SX1262 driver, not by application code.
// The module uses these to select the TX or RX RF path internally.
pub type RadioTxen = peripherals::PA0; // TXEN (PA0 / TXEN on module)
pub type RadioRxen = peripherals::PA1; // RXEN (PA1 / RXEN on module)

// ── MIDI UART ─────────────────────────────────────────────────────────────
// USART3 at 31250 baud 8N1.
// USART1 (PA9/PA10) is wired to the CH340C USB-serial chip — unavailable.
// USART2 (PA2/PA3) clashes with BUSY/NRST radio signals — unavailable.
pub type MidiUart = peripherals::USART3;
pub type MidiTx   = peripherals::PB10; // USART3_TX → FeatherWing RX
pub type MidiRx   = peripherals::PB11; // USART3_RX ← FeatherWing TX

// ── Debug / serial download ───────────────────────────────────────────────
// Wired to CH340C on the dev board.  Available for defmt serial output if
// probe-rs RTT is not used, but do not connect MIDI here.
pub type DebugUart = peripherals::USART1;
pub type DebugTx   = peripherals::PA9;  // USART1_TX → CH340C RX
pub type DebugRx   = peripherals::PA10; // USART1_RX ← CH340C TX

// ── Status LED ────────────────────────────────────────────────────────────
// Active-low onboard LED.
pub type StatusLed = peripherals::PC13;

// ── I²C display (RX role only, Milestone 6) ───────────────────────────────
pub type OledI2c = peripherals::I2C1;
pub type OledScl = peripherals::PB6; // I2C1_SCL
pub type OledSda = peripherals::PB7; // I2C1_SDA

// ── 5-way joystick (RX role only, Milestone 6) ────────────────────────────
// PA9/PA10 unavailable (CH340C). PA8, PB8, PB9 are free; remaining two
// picks (PB3, PB4) are on the expansion header — verify against actual
// joystick wiring before Milestone 6.
pub type JoyUp     = peripherals::PA8;
pub type JoyDown   = peripherals::PB8;
pub type JoyLeft   = peripherals::PB9;
pub type JoyRight  = peripherals::PB3;
pub type JoyCenter = peripherals::PB4;
