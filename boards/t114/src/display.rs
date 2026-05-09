// SPDX-License-Identifier: AGPL-3.0-or-later

//! Hand-rolled ST7789 driver for the T114's 1.14″ 240×135 colour TFT.
//!
//! Replaces the prior `mipidsi::Builder::init` path which hung on this
//! hardware for reasons not yet root-caused.  The init sequence here is
//! a straight port of the controller datasheet's power-on procedure;
//! draw operations write the panel directly via blocking SPI (no
//! framebuffer in RAM, no DMA, no async-from-sync trickery).
//!
//! ## Geometry
//!
//! The 1.14″ panel uses a window of the ST7789 controller's 240×320
//! RAM.  In the rotation we use here (MADCTL = 0x60, landscape, RGB
//! order) the visible window is `[40..280] × [53..188]` — i.e. a 240
//! pixel-wide × 135 pixel-tall strip starting at column 40, row 53.
//! [`X_OFFSET`] / [`Y_OFFSET`] encode that and are added to every
//! `set_window` call so user code can pretend the panel is the natural
//! 240×135 with origin at (0, 0).
//!
//! ## Why not mipidsi
//!
//! mipidsi 0.10's `Builder::init` sequence hangs on this T114 v2.0
//! hardware (`profiles/t114_ui_demo` was disabled with a comment to
//! that effect; see the boards/t114 git history).  Other paths
//! exercising TWISPI1 (raw `spi.blocking_write`) and `Delay::delay_ms`
//! verified working in isolation, so the issue is specific to the
//! mipidsi interaction.  Hand-rolling sidesteps that entirely with
//! ~150 lines of code and gives us full control over timing.
//!
//! ## DrawTarget
//!
//! Implements `embedded_graphics_core::DrawTarget<Color = Rgb565>`
//! synchronously via blocking SPI.  `draw_iter`, `fill_contiguous`,
//! and `fill_solid` all bracket their work in a single SPI transaction
//! per call.  Performance is fine for our 30 Hz UI (full clear at
//! 8 MHz SPI takes ~64 ms; partial redraws are well under one frame).

use core::convert::Infallible;

use embassy_nrf::gpio::Output;
use embassy_nrf::peripherals;
use embassy_nrf::spim::Spim;
use embassy_time::Timer;

// ── Pin / peripheral type aliases (board layout documentation) ──────────────

/// SPIM peripheral driving the panel.
pub type Spi = peripherals::TWISPI1;
/// SPI clock pin.
pub type Sck = peripherals::P1_08;
/// SPI MOSI (data into the panel).
pub type Mosi = peripherals::P1_09;
/// SPI chip-select.
pub type Cs = peripherals::P0_11;
/// Data/command-select line — low for command bytes, high for data.
pub type Dc = peripherals::P0_12;
/// Hardware reset line.
pub type Reset = peripherals::P0_02;
/// Backlight enable (active high).
pub type Backlight = peripherals::P0_15;
/// VTFT_CTRL — gates the display panel's power rail.  Drive HIGH
/// before any SPI activity to the panel; drive LOW to power-down.
pub type PwrCtrl = peripherals::P0_03;
use embedded_graphics_core::{
    draw_target::DrawTarget,
    geometry::{OriginDimensions, Size},
    pixelcolor::{raw::RawU16, Rgb565},
    prelude::*,
    primitives::Rectangle,
    Pixel,
};

/// Visible width of the panel in pixels.  240 columns in landscape.
pub const WIDTH: u16 = 240;
/// Visible height of the panel in pixels.  135 rows in landscape.
pub const HEIGHT: u16 = 135;

/// X offset into the ST7789's 240-column RAM where the visible 1.14″
/// panel's first column lives, with our chosen MADCTL / rotation.
const X_OFFSET: u16 = 40;
/// Y offset into the ST7789's 320-row RAM where the visible 1.14″
/// panel's first row lives, with our chosen MADCTL / rotation.
const Y_OFFSET: u16 = 53;

// ── ST7789 commands we emit ─────────────────────────────────────────────────
const CMD_SWRESET: u8 = 0x01;
const CMD_SLPOUT: u8 = 0x11;
const CMD_NORON: u8 = 0x13;
const CMD_INVON: u8 = 0x21;
const CMD_DISPON: u8 = 0x29;
const CMD_CASET: u8 = 0x2A;
const CMD_RASET: u8 = 0x2B;
const CMD_RAMWR: u8 = 0x2C;
const CMD_MADCTL: u8 = 0x36;
const CMD_COLMOD: u8 = 0x3A;
// Power / gamma commands.  Many ST7789 LCMs come from POR with
// safe defaults and don't need these; others ship with defaults
// that produce a black panel until VCOM / gamma are programmed
// explicitly.  Heltec sources LCMs from multiple suppliers within
// the T114 v2.1 SKU, so we send the Adafruit-compatible block
// unconditionally — harmless on the permissive panels, mandatory
// on the strict ones.  See ST7789V datasheet §6.2 for register
// definitions.
const CMD_PORCTRL: u8 = 0xB2; // Porch control
const CMD_GCTRL: u8 = 0xB7; // Gate control
const CMD_VCOMS: u8 = 0xBB; // VCOM voltage
const CMD_LCMCTRL: u8 = 0xC0; // LCM control
const CMD_VDVVRHEN: u8 = 0xC2; // VDV / VRH command enable
const CMD_VRHS: u8 = 0xC3; // VRH set
const CMD_VDVS: u8 = 0xC4; // VDV set
const CMD_FRCTRL2: u8 = 0xC6; // Frame rate control (normal mode)
const CMD_PWCTRL1: u8 = 0xD0; // Power control 1
const CMD_PVGAMCTRL: u8 = 0xE0; // Positive voltage gamma
const CMD_NVGAMCTRL: u8 = 0xE1; // Negative voltage gamma

/// 1.14″ ST7789 colour TFT.
///
/// Owns the SPI peripheral plus the chip-select, data/command, and
/// reset GPIO outputs.  Backlight and VTFT_CTRL power-gate pins are
/// owned by the `Resources` struct directly so apps can toggle them
/// independently of the display state — backlight in particular needs
/// to come on *after* a clear-to-background to avoid showing junk pixels.
pub struct St7789Display {
    spi: Spim<'static>,
    cs: Output<'static>,
    dc: Output<'static>,
    reset: Output<'static>,
    /// VTFT_CTRL — gates the TFT VDD rail (P0_03 on T114 v2.1).
    /// **Active LOW**: drive LOW to enable LCM power, HIGH to disable.
    /// Owned by the Display so `init()` can apply power, wait the
    /// rail-warmup interval, then run the IC's reset + init
    /// sequence.  Matches what Meshtastic / MeshCore do — see
    /// `[meshtastic firmware] variants/nrf52840/heltec_mesh_node_t114/
    /// variant.h` (`VTFT_CTRL = 3`, `VTFT_ON = LOW`).
    vtft: Output<'static>,
}

impl St7789Display {
    /// Construct a new display.  Does **not** run the init sequence —
    /// call [`init`](Self::init) for that.  Separated so init can be
    /// async (uses [`Timer`] for the datasheet-mandated delays) while
    /// the constructor stays sync, which matters for boards that
    /// build their `Resources` outside an async context.
    pub fn new(
        spi: Spim<'static>,
        cs: Output<'static>,
        dc: Output<'static>,
        reset: Output<'static>,
        vtft: Output<'static>,
    ) -> Self {
        Self { spi, cs, dc, reset, vtft }
    }

    /// Run the ST7789 power-on sequence: hardware reset pulse →
    /// SWRESET → SLPOUT → COLMOD (RGB565) → MADCTL (landscape, RGB
    /// order) → INVON → NORON → DISPON.  Datasheet-mandated delays
    /// are observed via `Timer`.  Returns when the panel is ready
    /// for `draw_*` calls.
    pub async fn init(&mut self) {
        #[cfg(feature = "defmt")]
        defmt::info!("st7789: init begin");

        // Power up the LCM rail via VTFT_CTRL (active LOW).
        // Up until 2026-05 the board crate had this pin wrong (was
        // driving P0_21 / VEXT, the GPS rail), which explained why
        // the display only worked when an SWD probe's 3.3 V wire
        // was attached: the probe back-fed the LCM rail through
        // P0_03's leakage path.  With VTFT_CTRL driven correctly
        // the on-board regulator alone is enough.
        self.vtft.set_low();

        // Rail warmup.  Meshtastic uses `PERIPHERAL_WARMUP_MS = 1000`
        // (one full second) before any SPI on this exact board.
        // 1 s is generous; the LCM's internal POR completes well
        // before that, but reading their delays as a "what works
        // reliably across all units" floor.  We can tune down later
        // if it bothers us.
        Timer::after_millis(1000).await;
        #[cfg(feature = "defmt")]
        defmt::info!("st7789: VTFT enabled, rail warm");

        // Hardware reset.  ST7789 datasheet says minimum 10 µs LOW,
        // but Meshtastic's working ST7789 driver uses **10 ms LOW**
        // for this exact panel — many ST7789 panel revisions don't
        // fully reset on a sub-millisecond pulse.  Mirror that:
        // HIGH 1 ms → LOW 10 ms → HIGH, then 120 ms post-reset wait
        // before the first SPI command.
        self.reset.set_high();
        Timer::after_millis(1).await;
        self.reset.set_low();
        Timer::after_millis(10).await;
        self.reset.set_high();
        Timer::after_millis(120).await;
        #[cfg(feature = "defmt")]
        defmt::info!("st7789: hw reset done");

        // Software reset.  Datasheet: max 120 ms before any further
        // command.  150 ms gives margin.
        self.write_command(CMD_SWRESET);
        Timer::after_millis(150).await;
        #[cfg(feature = "defmt")]
        defmt::info!("st7789: SWRESET done");

        // Sleep out.  Datasheet: 120 ms before sending another command,
        // and 5 ms before SLPOUT can be reissued.  10 ms suffices for
        // the path we take (SLPOUT → COLMOD).
        self.write_command(CMD_SLPOUT);
        Timer::after_millis(120).await;
        #[cfg(feature = "defmt")]
        defmt::info!("st7789: SLPOUT done");

        // Pixel format: 16-bit/pixel RGB565 (`0x55` = 65k colors over
        // SPI).  No delay required by datasheet; brief pause for
        // safety.
        self.write_command_data(CMD_COLMOD, &[0x55]);
        Timer::after_millis(10).await;

        // Memory access control: 0x60 = MX (mirror X) + MV (row/col
        // swap), RGB order.  This puts the panel in landscape with
        // origin (0, 0) at the upper-left of the visible window once
        // X_OFFSET / Y_OFFSET are applied.
        self.write_command_data(CMD_MADCTL, &[0x60]);

        // Power / gamma block — Adafruit-compatible defaults.  Many
        // ST7789 panels render fine with their POR power/gamma
        // settings; some ship with defaults that produce a black
        // panel and require explicit programming.  Heltec sources
        // LCMs from multiple suppliers within the T114 v2.1 SKU and
        // we've observed both kinds.  Sending this block
        // unconditionally costs a few ms of init time and works on
        // both populations.
        self.write_command_data(CMD_PORCTRL, &[0x0C, 0x0C, 0x00, 0x33, 0x33]);
        self.write_command_data(CMD_GCTRL, &[0x35]);
        self.write_command_data(CMD_VCOMS, &[0x19]);
        self.write_command_data(CMD_LCMCTRL, &[0x2C]);
        self.write_command_data(CMD_VDVVRHEN, &[0x01]);
        self.write_command_data(CMD_VRHS, &[0x12]);
        self.write_command_data(CMD_VDVS, &[0x20]);
        self.write_command_data(CMD_FRCTRL2, &[0x0F]);
        self.write_command_data(CMD_PWCTRL1, &[0xA4, 0xA1]);
        self.write_command_data(
            CMD_PVGAMCTRL,
            &[
                0xD0, 0x04, 0x0D, 0x11, 0x13, 0x2B, 0x3F, 0x54, 0x4C, 0x18, 0x0D, 0x0B, 0x1F,
                0x23,
            ],
        );
        self.write_command_data(
            CMD_NVGAMCTRL,
            &[
                0xD0, 0x04, 0x0C, 0x11, 0x13, 0x2C, 0x3F, 0x44, 0x51, 0x2F, 0x1F, 0x1F, 0x20,
                0x23,
            ],
        );
        Timer::after_millis(10).await;
        #[cfg(feature = "defmt")]
        defmt::info!("st7789: power+gamma programmed");

        // Display inversion ON — required for ST7789 to render normal
        // (non-inverted) colors.  Backwards from earlier ST77xx parts.
        self.write_command(CMD_INVON);
        Timer::after_millis(10).await;

        // Normal display mode (vs partial / idle).
        self.write_command(CMD_NORON);
        Timer::after_millis(10).await;

        // Display ON.  After this RAMWR will start showing pixels.
        self.write_command(CMD_DISPON);
        Timer::after_millis(10).await;

        #[cfg(feature = "defmt")]
        defmt::info!("st7789: init complete");
    }

    /// Diagnostic: send an arbitrary single-byte command (no data
    /// args) to the panel.  Public for use by the SPI viability
    /// smoke test in `t114_ui_demo`; not intended for normal use.
    pub async fn send_raw_command(&mut self, cmd: u8) {
        self.write_command(cmd);
    }

    /// Push the dirty region of a [`Framebuffer`](crate::framebuffer::Framebuffer)
    /// to the panel via **async** SPI, row by row.  Yields the
    /// executor during each DMA burst so other tasks (notably
    /// `osrf_link_runtime::run_rx`) can run between bursts —
    /// without this, a sync render of ~30 ms of SPI bursts blocks
    /// `run_rx` long enough that the SX1262 RX FIFO overflows and
    /// 5-12 % of inbound packets get dropped.
    ///
    /// The dirty bounding box is cleared on completion.  `set_window`
    /// stays sync — its bursts are tiny (a handful of 1-5-byte
    /// commands, ~50 µs total) and not worth the extra plumbing.
    /// The big-data path — the per-row pixel stream — is what
    /// matters and that's where we yield.
    pub async fn flush(&mut self, fb: &mut crate::framebuffer::Framebuffer) {
        use crate::framebuffer::FB_W;
        let Some(b) = fb.dirty_box() else {
            return;
        };
        self.set_window(b.x0, b.y0, b.x1, b.y1);
        self.dc.set_high();
        self.cs.set_low();

        let span = (b.x1 - b.x0 + 1) as usize;
        let mut row_buf = [0u8; FB_W as usize * 2];
        let pixels = fb.pixels();
        for y in b.y0..=b.y1 {
            let row_start = y as usize * FB_W as usize + b.x0 as usize;
            let row = &pixels[row_start..row_start + span];
            for (i, raw) in row.iter().enumerate() {
                row_buf[i * 2] = (raw >> 8) as u8;
                row_buf[i * 2 + 1] = (raw & 0xFF) as u8;
            }
            let _ = self.spi.write(&row_buf[..span * 2]).await;
        }

        self.cs.set_high();
        fb.clear_dirty();
    }

    // ── Low-level helpers (sync, blocking SPI) ───────────────────────

    /// Write a single command byte (DC low) in its own SPI transaction.
    fn write_command(&mut self, cmd: u8) {
        self.dc.set_low();
        self.cs.set_low();
        let _ = self.spi.blocking_write(&[cmd]);
        self.cs.set_high();
    }

    /// Write a command byte (DC low) followed by data bytes (DC high)
    /// in two back-to-back SPI transactions, with CS held low across
    /// the boundary.  ST7789 requires CS to stay asserted for the
    /// whole command+data unit; we lift it only at the very end.
    fn write_command_data(&mut self, cmd: u8, data: &[u8]) {
        self.dc.set_low();
        self.cs.set_low();
        let _ = self.spi.blocking_write(&[cmd]);
        if !data.is_empty() {
            self.dc.set_high();
            let _ = self.spi.blocking_write(data);
        }
        self.cs.set_high();
    }

    /// Set the active drawing window via CASET / RASET, then issue
    /// RAMWR.  Caller must keep CS asserted (re-asserted by the next
    /// data write) and follow up with the pixel data stream.
    fn set_window(&mut self, x0: u16, y0: u16, x1: u16, y1: u16) {
        let x0 = x0 + X_OFFSET;
        let x1 = x1 + X_OFFSET;
        let y0 = y0 + Y_OFFSET;
        let y1 = y1 + Y_OFFSET;
        self.write_command_data(
            CMD_CASET,
            &[(x0 >> 8) as u8, (x0 & 0xFF) as u8, (x1 >> 8) as u8, (x1 & 0xFF) as u8],
        );
        self.write_command_data(
            CMD_RASET,
            &[(y0 >> 8) as u8, (y0 & 0xFF) as u8, (y1 >> 8) as u8, (y1 & 0xFF) as u8],
        );
        // RAMWR — start the pixel stream.  Subsequent SPI writes in
        // data mode are RAM data.
        self.write_command(CMD_RAMWR);
    }

    /// Stream `count` copies of one RGB565 pixel as a single SPI
    /// burst.  Used by [`fill_solid`].  The pixel is buffered in
    /// `CHUNK_PIXELS`-pixel chunks so we don't allocate a full
    /// rectangle's worth of bytes on the stack for big fills.
    fn write_solid(&mut self, color: Rgb565, count: u32) {
        const CHUNK_PIXELS: usize = 64;
        let raw: u16 = RawU16::from(color).into_inner();
        let hi = (raw >> 8) as u8;
        let lo = (raw & 0xFF) as u8;
        let mut chunk = [0u8; CHUNK_PIXELS * 2];
        for i in 0..CHUNK_PIXELS {
            chunk[i * 2] = hi;
            chunk[i * 2 + 1] = lo;
        }

        self.dc.set_high();
        self.cs.set_low();
        let mut remaining = count as usize;
        while remaining > 0 {
            let take = remaining.min(CHUNK_PIXELS);
            let _ = self.spi.blocking_write(&chunk[..take * 2]);
            remaining -= take;
        }
        self.cs.set_high();
    }

    /// Stream a sequence of RGB565 pixels in row-major order within
    /// the previously-set window.  Used by [`fill_contiguous`] and
    /// indirectly by [`draw_iter`].
    fn write_pixels<I>(&mut self, pixels: I)
    where
        I: IntoIterator<Item = Rgb565>,
    {
        const CHUNK_PIXELS: usize = 64;
        let mut buf = [0u8; CHUNK_PIXELS * 2];
        let mut filled = 0usize;
        self.dc.set_high();
        self.cs.set_low();
        for p in pixels {
            let raw: u16 = RawU16::from(p).into_inner();
            buf[filled] = (raw >> 8) as u8;
            buf[filled + 1] = (raw & 0xFF) as u8;
            filled += 2;
            if filled == buf.len() {
                let _ = self.spi.blocking_write(&buf);
                filled = 0;
            }
        }
        if filled > 0 {
            let _ = self.spi.blocking_write(&buf[..filled]);
        }
        self.cs.set_high();
    }
}

// ── embedded-graphics-core trait impls ──────────────────────────────────────

impl OriginDimensions for St7789Display {
    fn size(&self) -> Size {
        Size::new(WIDTH as u32, HEIGHT as u32)
    }
}

impl DrawTarget for St7789Display {
    type Color = Rgb565;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        // For draw_iter we don't know the bounds up front, so we
        // emit one CASET/RASET/RAMWR per pixel.  Slow but correct
        // for sparse draws (e.g. `Line` rendering); bulk operations
        // should go through fill_solid / fill_contiguous instead.
        for Pixel(coord, color) in pixels {
            if coord.x < 0
                || coord.y < 0
                || coord.x >= WIDTH as i32
                || coord.y >= HEIGHT as i32
            {
                continue;
            }
            let x = coord.x as u16;
            let y = coord.y as u16;
            self.set_window(x, y, x, y);
            self.write_pixels(core::iter::once(color));
        }
        Ok(())
    }

    fn fill_contiguous<I>(&mut self, area: &Rectangle, colors: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Self::Color>,
    {
        if let Some(intersection) = area.intersection(&self.bounding_box()).bottom_right() {
            // bottom_right returns Some only if the rectangle is non-empty.
            let top_left = area.intersection(&self.bounding_box()).top_left;
            let x0 = top_left.x as u16;
            let y0 = top_left.y as u16;
            let x1 = intersection.x as u16;
            let y1 = intersection.y as u16;
            self.set_window(x0, y0, x1, y1);
            // We trust the caller to provide enough colors for the
            // *original* area; we filter to the on-screen subset by
            // discarding pixels that fall outside the intersection's
            // row-major sequence.  For our use case (UI redraws) the
            // area is always already in-bounds, so this is rarely hit.
            let area_w = area.size.width;
            let isect = area.intersection(&self.bounding_box());
            let isect_w = isect.size.width;
            let isect_h = isect.size.height;
            let off_x = (isect.top_left.x - area.top_left.x) as u32;
            let off_y = (isect.top_left.y - area.top_left.y) as u32;
            let mut iter = colors.into_iter();
            // Skip rows above the intersection.
            for _ in 0..(off_y * area_w) {
                if iter.next().is_none() {
                    return Ok(());
                }
            }
            for _ in 0..isect_h {
                // Skip the prefix of this row that's left of the intersection.
                for _ in 0..off_x {
                    if iter.next().is_none() {
                        return Ok(());
                    }
                }
                // Write the visible portion.
                let mut row_colors: heapless::Vec<Rgb565, 256> = heapless::Vec::new();
                for _ in 0..isect_w {
                    match iter.next() {
                        Some(c) => {
                            let _ = row_colors.push(c);
                        }
                        None => break,
                    }
                }
                self.write_pixels(row_colors.iter().copied());
                // Skip the suffix of this row that's right of the intersection.
                for _ in 0..(area_w - isect_w - off_x) {
                    if iter.next().is_none() {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        let isect = area.intersection(&self.bounding_box());
        if isect.size.width == 0 || isect.size.height == 0 {
            return Ok(());
        }
        let x0 = isect.top_left.x as u16;
        let y0 = isect.top_left.y as u16;
        let x1 = (isect.top_left.x + isect.size.width as i32 - 1) as u16;
        let y1 = (isect.top_left.y + isect.size.height as i32 - 1) as u16;
        self.set_window(x0, y0, x1, y1);
        let count = isect.size.width * isect.size.height;
        self.write_solid(color, count);
        Ok(())
    }

    fn clear(&mut self, color: Self::Color) -> Result<(), Self::Error> {
        self.fill_solid(&self.bounding_box(), color)
    }
}
