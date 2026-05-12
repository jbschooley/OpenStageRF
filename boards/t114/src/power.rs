// SPDX-License-Identifier: AGPL-3.0-or-later

//! Deep soft-off entry path for the T114.
//!
//! Callers (today: `profiles/t114_ui::ui_state_loop`) tear down their
//! own user-visible peripherals — display SLPIN + VTFT-gate, SX1262
//! to SLEEP, backlight off, link runtime parked — then call
//! [`enter_system_off`] to drop the chip into nRF52's System OFF mode
//! with the joystick Center pin armed as the wake source.
//!
//! Why a board-crate helper rather than inline code in the profile:
//! the GPIO PIN_CNF register layout, the joystick pin mapping, and
//! the SD-mediated SVC into System OFF are all platform-specific.
//! Keeping them here means a future board port (the Stage 2 receiver
//! board, eventually) reimplements one module instead of editing
//! every profile.
//!
//! ## Wake source
//!
//! Configures SENSE = "Sense Low" on **P0_13** (Center joystick) so
//! a press during System OFF generates a `DETECT` signal that wakes
//! the chip via `POWER`'s wake logic.  Wake = full reset; the OFF
//! bit in `RESETREAS` distinguishes this from any other reset
//! cause.  The four non-center joystick pins (Up/Down/Left/Right)
//! have their SENSE bits **cleared** so a stray bag-press on a
//! direction doesn't power the unit back on by accident.
//!
//! ## Why disable GPIOTE first
//!
//! embassy-nrf's GPIOTE PORT interrupt handler clears the per-pin
//! SENSE bit when its DETECT event fires (so the same edge isn't
//! re-asserted forever).  If a stray transition happens after we
//! prime PIN_CNF[13] but before we hit the SVC, the handler would
//! wipe our wake-source setup.  Masking GPIOTE in NVIC up front
//! means even if DETECT fires it can't run user code — the chip
//! is about to System OFF anyway.

/// Pin assignments echoed here so the address arithmetic below is
/// self-contained.  Mirrors `board::joystick::*` and `board::vext_power::Pin`;
/// kept inline so any future board port has one obvious place to
/// adjust the platform-specific bits.
const JOYSTICK_UP_PORT: u8 = 1;
const JOYSTICK_UP_PIN: u8 = 14;
const JOYSTICK_RIGHT_PORT: u8 = 1;
const JOYSTICK_RIGHT_PIN: u8 = 12;
const JOYSTICK_LEFT_PORT: u8 = 0;
const JOYSTICK_LEFT_PIN: u8 = 7;
const JOYSTICK_DOWN_PORT: u8 = 0;
const JOYSTICK_DOWN_PIN: u8 = 8;
const JOYSTICK_CENTER_PORT: u8 = 0;
const JOYSTICK_CENTER_PIN: u8 = 13;
const VEXT_PORT: u8 = 0;
const VEXT_PIN: u8 = 21;

// GPIO peripheral register-block bases.  P0 lives at 0x5000_0000,
// P1 (introduced on nRF52840) at 0x5000_0300.  PIN_CNF[n] is at
// offset 0x700 + 4*n inside each block; OUTCLR is at 0x50C.
const GPIO_P0_BASE: usize = 0x5000_0000;
const GPIO_P1_BASE: usize = 0x5000_0300;
const PIN_CNF_OFFSET: usize = 0x700;
const OUTCLR_OFFSET: usize = 0x50C;

const SENSE_MASK: u32 = 0b11 << 16;
const SENSE_LOW: u32 = 0b11 << 16;

/// PIN_CNF[n] address for the given port + pin.  Port must be 0 or 1.
#[inline(always)]
const fn pin_cnf_addr(port: u8, pin: u8) -> *mut u32 {
    let base = if port == 0 { GPIO_P0_BASE } else { GPIO_P1_BASE };
    (base + PIN_CNF_OFFSET + 4 * (pin as usize)) as *mut u32
}

/// OUTCLR address for the given GPIO port.
#[inline(always)]
const fn outclr_addr(port: u8) -> *mut u32 {
    let base = if port == 0 { GPIO_P0_BASE } else { GPIO_P1_BASE };
    (base + OUTCLR_OFFSET) as *mut u32
}

/// Tear down power-relevant pins and drop the chip into System OFF.
///
/// Sequence:
///   1. Mask the GPIOTE interrupt so a stray edge can't clobber the
///      SENSE config we're about to write (see module docs).
///   2. Drive VEXT (P0_21) LOW — peripheral / GPS 3.3 V rail off so
///      anything that survives the SD's own teardown stops drawing.
///   3. Configure SENSE = Disabled on the four non-center joystick
///      pins.  These are currently configured by the joystick task's
///      `wait_for_falling_edge` futures with SENSE = Low; clearing
///      it here ensures only Center can wake.
///   4. Configure SENSE = Low on P0_13 (Center).  Idempotent with the
///      joystick task's setup, but stated explicitly here so this
///      function is correct regardless of which future the joystick
///      task happens to be awaiting at the moment of soft-off.
///   5. Call `sd_power_system_off` SVC.  SD properly winds down its
///      RTC0 / TIMER0 / clock requests before the chip halts.
///
/// Never returns.  If the SVC fails (SD not enabled, or SD already
/// torn down), falls through to a WFE-spin — the WDT will reset us.
///
/// **Caller contract:** SD must still be enabled, and the joystick
/// pins must have been configured at boot (as they are by
/// `board::resources()` + the joystick task).  No active SPI / UART
/// transfers should be in flight (the SVC tolerates this in
/// principle, but it's cleaner to land the radio in SLEEP and quiesce
/// the MIDI UART before getting here).
pub fn enter_system_off() -> ! {
    use embassy_nrf::interrupt::{self, InterruptExt};

    // 1) Mask GPIOTE so PORT interrupts can't run.
    interrupt::GPIOTE.disable();

    // 2) VEXT low.  P0_21 was raised + leaked at boot; drive it low
    //    here for clean teardown.
    //
    // SAFETY: OUTCLR is write-1-to-clear; writing 0 in the unused
    // bits has no effect.  P0_21 was configured as Output by
    // `build_resources`, so writing OUT does what we expect.
    unsafe {
        core::ptr::write_volatile(outclr_addr(VEXT_PORT), 1u32 << VEXT_PIN);
    }

    // 3) Clear SENSE on the four non-center joystick pins.
    //
    // SAFETY: read-modify-write on PIN_CNF for pins the joystick task
    // has configured as Input with Pull::Up.  GPIOTE is masked above
    // so no other code can race us.
    unsafe {
        for (port, pin) in [
            (JOYSTICK_UP_PORT, JOYSTICK_UP_PIN),
            (JOYSTICK_RIGHT_PORT, JOYSTICK_RIGHT_PIN),
            (JOYSTICK_LEFT_PORT, JOYSTICK_LEFT_PIN),
            (JOYSTICK_DOWN_PORT, JOYSTICK_DOWN_PIN),
        ] {
            let p = pin_cnf_addr(port, pin);
            let v = core::ptr::read_volatile(p);
            core::ptr::write_volatile(p, v & !SENSE_MASK);
        }

        // 4) SENSE = Low on Center.
        let p = pin_cnf_addr(JOYSTICK_CENTER_PORT, JOYSTICK_CENTER_PIN);
        let v = core::ptr::read_volatile(p);
        core::ptr::write_volatile(p, (v & !SENSE_MASK) | SENSE_LOW);
    }

    // 5) Enter System OFF.  Documented as non-returning **in
    //    production**.  With a debugger attached (DBGEN bit set in
    //    DHCSR), SD refuses real System OFF — it would disconnect
    //    SWD mid-debug — and returns `NRF_ERROR_SOC_POWER_OFF_
    //    SHOULD_NOT_RETURN` (0x2006 / 8198) instead, leaving the
    //    chip in "emulated System OFF": CPU halts in WFE but the
    //    clocks + peripherals keep running.  Current draw stays in
    //    the mA range, not the < 1 µA we'd hit on real System OFF.
    //
    //    Dev workflow: detach probe-rs, power-cycle, then retest —
    //    SD's check is the bus `DBGEN` bit, not whether the probe
    //    is currently driving SWD pins.  The bit clears only on
    //    `NRESET` / power-cycle.
    //
    //    Behaviour on emulated-off return: warn + soft-reset.  We
    //    can't usefully WFE — GPIOTE is masked above so a Center
    //    press generates no event the CPU would wake on — and an
    //    infinite WFE looks like a hard hang from the user's POV.
    //    Soft-reset means dev sessions see "confirm power-off →
    //    unit reboots back to Idle," which at least exercises the
    //    full teardown + settings-persist + boot path without
    //    requiring the cable dance for every iteration.
    //
    // SAFETY: SVC into the SoftDevice with no arguments.  Caller
    // contract guarantees SD is enabled.
    let ret = unsafe { nrf_softdevice::raw::sd_power_system_off() };
    #[cfg(feature = "defmt")]
    defmt::warn!(
        "sd_power_system_off returned {=u32:#x} — likely debugger-attached \
         emulated mode.  Soft-resetting; real System OFF requires probe \
         detached + power-cycle.",
        ret,
    );
    // Discard the return value in non-defmt builds.
    let _ = ret;

    // Soft-reset.  Boots back through `run()` → Idle.  Settings
    // come back via M7 persistence; the boot log will note `sreq`
    // in `RESETREAS`.
    cortex_m::peripheral::SCB::sys_reset();
}