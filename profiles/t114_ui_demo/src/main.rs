// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! UI bench demo — T114.
//!
//! The board crate already initialises the ST7789 (power gate, reset
//! pulse, ST7789 command sequence run via `mipidsi::Builder::init`),
//! so this bin just hands the display to the bench loop.

use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embedded_graphics_core::draw_target::DrawTarget;
use embedded_graphics_core::geometry::Dimensions;
use embedded_graphics_core::pixelcolor::Rgb565;
use embedded_graphics_core::primitives::Rectangle;
use embedded_graphics_core::Pixel;
use osrf_board_t114 as board;
use panic_probe as _;

use osrf_app_ui_bench::FlushAsync;

// Newtype wrapper for the foreign `mipidsi::Display` — needed to satisfy
// the orphan rule (FlushAsync is foreign-via-app, mipidsi::Display is
// foreign-via-board).  Drawing forwards directly; flush is a no-op
// because mipidsi writes to the panel immediately.
struct St7789Display(board::Display);

impl Dimensions for St7789Display {
    fn bounding_box(&self) -> Rectangle {
        self.0.bounding_box()
    }
}

impl DrawTarget for St7789Display {
    type Color = Rgb565;
    type Error = <board::Display as DrawTarget>::Error;
    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        self.0.draw_iter(pixels)
    }
    fn fill_contiguous<I>(&mut self, area: &Rectangle, colors: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Self::Color>,
    {
        self.0.fill_contiguous(area, colors)
    }
    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        self.0.fill_solid(area, color)
    }
    fn clear(&mut self, color: Self::Color) -> Result<(), Self::Error> {
        self.0.clear(color)
    }
}

impl FlushAsync for St7789Display {
    type Error = core::convert::Infallible;
    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    #[cfg(feature = "usb-log")]
    let r = {
        let (r, usbd) = board::resources_and_usbd_with(board::clocks::usb_config());
        board::usb_log::spawn(&spawner, usbd);
        embassy_time::Timer::after_millis(500).await;
        r
    };
    #[cfg(not(feature = "usb-log"))]
    let r = {
        let _ = &spawner;
        board::resources()
    };

    defmt::info!("UI demo (T114): display already initialised; turning backlight on");
    #[cfg(feature = "usb-log")]
    log::info!("UI demo (T114): turning backlight on");

    // The board crate currently leaks the backlight pin in its LOW state
    // for first bring-up (no garbage flash on init).  Re-claim P0_15 here
    // and drive it HIGH so the panel is visible.
    //
    // SAFETY: P0_15 was leaked (forgotten) by the board crate after being
    // initialised LOW; this `steal()` re-claims the same pin.  This is a
    // first-bring-up shortcut — proper fix is a `set_backlight(bool)` API
    // on board::Resources.
    let bl_peri = unsafe { embassy_nrf::peripherals::P0_15::steal() };
    let bl = Output::new(bl_peri, Level::High, OutputDrive::Standard);
    core::mem::forget(bl);

    let mut wrapped = St7789Display(r.display);
    if let Err(e) = osrf_app_ui_bench::run(&mut wrapped).await {
        defmt::error!("ui_bench exited: {:?}", defmt::Debug2Format(&e));
    }
}
