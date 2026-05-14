// SPDX-License-Identifier: AGPL-3.0-or-later

//! 32-bit per-unit identifier sourced from the nRF52840's Factory
//! Information Configuration Registers (FICR).
//!
//! FICR holds factory-burned values including a 64-bit unique chip
//! ID at `0x1000_0060` (low half) and `0x1000_0064` (high half).
//! We use the low half cast to `u32` as the link layer's
//! `device_id`: enters the AEAD nonce alongside `boot_counter`
//! and `packet_seq` so two units with the same shared key can
//! never produce a colliding nonce sequence.
//!
//! ## Why 32 bits is enough
//!
//! The FICR.DEVICEID full 64-bit value is statistically unique per
//! chip; the low 32 bits inherit that uniqueness for the
//! AEAD-nonce purpose (no attacker advantage from knowing the
//! device_id — it's not a secret).  Truncating saves us 4 bytes
//! per nonce, which matters because the nonce layout is fixed at
//! 13 bytes by AES-CCM and we'd otherwise have to shrink
//! `boot_counter` or `session_seq` to fit.

/// FICR.DEVICEID[0] address.  Reading this register from non-
/// secure code is permitted on the nRF52840 (FICR is read-only
/// and visible to the CPU at all times).
const FICR_DEVICEID_LO: *const u32 = 0x1000_0060 as *const u32;

/// Read the low 32 bits of `FICR.DEVICEID` — a stable, factory-
/// burned, per-unit identifier.  See module docs for why we use
/// it.
///
/// Returns `0` only if the FICR is unreadable (shouldn't happen
/// in practice; would indicate a fundamentally broken chip).
/// Callers that hard-depend on a non-zero ID should sanity check.
pub fn device_id() -> u32 {
    // SAFETY: FICR is a read-only, always-mapped peripheral
    // register block on the nRF52840.  No alignment / aliasing
    // concerns since we only ever read u32 values.
    unsafe { core::ptr::read_volatile(FICR_DEVICEID_LO) }
}
