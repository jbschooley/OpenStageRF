// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

//! Panic / shutdown-record ring buffer.  Board-agnostic.
//!
//! Wraps [`sequential_storage::queue`] with a fixed on-flash record
//! format and a tiny API for the three things a profile needs to do
//! with the ring:
//!
//! 1. [`push`] a record after the boot path recovers a staged panic
//!    or detects a non-panic abnormal reset (WDT, low battery, etc).
//! 2. [`read_latest`] the most recent message for the About screen.
//! 3. [`clear`] the whole ring on operator request (e.g. long-press
//!    Right on About to acknowledge "I've seen this panic, stop
//!    showing it").
//!
//! Record format on flash:
//!
//! ```text
//! [reset_reas: u32 LE][message: UTF-8 bytes (≤ PANIC_MSG_LEN)]
//! ```
//!
//! The header `reset_reas` mirrors the nRF52840 `POWER->RESETREAS`
//! register layout (`DOG`/`SREQ`/`PIN`/`LOCKUP` bits), but the
//! consumer treats it as an opaque u32 — it's a forensic hint, not
//! a structured field.  `0` is a valid value meaning "non-panic
//! shutdown record" (e.g. low-battery shutdown).
//!
//! Callers supply the flash adapter and the flash range; this crate
//! deliberately doesn't know about board memory layouts.  Pair with
//! e.g. `osrf-board-t114`'s `storage::PANIC_RING_RANGE`.

use core::ops::Range;

use embedded_storage_async::nor_flash::{MultiwriteNorFlash, NorFlash};
use sequential_storage::cache::NoCache;

/// Maximum bytes of panic message stored per record.  Chosen to fit
/// `panicked at src/foo.rs:42:5` plus a small format-args summary
/// — anything longer is truncated at the [`push`] call site.  Kept
/// here (not at the call site) so the per-board cross-reset staging
/// buffer (`PanicStaging` in the board crate) can `pub use` this
/// constant for its in-RAM message buffer too, keeping the two
/// sizes locked.
pub const PANIC_MSG_LEN: usize = 192;

/// Capacity of the [`read_latest`] return string.  Smaller than
/// [`PANIC_MSG_LEN`] because the About screen wraps at ~24 chars ×
/// 4 lines = 96 chars — anything past that is truncated for the
/// UI display but stays in flash.
pub const MAX_LATEST_DISPLAY_LEN: usize = 96;

/// Record buffer size (header + max message).  Reused by callers as
/// the size of the stack buffer they pass to [`read_latest`] /
/// [`push`].
pub const RECORD_BUF_LEN: usize = 4 + PANIC_MSG_LEN;

/// Push a record to the ring.  Caller passes the reset-reason u32
/// (typically the `POWER->RESETREAS` value when the prior boot
/// crashed, or `0` for non-panic shutdowns) and the message bytes
/// (UTF-8, truncated to [`PANIC_MSG_LEN`]).  When the ring is full,
/// the oldest entry is overwritten — operator never has to clear
/// it for the ring to keep accepting new entries.
///
/// Logs at `warn` level on push failure but does not return an
/// error to the caller — losing a single panic record is bad but
/// not worth surfacing on the UI.  Callers who need failure
/// signalling can switch to the underlying [`sequential_storage`]
/// API directly.
pub async fn push<F>(flash: &mut F, range: Range<u32>, reset_reas: u32, message: &[u8])
where
    F: NorFlash + MultiwriteNorFlash,
{
    let mut cache = NoCache::new();
    let mut record = [0u8; RECORD_BUF_LEN];
    record[..4].copy_from_slice(&reset_reas.to_le_bytes());
    let msg_len = message.len().min(PANIC_MSG_LEN);
    record[4..4 + msg_len].copy_from_slice(&message[..msg_len]);

    if let Err(_e) = sequential_storage::queue::push(
        flash,
        range,
        &mut cache,
        &record[..4 + msg_len],
        true,
    )
    .await
    {
        #[cfg(feature = "defmt")]
        defmt::warn!(
            "panic-log: push failed: {:?}",
            defmt::Debug2Format(&_e)
        );
    }
}

/// Iterate the ring oldest → newest and return the UTF-8 decoding
/// of the *most recent* record's message portion, truncated to
/// [`MAX_LATEST_DISPLAY_LEN`].  Returns an empty `String` when the
/// ring is empty, unreadable, or holds no valid UTF-8 message.
///
/// `O(n)` over the ring contents — fine because the ring is small
/// (~30 records typical) and this only runs once per boot.
pub async fn read_latest<F>(
    flash: &mut F,
    range: Range<u32>,
) -> heapless::String<MAX_LATEST_DISPLAY_LEN>
where
    F: NorFlash,
{
    let mut cache = NoCache::new();
    let mut buf = [0u8; RECORD_BUF_LEN];
    let mut latest: heapless::String<MAX_LATEST_DISPLAY_LEN> = heapless::String::new();

    let mut iter_obj = match sequential_storage::queue::iter(flash, range, &mut cache).await {
        Ok(i) => i,
        Err(_e) => {
            #[cfg(feature = "defmt")]
            defmt::warn!(
                "panic-log: iter failed: {:?}",
                defmt::Debug2Format(&_e)
            );
            return latest;
        }
    };

    loop {
        match iter_obj.next(&mut buf).await {
            Ok(Some(entry)) => {
                let bytes = entry.into_buf();
                if bytes.len() >= 4 {
                    let msg_bytes = &bytes[4..];
                    if let Ok(s) = core::str::from_utf8(msg_bytes) {
                        latest.clear();
                        let take = s.len().min(latest.capacity());
                        let _ = latest.push_str(&s[..take]);
                    }
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    latest
}

/// Erase every flash page in `range`.  Returns the ring to "no
/// records" state so the About screen's "Last panic" line falls
/// back to its no-prior-panic placeholder.  Intended for operator-
/// driven clears — long-press Right on About in the t114-ui
/// profile.
///
/// Uses raw [`NorFlash::erase`] rather than a queue helper because
/// `sequential-storage` 5.x doesn't expose an "erase the whole
/// ring" operation directly.  Erasing the flash range puts it in
/// the same "all 0xFF" state as a fresh boot, which the queue
/// reader treats as empty.
pub async fn clear<F>(flash: &mut F, range: Range<u32>) -> Result<(), F::Error>
where
    F: NorFlash,
{
    flash.erase(range.start, range.end).await
}
