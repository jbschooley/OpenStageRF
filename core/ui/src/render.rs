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
    primitives::{PrimitiveStyle, Rectangle},
    text::{Baseline, Text, TextStyleBuilder},
};
use heapless::{String, Vec as HVec};

use crate::{Widget, WidgetList, MAX_WIDGETS};

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
    pub fn render<D>(&mut self, widgets: &WidgetList, display: &mut D) -> Result<(), D::Error>
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
            // correct.
            let prev_at_row = self.prev.iter().find(|w| widget_row(w) == row);
            if let Some(prev_w) = prev_at_row {
                if prev_w == widget {
                    continue;
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
pub fn render<D>(widgets: &WidgetList, display: &mut D) -> Result<(), D::Error>
where
    D: DrawTarget,
    D::Color: From<BinaryColor>,
{
    Renderer::new().render(widgets, display)
}

/// Map a [`Widget`] to the row index it occupies, using
/// [`FOOTER_ROW`] as a sentinel for footers (which are placed at
/// the bottom of the panel rather than at a numbered row).
fn widget_row(w: &Widget) -> u8 {
    match w {
        Widget::Title(_) => 0,
        Widget::Text { row, .. } => *row,
        Widget::Selector { row, .. } => *row,
        Widget::Footer(_) => FOOTER_ROW,
        Widget::LinkStatus { row, .. } => *row,
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
