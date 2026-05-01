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

use embassy_stm32::{bind_interrupts, dma, exti, i2c, interrupt, peripherals, usart};

pub mod clocks;

// EXTI15_10 (services lines 10..=15 on STM32F1) drives DIO1 = PC15.
// DMA1 channels 2 (RX) and 3 (TX) drive SPI1 transfers for the radio.
//
// USART3 binding feeds `BufferedUart` (interrupt-driven, no DMA): on the
// STM32F103C8 USART3 nominally maps to DMA1_CH2/CH3, but those channels
// are already taken by SPI1 above, and at 31250 baud (~3.1 KB/s) DMA is
// pure ceremony — the per-byte interrupt is fine.
// I2C1 EV/ER + DMA1_CH6/CH7 service the OLED on PB6/PB7.
// On STM32F103 the I2C1 RX/TX DMA channels are CH7/CH6 respectively (RM0008
// table 78); CH2/CH3 are taken by SPI1 above, so I²C and the radio do not
// fight for DMA channels.
bind_interrupts!(struct Irqs {
    EXTI15_10     => exti::InterruptHandler<interrupt::typelevel::EXTI15_10>;
    DMA1_CHANNEL2 => dma::InterruptHandler<peripherals::DMA1_CH2>;
    DMA1_CHANNEL3 => dma::InterruptHandler<peripherals::DMA1_CH3>;
    DMA1_CHANNEL6 => dma::InterruptHandler<peripherals::DMA1_CH6>;
    DMA1_CHANNEL7 => dma::InterruptHandler<peripherals::DMA1_CH7>;
    USART3        => usart::BufferedInterruptHandler<peripherals::USART3>;
    I2C1_EV       => i2c::EventInterruptHandler<peripherals::I2C1>;
    I2C1_ER       => i2c::ErrorInterruptHandler<peripherals::I2C1>;
});

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

/// SX1262 wrapper as it lives on this board: SPI1 + EXTI-driven DIO1 +
/// GPIO NRESET + two-pin RF switch (TXEN/RXEN).
pub type Radio0 = osrf_radio_sx126x::Sx1262Radio<
    embedded_hal_bus::spi::ExclusiveDevice<
        embassy_stm32::spi::Spi<'static, embassy_stm32::mode::Async, embassy_stm32::spi::mode::Master>,
        embassy_stm32::gpio::Output<'static>,
        embassy_time::Delay,
    >,
    embassy_stm32::exti::ExtiInput<'static, embassy_stm32::mode::Async>,
    embassy_stm32::gpio::Output<'static>,
    osrf_radio_sx126x::PinRfSwitch<embassy_stm32::gpio::Output<'static>, embassy_stm32::gpio::Output<'static>>,
>;

/// SSD1306 128×64 mono OLED on I²C1 (PB6=SCL, PB7=SDA), async via
/// `embedded-hal-async`.  Buffered-graphics mode means `embedded_graphics::
/// DrawTarget<Color = BinaryColor>` operations are sync (writing to an
/// internal pixel buffer); the user must `display.flush().await` to push
/// the buffer to the panel.  `init().await` must be called once before
/// drawing — `resources()` returns an *un-initialised* display because
/// it is itself sync.
pub type Display = ssd1306::Ssd1306Async<
    ssd1306::prelude::I2CInterface<embassy_stm32::i2c::I2c<'static, embassy_stm32::mode::Async, embassy_stm32::i2c::Master>>,
    ssd1306::prelude::DisplaySize128x64,
    ssd1306::mode::BufferedGraphicsModeAsync<ssd1306::prelude::DisplaySize128x64>,
>;

/// MIDI UART (USART3) configured at 31250 baud 8N1, interrupt-driven via
/// `BufferedUart`.  Implements `embedded_io_async::Read` and `Write`
/// directly so app crates can drive it through HAL-agnostic traits.
///
/// Why `BufferedUart` and not the DMA-backed `Uart`?  USART3 on the
/// STM32F103C8 is hardwired to DMA1_CH2 (RX) and DMA1_CH3 (TX) — the
/// same channels SPI1 (the SX1262 radio bus) requires.  DMA channel
/// allocation is fixed in silicon, so the only way to keep both the
/// radio and a MIDI UART alive simultaneously is to drop one of them
/// off DMA.  At MIDI's 31250 baud (~3.1 KB/s, byte-rate ~3 kHz),
/// per-byte interrupt servicing is trivial; SPI1 at 8 MHz needs DMA.
pub type MidiUart = embassy_stm32::usart::BufferedUart<'static>;

/// Eagerly-initialised on-board peripherals.  Apps that just want "the LED"
/// or "the MIDI UART" call `resources()` and read fields off the result.
pub struct Resources {
    /// Onboard status LED (PC13, active-low).  Implements
    /// `embedded_hal::digital::OutputPin`.
    pub status_led: embassy_stm32::gpio::Output<'static>,

    /// Built-in SX1262 radio on SPI1 with TXEN/RXEN external RF switch.
    ///
    /// NRESET has already been pulsed; caller can immediately
    /// `radio0.init().await` and proceed to configure modulation.
    pub radio0: Radio0,

    /// DIN MIDI UART on USART3 (PB10 TX, PB11 RX) at 31250 baud 8N1.
    pub midi_uart: MidiUart,

    /// SSD1306 128×64 mono OLED on I²C1 (PB6=SCL, PB7=SDA), 400 kHz, async.
    ///
    /// Returned in *un-initialised* buffered-graphics mode: the caller
    /// must `display.init().await` once before drawing, and call
    /// `display.flush().await` after each batch of `embedded_graphics`
    /// draw operations to push the in-memory buffer to the panel.  We
    /// can't init here because `resources()` is sync — the SSD1306 init
    /// sequence is async (it issues a sequence of I²C writes).
    pub display: Display,
}

/// Initialise hardware and bundle the common peripherals into `Resources`.
pub fn resources() -> Resources {
    use embassy_stm32::gpio::{Level, Output, Pull, Speed};
    use embassy_stm32::spi::{Config as SpiConfig, Spi};
    use embassy_stm32::time::Hertz;
    use embassy_stm32::exti::ExtiInput;

    let p = init();

    // ── Status LED (PC13, active-low) ───────────────────────────────────────
    let status_led = Output::new(p.PC13, Level::High, Speed::Low);

    // ── SX1262 SPI bus: SPI1, 8 MHz, MODE_0, DMA1_CH3 (TX) / DMA1_CH2 (RX) ──
    let mut spi_cfg = SpiConfig::default();
    spi_cfg.frequency = Hertz(8_000_000);
    let spi = Spi::new(
        p.SPI1,
        p.PA5,        // SCK
        p.PA7,        // MOSI
        p.PA6,        // MISO
        p.DMA1_CH3,   // TX DMA
        p.DMA1_CH2,   // RX DMA
        Irqs,
        spi_cfg,
    );
    let cs = Output::new(p.PA4, Level::High, Speed::Medium);
    let spi_dev = embedded_hal_bus::spi::ExclusiveDevice::new(spi, cs, embassy_time::Delay)
        .expect("CS pin set_high cannot fail (Infallible)");

    // ── DIO1 = PC15, EXTI15-driven async input ──────────────────────────────
    let dio1 = ExtiInput::new(p.PC15, p.EXTI15, Pull::Down, Irqs);

    // ── NRESET = PA3 ────────────────────────────────────────────────────────
    let mut reset = Output::new(p.PA3, Level::High, Speed::Medium);

    // ── RF switch GPIOs (TXEN=PA0, RXEN=PA1, both idle low) ─────────────────
    let txen = Output::new(p.PA0, Level::Low, Speed::Medium);
    let rxen = Output::new(p.PA1, Level::Low, Speed::Medium);

    // ── Hardware reset pulse: low ≥100 µs, then ≥10 ms post-reset wait ──────
    // We don't have an async runtime up yet, so use cycle-counted busy-waits.
    // SYSCLK is 64 MHz (HSI+PLL): 1 µs ≈ 64 cycles, 1 ms ≈ 64 000.  Be generous.
    reset.set_low();
    cortex_m::asm::delay(64 * 200);          // ~200 µs
    reset.set_high();
    cortex_m::asm::delay(64_000 * 15);       // ~15 ms

    let radio0 = osrf_radio_sx126x::Sx1262Radio::new(
        spi_dev,
        dio1,
        reset,
        osrf_radio_sx126x::PinRfSwitch::new(txen, rxen),
    );

    // ── MIDI UART: USART3 @ 31250 baud 8N1, interrupt-driven ──────────────
    // Static ring-buffer storage for the BufferedUart.  Sizes picked to
    // comfortably absorb one busy MIDI second (~3 KB/s) of input plus a
    // similar TX burst, even if the executor is briefly stalled.
    //
    // The unsafe block is the standard pattern for handing a 'static
    // mutable slice to embassy: the static storage exists for the
    // lifetime of the program, and we promise to call `resources()` only
    // once (it consumes the singleton `Peripherals` token).
    static mut MIDI_TX_BUF: [u8; 64] = [0; 64];
    static mut MIDI_RX_BUF: [u8; 256] = [0; 256];
    let mut uart_cfg = embassy_stm32::usart::Config::default();
    uart_cfg.baudrate = 31250;
    let midi_uart = embassy_stm32::usart::BufferedUart::new(
        p.USART3,
        p.PB11, // RX
        p.PB10, // TX
        // SAFETY: static storage, single-call resources() consumes
        // Peripherals, so this slice is uniquely owned.
        unsafe { &mut *core::ptr::addr_of_mut!(MIDI_TX_BUF) },
        unsafe { &mut *core::ptr::addr_of_mut!(MIDI_RX_BUF) },
        Irqs,
        uart_cfg,
    )
    .expect("USART3 BufferedUart init failed (baudrate divider out of range?)");

    // ── OLED display: I²C1 @ 400 kHz on PB6/PB7, async (DMA1_CH6/CH7) ──────
    // I²C1's DMA channels (CH6/CH7) don't conflict with SPI1's (CH2/CH3) or
    // USART3's (also CH2/CH3, but USART3 here uses BufferedUart instead).
    //
    // The SSD1306 lives at I²C address 0x3C by default; `I2CDisplayInterface
    // ::new` bakes that in.  Buffered-graphics mode keeps a 128×64-bit
    // (1024-byte) framebuffer in RAM; `flush().await` walks the buffer once
    // to the panel.  At 400 kHz that's ≈ 22 ms per full flush; partial
    // flushes via `flush_region` are an option later.
    // F103 is gpio_v1 (no sda_pullup/scl_pullup config — boards are expected
    // to provide external pull-ups on SDA/SCL, which both the LR30 add-on
    // header and any common SSD1306 module do).
    let mut i2c_cfg = embassy_stm32::i2c::Config::default();
    i2c_cfg.frequency = embassy_stm32::time::Hertz(400_000);
    let i2c = embassy_stm32::i2c::I2c::new(
        p.I2C1,
        p.PB6,        // SCL
        p.PB7,        // SDA
        p.DMA1_CH6,   // TX DMA (RM0008 table 78: I2C1_TX → CH6)
        p.DMA1_CH7,   // RX DMA (I2C1_RX → CH7)
        Irqs,
        i2c_cfg,
    );
    let interface = ssd1306::I2CDisplayInterface::new(i2c);
    let display = ssd1306::Ssd1306Async::new(
        interface,
        ssd1306::prelude::DisplaySize128x64,
        ssd1306::prelude::DisplayRotation::Rotate0,
    )
    .into_buffered_graphics_mode();

    Resources { status_led, radio0, midi_uart, display }
}
