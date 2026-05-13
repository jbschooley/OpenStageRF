// SPDX-License-Identifier: AGPL-3.0-or-later

//! Cross-System-OFF "soft-off was deliberate" flag.
//!
//! Set by [`crate::power::enter_system_off`] just before the SD SVC
//! drops the chip into System OFF.  Read at the next boot via
//! [`take`] to distinguish:
//!
//! - **Cold boot / brownout / panic-reset** — flag absent, no
//!   special handling; profile takes the normal boot path.
//! - **Wake from soft-off** — flag present.  Combined with the
//!   Center-pin level and VBUS status (see
//!   [`crate::power::detect_wake_source`]), the profile decides
//!   whether to fully resume or render a brief charging frame and
//!   re-sleep.
//!
//! ## Storage
//!
//! 32-bit magic in a `.uninit` RAM region.  `.uninit` is excluded
//! from cortex-m-rt's startup zero-init, so the value survives soft
//! resets (`SCB::sys_reset()`) and — crucially — System OFF wake,
//! which on nRF52840 retains all RAM banks by default.  It does
//! *not* survive battery removal or a deep brownout, both of which
//! also clear `RESETREAS.OFF` — so combining the two signals at
//! boot is robust against either being spurious.

use core::mem::MaybeUninit;

/// Magic distinguishing "we set this on purpose" from uninitialised
/// RAM noise.  ASCII "WAKE".
pub const WAKE_MAGIC: u32 = 0x57_41_4B_45;

/// Singleton flag in `.uninit`.  cortex-m-rt's startup will not
/// touch this; we read it pre-SD-enable on every boot.
#[link_section = ".uninit.WAKEFLAG"]
static mut WAKEFLAG: MaybeUninit<u32> = MaybeUninit::uninit();

/// Mark the upcoming System OFF as deliberate.  Call once,
/// immediately before `sd_power_system_off`.
///
/// # Safety
///
/// Called from a single-threaded soft-off entry path; no concurrent
/// access to the static.  Direct volatile write through
/// `addr_of_mut!` into the `MaybeUninit` slot.
pub unsafe fn set() {
    let p = core::ptr::addr_of_mut!(WAKEFLAG) as *mut u32;
    core::ptr::write_volatile(p, WAKE_MAGIC);
}

/// Read + clear the flag.  Returns `true` iff the previous boot
/// deliberately entered System OFF.  Idempotent within a boot — a
/// second caller sees `false` because the first call cleared.
///
/// # Safety
///
/// Must be called exactly once per boot, before anything else
/// might write to `WAKEFLAG`.  Caller is `board::power::detect_wake_source`.
pub unsafe fn take() -> bool {
    let p = core::ptr::addr_of_mut!(WAKEFLAG) as *mut u32;
    let v = core::ptr::read_volatile(p);
    core::ptr::write_volatile(p, 0);
    v == WAKE_MAGIC
}
