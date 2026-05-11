// SPDX-License-Identifier: AGPL-3.0-or-later

//! Flash-persistent storage regions for the T114.
//!
//! Three logical regions live at the top of the app FLASH window
//! (see `boards/t114/memory.x`):
//!
//! | Region      | Address range       | Size   | Pages | Purpose                                |
//! |-------------|---------------------|--------|-------|----------------------------------------|
//! | Settings    | `0xE7000 - 0xE9000` |  8 KB  |   2   | Channel / power / band-plan / UI prefs |
//! | Key store   | `0xE9000 - 0xEB000` |  8 KB  |   2   | AEAD key entries (Stage 3)             |
//! | Panic ring  | `0xEB000 - 0xED000` |  8 KB  |   2   | Panic + shutdown-reason records        |
//!
//! Each region is sized to `sequential-storage`'s minimum
//! (2 erase pages of 4 KB each on the nRF52840) so wear-leveling has
//! the headroom it needs.  Bumping a region just means re-slicing
//! the high end of FLASH and updating both `memory.x` and the
//! constants below in lock-step.
//!
//! All flash access goes through `nrf_softdevice::Flash`, which
//! routes erase / write commands through SD's flash SVCs.  SD
//! reserves the flash controller when enabled; direct register
//! access deadlocks.

use core::ops::Range;

use nrf_softdevice::{Flash, Softdevice};

/// Settings (channel index, band plan, TX power, active key fp,
/// UI prefs).  Values are written through `sequential-storage::map`
/// keyed by a small enum / integer tag — see `core/persist/` (TBD)
/// for the key schema.
pub const SETTINGS_RANGE: Range<u32> = 0xE7000..0xE9000;

/// Key store entries (16 slots × ~200 B each).  Stubbed for v1
/// (only `key_fp = 0x0000`, used).  Populated in Stage 3 once AEAD
/// lands.
pub const KEY_STORE_RANGE: Range<u32> = 0xE9000..0xEB000;

/// Panic + shutdown ring buffer.  Each record contains
/// `RESETREAS` + a truncated panic location string + a timestamp
/// (when available).  Read at boot to surface "last fault" info on
/// the About screen.
pub const PANIC_RING_RANGE: Range<u32> = 0xEB000..0xED000;

/// Flash page size on the nRF52840.  Matches `Flash::PAGE_SIZE`.
/// Exposed here so callers can size scratch buffers without pulling
/// in the `nrf-softdevice` crate just for the constant.
pub const PAGE_SIZE: usize = 4096;

/// Take the [`nrf_softdevice::Flash`] singleton.  Must be called
/// after [`super::softdevice::enable`] returned and the
/// `Softdevice::run` task is spawned — `Flash::take` panics if SD
/// isn't running yet.  Only one Flash instance exists per program;
/// hand it off to whichever module owns the actual persistence
/// task (typically `core/persist/`).
pub fn flash(sd: &'static Softdevice) -> Flash {
    Flash::take(sd)
}

/// Compile-time sanity check: the three regions stay in sync with
/// the FLASH length carved in `memory.x`.  If anything moves,
/// recompiling will fail loudly.
const _: () = {
    // Settings + KeyStore + Panic ring must fit between FLASH end and
    // 0xED000 (the bootloader's start).
    assert!(SETTINGS_RANGE.start == 0xE7000);
    assert!(SETTINGS_RANGE.end == KEY_STORE_RANGE.start);
    assert!(KEY_STORE_RANGE.end == PANIC_RING_RANGE.start);
    assert!(PANIC_RING_RANGE.end == 0xED000);
    // Each region is at least 2 erase pages — sequential-storage's
    // minimum.
    assert!((SETTINGS_RANGE.end - SETTINGS_RANGE.start) as usize >= 2 * PAGE_SIZE);
    assert!((KEY_STORE_RANGE.end - KEY_STORE_RANGE.start) as usize >= 2 * PAGE_SIZE);
    assert!((PANIC_RING_RANGE.end - PANIC_RING_RANGE.start) as usize >= 2 * PAGE_SIZE);
    // Each region is page-aligned (erase boundary).
    assert!(SETTINGS_RANGE.start as usize % PAGE_SIZE == 0);
    assert!(KEY_STORE_RANGE.start as usize % PAGE_SIZE == 0);
    assert!(PANIC_RING_RANGE.start as usize % PAGE_SIZE == 0);
};
