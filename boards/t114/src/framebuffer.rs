// SPDX-License-Identifier: AGPL-3.0-or-later

//! Re-export of [`osrf_driver_display_st7789::framebuffer`] under
//! the historical `board::framebuffer` path so existing call sites
//! don't break when the driver moved out to its own crate.

pub use osrf_driver_display_st7789::framebuffer::{DirtyBox, Framebuffer, FB_H, FB_W};
