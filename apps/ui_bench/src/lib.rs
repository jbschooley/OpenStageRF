// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

//! Board-agnostic UI bench.
//!
//! Draws "OpenStageRF" + a 1 Hz frame counter onto any
//! `embedded-graphics::DrawTarget` whose colour can be derived from
//! `BinaryColor`.  Intentionally small — proves the abstraction seam
//! between board-supplied display drivers (mono SSD1306 over I²C,
//! colour ST7789 over SPI) and the future UI render code.
//!
//! ## The flush split
//!
//! Some `DrawTarget` implementations (notably `ssd1306` in buffered
//! graphics mode) hold a RAM-side framebuffer; pixel writes are sync,
//! but pushing the buffer to the panel is *async* via a `flush()`
//! method that lives outside the `DrawTarget` trait.  Others (notably
//! `mipidsi`) are immediate — every draw call writes the panel.
//!
//! To unify both, this crate exposes a tiny [`FlushAsync`] trait.
//! Profile crates implement it for their concrete display type; the
//! impl is a no-op on immediate displays and a real `flush().await`
//! on buffered ones.  Coherence works out because the trait lives
//! here and the profile crates depend on this crate.
//!
//! ## DrawTarget colour bound
//!
//! All drawing is done in the abstract `BinaryColor` palette and
//! mapped via `Into<D::Color>` at the call site.  `BinaryColor`
//! has built-in `From` impls for `Rgb565` (and most other
//! embedded-graphics colour types), so the same `run()` function
//! drives both the mono and colour displays without per-board
//! cfg gates.

use core::fmt::Write;
use embassy_time::Timer;
use embedded_graphics::{
    mono_font::{ascii::FONT_6X10, MonoTextStyle},
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
    text::Text,
};
use heapless::String;

/// Async-flush adapter.  Implemented by profile crates for their
/// concrete display type (no-op for immediate displays, real flush
/// for buffered ones).  See the crate-level doc-comment for the
/// rationale.
pub trait FlushAsync {
    type Error;
    fn flush(&mut self) -> impl core::future::Future<Output = Result<(), Self::Error>>;
}

/// Draw "OpenStageRF" + a tick counter at 1 Hz.  Returns only on a
/// display error (in which case the error bubbles out).
///
/// Generic over any `DrawTarget` whose colour space accepts
/// `BinaryColor` via `From` — that covers both `Rgb565` (mipidsi) and
/// `BinaryColor` itself (ssd1306) for free.
///
/// Drawing errors and flush errors are reported through different
/// paths: drawing returns `Result<(), D::Error>`, flushing returns
/// `Result<(), <D as FlushAsync>::Error>`.  We convert both into a
/// shared opaque error via a helper enum so the public signature
/// stays simple.
pub async fn run<D>(
    display: &mut D,
) -> Result<(), Error<<D as DrawTarget>::Error, <D as FlushAsync>::Error>>
where
    D: DrawTarget + FlushAsync,
    <D as DrawTarget>::Color: From<BinaryColor>,
{
    let fg: <D as DrawTarget>::Color = BinaryColor::On.into();
    let bg: <D as DrawTarget>::Color = BinaryColor::Off.into();

    // Initial clear + static title.
    let bbox = display.bounding_box();
    Rectangle::new(Point::zero(), bbox.size)
        .into_styled(PrimitiveStyle::with_fill(bg))
        .draw(display)
        .map_err(Error::Draw)?;

    let title_style = MonoTextStyle::new(&FONT_6X10, fg);
    Text::new("OpenStageRF", Point::new(8, 16), title_style)
        .draw(display)
        .map_err(Error::Draw)?;
    Text::new("UI bench v0.1", Point::new(8, 30), title_style)
        .draw(display)
        .map_err(Error::Draw)?;
    display.flush().await.map_err(Error::Flush)?;

    let mut tick: u32 = 0;
    let mut s: String<32> = String::new();
    loop {
        // Repaint just the tick line each second.  64-pixel-tall mono
        // displays and 135-pixel-tall colour displays both have plenty
        // of headroom around y=50..62.
        Rectangle::new(Point::new(8, 50), Size::new(120, 12))
            .into_styled(PrimitiveStyle::with_fill(bg))
            .draw(display)
            .map_err(Error::Draw)?;

        s.clear();
        let _ = write!(&mut s, "tick {tick}");
        Text::new(&s, Point::new(8, 60), title_style)
            .draw(display)
            .map_err(Error::Draw)?;

        display.flush().await.map_err(Error::Flush)?;

        defmt::info!("ui tick {}", tick);
        tick = tick.wrapping_add(1);
        Timer::after_millis(1000).await;
    }
}

/// Composite error for drawing + flushing.  We keep the two channels
/// separate (instead of erasing to one type) so debug output preserves
/// which side of the bus failed.
#[derive(Debug)]
pub enum Error<DrawErr, FlushErr> {
    Draw(DrawErr),
    Flush(FlushErr),
}
