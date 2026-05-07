// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! UI bench demo — T114.
//!
//! Drives the built-in 1.14″ ST7789 TFT through `osrf-app-ui-bench`.
//! Display is initialised by `display.init().await`, backlight is
//! pulled LOW (active-low on this panel), then `ui_bench::run` paints
//! "OpenStageRF" + a 1 Hz tick counter forever.
//!
//! See `boards/t114/src/{lib.rs,display.rs}` for the v2.1 display
//! bring-up notes — including the nRF52840 SPIM-on-SCK quirk that
//! required a manual `PIN_CNF.INPUT=Connect` poke after `Spim::new`.

use defmt_rtt as _;
use embassy_executor::Spawner;
use embedded_graphics_core::pixelcolor::Rgb565;
use embedded_graphics_core::prelude::*;
use embedded_graphics_core::primitives::Rectangle;
use embedded_graphics_core::Pixel;
use osrf_board_t114 as board;
use panic_probe as _;

use osrf_app_ui_bench::FlushAsync;

// Newtype wrapper so we can implement the foreign `FlushAsync` trait
// (defined in osrf-app-ui-bench) for the foreign `St7789Display`
// (defined in osrf-board-t114).  Drawing forwards directly; flush is
// a no-op since the hand-rolled driver writes pixels to the panel
// immediately (no RAM-side framebuffer to push).
struct DemoDisplay(board::Display);

impl Dimensions for DemoDisplay {
    fn bounding_box(&self) -> Rectangle {
        self.0.bounding_box()
    }
}

impl DrawTarget for DemoDisplay {
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

impl FlushAsync for DemoDisplay {
    type Error = core::convert::Infallible;
    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    board::bootloader_handoff();
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let mut r = board::resources();
    defmt::info!("UI demo (T114): initialising ST7789");

    r.display.init().await;

    // Clear to black BEFORE turning the backlight on so the user
    // doesn't see junk pixels at boot.
    let mut wrapped = DemoDisplay(r.display);
    let _ = wrapped.clear(Rgb565::BLACK);

    // Backlight on (active LOW on this panel).
    r.display_backlight.set_low();

    if let Err(e) = osrf_app_ui_bench::run(&mut wrapped).await {
        defmt::error!("ui_bench exited: {:?}", defmt::Debug2Format(&e));
    }
}
