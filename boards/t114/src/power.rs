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

/// Why this boot is running, as far as the soft-off machinery can
/// tell.  Returned by [`detect_wake_source`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum WakeSource {
    /// First boot, brownout recovery, panic-reset, or any reset
    /// that wasn't preceded by an [`enter_system_off`] call.  No
    /// special handling — profile boots normally.
    ColdBoot,
    /// Wake from a deliberate soft-off via the configured Center
    /// SENSE source.  User pressed Center to power the unit back
    /// on; profile resumes the full UI.
    CenterPress,
    /// Wake from a deliberate soft-off while USB was being plugged
    /// in.  Profile should render a brief charging frame and then
    /// re-enter soft-off (per the M8 "USB-plug wake" feature).
    UsbPlug,
}

/// Identify what woke this boot.
///
/// **Order matters here.**  Live Center-press detection runs
/// *before* the RAM-side wakeflag check, because a USB-cable ESD
/// event can brown-out the chip and wipe RAM — in which case the
/// wakeflag is gone but the user might still be holding Center.
/// Putting the live-pin poll first means a deliberate Center wake
/// always reaches Idle, regardless of whether the wake mechanism
/// was a clean SENSE event or a brown-out reset.
///
/// Decision tree:
///   1. Configure `PIN_CNF[13]` for Input + Connect + Pull-up so
///      `P0.IN` reflects the actual pin voltage.  (Brown-out resets
///      PIN_CNF to `INPUT=Disconnect`, which reads back 0 for every
///      pin — that's the bug we're correcting.)
///   2. Poll Center for ~100 ms with early-out on LOW.  Any LOW
///      sample → [`WakeSource::CenterPress`].
///   3. No live press observed.  Consume the wakeflag.  Absent →
///      [`WakeSource::ColdBoot`] (truly cold; profile may upgrade
///      via its flash-backed soft-off-intent flag).
///   4. Wakeflag present → deliberate soft-off was active when this
///      reset happened, but Center isn't being pressed — return
///      [`WakeSource::UsbPlug`] for the brief-charging-frame path.
///
/// `wakeflag::take` is destructive and only consulted on the no-
/// live-press branch, so live-press cases leave the flag intact for
/// the next iteration.  That's deliberate: if a Center wake races a
/// brown-out and we return CenterPress via the live read, leaving
/// the wakeflag set means the *next* (hypothetical) reset can still
/// recognise the prior soft-off — at worst it leads to a charging
/// frame on the next event, never a stuck state.
pub fn detect_wake_source() -> WakeSource {
    // Clear LATCH (write-1-to-clear) — diagnostic hygiene only, the
    // decision logic below doesn't read it.  ESD can spuriously
    // latch bits, so don't carry stale state to subsequent boots.
    //
    // SAFETY: GPIO is app-owned; single-threaded boot-time access.
    const P0_LATCH: *mut u32 = (GPIO_P0_BASE + 0x520) as *mut u32;
    unsafe {
        let l = core::ptr::read_volatile(P0_LATCH);
        if l != 0 {
            core::ptr::write_volatile(P0_LATCH, l);
        }
    }

    // Configure PIN_CNF[13] for input + pull-up so live reads work.
    //
    // **Critical for brown-out wakes**: nRF52840 resets PIN_CNF to
    // default on POR (Input + INPUT=Disconnect + no pull + no
    // SENSE).  When `INPUT=Disconnect`, the input buffer is gated
    // off and `P0.IN` reads back 0 regardless of actual pin
    // voltage.  Without this write we'd false-positive "Center
    // pressed" on every brown-out wake.  If PIN_CNF *wasn't* reset
    // (clean SENSE wake retains it), this overwrite is harmless —
    // SENSE goes to Disabled, the joystick task re-arms it later.
    const P0_PIN_CNF_CENTER: *mut u32 =
        (GPIO_P0_BASE + PIN_CNF_OFFSET + 4 * (JOYSTICK_CENTER_PIN as usize)) as *mut u32;
    // DIR=0 (Input), INPUT=0 (Connect), PULL=3 (Pull-up),
    // DRIVE=0, SENSE=0 → 0x0C.
    const PIN_CNF_INPUT_PULLUP: u32 = 0x0000_000C;
    unsafe {
        core::ptr::write_volatile(P0_PIN_CNF_CENTER, PIN_CNF_INPUT_PULLUP);
    }
    // ~5 ms initial settle for the internal ~13 kΩ pull-up to
    // charge the line capacitance.
    //
    // Why so long: an external joystick board on a cable can add
    // hundreds of pF to a nF of line capacitance.  With C=1 nF and
    // R=13 kΩ, RC ≈ 13 µs and ~3*RC ≈ 40 µs is needed to reach a
    // valid logic-HIGH from a discharged start.  Polling too early
    // catches the line still in the LOW-side transition and the
    // 10-consecutive-LOW debounce triggers a spurious `CenterPress`.
    // 5 ms is comfortably past any realistic RC for this setup.
    // Adds nothing to perceived boot latency on the charging-frame
    // path (~1.6 s display init), and the Idle path is fast enough
    // either way.
    //
    // 64 000 cycles/ms at 64 MHz HFINT.
    cortex_m::asm::delay(320_000);

    // Poll Center for up to ~100 ms with a debounce against ESD
    // pulses.
    //
    // **Why debounce**: USB-cable ESD events can briefly couple
    // through PCB traces and pull P0_13 LOW for microseconds.  A
    // naive "any single LOW sample → press" reads those transients
    // as Center presses and false-routes USB plug-in events to a
    // full Idle boot.  A deliberate finger-on-button keeps the
    // line LOW for hundreds of ms — so any honest press easily
    // beats a 10-sample (≈10 ms) stability requirement.
    //
    // Early-exit when we accumulate 10 ms of stable LOW.  Fallback
    // at end-of-window: if more than half the samples were LOW
    // (i.e. press was real but bouncier than expected), also count
    // it as a press.  Worst-case 100 ms cost in the no-press path;
    // invisible against the ~1.6 s of display init that follows a
    // charging-frame branch.
    //
    // 64 000 cycles/ms at 64 MHz HFINT.
    const P0_IN: *const u32 = (GPIO_P0_BASE + 0x510) as *const u32;
    const SAMPLE_DELAY_CYCLES: u32 = 64_000; // 1 ms at 64 MHz
    const MAX_SAMPLES: u32 = 100;
    /// Number of consecutive 1-ms LOW samples required to call it
    /// a press.  10 ms is comfortably past any ESD pulse duration
    /// and well inside the shortest deliberate button-press.
    const STABLE_LOW_SAMPLES: u32 = 10;
    let mut consecutive_low: u32 = 0;
    let mut total_low: u32 = 0;
    for _ in 0..MAX_SAMPLES {
        let in_val = unsafe { core::ptr::read_volatile(P0_IN) };
        let pin_low = (in_val & (1u32 << JOYSTICK_CENTER_PIN)) == 0;
        if pin_low {
            consecutive_low += 1;
            total_low += 1;
            if consecutive_low >= STABLE_LOW_SAMPLES {
                // ≥10 ms of stable LOW.  Confident press.  Don't
                // call `wakeflag::take` here — leaving the flag
                // intact across a CenterPress return is harmless.
                return WakeSource::CenterPress;
            }
        } else {
            consecutive_low = 0;
        }
        cortex_m::asm::delay(SAMPLE_DELAY_CYCLES);
    }
    // Stable-LOW threshold not met.  Fallback: > 50 % LOW overall
    // is still a press — covers cases where the button bounces or
    // the user's hand wobbles during the read window.
    if total_low > MAX_SAMPLES / 2 {
        return WakeSource::CenterPress;
    }

    // No live press observed in the 100 ms window.  Consult the
    // RAM wakeflag (destructive read).  Absent → ColdBoot path
    // (profile may upgrade via flash-backed intent + VBUS).
    //
    // SAFETY: single-threaded boot path; called exactly once.
    let intentional = unsafe { crate::wakeflag::take() };
    if !intentional {
        WakeSource::ColdBoot
    } else {
        // Wake from deliberate soft-off, but no live Center press.
        // Most likely cause: clean SENSE wake where the user
        // released before our 100 ms poll caught them, or some
        // non-Center wake event (USB plug-in in real System OFF —
        // see Aside below for why that can wake the chip).  Brief
        // charging frame → re-sleep is the right behaviour for
        // both.
        WakeSource::UsbPlug
    }
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

    // 1) Mask app-level interrupts before any of the GPIO writes
    //    below.  Two reasons:
    //
    //    - **GPIOTE**: its PORT handler in embassy-nrf clears the
    //      per-pin SENSE bit on DETECT, which would defeat the wake
    //      source we're about to set up if a stray edge arrived
    //      during the few instructions between configuring PIN_CNF
    //      and the `sd_power_system_off` SVC.
    //
    //    - **EGU0_SWI0**: this is where the link runtime's
    //      interrupt executor lives in `profiles/t114_ui`.  Its
    //      shutdown handler is concurrently running a 720 ms LED-
    //      blink sequence at the moment we get here (`enter_soft_off`'s
    //      cooldown is only 250 ms), and it can preempt our GPIO
    //      writes — toggling the status LED back HIGH after we
    //      OUTCLR'd it LOW, leaving it stuck on through System OFF.
    //      Masking it locks the link runtime out for the few µs
    //      between our LED clear and the SVC.
    //
    //    SD's own interrupts (P0/P1/P4) stay enabled — they're
    //    needed for `sd_power_system_off` to do its work, and
    //    they don't touch our GPIOs.
    interrupt::GPIOTE.disable();
    interrupt::EGU0_SWI0.disable();

    // 2) Drive output rails to the off / dark state.
    //
    //    - VEXT (P0_21): peripheral / GPS 3.3 V rail.  Active HIGH.
    //      Raised + leaked by `build_resources`; pulled LOW here so
    //      VEXT-powered peripherals (the external joystick board's
    //      LED, GPS module, etc.) drop their current draw.
    //
    //    - Status LED (P1_03): green LED, **active LOW** (the
    //      board crate's earlier "active-high" comment was wrong;
    //      verified by observation — driving the pin LOW lights
    //      the LED, driving HIGH turns it off).  Could be in
    //      either state from the link-runtime shutdown blink at
    //      the moment we hit this function (`enter_soft_off`'s
    //      250 ms cooldown is shorter than link's full 720 ms
    //      LED-blink sequence).  Force HIGH here via OUTSET so the
    //      LED is reliably off through System OFF.
    //
    //    - NeoPixel (P0_14): WS2812 data line, parked LOW already
    //      by `build_resources`.  Clearing again is a no-op, but
    //      cheap defensive hygiene given how visually noisy a
    //      stuck-on WS2812 would be.
    //
    // SAFETY: OUTCLR / OUTSET are write-1-to-act registers; bits
    // we don't set have no effect.  All three pins were configured
    // as Output by `build_resources`, so the OUT change drives the
    // pin.
    // OUTSET is at GPIO offset 0x508 (one register before OUTCLR).
    const P1_OUTSET_ADDR: *mut u32 = (GPIO_P1_BASE + 0x508) as *mut u32;
    unsafe {
        // VEXT + NeoPixel: drive LOW (off / parked).
        core::ptr::write_volatile(
            outclr_addr(VEXT_PORT),
            (1u32 << VEXT_PIN) | (1u32 << 14), // both on P0
        );
        // Status LED: drive HIGH (off — LED is active LOW).
        core::ptr::write_volatile(P1_OUTSET_ADDR, 1u32 << 3);
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

    // 4b) Clear GPIO.LATCH so the next boot's
    //     `detect_wake_source` sees only bits set *by the wake
    //     event*, not stale matches from before sleep.  Write-1-
    //     to-clear: writing back the current value clears every
    //     latched bit.  Both P0 and P1 — we only have wake sources
    //     on P0_13, but a stale P1 latch could survive and bias
    //     future debugging if we ever look there.
    //
    // SAFETY: GPIO peripheral; app-owned; we just disabled GPIOTE
    // above so nothing else races with us.
    unsafe {
        const P0_LATCH: *mut u32 = (GPIO_P0_BASE + 0x520) as *mut u32;
        const P1_LATCH: *mut u32 = (GPIO_P1_BASE + 0x520) as *mut u32;
        let p0 = core::ptr::read_volatile(P0_LATCH);
        if p0 != 0 {
            core::ptr::write_volatile(P0_LATCH, p0);
        }
        let p1 = core::ptr::read_volatile(P1_LATCH);
        if p1 != 0 {
            core::ptr::write_volatile(P1_LATCH, p1);
        }
    }

    // 5a) Mark the upcoming wake as deliberate.  The next boot's
    //     `detect_wake_source` keys off this magic value to know
    //     whether we're resuming from soft-off (and need to
    //     disambiguate Center-press from USB-plug wake) vs. cold
    //     booting / recovering from a fault.  RAM in `.uninit`
    //     survives System OFF wake on nRF52840.
    //
    // SAFETY: single-threaded soft-off entry; nobody else touches
    // the flag.
    unsafe { crate::wakeflag::set() };

    // 5b) Enter System OFF.  Documented as non-returning **in
    //     production**.  With a debugger attached (DBGEN bit set in
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
    //    Behaviour on emulated-off return: park in WFI forever.
    //    Earlier versions of this function `sys_reset`'d here, but
    //    that creates a busy reboot loop in dev (boot → silent
    //    re-sleep → emulated SVC returns → reset → boot ...) that
    //    re-runs `build_resources` each iteration, causing VEXT to
    //    flicker HIGH/LOW and the status / joystick LEDs to look
    //    stuck on.  WFI-forever keeps the chip quiet for the rest
    //    of the session — operator sees "device powered off"
    //    consistently until they power-cycle for the next test.
    //
    // SAFETY: SVC into the SoftDevice with no arguments.  Caller
    // contract guarantees SD is enabled.
    let ret = unsafe { nrf_softdevice::raw::sd_power_system_off() };
    #[cfg(feature = "defmt")]
    defmt::warn!(
        "sd_power_system_off returned {=u32:#x} — debugger-attached \
         emulated mode.  Parking in WFI; real System OFF requires probe \
         detached + power-cycle.",
        ret,
    );
    let _ = ret;

    // Park.  WFI is preferable to `wfe` here: we don't have any
    // pending events to wait on, and `wfi` halts the CPU until an
    // interrupt fires.  GPIOTE is masked above so user input can't
    // wake us; only NRESET / power-cycle gets the chip back.
    loop {
        cortex_m::asm::wfi();
    }
}