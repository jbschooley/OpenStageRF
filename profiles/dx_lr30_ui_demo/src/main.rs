// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! UI bench demo — DX-LR30.
//!
//! Brings the SSD1306 OLED out of post-power-on quiescence (`init().await`)
//! then hands it to the board-agnostic `osrf-app-ui-bench` loop.

use defmt_rtt as _;
use embassy_executor::Spawner;
use embedded_graphics_core::draw_target::DrawTarget;
use embedded_graphics_core::geometry::Dimensions;
use embedded_graphics_core::pixelcolor::BinaryColor;
use embedded_graphics_core::primitives::Rectangle;
use embedded_graphics_core::Pixel;
use osrf_board_dx_lr30 as board;
use panic_probe as _;

use osrf_app_ui_bench::FlushAsync;

// Newtype wrapper around the board's foreign `Ssd1306Async` so we can
// satisfy the orphan rule (the trait is foreign-via-app, the type is
// foreign-via-board — neither is local without the wrapper).  The
// wrapper forwards `DrawTarget` directly and provides the async flush.
struct Ssd1306Display(board::Display);

impl Dimensions for Ssd1306Display {
    fn bounding_box(&self) -> Rectangle {
        self.0.bounding_box()
    }
}

impl DrawTarget for Ssd1306Display {
    type Color = BinaryColor;
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

impl FlushAsync for Ssd1306Display {
    type Error = display_interface::DisplayError;
    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.0.flush().await
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    defmt::info!("UI demo (DX-LR30): initialising display");

    let mut r = board::resources();

    // The board returns an un-initialised SSD1306 (resources() is sync;
    // SSD1306 init runs an async I²C command sequence).
    use ssd1306::prelude::DisplayConfigAsync;
    r.display
        .init()
        .await
        .expect("SSD1306 init failed (check wiring + I²C address)");

    let mut wrapped = Ssd1306Display(r.display);
    if let Err(e) = osrf_app_ui_bench::run(&mut wrapped).await {
        defmt::error!("ui_bench exited: {:?}", defmt::Debug2Format(&e));
    }
}
