// SPDX-License-Identifier: AGPL-3.0-or-later

//! Display-agnostic renderer: turns a [`crate::WidgetList`] into
//! pixel writes on any [`embedded_graphics_core::draw_target::DrawTarget`]
//! whose colour space accepts [`BinaryColor`] via `From`.
//!
//! Layout uses [`embedded_graphics::mono_font::ascii::FONT_9X18`]
//! (9 px wide × 18 px tall glyphs) with a 19 px row pitch (18 + 1
//! gap):
//!
//!   - Colour 240×135: 26 cols × 7 rows visible (title + 5 body + footer)
//!
//! `WidgetList` row indices are interpreted as 0-based row numbers
//! into this grid.  Widgets with row indices larger than the panel
//! can hold simply don't render — useful for the same widget tree
//! being shared between two panel sizes.
//!
//! ## Stateful incremental rendering
//!
//! [`render`] is a wrapper that uses a fresh [`Renderer`] each
//! call — convenient for one-shot smoke tests but causes a
//! full-screen clear flash on every call.  Production code should
//! hold a single [`Renderer`] for the lifetime of the program and
//! call [`Renderer::render`]: the renderer tracks which rows had
//! widgets in the previous frame and only repaints rows whose
//! content **could** have changed (everything that's in the new
//! frame, plus stale rows that need clearing).  Result: no
//! visible flash on screen transitions — pixels only flip in
//! regions whose contents change.

use core::fmt::Write as _;
use embedded_graphics::{
    mono_font::{ascii::FONT_9X18, MonoFont, MonoTextStyle, MonoTextStyleBuilder},
    pixelcolor::BinaryColor,
    prelude::*,
    primitives::{Line, PrimitiveStyle, Rectangle},
    text::{Baseline, Text, TextStyleBuilder},
};
use heapless::{String, Vec as HVec};

use crate::{ScanState, Widget, WidgetList, MAX_WIDGETS, SCAN_NO_DATA};

/// Glyph height + 1 px inter-row gap.
const ROW_HEIGHT_PX: u32 = 19;
/// Left margin before any text (leaves room for a `>` cursor mark).
const X_MARGIN_PX: u32 = 2;
/// Right margin so values don't run into the panel edge.
const RIGHT_MARGIN_PX: u32 = 2;
/// Glyph width of [`FONT_9X18`] — used to position right-aligned
/// values.
const GLYPH_WIDTH_PX: u32 = 9;

/// Shared font for all widgets.  `FONT_9X18` (9 px wide × 18 px
/// tall) gives 26 cols × 7 rows visible on a 240×135 colour TFT —
/// readable at arm's length.
const FONT: &MonoFont = &FONT_9X18;

/// Sentinel row index meaning "this widget is the footer, anchored
/// to the bottom of the panel rather than at a numbered row."  Row
/// numbers are u8 so this fits.
const FOOTER_ROW: u8 = 254;

/// Sentinel row index meaning "this widget paints the entire
/// panel."  Used by [`Widget::ScanGraph`].  When this row appears
/// in the previous frame but not the current one, the renderer
/// clears the whole panel so leftover scan pixels don't bleed
/// into the next screen.
const FULLSCREEN_ROW: u8 = 253;

/// Range of dBm shown by the scan-graph bars.  Anything at or
/// below `SCAN_DBM_MIN` is a 0-px bar; anything at or above
/// `SCAN_DBM_MAX` is a full-height bar; in between scales
/// linearly.  Tuned for 902–928 MHz ISM where typical noise floor
/// is around -110 dBm and a strong nearby transmitter can hit
/// -30 dBm.
const SCAN_DBM_MIN: i16 = -120;
const SCAN_DBM_MAX: i16 = -30;

/// Stateful renderer.  Caches the previous frame's widget list so
/// the next [`Renderer::render`] call can do a content-level diff:
/// only rows whose widget actually changed (or rows that vanished
/// entirely) get repainted.  Unchanged rows are never touched, so
/// there is **no per-row flicker** on UI updates that don't affect
/// every row.
///
/// One instance per display.  Construct once at startup, call
/// [`Renderer::render`] on every UI update.
///
/// Memory cost: a copy of the previous [`WidgetList`] (≤
/// `MAX_WIDGETS * sizeof(Widget)` ≈ 600 bytes).
pub struct Renderer {
    /// The full widget list from the previous frame.  Stored so
    /// the next `render` call can compare each new widget to its
    /// previous content at the same row index and skip rows that
    /// are byte-identical.
    prev: WidgetList,
    /// True until the first render completes — on first render we
    /// do a full-screen clear so we don't paint over whatever was
    /// in panel RAM at boot (uninitialised pixel garbage).
    first_frame: bool,
}

impl Default for Renderer {
    fn default() -> Self {
        Self {
            prev: WidgetList::new(),
            first_frame: true,
        }
    }
}

impl Renderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Render `widgets` to `display`.  On first call, performs a
    /// full clear-to-background and paints all widgets.  On
    /// subsequent calls, only repaints rows whose widget content
    /// has changed since the previous frame; unchanged rows are
    /// not touched.
    pub fn render<D>(
        &mut self,
        widgets: &WidgetList,
        scan: &ScanState,
        display: &mut D,
    ) -> Result<(), D::Error>
    where
        D: DrawTarget,
        D::Color: From<BinaryColor>,
    {
        let fg: D::Color = BinaryColor::On.into();
        let bg: D::Color = BinaryColor::Off.into();
        let bbox = display.bounding_box();

        // First render: full clear so panel RAM garbage at boot
        // doesn't bleed through.
        if self.first_frame {
            Rectangle::new(Point::zero(), bbox.size)
                .into_styled(PrimitiveStyle::with_fill(bg))
                .draw(display)?;
            self.first_frame = false;
        }

        // Compute the row set for this frame.
        let mut new_rows: HVec<u8, MAX_WIDGETS> = HVec::new();
        for w in widgets.iter() {
            let _ = new_rows.push(widget_row(w));
        }

        // Clear stale rows: any row that had a widget last frame
        // but doesn't this frame.
        for old_widget in self.prev.iter() {
            let old_row = widget_row(old_widget);
            if !new_rows.contains(&old_row) {
                clear_row(display, old_row, bbox.size, bg)?;
            }
        }

        // Background-aware text style: each glyph cell paints both
        // the foreground glyph pixels AND the surrounding empty
        // pixels with bg colour.  This means a single
        // `Text::draw` call fully overwrites whatever was there
        // before, no separate clear pass needed → no flicker
        // between clear and redraw.
        let bgfg_style = MonoTextStyleBuilder::new()
            .font(FONT)
            .text_color(fg)
            .background_color(bg)
            .build();
        // Inverted style for the title (text in bg colour over fg
        // background — no surrounding bg fill, the fg-filled rect
        // handles that).
        let inverted_style = MonoTextStyle::new(FONT, bg);
        let baseline_top = TextStyleBuilder::new().baseline(Baseline::Top).build();

        // Number of glyph cells per row at the panel's current width.
        let panel_cols = (bbox.size.width / GLYPH_WIDTH_PX) as usize;
        // Where the formatted line starts in pixels (small left
        // margin so the cursor mark isn't flush against the panel
        // edge).
        let line_x = X_MARGIN_PX as i32;
        // Effective columns available after the left margin —
        // these are the cells whose pixels we actually overwrite.
        let line_cols = panel_cols.saturating_sub(
            (X_MARGIN_PX as usize + RIGHT_MARGIN_PX as usize) / GLYPH_WIDTH_PX as usize,
        );

        for widget in widgets.iter() {
            let row = widget_row(widget);

            // Content-level diff: if the previous frame had a
            // widget at this same row that's byte-identical to
            // the new one, skip — the panel pixels are already
            // correct.  ScanGraph is exempt because its data
            // (the per-channel arrays in `scan`) lives outside
            // the widget; we always repaint when it's present.
            if !matches!(widget, Widget::ScanGraph { .. }) {
                let prev_at_row = self.prev.iter().find(|w| widget_row(w) == row);
                if let Some(prev_w) = prev_at_row {
                    if prev_w == widget {
                        continue;
                    }
                }
            }

            match widget {
                Widget::Title(text) => {
                    // Inverted bar: fill row with foreground, draw
                    // text in background colour.  The fill takes
                    // care of replacing whatever was there.
                    let title_h = ROW_HEIGHT_PX;
                    Rectangle::new(Point::zero(), Size::new(bbox.size.width, title_h))
                        .into_styled(PrimitiveStyle::with_fill(fg))
                        .draw(display)?;
                    Text::with_text_style(
                        text,
                        Point::new(line_x, 1),
                        inverted_style,
                        baseline_top,
                    )
                    .draw(display)?;
                }

                Widget::BatteryIndicator {
                    voltage_mv,
                    percent,
                    plugged_in,
                } => {
                    // Format as "  87% 4.05V" — fixed 11 chars so
                    // width-change cases (99→100, etc.) don't
                    // leave trailing pixels.  "—" placeholder when
                    // no reading yet.
                    let mut text_buf: String<12> = String::new();
                    if *voltage_mv == 0 {
                        // No battery present (or pre-first-reading).
                        // `BatteryStatus::from_reading` zeroes
                        // `voltage_mv` whenever the raw reading
                        // falls below the active chemistry's no-
                        // battery floor — so the renderer doesn't
                        // need to know which chemistry the profile
                        // is using.  Show a placeholder.
                        let _ = write!(&mut text_buf, " --% ----V");
                    } else {
                        // Voltage rendered as X.XXV (millivolt-rounded
                        // to the hundredth).  Percent right-padded
                        // to 3 chars so we don't shrink when going
                        // from "100%" down to "99%".
                        let v_int = (*voltage_mv) / 1000;
                        let v_frac = ((*voltage_mv) % 1000) / 10;
                        let _ = write!(
                            &mut text_buf,
                            "{:>3}% {}.{:02}V",
                            percent, v_int, v_frac
                        );
                    }

                    // Compute pixel x where the text starts.  Right-
                    // align the block (text + optional bolt) to the
                    // panel's right edge with `RIGHT_MARGIN_PX`
                    // breathing room.
                    let bolt_w: u32 = if *plugged_in { 7 } else { 0 };
                    let bolt_gap: u32 = if *plugged_in { 3 } else { 0 };
                    let text_w = text_buf.len() as u32 * GLYPH_WIDTH_PX;
                    let total_w = text_w + bolt_gap + bolt_w + RIGHT_MARGIN_PX;
                    let x_text = (bbox.size.width as i32) - (total_w as i32);

                    // Clear our slot to fg (matching the title bar
                    // background) so a width shrink doesn't leave
                    // stale glyphs hanging behind.
                    Rectangle::new(
                        Point::new(x_text - 2, 0),
                        Size::new(total_w + 2, ROW_HEIGHT_PX),
                    )
                    .into_styled(PrimitiveStyle::with_fill(fg))
                    .draw(display)?;

                    Text::with_text_style(
                        &text_buf,
                        Point::new(x_text, 1),
                        inverted_style,
                        baseline_top,
                    )
                    .draw(display)?;

                    if *plugged_in {
                        // Tiny 3-segment lightning bolt centred to
                        // the right of the text.  FONT_9X18 doesn't
                        // have a bolt glyph and iso_8859_1 doesn't
                        // either, so we hand-draw one from line
                        // segments.  Origin is the bolt's top-left
                        // corner; the bolt occupies ~7×13 px.
                        let bx = x_text + text_w as i32 + bolt_gap as i32;
                        let by: i32 = 3;
                        let stroke = PrimitiveStyle::with_stroke(bg, 1);
                        // Top diagonal: top-right down to mid-left.
                        Line::new(
                            Point::new(bx + 4, by),
                            Point::new(bx + 1, by + 6),
                        )
                        .into_styled(stroke)
                        .draw(display)?;
                        // Horizontal crossbar at the kink.
                        Line::new(
                            Point::new(bx + 1, by + 6),
                            Point::new(bx + 5, by + 6),
                        )
                        .into_styled(stroke)
                        .draw(display)?;
                        // Bottom diagonal: from crossbar's right down
                        // to lower-left tip.
                        Line::new(
                            Point::new(bx + 5, by + 6),
                            Point::new(bx + 2, by + 12),
                        )
                        .into_styled(stroke)
                        .draw(display)?;
                    }
                }

                Widget::Text { row, text } => {
                    let y = row_to_y(*row);
                    if y >= bbox.size.height as i32 {
                        continue;
                    }
                    let line = pad_right_to(text, line_cols);
                    Text::with_text_style(
                        &line,
                        Point::new(line_x, y),
                        bgfg_style,
                        baseline_top,
                    )
                    .draw(display)?;
                }

                Widget::Selector {
                    row,
                    label,
                    value,
                    selected,
                    active,
                    editing,
                } => {
                    let y = row_to_y(*row);
                    if y >= bbox.size.height as i32 {
                        continue;
                    }
                    let active_char = if *active { '*' } else { ' ' };
                    let cursor_char = if *selected { '>' } else { ' ' };
                    // Build display value with edit-mode brackets.
                    let mut value_str: String<16> = String::new();
                    if *editing {
                        let _ = write!(&mut value_str, "[{}]", value);
                    } else {
                        let _ = value_str.push_str(value);
                    }
                    let line = build_selector_line(
                        active_char,
                        cursor_char,
                        label,
                        &value_str,
                        line_cols,
                    );
                    Text::with_text_style(
                        &line,
                        Point::new(line_x, y),
                        bgfg_style,
                        baseline_top,
                    )
                    .draw(display)?;
                }

                Widget::Footer(text) => {
                    let y = (bbox.size.height as i32) - (ROW_HEIGHT_PX as i32);
                    if y < 0 {
                        continue;
                    }
                    let line = pad_right_to(text, line_cols);
                    Text::with_text_style(
                        &line,
                        Point::new(line_x, y),
                        bgfg_style,
                        baseline_top,
                    )
                    .draw(display)?;
                }

                Widget::LinkStatus { row, up: _, text } => {
                    let y = row_to_y(*row);
                    if y >= bbox.size.height as i32 {
                        continue;
                    }
                    let line = pad_right_to(text, line_cols);
                    Text::with_text_style(
                        &line,
                        Point::new(line_x, y),
                        bgfg_style,
                        baseline_top,
                    )
                    .draw(display)?;
                }

                Widget::ScanGraph {
                    channel_count,
                    cursor,
                    active,
                    title,
                } => {
                    draw_scan_graph(
                        display,
                        bbox.size,
                        fg,
                        bg,
                        bgfg_style,
                        baseline_top,
                        line_cols,
                        line_x,
                        &scan.current_dbm,
                        &scan.peak_dbm,
                        *channel_count,
                        *cursor,
                        *active,
                        title,
                    )?;
                }
            }
        }

        // Save this frame's widget list for next frame's diff.
        self.prev = widgets.clone();
        Ok(())
    }
}

/// One-shot render.  Constructs a fresh [`Renderer`] each call,
/// which means a full-screen clear every time → flash.  Useful
/// for smoke tests; production code should hold a single
/// [`Renderer`] across renders for the no-flash incremental path.
pub fn render<D>(
    widgets: &WidgetList,
    scan: &ScanState,
    display: &mut D,
) -> Result<(), D::Error>
where
    D: DrawTarget,
    D::Color: From<BinaryColor>,
{
    Renderer::new().render(widgets, scan, display)
}

/// Map a [`Widget`] to the row index it occupies, using
/// [`FOOTER_ROW`] as a sentinel for footers (which are placed at
/// the bottom of the panel rather than at a numbered row) and
/// [`FULLSCREEN_ROW`] for widgets that paint the whole panel.
fn widget_row(w: &Widget) -> u8 {
    match w {
        Widget::Title(_) => 0,
        Widget::Text { row, .. } => *row,
        Widget::Selector { row, .. } => *row,
        Widget::Footer(_) => FOOTER_ROW,
        Widget::LinkStatus { row, .. } => *row,
        Widget::ScanGraph { .. } => FULLSCREEN_ROW,
        // Battery indicator paints over the right side of the title
        // row; treated as row 0 so the diff machinery groups it with
        // the title for clear/redraw scoping.
        Widget::BatteryIndicator { .. } => 0,
    }
}

/// Clear one row's worth of pixels to background.  Handles the
/// footer's "anchored to bottom" sentinel.
fn clear_row<D>(
    display: &mut D,
    row: u8,
    panel_size: Size,
    bg: D::Color,
) -> Result<(), D::Error>
where
    D: DrawTarget,
{
    if row == FULLSCREEN_ROW {
        // Whole-panel clear (used when leaving the Scan screen).
        return Rectangle::new(Point::zero(), panel_size)
            .into_styled(PrimitiveStyle::with_fill(bg))
            .draw(display);
    }
    let y = if row == FOOTER_ROW {
        (panel_size.height as i32) - (ROW_HEIGHT_PX as i32)
    } else {
        (row as u32 * ROW_HEIGHT_PX) as i32
    };
    if y < 0 || y >= panel_size.height as i32 {
        return Ok(());
    }
    Rectangle::new(Point::new(0, y), Size::new(panel_size.width, ROW_HEIGHT_PX))
        .into_styled(PrimitiveStyle::with_fill(bg))
        .draw(display)
}

/// Map a widget row index to a pixel y-coordinate.  Title row 0
/// occupies 0..ROW_HEIGHT_PX with inverted text; subsequent rows
/// (1, 2, …) are at `row * ROW_HEIGHT_PX`.
fn row_to_y(row: u8) -> i32 {
    (row as u32 * ROW_HEIGHT_PX) as i32
}

/// Build a fixed-width line by left-aligning `text` and padding
/// the right with spaces to fill `cols` characters.  Truncates if
/// `text` is too long.  Output is always exactly `cols` chars
/// (capped at the string's capacity, currently 32).
fn pad_right_to(text: &str, cols: usize) -> String<32> {
    let mut out: String<32> = String::new();
    for c in text.chars().take(cols) {
        let _ = out.push(c);
    }
    while out.len() < cols && out.len() < out.capacity() {
        let _ = out.push(' ');
    }
    out
}

/// Render the entire Scan screen: title row, bars (one per
/// channel) with a peak tick on top, two thin stripes underneath
/// the bars (cursor stripe directly under the bar, active stripe
/// directly below that), and a footer.  Bar width adapts to
/// channel count — 75% of the column for ≥3 px columns (with bg
/// gap), full column for tighter densities (spectrum-trace mode).
#[allow(clippy::too_many_arguments)]
fn draw_scan_graph<D>(
    display: &mut D,
    panel_size: Size,
    fg: D::Color,
    bg: D::Color,
    bgfg_style: MonoTextStyle<'_, D::Color>,
    baseline_top: embedded_graphics::text::TextStyle,
    line_cols: usize,
    line_x: i32,
    current_dbm: &[i16],
    peak_dbm: &[i16],
    channel_count: u8,
    cursor: u8,
    active: u8,
    title: &str,
) -> Result<(), D::Error>
where
    D: DrawTarget,
    D::Color: From<BinaryColor>,
{
    let panel_w = panel_size.width as i32;
    let panel_h = panel_size.height as i32;

    // Layout: title (row 0, 19 px) + bars (variable) + cursor
    // stripe (5 px, directly under bars) + 1 px gap + active
    // stripe (5 px) + footer (19 px).  Total marker zone = 11 px.
    // On 240×135 that leaves 86 px for bars (vs 76 px when the
    // marker zone was a full 19 px row).
    const CURSOR_STRIPE_H: i32 = 5;
    const STRIPE_GAP_H: i32 = 1;
    const ACTIVE_STRIPE_H: i32 = 5;
    const MARKER_ZONE_H: i32 = CURSOR_STRIPE_H + STRIPE_GAP_H + ACTIVE_STRIPE_H;
    let title_h = ROW_HEIGHT_PX as i32;
    let footer_h = ROW_HEIGHT_PX as i32;
    let bars_y0 = title_h;
    let bars_y1 = panel_h - footer_h - MARKER_ZONE_H;
    let bars_h = (bars_y1 - bars_y0).max(0);
    let cursor_stripe_y = bars_y1;
    let active_stripe_y = cursor_stripe_y + CURSOR_STRIPE_H + STRIPE_GAP_H;
    let footer_y = panel_h - footer_h;

    // No full-screen bg clear — that's what causes the panel-wide
    // black flash on each tick.  Instead, every region (title,
    // each column, marker row, footer) is painted so each pixel
    // is set exactly once per frame to its final colour.  The
    // outer renderer's content-level diff already short-circuits
    // unchanged frames; here we just want to redraw the changed
    // ScanGraph without the destructive clear.

    // Title row: paint as a bg-aware text in *inverted* style
    // (text colour = bg, surrounding cell = fg) over a padded
    // string, so one paint covers the full row's pixels with no
    // separate clear pass.  Pad to fill the row so trailing
    // glyph cells from a longer previous title are overwritten.
    let inverted_bg = MonoTextStyleBuilder::new()
        .font(FONT)
        .text_color(bg)
        .background_color(fg)
        .build();
    let title_padded = pad_right_to(title, line_cols);
    Text::with_text_style(
        &title_padded,
        Point::new(line_x, 1),
        inverted_bg,
        baseline_top,
    )
    .draw(display)?;
    // Fill the 2 px left-margin + right-margin slivers the
    // padded text doesn't cover, so the title row is fully fg
    // edge-to-edge.
    Rectangle::new(Point::zero(), Size::new(line_x as u32, title_h as u32))
        .into_styled(PrimitiveStyle::with_fill(fg))
        .draw(display)?;
    let right_x = (line_cols as i32) * (GLYPH_WIDTH_PX as i32) + line_x;
    if right_x < panel_w {
        Rectangle::new(
            Point::new(right_x, 0),
            Size::new((panel_w - right_x) as u32, title_h as u32),
        )
        .into_styled(PrimitiveStyle::with_fill(fg))
        .draw(display)?;
    }

    // Compute per-channel column geometry.  Bars stretch edge to
    // edge; col_w shrinks as N grows.  bar_w adapts:
    //   col_w >= 3: bar_w = 75% of col_w (visible gap between bars)
    //   col_w  < 3: bar_w = col_w (no gap; spectrum-trace mode)
    let n = (channel_count as i32).min(current_dbm.len() as i32);
    if n > 0 && bars_h > 0 {
        let usable_w = panel_w.max(0);
        let col_w = (usable_w / n).max(1);
        let col_left_margin = (usable_w - col_w * n) / 2;
        let (bar_w, bar_x_offset) = if col_w >= 3 {
            let bw = ((col_w * 3) / 4).max(1);
            (bw, (col_w - bw) / 2)
        } else {
            (col_w, 0)
        };

        // Margin slivers (left of first column + right of last)
        // get bg-filled once across the entire bars+marker zone
        // so leftover pixels from a previous screen don't bleed
        // in.  Single rect each, doesn't flicker.
        let combined_h = (bars_h + MARKER_ZONE_H) as u32;
        if col_left_margin > 0 {
            Rectangle::new(
                Point::new(0, bars_y0),
                Size::new(col_left_margin as u32, combined_h),
            )
            .into_styled(PrimitiveStyle::with_fill(bg))
            .draw(display)?;
        }
        let row_right = col_left_margin + col_w * n;
        if row_right < panel_w {
            Rectangle::new(
                Point::new(row_right, bars_y0),
                Size::new((panel_w - row_right) as u32, combined_h),
            )
            .into_styled(PrimitiveStyle::with_fill(bg))
            .draw(display)?;
        }

        for i in 0..(n as usize) {
            let cx = col_left_margin + (i as i32) * col_w;
            let bar_x = cx + bar_x_offset;
            let cur = current_dbm[i];
            let peak = peak_dbm[i];

            // Per-column split paint: every pixel in this column's
            // bars region is touched exactly once with its final
            // colour.  No bg→fg transition for any pixel, so no
            // flash even when bar heights change every tick.
            //
            // Layout within column (top-to-bottom):
            //   [bg above bar]           — col_w wide
            //   [bg | fg bar | bg]       — bar_h tall, bar_w fg in middle
            //
            // After this, the peak tick paints fg over a 2 px
            // horizontal stripe at peak height (overwrites bg
            // already set).  Tick is small so its bg→fg transition
            // is imperceptible.
            let bar_h = if cur == SCAN_NO_DATA {
                0
            } else {
                dbm_to_bar_height(cur, bars_h)
            };
            let above_h = bars_h - bar_h;

            if above_h > 0 {
                Rectangle::new(
                    Point::new(cx, bars_y0),
                    Size::new(col_w as u32, above_h as u32),
                )
                .into_styled(PrimitiveStyle::with_fill(bg))
                .draw(display)?;
            }
            if bar_h > 0 {
                // Left bg sliver inside column (if bar is narrower).
                if bar_x_offset > 0 {
                    Rectangle::new(
                        Point::new(cx, bars_y1 - bar_h),
                        Size::new(bar_x_offset as u32, bar_h as u32),
                    )
                    .into_styled(PrimitiveStyle::with_fill(bg))
                    .draw(display)?;
                }
                // Bar itself.
                Rectangle::new(
                    Point::new(bar_x, bars_y1 - bar_h),
                    Size::new(bar_w as u32, bar_h as u32),
                )
                .into_styled(PrimitiveStyle::with_fill(fg))
                .draw(display)?;
                // Right bg sliver inside column.
                let right_sliver_x = bar_x + bar_w;
                let right_sliver_w = col_w - bar_x_offset - bar_w;
                if right_sliver_w > 0 {
                    Rectangle::new(
                        Point::new(right_sliver_x, bars_y1 - bar_h),
                        Size::new(right_sliver_w as u32, bar_h as u32),
                    )
                    .into_styled(PrimitiveStyle::with_fill(bg))
                    .draw(display)?;
                }
            }

            if peak != SCAN_NO_DATA {
                let peak_h = dbm_to_bar_height(peak, bars_h);
                let peak_y = (bars_y1 - peak_h - 1).max(bars_y0);
                Rectangle::new(
                    Point::new(cx, peak_y),
                    Size::new(col_w as u32, 2),
                )
                .into_styled(PrimitiveStyle::with_fill(fg))
                .draw(display)?;
            }
        }

        // Marker stripes — two thin horizontal stripes spanning
        // the bars row, painted as 3 rects each (left bg, fg
        // stripe at the marked column, right bg) so each pixel
        // is set exactly once.  Cursor stripe directly under the
        // bars; active stripe under that with a 1-px gap.
        let cursor_x = col_left_margin + (cursor as i32) * col_w;
        let active_x = col_left_margin + (active as i32) * col_w;
        paint_marker_stripe(
            display,
            cursor_stripe_y,
            CURSOR_STRIPE_H,
            cursor_x,
            col_w,
            col_left_margin,
            row_right,
            cursor < (n as u8),
            fg,
            bg,
        )?;
        // Gap between stripes — bg full-width so old pixels there
        // are wiped in one shot.
        if STRIPE_GAP_H > 0 {
            Rectangle::new(
                Point::new(col_left_margin, cursor_stripe_y + CURSOR_STRIPE_H),
                Size::new((row_right - col_left_margin) as u32, STRIPE_GAP_H as u32),
            )
            .into_styled(PrimitiveStyle::with_fill(bg))
            .draw(display)?;
        }
        paint_marker_stripe(
            display,
            active_stripe_y,
            ACTIVE_STRIPE_H,
            active_x,
            col_w,
            col_left_margin,
            row_right,
            active < (n as u8),
            fg,
            bg,
        )?;
    }

    // Footer (bg-aware text overpaints, no separate clear).
    let footer = pad_right_to("Dn=back LR=scroll Cen=ok", line_cols);
    Text::with_text_style(
        &footer,
        Point::new(line_x, footer_y),
        bgfg_style,
        baseline_top,
    )
    .draw(display)?;
    Ok(())
}

/// Paint one marker stripe spanning the bars-row width.  The
/// stripe is bg everywhere except the column at `mark_x..mark_x +
/// col_w`, which is fg if `present` is true (otherwise the whole
/// stripe is bg — used when the cursor or active index is out of
/// the current channel range).  Three rect fills total: left bg,
/// fg mark (if present), right bg.  Each pixel is set exactly
/// once per frame.
#[allow(clippy::too_many_arguments)]
fn paint_marker_stripe<D>(
    display: &mut D,
    y: i32,
    h: i32,
    mark_x: i32,
    col_w: i32,
    row_left: i32,
    row_right: i32,
    present: bool,
    fg: D::Color,
    bg: D::Color,
) -> Result<(), D::Error>
where
    D: DrawTarget,
{
    if !present {
        Rectangle::new(
            Point::new(row_left, y),
            Size::new((row_right - row_left).max(0) as u32, h as u32),
        )
        .into_styled(PrimitiveStyle::with_fill(bg))
        .draw(display)?;
        return Ok(());
    }
    if mark_x > row_left {
        Rectangle::new(
            Point::new(row_left, y),
            Size::new((mark_x - row_left) as u32, h as u32),
        )
        .into_styled(PrimitiveStyle::with_fill(bg))
        .draw(display)?;
    }
    Rectangle::new(
        Point::new(mark_x, y),
        Size::new(col_w as u32, h as u32),
    )
    .into_styled(PrimitiveStyle::with_fill(fg))
    .draw(display)?;
    let after = mark_x + col_w;
    if after < row_right {
        Rectangle::new(
            Point::new(after, y),
            Size::new((row_right - after) as u32, h as u32),
        )
        .into_styled(PrimitiveStyle::with_fill(bg))
        .draw(display)?;
    }
    Ok(())
}

/// Map a dBm value to bar height in pixels, clipping to
/// `[SCAN_DBM_MIN, SCAN_DBM_MAX]` and scaling linearly across the
/// available `total_h` pixels.
fn dbm_to_bar_height(dbm: i16, total_h: i32) -> i32 {
    let clamped = dbm.clamp(SCAN_DBM_MIN, SCAN_DBM_MAX);
    let span = (SCAN_DBM_MAX - SCAN_DBM_MIN) as i32; // 90
    let above_min = (clamped - SCAN_DBM_MIN) as i32; // 0..=90
    (above_min * total_h) / span
}

/// Build a Selector row: active marker (1 char) + cursor mark (1
/// char) + label (left-aligned, truncated as needed) + spaces
/// filling the gap + value (right-aligned).  Always exactly
/// `cols` characters wide so the background-aware text style
/// overpaints the entire row in one pass — no leftover pixels
/// from the previous frame.
///
/// Two prefix characters give the user independent visibility
/// into "what's hovered" (cursor `>`) and "what's currently
/// applied" (active `*`).  Both can show simultaneously.
fn build_selector_line(
    active_mark: char,
    cursor: char,
    label: &str,
    value: &str,
    cols: usize,
) -> String<32> {
    let mut out: String<32> = String::new();
    let _ = out.push(active_mark);
    let _ = out.push(cursor);
    let value_len = value.chars().count();
    // Reserve `value_len` chars on the right; everything between
    // the 2-char prefix and the value is label + padding.
    let label_max = cols.saturating_sub(2).saturating_sub(value_len);
    for c in label.chars().take(label_max) {
        let _ = out.push(c);
    }
    // Pad with spaces up to where the value begins.
    let value_start = cols.saturating_sub(value_len);
    while out.len() < value_start && out.len() < out.capacity() {
        let _ = out.push(' ');
    }
    for c in value.chars().take(cols.saturating_sub(out.len())) {
        let _ = out.push(c);
    }
    out
}
