// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![allow(async_fn_in_trait)]

//! Board-agnostic UI runtime: the state-machine driver loop, battery
//! monitor, deep-soft-off coordination, USB-wake helpers, settings
//! persistence, and all the cross-task signals + statics that glue
//! them together.
//!
//! Profiles bring the concrete board peripherals (display, joystick,
//! flash, watchdog, battery monitor, system-off entry point) and
//! spawn the embassy tasks that pump this runtime; everything in
//! between lives here.  Per the portability guardrails in `PLAN.md`,
//! this crate only depends on `embedded-hal*` / `embedded-storage-
//! async` traits + embassy-time / -sync — no vendor HAL.
//!
//! ## Topology
//!
//! Tasks (defined in the profile, calling into this crate):
//!
//!   - **`ui_state_loop`** — the main state-machine driver.  Consumes
//!     joystick events from [`EVENT_CHAN`], snapshots link stats from
//!     [`STATS`], pushes config updates to [`CONFIG_UPDATES`],
//!     produces frames to [`FRAME`], polls [`POWEROFF_REASON`] for
//!     soft-off triggers, watches VBUS for the Wired-policy timeout.
//!   - **`battery_loop`** — periodic SAADC sampler.  Writes
//!     [`BATTERY`], latches [`POWEROFF_REASON_LOW_BATTERY`] +
//!     [`SHUTDOWN`] on sustained low Vbat.
//!   - Profile-owned: `joystick_task` (pushes [`EVENT_CHAN`]),
//!     `ui_render_task` (consumes [`FRAME`] + [`POWER_OFF_DISPLAY`]),
//!     `link_*_task` (wraps `osrf_app_midi_node::run_*` with concrete
//!     radio + UART + the [`SHUTDOWN`] / [`CONFIG_UPDATES`] /
//!     [`SCAN`] signals).
//!
//! Soft-off path: [`enter_soft_off`] coordinates a clean
//! teardown — render the goodbye frame, sleep the radio via
//! [`SHUTDOWN`], park the display via [`POWER_OFF_DISPLAY`], persist
//! the soft-off-intent flag, then call into the board-supplied
//! `system_off` function pointer.

use core::ops::Range;
use core::sync::atomic::{AtomicU8, Ordering};

use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};

use osrf_app_midi_node::LinkConfig;
use osrf_driver_input_joystick5way::JoystickEvent;
use osrf_link_runtime::{LinkConfigSignal, LinkStatsCell, LinkStats, ScanController, ShutdownSignal};
use osrf_ui::{
    band_plan_channel, band_plan_index, build_screen, max_channel_index, AboutData, BandPlan,
    BatteryChemistry, BatteryStatus, Command, KeyStore, LinkStatus, PowerPolicy, ScanState,
    ScreenId, Settings, UiState, Widget, WidgetList, BAND_PLANS, MAX_SCAN_CHANNELS,
    WIRED_USB_LOSS_GRACE_SECS,
};
use sequential_storage::cache::NoCache;
use sequential_storage::map;

// Note: profiles import `osrf_ui` and `osrf_link_runtime` directly
// for the names they need.  We don't re-export from here to avoid
// a noisy glob vs. internal-use shadowing.

// ── Cross-task signals + state ───────────────────────────────────

/// Input event channel — joystick task pushes, ui_state pops.
pub static EVENT_CHAN: Channel<CriticalSectionRawMutex, JoystickEvent, 8> = Channel::new();

/// Cross-task shared link-runtime stats.  `run_rx` / `run_tx` write
/// counters + RSSI + link-up here on every loop iteration; the
/// ui_state loop snapshots the latest values each frame build and
/// translates them into a `LinkStatus` for Idle / Link Stats.
pub static STATS: LinkStatsCell = LinkStatsCell::new();

/// Live config-update channel from ui_state → link runtime.  When the
/// user applies a new channel / band plan / TX power, ui_state
/// rebuilds a `LinkConfig` and signals here; the runtime's `select`
/// arm wakes, re-runs `configure_radio`, and resumes (re-`rx_start`
/// for RX).  Latest-wins, so two rapid changes collapse to the most
/// recent.
pub static CONFIG_UPDATES: LinkConfigSignal = LinkConfigSignal::new();

/// Channel-scan handoff between ui_state and link runtime.  When
/// the user enters the Scan screen, ui_state calls `start()` with
/// the current band plan's frequency list; the runtime walks the
/// list sampling RSSI per channel.  Each render tick ui_state
/// snapshots the results into `state.scan` via `apply_scan_pass`.
pub static SCAN: ScanController = ScanController::new();

/// Soft-off coordinator (link-runtime side).  `battery_loop` fires
/// this after observing `SHUTDOWN_BATTERY_SUSTAINED_SAMPLES`
/// consecutive readings at-or-below `chemistry.shutdown_mv()`; the
/// operator-soft-off and Wired-USB-loss paths also signal it.  The
/// link task (`run_rx` / `run_tx`) `select`s on it, sinks all-notes-
/// off, parks the radio, and idles forever.  Single-consumer —
/// `embassy_sync::Signal` takes-on-wait, so the matching
/// [`POWEROFF_REASON`] atomic carries the reason for the ui side.
pub static SHUTDOWN: ShutdownSignal = ShutdownSignal::new();

/// Latched soft-off reason.  Set alongside [`SHUTDOWN`] by either
/// `battery_loop` (sustained low Vbat), the UI's [`Command::PowerOff`]
/// handler, or `ui_state_loop`'s Wired-USB-loss grace timer.
/// Polled by `ui_state_loop` each tick; dispatches to
/// [`enter_soft_off`] on any non-zero value.
///
/// Separate from `SHUTDOWN` because `Signal::wait` consumes the
/// value (single-consumer) and we want both the link runtime and
/// the UI to observe the same event.  Polling at the 300 ms scan-
/// tick cadence is fine — the soft-off budget is "user sees goodbye,
/// peripherals quiesce, chip enters System OFF," all on the order
/// of seconds.
pub static POWEROFF_REASON: AtomicU8 = AtomicU8::new(POWEROFF_REASON_NONE);

/// Atomic-friendly encoding of the soft-off reason latched in
/// [`POWEROFF_REASON`].  `None` is the default zero so a fresh boot
/// is implicitly "no soft-off requested."
pub const POWEROFF_REASON_NONE: u8 = 0;
/// Operator chose Power Off → Confirm.  Normal-user-flow soft-off;
/// not logged to the panic ring.
pub const POWEROFF_REASON_OPERATOR: u8 = 1;
/// `battery_loop` saw [`SHUTDOWN_BATTERY_SUSTAINED_SAMPLES`]
/// consecutive readings at-or-below the chemistry's shutdown floor
/// with USB unplugged.  `enter_soft_off` pushes a `low-battery
/// shutdown` panic-ring record before the System OFF call so the
/// next boot's About screen surfaces the cause.
pub const POWEROFF_REASON_LOW_BATTERY: u8 = 2;
/// [`PowerPolicy::Wired`] + USB power has been absent for the
/// [`WIRED_USB_LOSS_GRACE_SECS`] grace.  Renders "USB disconnected"
/// goodbye and re-enters System OFF; the next USB plug or Center
/// press cold-boots back to Idle.  No panic-ring record — normal flow.
pub const POWEROFF_REASON_WIRED_USB_LOST: u8 = 3;

/// "Power off the display" handshake from `ui_state_loop` to
/// `ui_render_task`.  The render task owns the display and we need
/// `display.power_off()` (DISPOFF + SLPIN + VTFT high) to run before
/// the chip enters System OFF — otherwise the panel sits in normal
/// mode with VDD on through sleep, defeating the soft-off current
/// target.  Fired exactly once per deep-soft-off entry; the render
/// task drops into WDT-pet idle after handling it.
pub static POWER_OFF_DISPLAY: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Number of consecutive sub-`chemistry.shutdown_mv()` battery
/// samples required before firing [`SHUTDOWN`].  With a 5 s sample
/// interval this is `5 * 5 = 25 s` of sustained-low — long enough
/// that a transient dip (TX-burst sag, plug-event transient)
/// doesn't trip shutdown, short enough that a genuinely dying cell
/// still has run-time left to do the all-notes-off + radio-park
/// dance.
pub const SHUTDOWN_BATTERY_SUSTAINED_SAMPLES: u32 = 5;

/// Latest battery reading.  Written by [`battery_loop`] every
/// [`BATTERY_SAMPLE_INTERVAL_S`] seconds, read by `ui_state_loop`
/// each frame build to populate the top-bar indicator.
/// `Cell<BatteryStatus>` is fine here: `BatteryStatus` is `Copy`
/// and updates are infrequent.
pub static BATTERY: critical_section::Mutex<core::cell::Cell<BatteryStatus>> =
    critical_section::Mutex::new(core::cell::Cell::new(BatteryStatus::UNKNOWN));

/// Sample-rate for battery monitoring.  Meshtastic uses 5 s; we
/// match.  Faster polling buys nothing (LiPo Vbat moves slowly
/// relative to a 5 s window) and burns extra current through the
/// divider.
pub const BATTERY_SAMPLE_INTERVAL_S: u64 = 5;

/// How often `ui_render_task` falls through its FRAME wait to pet
/// the watchdog when the display is off (no frames signalled).
/// Must be comfortably less than the WDT timeout.
pub const WDT_RENDER_IDLE_PET_S: u64 = 2;

/// "Render this frame" handoff from ui_state → ui_render.  ui_state
/// builds a fresh [`WidgetList`] from the UI state machine, snapshots
/// the [`ScanState`] alongside it, and signals.  ui_render awaits
/// here; latest-wins, so a render in flight when a newer frame is
/// signalled means the older frame is dropped — we always show the
/// most recent state.
pub static FRAME: Signal<CriticalSectionRawMutex, FrameData> = Signal::new();

/// Snapshot of UI state needed for one render.  `ScanState` is
/// copied (not referenced) because the ui_render task runs on a
/// different executor and can't hold a reference into ui_state's
/// locals.
#[derive(Clone)]
pub struct FrameData {
    pub widgets: WidgetList,
    pub scan: ScanState,
}

// ── Watchdog trait ──────────────────────────────────────────────

/// Tiny trait abstracting a hardware watchdog slot the profile owns.
/// Each task that owns a slot calls `pet()` at its loop top; if any
/// slot misses its window the WDT triggers a chip reset.
///
/// Implemented by the board crate for its concrete `WatchdogHandle`
/// type — keeps embassy-nrf out of this app crate.
pub trait Watchdog {
    fn pet(&mut self);
}

// ── UI state machine driver ─────────────────────────────────────

/// Idle screen → backlight off when this long with no input.
const IDLE_OFF_TIMEOUT: Duration = Duration::from_secs(15);
/// Any non-Idle / non-LinkStats / non-Scan screen → go_home when
/// this long with no input (whereupon `IDLE_OFF_TIMEOUT` runs).
const MENU_TO_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Wired-mode USB-loss grace period.  See [`WIRED_USB_LOSS_GRACE_SECS`].
const WIRED_GRACE: Duration = Duration::from_secs(WIRED_USB_LOSS_GRACE_SECS);

/// UI state-machine driver.  Runs as the main task on the profile's
/// thread executor.  Responsibilities:
///   - Receive joystick events from [`EVENT_CHAN`].
///   - Dispatch them through [`UiState::handle_event`].
///   - Push live config updates to [`CONFIG_UPDATES`].
///   - Reconcile [`SCAN`] state with the active screen + band plan.
///   - Implement the inactivity / auto-off policy.
///   - Build a fresh [`FrameData`] each tick or event and hand it
///     off to ui_render via [`FRAME`].
///   - Poll [`POWEROFF_REASON`] for deep-soft-off triggers and
///     dispatch into [`enter_soft_off`].
///   - In [`PowerPolicy::Wired`] mode, run the VBUS-loss grace timer.
#[allow(clippy::too_many_arguments)]
pub async fn ui_state_loop<BL, F, W>(
    backlight: &mut BL,
    flash: &mut F,
    mut wdt: W,
    state: &mut UiState,
    mut settings: Settings,
    keys: KeyStore,
    widgets: &mut WidgetList,
    session_id: u16,
    last_panic_msg: &mut heapless::String<96>,
    firmware_version: &'static str,
    git_hash: &'static str,
    chemistry: BatteryChemistry,
    power_policy: PowerPolicy,
    settings_range: Range<u32>,
    panic_ring_range: Range<u32>,
    vbus_present: fn() -> bool,
    system_off: fn() -> !,
) -> !
where
    BL: embedded_hal::digital::OutputPin,
    F: embedded_storage_async::nor_flash::MultiwriteNorFlash,
    W: Watchdog,
{
    let scan_tick = Duration::from_millis(300);
    let mut last_input = Instant::now();
    let mut display_on = true;

    let mut scan_running = false;
    let mut scan_plan: Option<BandPlan> = None;

    // Wired-mode USB-loss grace timer.  Initialised to `now` so a
    // Wired-mode boot with USB absent still gets the full grace
    // window before shutting down.  In Battery builds the block
    // below const-folds away.
    let mut last_vbus_present_at = Instant::now();

    loop {
        // Pet the WDT at every loop iteration top.  Cadence is the
        // scan_tick (~300 ms) plus whatever flash-write time an
        // Apply* command added — comfortably under the typical 5 s
        // WDT window.  Bursts of joystick events pet faster than that.
        wdt.pet();

        // Wired-mode VBUS tracking.  In Battery builds the whole
        // block const-folds away.  USB-state-change events on the
        // T114 don't have an interrupt routed (USBDETECTED is
        // consumed by the SD's POWER handler for wake-from-System-
        // OFF), so we poll on each scan_tick.
        if matches!(power_policy, PowerPolicy::Wired) {
            if vbus_present() {
                last_vbus_present_at = Instant::now();
            } else if Instant::now().duration_since(last_vbus_present_at) >= WIRED_GRACE
                && POWEROFF_REASON.load(Ordering::Acquire) == POWEROFF_REASON_NONE
            {
                defmt::warn!(
                    "ui: Wired mode + USB absent > {} s → latching soft-off",
                    WIRED_USB_LOSS_GRACE_SECS,
                );
                POWEROFF_REASON.store(POWEROFF_REASON_WIRED_USB_LOST, Ordering::Release);
            }
        }

        // Deep soft-off: any of the latch sources above (operator,
        // low-battery, Wired-USB-lost) want the same teardown.
        let reason = POWEROFF_REASON.load(Ordering::Acquire);
        if reason != POWEROFF_REASON_NONE {
            enter_soft_off(
                reason,
                backlight,
                &mut wdt,
                flash,
                widgets,
                state,
                display_on,
                settings_range.clone(),
                panic_ring_range.clone(),
                system_off,
            )
            .await;
        }

        let next_tick = Timer::after(scan_tick);
        match select(EVENT_CHAN.receive(), next_tick).await {
            Either::First(event) => {
                last_input = Instant::now();
                if !display_on {
                    // Wake from sleep — first press just lights the
                    // panel back up.  Don't dispatch the event.
                    let _ = backlight.set_low();
                    display_on = true;
                    defmt::info!("ui: wake (joystick input)");
                } else if let Some(cmd) = state.handle_event(&mut settings, &keys, event) {
                    defmt::info!("ui command: {:?}", cmd);
                    match cmd {
                        Command::ApplyChannel(ch) => {
                            CONFIG_UPDATES.signal(link_config_from(&settings));
                            save_channel(flash, settings_range.clone(), ch).await;
                        }
                        Command::ApplyBandPlan(plan) => {
                            CONFIG_UPDATES.signal(link_config_from(&settings));
                            save_band_plan(flash, settings_range.clone(), plan).await;
                            // Band-plan change resets channel to 0; persist
                            // the new channel too so a reboot in the new plan
                            // picks up where the user landed.
                            save_channel(flash, settings_range.clone(), settings.channel).await;
                        }
                        Command::ApplyTxPower(dbm) => {
                            CONFIG_UPDATES.signal(link_config_from(&settings));
                            save_tx_power(flash, settings_range.clone(), dbm).await;
                        }
                        Command::ApplySetActiveKey(fp) => {
                            // No live-config update (AEAD not wired into
                            // LinkConfig yet); persist for next boot.
                            save_active_key(flash, settings_range.clone(), fp).await;
                        }
                        Command::PowerOff => {
                            // Operator-initiated soft-off.  Latch the
                            // reason and let the next loop-top poll
                            // dispatch.  Going through the shared latch
                            // (vs. inlining) means low-battery + operator
                            // paths share code, and pending scan_tick /
                            // frame work lands cleanly on the way out.
                            defmt::info!("ui: operator power-off confirmed");
                            POWEROFF_REASON.store(POWEROFF_REASON_OPERATOR, Ordering::Release);
                        }
                        Command::ForcePanic => {
                            // Diagnostic — fires the profile's panic
                            // handler so the staging → sys_reset →
                            // recovery → About flow can be exercised
                            // end-to-end without a rebuild.
                            panic!("forced panic from menu (test)");
                        }
                        Command::ForceWdtHang => {
                            // Diagnostic — busy-spin so this task stops
                            // petting WDT.  After the timeout the HW WDT
                            // fires, chip resets, next boot's recovery
                            // sees DOG in RESETREAS without a staged panic
                            // and pushes "watchdog: task hung" to the
                            // panic ring.
                            defmt::warn!("ui: forced WDT hang — chip will reset shortly");
                            loop {
                                core::hint::spin_loop();
                            }
                        }
                        Command::ClearPanicLog => {
                            match osrf_panic_log::clear(flash, panic_ring_range.clone()).await {
                                Ok(()) => {
                                    last_panic_msg.clear();
                                    defmt::info!("ui: panic ring cleared");
                                }
                                Err(_e) => {
                                    defmt::warn!(
                                        "ui: panic-ring clear failed: {:?}",
                                        defmt::Debug2Format(&_e)
                                    );
                                }
                            }
                        }
                    }
                }
            }
            Either::Second(()) => {
                if display_on && state.screen == ScreenId::Scan {
                    let mut buf = [0i16; MAX_SCAN_CHANNELS];
                    let n = state.scan.channel_count as usize;
                    SCAN.read_results(&mut buf[..n]);
                    state.apply_scan_pass(&buf[..n]);
                }
            }
        }

        // Reconcile the runtime scanner's mode with the UI's current
        // screen.  Gated on `display_on` — pausing scan when the
        // panel sleeps lets the link resume between sleeps.
        let want_scan = display_on && state.screen == ScreenId::Scan;
        let cur_plan = settings.band_plan;
        match (scan_running, want_scan, scan_plan == Some(cur_plan)) {
            (false, true, _) => {
                let mut freqs = [0u32; MAX_SCAN_CHANNELS];
                let n = collect_scan_frequencies(cur_plan, &mut freqs);
                SCAN.start(&freqs[..n]);
                scan_running = true;
                scan_plan = Some(cur_plan);
            }
            (true, true, false) => {
                let mut freqs = [0u32; MAX_SCAN_CHANNELS];
                let n = collect_scan_frequencies(cur_plan, &mut freqs);
                SCAN.start(&freqs[..n]);
                scan_plan = Some(cur_plan);
            }
            (true, false, _) => {
                SCAN.stop();
                scan_running = false;
                scan_plan = None;
            }
            _ => {}
        }

        if display_on {
            let idle_for = Instant::now().duration_since(last_input);
            match state.screen {
                ScreenId::LinkStats | ScreenId::Scan => {}
                ScreenId::Idle => {
                    if idle_for >= IDLE_OFF_TIMEOUT {
                        defmt::info!("ui: auto-off (idle {} s)", IDLE_OFF_TIMEOUT.as_secs());
                        let _ = backlight.set_high();
                        display_on = false;
                    }
                }
                _ => {
                    if idle_for >= MENU_TO_IDLE_TIMEOUT {
                        defmt::info!(
                            "ui: menu->idle ({} s no input)",
                            MENU_TO_IDLE_TIMEOUT.as_secs()
                        );
                        state.go_home();
                        last_input = Instant::now();
                    }
                }
            }
        }

        // Build a fresh frame and hand it off to ui_render.  When
        // the panel is off we don't signal — the render task sleeps
        // and the panel keeps whatever it had last frame (which
        // doesn't matter because the backlight is off).
        if display_on {
            let status = link_status_from_stats(&STATS.get());
            let about = about_data(session_id, last_panic_msg, firmware_version, git_hash);
            build_screen(state, &settings, &keys, &status, &about, widgets);
            // Top-bar battery indicator — pushed here (not by
            // `build_screen`) so every screen gets it without each
            // `build_*` having to opt in.
            let battery = critical_section::with(|cs| BATTERY.borrow(cs).get());
            let _ = widgets.push(Widget::BatteryIndicator {
                voltage_mv: battery.voltage_mv,
                percent: battery.percent,
                plugged_in: battery.plugged_in,
            });
            FRAME.signal(FrameData {
                widgets: widgets.clone(),
                scan: state.scan.clone(),
            });
        }

        // Touch the chemistry binding so `clippy::needless_pass_by_value`
        // doesn't complain about it appearing only in flash-helper docs.
        let _ = chemistry;
    }
}

// ── Deep soft-off ────────────────────────────────────────────────

/// Tear the device down to sub-µA System OFF.  Diverges; the only
/// path out is the chip resetting via the SENSE wake configured by
/// the board-supplied `system_off` fn.
#[allow(clippy::too_many_arguments)]
pub async fn enter_soft_off<BL, F, W>(
    reason: u8,
    backlight: &mut BL,
    wdt: &mut W,
    flash: &mut F,
    widgets: &mut WidgetList,
    state: &UiState,
    display_on: bool,
    settings_range: Range<u32>,
    panic_ring_range: Range<u32>,
    system_off: fn() -> !,
) -> !
where
    BL: embedded_hal::digital::OutputPin,
    F: embedded_storage_async::nor_flash::MultiwriteNorFlash,
    W: Watchdog,
{
    let reason_label = match reason {
        POWEROFF_REASON_OPERATOR => "operator",
        POWEROFF_REASON_LOW_BATTERY => "low-battery",
        POWEROFF_REASON_WIRED_USB_LOST => "wired-usb-lost",
        _ => "unknown",
    };
    defmt::warn!("ui: deep soft-off ({}) — rendering goodbye", reason_label);

    // 1) Wake the panel if it had auto-slept.
    if !display_on {
        let _ = backlight.set_low();
    }

    // 2) Goodbye frame.
    widgets.clear();
    build_power_off_goodbye(reason, widgets);
    let battery = critical_section::with(|cs| BATTERY.borrow(cs).get());
    let _ = widgets.push(Widget::BatteryIndicator {
        voltage_mv: battery.voltage_mv,
        percent: battery.percent,
        plugged_in: battery.plugged_in,
    });
    FRAME.signal(FrameData {
        widgets: widgets.clone(),
        scan: state.scan.clone(),
    });

    // 3) Hold the goodbye visible for ~1 s, petting the WDT.
    for _ in 0..4 {
        wdt.pet();
        Timer::after_millis(250).await;
    }

    // 4) Low-battery: panic-ring audit-trail entry.
    if reason == POWEROFF_REASON_LOW_BATTERY {
        osrf_panic_log::push(flash, panic_ring_range, 0, b"low-battery shutdown").await;
    }

    // 5) Backlight off — TFT VDD follows in step 7 when the render
    //    task gates it; backlight first avoids a brief "lit blank
    //    panel" flash during the controller's sleep handshake.
    let _ = backlight.set_high();

    // 6) Link runtime teardown.  Picks up SHUTDOWN, runs
    //    all-notes-off (RX) / radio standby_rc / blink / set_sleep.
    SHUTDOWN.signal();

    // 7) Display teardown.  Render task picks up POWER_OFF_DISPLAY,
    //    runs display.power_off(), then sits petting wdt_render.
    POWER_OFF_DISPLAY.signal(());

    // 8) Cooldown so link + render land their teardown.
    wdt.pet();
    Timer::after_millis(250).await;
    wdt.pet();

    // 9) Persist soft-off intent so the next boot recognises us.
    save_soft_off_intent(flash, settings_range, true).await;
    wdt.pet();

    // 10) Enter System OFF via the board-supplied entry point.
    defmt::info!("ui: entering System OFF — wake on Center press");
    system_off()
}

/// No-UI re-entry into System OFF for wakes that were neither a
/// deliberate Center press nor a confirmed USB plug-in.  Covers
/// shield-touch ESD events and battery-pulls during soft-off.
/// User-facing effect: chip appears to "stay off."
///
/// No flash write needed — we got here because `flash_intent` was
/// already true on entry to the profile's `run()` and the Idle-
/// path end-of-init clear didn't fire.  Flag is still true; next
/// wake will recognise the prior soft-off intent.
pub fn unexpected_wake_resleep(system_off: fn() -> !) -> ! {
    defmt::info!(
        "ui: unexpected wake (soft-off intent set, no live press, no VBUS) → silent re-sleep"
    );
    system_off()
}

// ── UI helpers ───────────────────────────────────────────────────

/// Construct an [`AboutData`] borrowing the long-lived
/// `last_panic_msg` buffer.  Empty panic message → `None`.
pub fn about_data<'a>(
    session_id: u16,
    last_panic_msg: &'a heapless::String<96>,
    firmware_version: &'static str,
    git_hash: &'static str,
) -> AboutData<'a> {
    AboutData {
        firmware_version,
        git_hash,
        session_id,
        last_panic: if last_panic_msg.is_empty() {
            None
        } else {
            Some(last_panic_msg.as_str())
        },
    }
}

/// Translate `osrf-link-runtime`'s `LinkStats` snapshot into the
/// `osrf-ui` `LinkStatus` shape that the renderer expects.
pub fn link_status_from_stats(s: &LinkStats) -> LinkStatus {
    LinkStatus {
        up: s.link_up,
        last_rssi_dbm: s
            .last_rssi_dbm
            .map(|r| r.clamp(i8::MIN as i16, i8::MAX as i16) as i8),
        recent_loss_pct: s.recent_loss_pct,
        total_accepted: s.total_accepted,
        stuck_recoveries: s.stuck_recoveries,
    }
}

/// Build a [`LinkConfig`] from the UI's [`Settings`].  Today only
/// `frequency_hz` and `tx_power_dbm` flow through; the rest stays
/// at `default_915()` values.
pub fn link_config_from(settings: &Settings) -> LinkConfig {
    let mut c = LinkConfig::default_915();
    c.frequency_hz = settings.current_channel().frequency_khz * 1000;
    c.tx_power_dbm = settings.tx_power_dbm;
    c
}

/// Build the frequency list (Hz) for a band plan, in channel-index
/// order.  Used to seed [`SCAN`]'s frequency table when the user
/// enters the Scan screen.
pub fn collect_scan_frequencies(plan: BandPlan, out: &mut [u32; MAX_SCAN_CHANNELS]) -> usize {
    let max_idx = max_channel_index(plan);
    let n = (max_idx as usize + 1).min(MAX_SCAN_CHANNELS);
    for (i, slot) in out.iter_mut().enumerate().take(n) {
        *slot = band_plan_channel(plan, i as u8).frequency_khz * 1000;
    }
    n
}

/// Tiny helper: build a fixed-size `heapless::String` from a
/// `&'static str`.
pub fn short_str<const N: usize>(literal: &'static str) -> heapless::String<N> {
    let mut out = heapless::String::new();
    let _ = out.push_str(literal);
    out
}

/// Push the reason-specific goodbye widgets for [`enter_soft_off`].
pub fn build_power_off_goodbye(reason: u8, out: &mut WidgetList) {
    let _ = out.push(Widget::Title(short_str::<24>("Powering off")));
    let (line2, line3) = match reason {
        POWEROFF_REASON_LOW_BATTERY => ("Battery low", "Plug in to charge"),
        POWEROFF_REASON_WIRED_USB_LOST => ("USB disconnected", "Plug back in to wake"),
        // Operator (or any unforeseen value) — safe default is
        // "tell the user how to bring it back."
        _ => ("Goodnight", "Press Center to wake"),
    };
    let _ = out.push(Widget::Text {
        row: 2,
        text: short_str::<24>(line2),
    });
    let _ = out.push(Widget::Text {
        row: 3,
        text: short_str::<24>(line3),
    });
}

/// Build the widget list for the USB-wake brief charging frame.
/// Pure widget assembly — the profile handles display init / flush /
/// teardown around this.
pub fn build_charging_frame(status: BatteryStatus, out: &mut WidgetList) {
    use core::fmt::Write as _;
    let _ = out.push(Widget::Title(short_str::<24>("Charging")));
    let mut mv_text: heapless::String<24> = heapless::String::new();
    let _ = write!(&mut mv_text, "{} mV", status.voltage_mv);
    let _ = out.push(Widget::Text { row: 2, text: mv_text });
    let mut pct_text: heapless::String<24> = heapless::String::new();
    let _ = write!(&mut pct_text, "{}%", status.percent);
    let _ = out.push(Widget::Text { row: 3, text: pct_text });
    let _ = out.push(Widget::BatteryIndicator {
        voltage_mv: status.voltage_mv,
        percent: status.percent,
        plugged_in: true,
    });
}

// ── Battery monitor ──────────────────────────────────────────────

/// Trait the profile implements over its concrete battery monitor
/// peripheral so [`battery_loop`] can sample voltages without
/// depending on the board's HAL.
pub trait BatterySampler {
    /// Sample the cell voltage in millivolts.  Should also handle
    /// any divider-enable / SAADC settling internal to the
    /// implementation.
    async fn sample_mv(&mut self) -> u16;

    /// Return `true` when USB power is detected at the chip.  Used
    /// by the shutdown-eligibility check (we don't soft-off from
    /// low battery if USB is plugged in — the user is charging).
    fn vbus_present(&self) -> bool;
}

/// Periodic battery sampler.  Wakes every
/// [`BATTERY_SAMPLE_INTERVAL_S`] seconds, reads Vbat, polls VBUS,
/// writes the shared [`BATTERY`] cell, and latches a soft-off
/// trigger after [`SHUTDOWN_BATTERY_SUSTAINED_SAMPLES`] consecutive
/// sub-threshold readings with USB unplugged.
///
/// Low / critical warning logs only fire when an actual battery is
/// detected (Vbat ≥ `chemistry.no_battery_mv()`) — a probe-only
/// board with no cell wired in stays quiet.
pub async fn battery_loop<S>(mut sampler: S, chemistry: BatteryChemistry) -> !
where
    S: BatterySampler,
{
    let mut shutdown_run: u32 = 0;
    let mut shutdown_fired = false;
    loop {
        let mv = sampler.sample_mv().await;
        let plugged_in = sampler.vbus_present();
        let status = BatteryStatus::from_reading(mv, plugged_in, chemistry);
        critical_section::with(|cs| BATTERY.borrow(cs).set(status));
        if status.is_critical() {
            defmt::warn!(
                "battery critical: {=u16} mV ({=u8} %)",
                status.voltage_mv,
                status.percent
            );
        } else if status.is_low() {
            defmt::info!(
                "battery low: {=u16} mV ({=u8} %)",
                status.voltage_mv,
                status.percent
            );
        }
        let shutdown_eligible = !plugged_in
            && mv >= chemistry.no_battery_mv()
            && mv <= chemistry.shutdown_mv();
        if shutdown_eligible {
            shutdown_run = shutdown_run.saturating_add(1);
            if !shutdown_fired && shutdown_run >= SHUTDOWN_BATTERY_SUSTAINED_SAMPLES {
                defmt::warn!(
                    "battery shutdown threshold: {=u16} mV sustained for {=u32} samples \
                     — signalling SHUTDOWN",
                    mv,
                    shutdown_run,
                );
                SHUTDOWN.signal();
                POWEROFF_REASON.store(POWEROFF_REASON_LOW_BATTERY, Ordering::Release);
                shutdown_fired = true;
            }
        } else {
            shutdown_run = 0;
        }
        Timer::after_secs(BATTERY_SAMPLE_INTERVAL_S).await;
    }
}

// ── Settings persistence ─────────────────────────────────────────
//
// Each `Settings` field gets its own [`sequential-storage`] key in
// the Settings flash region.  Per-field keys mean an Apply* command
// only rewrites the changed field — others stay where they are.  At
// our write cadence the wear cost is negligible.
//
// Schema (key → value type):
//   KEY_CHANNEL         → u32     channel index in the active band plan
//   KEY_BAND_PLAN       → u32     index into `BAND_PLANS`
//   KEY_TX_POWER        → i32     dBm, range MIN_TX_POWER_DBM..=MAX_TX_POWER_DBM
//   KEY_ACTIVE_KEY_FP   → u32     fingerprint, 0 = "no key" (== `None`)
//   KEY_SOFT_OFF_INTENT → u32     1 if last run entered soft-off; 0 otherwise.
//                                 Survives brown-out + battery-pull (unlike the
//                                 RAM-based wakeflag in the board crate),
//                                 which is what makes the brief-charging-frame
//                                 work when USB plug causes a brown-out reset.

pub const KEY_CHANNEL: u8 = 0;
pub const KEY_BAND_PLAN: u8 = 1;
pub const KEY_TX_POWER: u8 = 2;
pub const KEY_ACTIVE_KEY_FP: u8 = 3;
pub const KEY_SOFT_OFF_INTENT: u8 = 4;

/// Scratch buffer size for sequential-storage's record assembly.
/// 64 B is comfortable for any of our values (max payload is u32 ≈
/// 4 B plus overhead).
const PERSIST_BUF_LEN: usize = 64;

/// Read all `Settings` fields from flash and apply to `settings`.
/// Fields not present in flash stay at whatever default the caller
/// seeded `settings` with.  Errors logged but not propagated —
/// persistence failures fall back to defaults, not fatal.
pub async fn load_settings<F>(flash: &mut F, range: Range<u32>, settings: &mut Settings)
where
    F: embedded_storage_async::nor_flash::MultiwriteNorFlash,
{
    let mut buf = [0u8; PERSIST_BUF_LEN];
    let mut cache = NoCache::new();

    match map::fetch_item::<u8, u32, _>(flash, range.clone(), &mut cache, &mut buf, &KEY_CHANNEL)
        .await
    {
        Ok(Some(v)) => settings.channel = v as u8,
        Ok(None) => defmt::info!("persist: no stored channel, using default"),
        Err(e) => defmt::warn!("persist: load channel failed: {:?}", defmt::Debug2Format(&e)),
    }
    match map::fetch_item::<u8, u32, _>(
        flash,
        range.clone(),
        &mut cache,
        &mut buf,
        &KEY_BAND_PLAN,
    )
    .await
    {
        Ok(Some(v)) if (v as usize) < BAND_PLANS.len() => {
            settings.band_plan = BAND_PLANS[v as usize];
        }
        Ok(Some(v)) => defmt::warn!(
            "persist: stored band_plan index {} out of range, using default",
            v
        ),
        Ok(None) => defmt::info!("persist: no stored band_plan, using default"),
        Err(e) => defmt::warn!(
            "persist: load band_plan failed: {:?}",
            defmt::Debug2Format(&e)
        ),
    }
    match map::fetch_item::<u8, i32, _>(flash, range.clone(), &mut cache, &mut buf, &KEY_TX_POWER)
        .await
    {
        Ok(Some(v)) => settings.tx_power_dbm = v as i8,
        Ok(None) => defmt::info!("persist: no stored tx_power, using default"),
        Err(e) => defmt::warn!("persist: load tx_power failed: {:?}", defmt::Debug2Format(&e)),
    }
    match map::fetch_item::<u8, u32, _>(flash, range, &mut cache, &mut buf, &KEY_ACTIVE_KEY_FP)
        .await
    {
        Ok(Some(0)) => settings.active_key_fp = None,
        Ok(Some(v)) => settings.active_key_fp = Some(v),
        Ok(None) => defmt::info!("persist: no stored active_key_fp, using default"),
        Err(e) => defmt::warn!("persist: load key_fp failed: {:?}", defmt::Debug2Format(&e)),
    }

    defmt::info!(
        "persist: loaded ch={} plan={=usize} pwr={} key_fp={:?}",
        settings.channel,
        band_plan_index(settings.band_plan),
        settings.tx_power_dbm,
        settings.active_key_fp,
    );
}

async fn save_u32<F>(flash: &mut F, range: Range<u32>, key: u8, value: u32, what: &str)
where
    F: embedded_storage_async::nor_flash::MultiwriteNorFlash,
{
    let mut buf = [0u8; PERSIST_BUF_LEN];
    let mut cache = NoCache::new();
    if let Err(e) =
        map::store_item::<u8, u32, _>(flash, range, &mut cache, &mut buf, &key, &value).await
    {
        defmt::warn!("persist: save {} failed: {:?}", what, defmt::Debug2Format(&e));
    }
}

async fn save_i32<F>(flash: &mut F, range: Range<u32>, key: u8, value: i32, what: &str)
where
    F: embedded_storage_async::nor_flash::MultiwriteNorFlash,
{
    let mut buf = [0u8; PERSIST_BUF_LEN];
    let mut cache = NoCache::new();
    if let Err(e) =
        map::store_item::<u8, i32, _>(flash, range, &mut cache, &mut buf, &key, &value).await
    {
        defmt::warn!("persist: save {} failed: {:?}", what, defmt::Debug2Format(&e));
    }
}

pub async fn save_channel<F>(flash: &mut F, range: Range<u32>, ch: u8)
where
    F: embedded_storage_async::nor_flash::MultiwriteNorFlash,
{
    save_u32(flash, range, KEY_CHANNEL, ch as u32, "channel").await;
}

pub async fn save_band_plan<F>(flash: &mut F, range: Range<u32>, plan: BandPlan)
where
    F: embedded_storage_async::nor_flash::MultiwriteNorFlash,
{
    save_u32(
        flash,
        range,
        KEY_BAND_PLAN,
        band_plan_index(plan) as u32,
        "band_plan",
    )
    .await;
}

pub async fn save_tx_power<F>(flash: &mut F, range: Range<u32>, dbm: i8)
where
    F: embedded_storage_async::nor_flash::MultiwriteNorFlash,
{
    save_i32(flash, range, KEY_TX_POWER, dbm as i32, "tx_power").await;
}

pub async fn save_active_key<F>(flash: &mut F, range: Range<u32>, fp: Option<u32>)
where
    F: embedded_storage_async::nor_flash::MultiwriteNorFlash,
{
    save_u32(flash, range, KEY_ACTIVE_KEY_FP, fp.unwrap_or(0), "key_fp").await;
}

/// Persist the soft-off intent flag.  Set from [`enter_soft_off`]
/// just before the system-off SVC.  Flash-backed because USB-plug
/// events sometimes brown-out the chip on the T114 (TP4054 mode-
/// switch transient or ESD on the cable shield) — RAM wakeflag is
/// wiped, but flash survives, so the next boot still recognises
/// "we meant to be off."
pub async fn save_soft_off_intent<F>(flash: &mut F, range: Range<u32>, intent: bool)
where
    F: embedded_storage_async::nor_flash::MultiwriteNorFlash,
{
    save_u32(
        flash,
        range,
        KEY_SOFT_OFF_INTENT,
        if intent { 1 } else { 0 },
        "soft_off_intent",
    )
    .await;
}

/// Read the persisted soft-off intent flag.  Returns `false` on
/// missing / corrupt records — safer to drop into a normal boot
/// than to falsely re-sleep on first-ever boot.
pub async fn load_soft_off_intent<F>(flash: &mut F, range: Range<u32>) -> bool
where
    F: embedded_storage_async::nor_flash::MultiwriteNorFlash,
{
    let mut buf = [0u8; PERSIST_BUF_LEN];
    let mut cache = NoCache::new();
    match map::fetch_item::<u8, u32, _>(
        flash,
        range,
        &mut cache,
        &mut buf,
        &KEY_SOFT_OFF_INTENT,
    )
    .await
    {
        Ok(Some(v)) => v == 1,
        Ok(None) => false,
        Err(e) => {
            defmt::warn!(
                "persist: load soft_off_intent failed: {:?}",
                defmt::Debug2Format(&e)
            );
            false
        }
    }
}
