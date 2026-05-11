// SPDX-License-Identifier: AGPL-3.0-or-later

//! Cross-reset panic staging for the T114.
//!
//! Approach: a panic handler can't safely talk to the SoftDevice
//! (which owns the flash controller) — by the time we're panicking,
//! the SD's invariants may already be broken, and even healthy SD
//! flash writes take ~tens of ms while the rest of the system is
//! supposed to be halted.  Instead the panic handler writes a small
//! record into a "staging" buffer in *uninitialised* RAM, then
//! triggers a software reset.  The next boot's `main` reads the
//! staged record while the SD is dormant, copies it to the panic-ring
//! flash region via the normal `sequential-storage` path, and clears
//! the staging buffer so the *next-next* boot doesn't re-report it.
//!
//! The staging buffer lives in the `.uninit` section, which
//! `cortex-m-rt`'s startup code explicitly does *not* zero — that's
//! what makes the data survive the soft reset.  A magic value in the
//! first u32 distinguishes "valid panic record from prior boot" from
//! "cold-boot RAM noise."

use core::mem::MaybeUninit;

/// Magic value tagging a populated [`PanicStaging`].  Chosen to be
/// distinctive in a memory-dump (`50 41 4E 49` = ASCII "PANI") and
/// unlikely to occur as coincidental power-on RAM contents.
pub const PANIC_MAGIC: u32 = 0x5041_4E49; // "PANI"

/// Capacity of the staged panic message (bytes).  Format is plain
/// UTF-8 text from `core::panic::PanicInfo`'s Display impl —
/// typically `"src/foo.rs:42:5: panicked at 'unreachable'"` plus
/// any user-supplied format args, truncated to fit.
pub const PANIC_MSG_LEN: usize = 192;

/// Cross-reset panic record.  Layout pinned with `repr(C)` so the
/// in-memory representation is stable across firmware revisions
/// — useful when a panic-staged-by-old-firmware boot lands on
/// new-firmware code (unlikely but possible during dev / OTA).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PanicStaging {
    /// [`PANIC_MAGIC`] when populated.  Anything else means
    /// "no valid record" (cold boot, RAM noise, or already cleared).
    pub magic: u32,
    /// Number of valid bytes in [`Self::message`].  Bounded by
    /// [`PANIC_MSG_LEN`].
    pub message_len: u32,
    /// UTF-8 panic text.  Not null-terminated; consume with
    /// `&message[..message_len as usize]`.
    pub message: [u8; PANIC_MSG_LEN],
}

/// Singleton staging buffer.  Lives in `.uninit` so the startup
/// code doesn't zero it on the post-panic reset.  The panic
/// handler writes through `addr_of_mut!(PANIC_PENDING)` directly
/// (no `assume_init` — we treat the bytes as `MaybeUninit` from
/// the panic side too, only fully initialising on a confirmed
/// staging operation).
#[link_section = ".uninit.PANIC_PENDING"]
pub static mut PANIC_PENDING: MaybeUninit<PanicStaging> = MaybeUninit::uninit();

/// Reset-reason bits relevant to our diagnostics.  Maps to the
/// nRF52840's `POWER->RESETREAS` register layout (datasheet §5.2.5).
pub mod reset_reason {
    /// Reset from the NRESET pin (debugger reset, user button on
    /// boards that wire one).
    pub const RESETPIN: u32 = 1 << 0;
    /// Watchdog timeout reset.
    pub const DOG: u32 = 1 << 1;
    /// Software-requested reset (`SCB::sys_reset()` — our panic
    /// handler triggers this).
    pub const SREQ: u32 = 1 << 2;
    /// CPU lockup reset.  Catastrophic — usually means a fault
    /// handler itself faulted.
    pub const LOCKUP: u32 = 1 << 3;
}

/// Read `POWER->RESETREAS` (does **not** clear).
///
/// SD-safe.  SoftDevice restricts *writes* to peripheral 0 (POWER)
/// but reads of the status register are unrestricted — same trick
/// we use in `battery::vbus_present()`.  Clearing the register
/// from app code requires going through SD's `sd_power_reset_reason_clr`
/// SVC, which we don't wire up because flags accumulating across
/// boots is fine for our purposes: we use the cross-reset panic
/// magic as the primary "was this boot a panic recovery?" signal,
/// and treat `RESETREAS` as informational.
///
/// # Safety
///
/// Read-only access to a memory-mapped peripheral register; no
/// aliasing concerns.  SD permits reads of POWER registers.
pub unsafe fn read_reset_reason() -> u32 {
    const RESETREAS: *const u32 = 0x4000_0400 as *const u32;
    core::ptr::read_volatile(RESETREAS)
}

/// Recover any pending panic record from the prior boot.  Returns
/// `None` for cold boots / non-panic resets.  Always clears the
/// magic on the way out so the same record isn't re-reported on
/// subsequent boots.
///
/// # Safety
///
/// Must be called exactly once per boot (the clear-on-take
/// semantics mean a second caller in the same boot sees `None`).
pub unsafe fn take_panic_record() -> Option<PanicStaging> {
    let pending_ptr = core::ptr::addr_of_mut!(PANIC_PENDING) as *mut PanicStaging;
    let magic = core::ptr::read_volatile(core::ptr::addr_of!((*pending_ptr).magic));
    if magic != PANIC_MAGIC {
        return None;
    }
    let record = core::ptr::read(pending_ptr);
    // Clear the magic.  We could also zero the whole buffer but
    // that's wasted cycles — magic alone gates the recovery.
    core::ptr::write_volatile(
        core::ptr::addr_of_mut!((*pending_ptr).magic),
        0,
    );
    Some(record)
}
