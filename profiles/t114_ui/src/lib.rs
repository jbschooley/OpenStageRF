// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

//! Milestone 6 UI smoke test, T114 deployment.  Shared between
//! the [`ui_tx`](../bin/ui_tx.rs), [`ui_rx`](../bin/ui_rx.rs),
//! and [`ui_bench_tx`](../bin/ui_bench_tx.rs) binaries — each
//! picks a [`Role`] / [`TxSource`] and calls [`run`].
//!
//! Brings up:
//!   - ST7789 TFT (board crate's hand-rolled driver)
//!   - 5-way joystick on `joystick` pins per the board crate
//!     (Up=P1_14, Right=P1_12, Left=P0_07, Down=P0_08, Center=P0_13).
//!     Wired active-low: COMMON to GND, each direction terminal to
//!     its GPIO pin, internal pull-ups holding idle HIGH.
//!   - Edge-wake joystick driver from `osrf-driver-input-joystick5way`.
//!   - `osrf-ui` state machine + renderer.
//!
//! # Task topology
//!
//! As of M6's task split, the work is decomposed across four async
//! tasks plus the SoftDevice run task:
//!
//! | Task            | Executor            | Priority | Responsibilities                                |
//! |-----------------|---------------------|----------|--------------------------------------------------|
//! | `softdevice run`| thread (main)       | (SD)     | SD event dispatch                               |
//! | `joystick`      | thread (main)       | low      | Edge-wake joystick → [`EVENT_CHAN`]              |
//! | `ui_render`     | thread (main)       | low      | Renderer + framebuffer flush, awaits [`FRAME`]   |
//! | **main** task   | thread (main)       | low      | UI state machine; produces frames + scan/cfg signals |
//! | `link_runtime`  | interrupt executor  | **P2**   | Radio TX/RX, scan, live config                  |
//!
//! The interrupt executor is bound to `SWI0_EGU0` at priority P2 so
//! `link_runtime` preempts everything else app-side when a radio IRQ
//! lands.  SD's own P0/P1 interrupts preempt all of the above (we
//! don't fight that).  See [the task split rationale in PLAN.md] for
//! why we did this even though packet loss already measured 0 % under
//! the joined-task design — short version: groundwork for future
//! audio + dual-core + concurrent BLE.
//!
//! [the task split rationale in PLAN.md]: ../../../PLAN.md

use embassy_executor::{InterruptExecutor, Spawner};
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use osrf_app_link_bench::synthetic::ScenarioSource;
use osrf_app_midi_node::{run_rx, run_tx, LinkConfig, UartMidiSink, UartMidiSource};
use osrf_board_t114 as board;
use osrf_driver_input_joystick5way::{Joystick5Way, JoystickEvent};
use nrf_softdevice::Flash;
use osrf_link_runtime::{LinkConfigSignal, LinkStatsCell, ScanController, ShutdownSignal};
use osrf_ui::{
    band_plan_channel, band_plan_index, build_screen, max_channel_index, AboutData, BandPlan,
    BatteryChemistry, BatteryStatus, Command, KeyStore, LinkStatus, PowerPolicy, Renderer, Role,
    ScanState, ScreenId, Settings, UiState, Widget, WidgetList, BAND_PLANS, MAX_SCAN_CHANNELS,
    WIRED_USB_LOSS_GRACE_SECS,
};

/// Battery chemistry for this build.  Stock T114 ships with a
/// single-cell LiPo pouch; swap to `BatteryChemistry::NimhPack
/// { cells: 3 }` if you've replaced the cell with a 3-AA NiMH
/// holder (requires de-popping the TP4054 charger to avoid
/// over-charging the pack on USB-plug).  See `PLAN.md` M8 → battery
/// chemistry bullet and the `core/ui/battery.rs` module docs.
const CHEMISTRY: BatteryChemistry = BatteryChemistry::LiPoSingle;

/// Power policy for this build.  Default is
/// [`PowerPolicy::Battery`]: user controls on/off via long-press
/// gesture, USB plug-in shows a brief charging frame.  Switch to
/// [`PowerPolicy::Wired`] when the device is permanently mounted on
/// a host instrument (keytar / keyboard USB port) — the chip then
/// tracks the host's USB power and auto-soft-offs ~10 s after USB
/// is lost.  See `docs/hardware_guides/battery_options.md` and the
/// `PowerPolicy` enum docs in `core/ui`.
const POWER_POLICY: PowerPolicy = PowerPolicy::Wired;
use sequential_storage::cache::NoCache;
use sequential_storage::map;

use board::embassy_nrf::gpio::{Input, Output, Pull};
use board::embassy_nrf::interrupt::{self, InterruptExt, Priority};
use board::embassy_nrf::wdt::{Config as WdtConfig, Watchdog, WatchdogHandle};
use board::framebuffer::Framebuffer;

/// Joystick pin types per `boards/t114/src/lib.rs::joystick`.
type Joystick = Joystick5Way<
    Input<'static>,
    Input<'static>,
    Input<'static>,
    Input<'static>,
    Input<'static>,
>;

// ── Shared statics ──────────────────────────────────────────────

/// Input event channel — joystick task pushes, ui_state (main task) pops.
static EVENT_CHAN: Channel<CriticalSectionRawMutex, JoystickEvent, 8> = Channel::new();

/// Cross-task shared link-runtime stats.  `run_rx` / `run_tx` write
/// counters + RSSI + link-up here on every loop iteration; the ui_state
/// task snapshots the latest values each frame build and translates
/// them into a `LinkStatus` for the Idle / Link Stats screens.
static STATS: LinkStatsCell = LinkStatsCell::new();

/// Live config-update channel from ui_state → link_runtime.  When the
/// user applies a new channel / band plan / TX power, ui_state rebuilds
/// a `LinkConfig` and signals here; the runtime's `select` arm wakes,
/// re-runs `configure_radio`, and resumes (re-`rx_start` for RX).
/// Latest-wins, so two rapid changes collapse to the most recent.
static CONFIG_UPDATES: LinkConfigSignal = LinkConfigSignal::new();

/// Channel-scan handoff between ui_state and link_runtime.  When the
/// user enters the Scan screen, ui_state calls `start()` with the
/// current band plan's frequency list; the runtime walks the list,
/// sampling RSSI per channel.  Each render tick the ui_state task
/// snapshots the results into `state.scan` via `apply_scan_pass`.
static SCAN: ScanController = ScanController::new();

/// Low-battery shutdown coordinator (link-runtime side).  `battery_task`
/// fires this after observing `SHUTDOWN_BATTERY_SUSTAINED_SAMPLES`
/// consecutive sub-`SHUTDOWN_MV` readings.  The link-runtime task
/// (`run_rx` or `run_tx`) `select`s on it, sinks all-notes-off,
/// parks the radio, and idles forever.  Single-consumer —
/// `embassy_sync::Signal` takes-on-wait, so a separate
/// [`POWEROFF_REASON`] latch is needed for the ui_state_loop side.
static SHUTDOWN: ShutdownSignal = ShutdownSignal::new();

/// Latched soft-off reason.  Set alongside [`SHUTDOWN`] by either
/// `battery_task` (sustained low-battery) or the UI's
/// [`Command::PowerOff`] handler (long-press Center → confirm).
/// Polled by `ui_state_loop`'s loop tick, which dispatches to
/// `enter_soft_off()` on any non-zero value.
///
/// Separate from `SHUTDOWN` because `Signal::wait` consumes the
/// value (single-consumer) and we want both the link runtime and
/// the UI to observe the same event.  Polling at the 300 ms
/// scan-tick cadence is fine — the soft-off budget is "user sees
/// goodbye, peripherals quiesce, chip enters System OFF," all on
/// the order of seconds.
///
/// Values: see [`PowerOffReason`] constants below.  `u8` instead
/// of a real enum so the static is `AtomicU8` — `core` has no
/// `AtomicEnum`.  Never cleared once set.
static POWEROFF_REASON: core::sync::atomic::AtomicU8 =
    core::sync::atomic::AtomicU8::new(POWEROFF_REASON_NONE);

/// Atomic-friendly encoding of the soft-off reason latched in
/// [`POWEROFF_REASON`].  `None` is the default zero so a fresh boot
/// is implicitly "no soft-off requested."
const POWEROFF_REASON_NONE: u8 = 0;
/// Operator chose `Settings → Power off → Confirm` (long-press
/// Center from Idle followed by Center on the confirm screen).
/// Normal-user-flow soft-off; not logged to the panic ring.
const POWEROFF_REASON_OPERATOR: u8 = 1;
/// `battery_task` saw [`SHUTDOWN_BATTERY_SUSTAINED_SAMPLES`]
/// consecutive sub-`SHUTDOWN_MV` readings with USB unplugged.
/// `enter_soft_off()` pushes a `low-battery shutdown` panic-ring
/// record before the System OFF call so next boot's About screen
/// surfaces the cause.
const POWEROFF_REASON_LOW_BATTERY: u8 = 2;
/// [`PowerPolicy::Wired`] mode + USB power has been absent for
/// [`WIRED_USB_LOSS_GRACE_SECS`] seconds, with no sign of recovery.
/// `enter_soft_off()` renders a "USB disconnected" goodbye and
/// drops the chip into real System OFF; the next USB plug-in or
/// Center press cold-boots back to Idle (the Wired policy will
/// then either keep us on if USB is back, or re-start the grace
/// timer if not).  No panic-ring record — this is normal-flow.
const POWEROFF_REASON_WIRED_USB_LOST: u8 = 3;

/// "Power off the display" handshake from `ui_state_loop` to
/// `ui_render_task`.  The render task owns the display and we need
/// `display.power_off()` (DISPOFF + SLPIN + VTFT high) to run before
/// the chip enters System OFF — otherwise the panel sits in normal
/// mode with VDD on through sleep, defeating the soft-off current
/// target.  Fired exactly once per deep-soft-off entry; after
/// handling it, the render task drops into a WDT-pet idle loop.
static POWER_OFF_DISPLAY: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Number of consecutive sub-`SHUTDOWN_MV` battery samples required
/// before firing [`SHUTDOWN`].  With a 5 s sample interval this is
/// `5 * 5 = 25 s` of sustained-low — long enough that a transient
/// dip (TX-burst sag, plug-event transient) doesn't trip shutdown,
/// short enough that a genuinely dying cell still has run-time left
/// to do the all-notes-off + radio-park dance.
const SHUTDOWN_BATTERY_SUSTAINED_SAMPLES: u32 = 5;

/// Latest battery reading.  Written by [`battery_task`] every
/// [`BATTERY_SAMPLE_INTERVAL_S`] seconds and on every USB-plug
/// event, read by `ui_state_loop` each frame build to populate the
/// top-bar indicator.  `Cell<BatteryStatus>` is fine here:
/// `BatteryStatus` is `Copy` and updates are infrequent.
static BATTERY: critical_section::Mutex<core::cell::Cell<BatteryStatus>> =
    critical_section::Mutex::new(core::cell::Cell::new(BatteryStatus::UNKNOWN));

/// Sample-rate for battery monitoring.  Meshtastic uses 5 s; we
/// match.  Faster polling buys nothing (LiPo Vbat moves slowly
/// relative to a 5 s window) and burns extra current through the
/// divider.
const BATTERY_SAMPLE_INTERVAL_S: u64 = 5;

/// Hardware-watchdog timeout in 32 768 Hz ticks (5 seconds).  Each
/// monitored task owns one [`WatchdogHandle`] and must call
/// [`WatchdogHandle::pet`] at least this often, or the WDT triggers
/// a chip reset.  5 s is comfortable for our task cadences
/// (`ui_state_loop` pets every ~300 ms, `ui_render_task` pets after
/// each frame or a 2 s no-frame timer) while giving meaningful hang
/// detection — a stuck task is caught within 5 s, on the next boot
/// `RESETREAS` will have the DOG bit set so the diagnostic surfaces.
///
/// Link-runtime is *not* watchdog-monitored in this version.  The
/// link layer has its own 200 ms link-loss watchdog; a HW-WDT slot
/// for `run_rx` / `run_tx` would couple the runtime to embassy-nrf
/// and we'd rather keep `core/link_runtime` HAL-agnostic.  If the
/// runtime ever hangs the link goes silent + the RX-side watchdog
/// fires all-notes-off, which is the right operational signal.
const WDT_TIMEOUT_TICKS: u32 = 5 * 32_768;
/// How often `ui_render_task` falls through its FRAME wait to pet
/// the WDT when the display is off (no frames signalled).  Must be
/// comfortably less than `WDT_TIMEOUT_TICKS` worth of seconds.
const WDT_RENDER_IDLE_PET_S: u64 = 2;

/// "Render this frame" handoff from ui_state → ui_render.  ui_state
/// builds a fresh [`WidgetList`] from the UI state machine, snapshots
/// the [`ScanState`] alongside it, and signals.  ui_render awaits
/// here; latest-wins, so a render in flight when a newer frame
/// signals means the older frame is dropped (which is what we want —
/// always show the most recent state).
static FRAME: Signal<CriticalSectionRawMutex, FrameData> = Signal::new();

/// Snapshot of UI state needed for one render.  ScanState is copied
/// (not referenced) because the ui_render task runs on a different
/// executor and can't hold a reference into ui_state's locals.
#[derive(Clone)]
struct FrameData {
    widgets: WidgetList,
    scan: ScanState,
}

/// 64 KB in-RAM framebuffer the renderer paints into (sync via
/// `DrawTarget`).  ui_render owns it after the initial paint; it lives
/// in BSS because `Framebuffer::new()` is `const fn`, avoiding a
/// stack-allocated 64 KB which would blow embassy's task pool.
static mut FRAMEBUFFER: Framebuffer = Framebuffer::new();

/// Interrupt-driven executor running the `link_runtime` task at the
/// highest app-allowed priority (P2 — P0/P1/P4 are reserved by the
/// SoftDevice).  Bound to `SWI0_EGU0`; the radio's GPIOTE / SPIM
/// interrupts wake the task through their wakers regardless of
/// executor, but with `link_runtime` here the actual work runs at
/// P2 instead of cooperating with UI rendering on the main task.
static EXECUTOR_LINK: InterruptExecutor = InterruptExecutor::new();

#[cortex_m_rt::interrupt]
#[allow(non_snake_case)]
unsafe fn EGU0_SWI0() {
    EXECUTOR_LINK.on_interrupt()
}

// ── Public API ──────────────────────────────────────────────────

/// Which `MidiSource` flavour the TX-role build should drive the
/// runtime with.  `Uart` reads real DIN MIDI from the FeatherWing
/// UART (production path).  `Scenario` runs the synthetic burst-
/// pattern source from `osrf-app-link-bench` — used by the
/// `ui_bench_tx` binary to stress-test the link with the UI active.
/// Ignored for `Role::Rx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxSource {
    Uart,
    Scenario,
}

/// Bring up the board, spawn all tasks, run the UI state machine
/// forever.  Called from each binary's `#[embassy_executor::main]`.
pub async fn run(spawner: Spawner, role: Role, tx_source: TxSource) -> ! {
    // Clear DEMCR (Debug Exception and Monitor Control Register).
    // `probe-rs run` defaults to setting `VC_HARDERR` and
    // `VC_CORERESET` so a debugger can break on crashes; those
    // bits persist across SYSRESETREQ (ARM debug subsystem isn't
    // reset by it).  With no debugger attached after STLink is
    // unplugged, any HardFault halts the core forever instead of
    // running its handler — looks like a chip-level freeze, only
    // WDT reset or NRESET escapes it.  Production boots never see
    // this (no probe → no DEMCR bits → no catch), but during dev
    // we have to scrub them ourselves.  Cheap (single mmio write)
    // and harmless if the bits weren't set.
    const DEMCR: *mut u32 = 0xE000_EDFC as *mut u32;
    unsafe { core::ptr::write_volatile(DEMCR, 0) };

    // Note on emulated System OFF after `cargo run`:
    //
    // The probe leaves the chip's AHB-AP-level `DBGEN` signal
    // asserted, which SD reads at every `sd_power_system_off` call.
    // When set, SD refuses real System OFF and "emulates" it by
    // parking in WFE; any interrupt wakes the CPU and SVC returns
    // NRF_ERROR_SOC_POWER_OFF_SHOULD_NOT_RETURN (0x2006).  We
    // tried clearing `DHCSR.C_DEBUGEN` here — it didn't propagate
    // to DBGEN.  The signal is reset only by NRESET / POR, so the
    // dev workflow is "power-cycle once after each flash" before
    // testing the soft-off → wake flow.  Memorialised in
    // `t114_emulated_system_off.md`.

    defmt::info!(
        "ui (T114, {:?}): bringing up SD + display + joystick + link",
        role
    );

    // Order: `embassy_nrf::init()` (inside `board::resources()`)
    // **must** come before `Softdevice::enable()` — SD claims CLOCK +
    // POWER on activation; embassy can no longer configure those
    // afterwards.
    let r = board::resources();

    // Identify the RAM-side wake signal *before* SD enable — both
    // sub-signals (Center pin level + LATCH bit 13) are direct
    // register reads and SD-safe.  Doing it pre-SD keeps the
    // latency between reset-vector entry and the Center read low,
    // which matters for the race against the user releasing their
    // press.  The destructive `wakeflag::take` is done here so
    // subsequent boots aren't tricked by a stale magic.
    let early_wake = board::power::detect_wake_source();
    defmt::info!("ui: early_wake = {:?}", early_wake);

    let sd = board::softdevice::enable();
    spawner.spawn(board::softdevice::run(sd).expect("alloc softdevice run task"));

    // Brief settling delay so the SD's first event-loop tick has
    // happened before we take Flash.  Cheap insurance against
    // taking Flash mid-SD-startup.
    Timer::after_millis(10).await;
    let mut flash = board::storage::flash(sd);

    // Dispatch the boot path based on three signals:
    //
    //   - `early_wake`: live-Center poll caught a press? Or RAM-side
    //     wakeflag said this was an intentional soft-off?
    //   - `flash_intent`: flash-backed mirror of "we were soft-off
    //     when the prior reset happened."  Survives brown-out /
    //     battery-pull, unlike RAM wakeflag.
    //   - `vbus_at_boot`: USB voltage detected on the chip's USB
    //     pins right now.
    //
    // Decision matrix:
    //
    //   | early_wake   | flash_intent | VBUS | path           |
    //   |--------------|--------------|------|----------------|
    //   | CenterPress  | any          | any  | Idle           |
    //   | UsbPlug      | any          | yes  | charging frame |
    //   | UsbPlug      | any          | no   | silent re-sleep|
    //   | ColdBoot     | yes          | yes  | charging frame |
    //   | ColdBoot     | yes          | no   | silent re-sleep|
    //   | ColdBoot     | no           | any  | Idle           |
    //
    // **Silent re-sleep** covers the cases the user explicitly
    // doesn't want to wake on: USB-shield ESD touches (intentional
    // flag set + no sustained VBUS), and battery-pull-after-soft-
    // off (same signature).  See `unexpected_wake_resleep` docs.
    //
    // **Charging frame** is reserved for confirmed USB plug-in
    // (VBUS present + we know we were soft-off).
    //
    // **Idle** for a genuine cold boot (no flash_intent — usually
    // the very first boot after a firmware install, or a power-on
    // after the user reached Idle and then power-cycled cleanly).
    let flash_intent = load_soft_off_intent(&mut flash).await;
    let vbus_at_boot = board::battery::vbus_present();
    // Clear the flash flag now.  The two re-sleep paths
    // (`enter_soft_off`, `usb_wake_charging_frame`,
    // `unexpected_wake_resleep`) re-set it before their respective
    // `enter_system_off` calls.  Clearing here handles the
    // "Center press → Idle" path: their next true cold-power-on
    // boot won't be misread as USB-wake.
    save_soft_off_intent(&mut flash, false).await;

    // Wired mode is its own short-circuit: device should be on
    // whenever USB is present.  Skip the charging-frame and silent-
    // re-sleep paths entirely and boot straight to Idle.  The
    // 10-second USB-loss grace timer in `ui_state_loop` enforces
    // the rest of the policy (auto-soft-off when USB has been gone
    // for too long).  This means in Wired mode every wake reason
    // produces the same user-facing behaviour — VBUS-present at
    // boot → Idle, VBUS-absent at boot → Idle (and the grace timer
    // shuts us back down 10 s later if USB doesn't return).
    if matches!(POWER_POLICY, PowerPolicy::Wired) {
        defmt::info!(
            "ui: Wired policy → Idle (VBUS={}, early_wake={:?})",
            vbus_at_boot,
            early_wake
        );
    } else {
        match early_wake {
            // Live Center press caught — full boot to Idle.
            board::power::WakeSource::CenterPress => {
                defmt::info!("ui: wake = CenterPress → Idle");
            }
            // Intentional wake with VBUS — confirmed USB plug-in
            // alongside the prior soft-off.  Brief charging frame
            // then real System OFF.
            board::power::WakeSource::UsbPlug if vbus_at_boot => {
                defmt::info!("ui: wake = UsbPlug + VBUS → charging frame");
                usb_wake_charging_frame(r.display, r.display_backlight, r.battery, flash)
                    .await;
            }
            // Intentional wake without VBUS — most likely a SENSE
            // event on Center triggered by ESD or a quick press we
            // missed during the live poll.  Silent re-sleep.
            board::power::WakeSource::UsbPlug => {
                unexpected_wake_resleep(flash).await;
            }
            // Cold boot + flash_intent + VBUS — brown-out from a USB
            // plug-in event.  RAM wiped but the cable is plugged in;
            // show charging frame.
            board::power::WakeSource::ColdBoot if flash_intent && vbus_at_boot => {
                defmt::info!(
                    "ui: wake = ColdBoot + flash_intent + VBUS → charging frame \
                     (probable brown-out from USB plug-in)"
                );
                usb_wake_charging_frame(r.display, r.display_backlight, r.battery, flash)
                    .await;
            }
            // Cold boot + flash_intent + no VBUS — most likely a USB-
            // shield ESD event or a battery-pull while soft-off was
            // active.  Silent re-sleep — neither warrants user-
            // visible wake activity.
            board::power::WakeSource::ColdBoot if flash_intent => {
                unexpected_wake_resleep(flash).await;
            }
            // Genuinely cold boot — fresh power-on without a prior
            // soft-off in the flash flag.  Boot to Idle.
            board::power::WakeSource::ColdBoot => {
                defmt::info!("ui: wake = ColdBoot (no flash_intent) → Idle");
            }
        }
    }

    // Recover any panic staged by the prior boot (if any).  Reads
    // and clears RESETREAS, takes the staged record from .uninit,
    // logs + persists to the panic-ring flash region.  Idempotent:
    // a clean cold boot here is a no-op.
    recover_pending_panic(&mut flash).await;

    // Read the most-recent panic from the ring for the About screen.
    // Only happens once at boot — the value is then constant for the
    // session.  Empty `String` if the ring has no entries.
    let mut last_panic_msg = osrf_panic_log::read_latest(
        &mut flash,
        board::storage::PANIC_RING_RANGE,
    )
    .await;

    // ── Display init + initial paint (synchronous, before split) ──
    //
    // The initial frame is rendered + flushed before we hand the
    // display off to `ui_render`, then backlight goes on.  Order
    // matters: rendering first means the user never sees the panel's
    // power-on RAM state.
    let mut display = r.display;
    let mut backlight = r.display_backlight;
    display.init().await;
    // SAFETY: this is the one and only place we borrow FRAMEBUFFER.
    let fb: &'static mut Framebuffer = unsafe { &mut *core::ptr::addr_of_mut!(FRAMEBUFFER) };

    let mut state = UiState::with_role(role);
    let mut settings = Settings::default();
    // Restore persisted setting fields (channel, band plan, tx power,
    // active key fp).  Fields never written stay at `Default`.
    load_settings(&mut flash, &mut settings).await;
    let mut keys = KeyStore::new();
    let _ = keys.add("Studio A", 0x111111);
    let _ = keys.add("Backup", 0x222222);
    let mut widgets: WidgetList = WidgetList::new();
    let mut renderer = Renderer::new();

    // Random per-boot 16-bit ID — surfaces on About as `Session
    // 0xNNNN` for both roles, and gets reused as the TX-side
    // `boot_counter` below.  Generated here so the initial frame
    // can render it.
    let session_id = read_random_u16();
    defmt::info!("session_id = 0x{=u16:04X} (random per-boot)", session_id);

    let initial_status = link_status_from_stats(&STATS.get());
    let initial_about = about_data(session_id, &last_panic_msg);
    build_screen(
        &mut state,
        &settings,
        &keys,
        &initial_status,
        &initial_about,
        &mut widgets,
    );
    // Include the (still-unknown) battery indicator on the initial
    // paint so the title bar's right side is properly themed from
    // frame zero rather than briefly showing whatever was beneath.
    let initial_battery = critical_section::with(|cs| BATTERY.borrow(cs).get());
    let _ = widgets.push(Widget::BatteryIndicator {
        voltage_mv: initial_battery.voltage_mv,
        percent: initial_battery.percent,
        plugged_in: initial_battery.plugged_in,
    });
    let _ = renderer.render(&widgets, &state.scan, fb);
    display.flush(fb).await;
    backlight.set_low(); // backlight on (active LOW)
    defmt::info!("ui ready: role={:?} screen={:?}", role, state.screen);

    // ── Joystick spawn ─────────────────────────────────────────────
    let pins = unsafe {
        use board::embassy_nrf::peripherals::*;
        (
            P1_14::steal(), // Up
            P0_08::steal(), // Down
            P0_07::steal(), // Left
            P1_12::steal(), // Right
            P0_13::steal(), // Center
        )
    };
    let js_up = Input::new(pins.0, Pull::Up);
    let js_dn = Input::new(pins.1, Pull::Up);
    let js_lt = Input::new(pins.2, Pull::Up);
    let js_rt = Input::new(pins.3, Pull::Up);
    let js_ct = Input::new(pins.4, Pull::Up);
    let joystick = Joystick5Way::new(js_up, js_dn, js_lt, js_rt, js_ct);
    spawner.spawn(joystick_task(joystick).expect("alloc joystick_task"));

    // ── Battery monitor — periodic SAADC sampler ──────────────────
    spawner.spawn(battery_task(r.battery).expect("alloc battery_task"));

    // ── Hardware watchdog — arm with 2 slots (main + ui_render) ───
    // Done late in boot so the slow startup steps (display rail
    // warmup, SD enable, initial flash reads) don't trip the WDT
    // before any task is alive to pet it.  Once armed, can't be
    // disarmed — any reset path resets the chip.
    let mut wdt_config = WdtConfig::default();
    wdt_config.timeout_ticks = WDT_TIMEOUT_TICKS;
    let (_wdt, [wdt_main, wdt_render]) =
        Watchdog::try_new(r.wdt, wdt_config).expect("WDT already configured differently");

    // ── ui_render task — owns display + fb + renderer + WDT handle ─
    spawner.spawn(
        ui_render_task(display, fb, renderer, wdt_render).expect("alloc ui_render_task"),
    );

    // ── link_runtime on its own interrupt executor at P2 ──────────
    //
    // The build initial `LinkConfig` from the UI's `Settings`.  Live
    // updates flow through `CONFIG_UPDATES` after this.
    let config = link_config_from(&settings);

    let irq = interrupt::EGU0_SWI0;
    irq.set_priority(Priority::P2);
    let spawner_link = EXECUTOR_LINK.start(irq);

    match role {
        Role::Rx => {
            let sink = UartMidiSink::new(r.midi_uart);
            spawner_link.spawn(
                link_rx_task(r.radio0, r.status_led, sink, config)
                    .expect("alloc link_rx_task"),
            );
        }
        Role::Tx => {
            let boot_counter = session_id;
            match tx_source {
                TxSource::Uart => {
                    let source = UartMidiSource::new(r.midi_uart);
                    spawner_link.spawn(
                        link_tx_uart_task(
                            r.radio0,
                            r.status_led,
                            source,
                            boot_counter,
                            config,
                        )
                        .expect("alloc link_tx_uart_task"),
                    );
                }
                TxSource::Scenario => {
                    defmt::info!("ui_bench_tx: synthetic scenario source running");
                    spawner_link.spawn(
                        link_tx_scenario_task(
                            r.radio0,
                            r.status_led,
                            boot_counter,
                            config,
                        )
                        .expect("alloc link_tx_scenario_task"),
                    );
                }
            }
        }
    }

    // Drop neopixel — it was parked Low by `board::resources()` and
    // we don't drive it from any task in this profile.  Leaking it
    // keeps the pin held; dropping it would float.
    core::mem::forget(r.neopixel_parked);

    // ── Main task body = UI state loop ─────────────────────────────
    ui_state_loop(
        &mut backlight,
        &mut flash,
        wdt_main,
        &mut state,
        settings,
        keys,
        &mut widgets,
        session_id,
        &mut last_panic_msg,
    )
    .await
}

/// Construct an [`AboutData`] borrowing the long-lived
/// `last_panic_msg` buffer that lives on `run()`'s stack.  Empty
/// panic message → `None`.  `firmware_version` baked at compile
/// time from this crate's Cargo.toml.
fn about_data<'a>(session_id: u16, last_panic_msg: &'a heapless::String<96>) -> AboutData<'a> {
    AboutData {
        firmware_version: concat!("v", env!("CARGO_PKG_VERSION")),
        git_hash: board::GIT_HASH,
        session_id,
        last_panic: if last_panic_msg.is_empty() {
            None
        } else {
            Some(last_panic_msg.as_str())
        },
    }
}

// ── Tasks ───────────────────────────────────────────────────────

/// Renderer task — awaits a fresh [`FrameData`] on [`FRAME`], paints
/// it into the framebuffer, and flushes the dirty region to the panel
/// via async SPI.  Owns the display, framebuffer, renderer, and one
/// hardware-watchdog handle for the lifetime of the program; nothing
/// else writes to them.
///
/// WDT handling: pet after every render *or* every
/// [`WDT_RENDER_IDLE_PET_S`] seconds of no-frame idle.  The latter
/// matters when the panel is off (auto-off or pre-soft-on-boot):
/// `ui_state` stops signalling frames, so without the timer fallback
/// the slot would go stale and the chip would reset every 5 s.
///
/// Deep soft-off path: a [`POWER_OFF_DISPLAY`] signal from
/// `ui_state_loop` triggers `display.power_off()` (DISPOFF + SLPIN +
/// VTFT gate high) so the panel is in its lowest-power state before
/// the chip enters System OFF.  After that the task transitions to a
/// pet-only loop — frames stop coming, the display is dead, and we
/// just keep `wdt_render` fed until `sd_power_system_off` halts
/// everything.
#[embassy_executor::task]
async fn ui_render_task(
    mut display: board::Display,
    fb: &'static mut Framebuffer,
    mut renderer: Renderer,
    mut wdt: WatchdogHandle,
) -> ! {
    use embassy_futures::select::{select3, Either3};
    loop {
        match select3(
            FRAME.wait(),
            Timer::after_secs(WDT_RENDER_IDLE_PET_S),
            POWER_OFF_DISPLAY.wait(),
        )
        .await
        {
            Either3::First(frame) => {
                let _ = renderer.render(&frame.widgets, &frame.scan, fb);
                display.flush(fb).await;
            }
            Either3::Second(()) => {
                // No frame arrived in the idle-pet window — display
                // is off or ui_state is quiet.  Nothing to render;
                // just fall through to pet the WDT and re-wait.
            }
            Either3::Third(()) => {
                // Deep soft-off: put the panel to sleep + gate VDD,
                // then spin pet-only until the chip enters System OFF.
                // We don't `return` because the task has a WDT slot
                // (`wdt_render`); leaving the task would stop pets
                // and the chip would reset every 5 s, leaving the
                // user staring at a never-quite-off-but-also-doesn't-
                // boot brick.
                display.power_off().await;
                defmt::info!("ui_render: display powered off — entering pet-only idle");
                loop {
                    wdt.pet();
                    Timer::after_secs(WDT_RENDER_IDLE_PET_S).await;
                }
            }
        }
        wdt.pet();
    }
}

/// Link-runtime task, RX role.  Owns the radio + status LED + the
/// `UartMidiSink` driving the FeatherWing DIN MIDI OUT.  Runs on the
/// high-priority interrupt executor so radio IRQ → packet handling
/// preempts UI rendering / state work on the main task.
#[embassy_executor::task]
async fn link_rx_task(
    mut radio0: board::Radio0,
    mut status_led: Output<'static>,
    mut sink: UartMidiSink<board::MidiUart>,
    config: LinkConfig,
) -> ! {
    run_rx(
        &mut radio0,
        &mut status_led,
        &mut sink,
        &config,
        &STATS,
        Some(&CONFIG_UPDATES),
        Some(&SCAN),
        Some(&SHUTDOWN),
    )
    .await
}

/// Link-runtime task, TX role with real UART MIDI source.  Same
/// priority story as [`link_rx_task`].
#[embassy_executor::task]
async fn link_tx_uart_task(
    mut radio0: board::Radio0,
    mut status_led: Output<'static>,
    mut source: UartMidiSource<board::MidiUart>,
    boot_counter: u16,
    config: LinkConfig,
) -> ! {
    run_tx(
        &mut radio0,
        &mut status_led,
        &mut source,
        boot_counter,
        &config,
        &STATS,
        Some(&CONFIG_UPDATES),
        Some(&SCAN),
        Some(&SHUTDOWN),
    )
    .await
}

/// Link-runtime task, TX role with the synthetic burst-pattern source.
/// Same priority story as [`link_rx_task`]; the FeatherWing UART
/// (`r.midi_uart`) is dropped when `run()` returns ownership of
/// `r.midi_uart` to this profile and never used.
#[embassy_executor::task]
async fn link_tx_scenario_task(
    mut radio0: board::Radio0,
    mut status_led: Output<'static>,
    boot_counter: u16,
    config: LinkConfig,
) -> ! {
    let mut source = ScenarioSource::new();
    run_tx(
        &mut radio0,
        &mut status_led,
        &mut source,
        boot_counter,
        &config,
        &STATS,
        Some(&CONFIG_UPDATES),
        Some(&SCAN),
        Some(&SHUTDOWN),
    )
    .await
}

#[embassy_executor::task]
async fn joystick_task(mut js: Joystick) {
    loop {
        let ev = js.next_event().await;
        EVENT_CHAN.send(ev).await;
    }
}

/// Periodic battery sampler.  Wakes every
/// [`BATTERY_SAMPLE_INTERVAL_S`] seconds, reads Vbat via the
/// SAADC + divider, polls VBUS presence via
/// [`board::battery::vbus_present`], writes to the shared
/// [`BATTERY`] cell.
///
/// Low / critical warning logs only fire when an actual battery is
/// detected (Vbat ≥ `NO_BATTERY_MV`) — a probe-only board with no
/// cell wired in stays quiet rather than spamming the log with
/// "critical: 0 mV" messages.
#[embassy_executor::task]
async fn battery_task(mut monitor: board::battery::BatteryMonitor) {
    // Consecutive samples that satisfied the shutdown condition.
    // A debounce against single-sample transients (TX-burst sag, USB
    // detach glitch).  Reset to 0 on any sample that fails the
    // condition.  Once it reaches `SHUTDOWN_BATTERY_SUSTAINED_SAMPLES`
    // we fire [`SHUTDOWN`] and leave it pinned so we don't re-signal
    // every iteration thereafter.
    let mut shutdown_run: u32 = 0;
    let mut shutdown_fired = false;
    loop {
        let mv = monitor.sample().await;
        let plugged_in = board::battery::vbus_present();
        let status = BatteryStatus::from_reading(mv, plugged_in, CHEMISTRY);
        critical_section::with(|cs| BATTERY.borrow(cs).set(status));
        // Only nag when there's a real battery to nag about.  An
        // unpopulated cell socket reads ~0 mV and would otherwise
        // log "critical" forever.
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
        // Shutdown predicate: real battery present, voltage in the
        // hard-shutdown zone, USB not plugged in (plugged-in means
        // user is charging — `vbus_present()` flips on with as little
        // as ~10 mA into the charger, so any USB connection is
        // grounds to defer shutdown).  Once fired we latch — a
        // single low-voltage spike + recovery shouldn't un-shut us
        // down, because the link tasks have already started parking.
        let shutdown_eligible = !plugged_in
            && mv >= CHEMISTRY.no_battery_mv()
            && mv <= CHEMISTRY.shutdown_mv();
        if shutdown_eligible {
            shutdown_run = shutdown_run.saturating_add(1);
            if !shutdown_fired && shutdown_run >= SHUTDOWN_BATTERY_SUSTAINED_SAMPLES {
                defmt::warn!(
                    "battery shutdown threshold: {=u16} mV sustained for {=u32} samples — signalling SHUTDOWN",
                    mv,
                    shutdown_run
                );
                SHUTDOWN.signal();
                POWEROFF_REASON.store(
                    POWEROFF_REASON_LOW_BATTERY,
                    core::sync::atomic::Ordering::Release,
                );
                shutdown_fired = true;
            }
        } else {
            shutdown_run = 0;
        }
        embassy_time::Timer::after_secs(BATTERY_SAMPLE_INTERVAL_S).await;
    }
}

// ── UI state loop (main task body) ──────────────────────────────

/// UI state machine driver.  Runs as the main task on the thread
/// executor.  Responsibilities:
///   - Receive joystick events from [`EVENT_CHAN`].
///   - Dispatch them through [`UiState::handle_event`].
///   - Push live config updates to [`CONFIG_UPDATES`].
///   - Reconcile [`SCAN`] state with the active screen + band plan.
///   - Implement the inactivity / auto-off policy.
///   - Build a fresh [`FrameData`] each tick or event and hand it to
///     `ui_render` via [`FRAME`].
///
/// ## Inactivity / auto-off
///
/// Per-screen idle policy:
///   - `Idle` → backlight off after [`IDLE_OFF_TIMEOUT`].
///   - `LinkStats` and `Scan` → never auto-off (live readouts).
///   - everything else → return to Idle after [`MENU_TO_IDLE_TIMEOUT`],
///     at which point Idle's 15 s timer can then drop the backlight.
///
/// Wake-from-sleep is "next joystick input is consumed, not
/// dispatched" so the user doesn't accidentally fire a menu action on
/// the wake press.
async fn ui_state_loop(
    backlight: &mut Output<'static>,
    flash: &mut Flash,
    mut wdt: WatchdogHandle,
    state: &mut UiState,
    mut settings: Settings,
    keys: KeyStore,
    widgets: &mut WidgetList,
    session_id: u16,
    last_panic_msg: &mut heapless::String<96>,
) -> ! {
    /// Idle screen → backlight off when this long with no input.
    const IDLE_OFF_TIMEOUT: Duration = Duration::from_secs(15);
    /// Any non-Idle / non-LinkStats / non-Scan screen → go_home when
    /// this long with no input (whereupon `IDLE_OFF_TIMEOUT` runs).
    const MENU_TO_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

    let scan_tick = Duration::from_millis(300);
    let mut last_input = Instant::now();
    let mut display_on = true;

    let mut scan_running = false;
    let mut scan_plan: Option<BandPlan> = None;

    // Wired-mode USB-loss grace timer.  Tracks the most recent
    // moment we observed VBUS-present; if that timestamp falls
    // more than `WIRED_USB_LOSS_GRACE_SECS` seconds behind `now`,
    // the policy says "USB has been gone too long" and we latch a
    // [`POWEROFF_REASON_WIRED_USB_LOST`] soft-off.
    //
    // Initialised to `now` so a Wired-mode boot with USB absent
    // still gets the full grace window before shutting down — gives
    // the user a chance to plug in if they powered on without USB
    // first.  In `PowerPolicy::Battery` builds this variable is
    // updated but never consulted (the const-folded check below
    // short-circuits), so the storage is essentially free.
    let mut last_vbus_present_at = Instant::now();
    const WIRED_GRACE: Duration = Duration::from_secs(WIRED_USB_LOSS_GRACE_SECS);

    loop {
        // Pet the WDT at every loop iteration top.  Cadence is the
        // scan_tick (~300 ms) plus whatever flash-write time an
        // Apply* command added — comfortably under the 5 s timeout.
        // Bursts of joystick events may pet faster than that.
        wdt.pet();

        // Wired-mode VBUS tracking.  Sample once per loop iteration
        // (~300 ms).  USB-state change events on the T114 don't have
        // an interrupt routed (USBDETECTED is consumed by the SD's
        // POWER handler for wake-from-System-OFF), so we poll.
        let vbus = board::battery::vbus_present();
        if vbus {
            last_vbus_present_at = Instant::now();
        }
        if matches!(POWER_POLICY, PowerPolicy::Wired)
            && !vbus
            && Instant::now().duration_since(last_vbus_present_at) >= WIRED_GRACE
        {
            defmt::warn!(
                "ui: Wired mode + USB absent > {} s → latching soft-off",
                WIRED_USB_LOSS_GRACE_SECS,
            );
            POWEROFF_REASON.store(
                POWEROFF_REASON_WIRED_USB_LOST,
                core::sync::atomic::Ordering::Release,
            );
        }

        // Deep soft-off: either `battery_task` latched
        // `POWEROFF_REASON_LOW_BATTERY` (sustained low Vbat), the
        // operator confirmed `Command::PowerOff`, or the Wired-mode
        // USB grace timer above expired.  All reasons run the same
        // teardown — radio to SLEEP, display SLPIN + VDD gate, VEXT
        // off, GPIO SENSE wake on Center, System OFF — and the
        // helper diverges so we never re-enter this loop.
        let reason = POWEROFF_REASON.load(core::sync::atomic::Ordering::Acquire);
        if reason != POWEROFF_REASON_NONE {
            enter_soft_off(
                reason,
                backlight,
                &mut wdt,
                flash,
                widgets,
                state,
                display_on,
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
                    backlight.set_low();
                    display_on = true;
                    defmt::info!("ui: wake (joystick input)");
                } else if let Some(cmd) = state.handle_event(&mut settings, &keys, event) {
                    defmt::info!("ui command: {:?}", cmd);
                    match cmd {
                        Command::ApplyChannel(ch) => {
                            CONFIG_UPDATES.signal(link_config_from(&settings));
                            save_channel(flash, ch).await;
                        }
                        Command::ApplyBandPlan(plan) => {
                            CONFIG_UPDATES.signal(link_config_from(&settings));
                            save_band_plan(flash, plan).await;
                            // Band-plan change resets channel to 0; persist the
                            // new channel too so a reboot in the new plan picks
                            // up where the user landed instead of jumping back
                            // to whatever channel the OLD plan had at index 0.
                            save_channel(flash, settings.channel).await;
                        }
                        Command::ApplyTxPower(dbm) => {
                            CONFIG_UPDATES.signal(link_config_from(&settings));
                            save_tx_power(flash, dbm).await;
                        }
                        Command::ApplySetActiveKey(fp) => {
                            // No live-config update needed (AEAD not wired into
                            // LinkConfig yet), but do persist so a reboot
                            // restores the user's last selection.
                            save_active_key(flash, fp).await;
                        }
                        Command::ForcePanic => {
                            // Diagnostic — fires the production
                            // panic handler so the staging →
                            // sys_reset → recovery → About flow can
                            // be exercised end-to-end without a
                            // rebuild.  Message is distinctive so
                            // it's obvious on About that this came
                            // from the menu rather than a real bug.
                            panic!("forced panic from menu (test)");
                        }
                        Command::ForceWdtHang => {
                            // Diagnostic — busy-spin so wdt_main
                            // stops getting petted at the top of
                            // ui_state_loop.  After WDT_TIMEOUT_TICKS
                            // (5 s) the hardware WDT fires, chip
                            // resets, next boot's
                            // `recover_pending_panic` sees DOG in
                            // RESETREAS without a staged panic and
                            // pushes "watchdog: task hung" to the
                            // panic ring.  The other tasks
                            // (ui_render with its own WDT slot,
                            // joystick, battery, link runtime)
                            // continue running for those ~5 s; only
                            // ui_state_loop hangs.
                            defmt::warn!("ui: forced WDT hang — chip will reset in ~5s");
                            loop {
                                cortex_m::asm::nop();
                            }
                        }
                        Command::PowerOff => {
                            // Operator-initiated deep soft-off.  Latch
                            // the reason and let the next loop-top
                            // poll dispatch into `enter_soft_off`,
                            // which handles the full teardown +
                            // System OFF entry.  Going through the
                            // shared latch (vs. inlining the
                            // teardown here) means low-battery and
                            // operator paths share one code path,
                            // and we honour any pending scan_tick /
                            // frame work cleanly on the way out.
                            defmt::info!("ui: operator power-off confirmed");
                            POWEROFF_REASON.store(
                                POWEROFF_REASON_OPERATOR,
                                core::sync::atomic::Ordering::Release,
                            );
                        }
                        Command::ClearPanicLog => {
                            match osrf_panic_log::clear(
                                flash,
                                board::storage::PANIC_RING_RANGE,
                            )
                            .await
                            {
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
                        backlight.set_high();
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

        // Build a fresh frame and hand it off to `ui_render`.  When
        // the panel is off we just don't signal — the render task
        // sleeps and the panel keeps whatever it had last frame
        // (which doesn't matter because the backlight is off).
        if display_on {
            let status = link_status_from_stats(&STATS.get());
            let about = about_data(session_id, last_panic_msg);
            build_screen(state, &settings, &keys, &status, &about, widgets);
            // Top-bar battery indicator — pushed by the profile (not
            // by `build_screen`) so every screen gets it without each
            // `build_*` having to opt in.  Renderer paints over the
            // right side of the inverted title bar.
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
    }
}

// ── Unexpected-wake silent re-sleep ─────────────────────────────

/// No-UI re-entry into System OFF for wakes that were neither
/// a deliberate Center press nor a confirmed USB plug-in (VBUS
/// sustained at boot).  Covers:
///
///   - USB cable shield-touch causing brown-out / NRESET via ESD.
///   - Battery pull during a soft-off session, then reinsertion.
///   - Spontaneous SENSE-like events on Center from ESD that
///     weren't followed by a deliberate hold.
///
/// User-facing effect: the chip appears to "stay off."  No display
/// activity, no backlight, no charging frame.  A brief joystick-
/// LED flicker is unavoidable while `build_resources` raises VEXT
/// for the ~half-second of CPU work before `enter_system_off`
/// gates it back down.  To actually wake to Idle the operator has
/// to deliberately press Center (the live-poll in
/// `detect_wake_source` debounces 10 ms of stable LOW, well
/// outside ESD-pulse territory).
///
/// **Why no UI**: rendering anything here turns a noise event into
/// a noticeable interruption.  Silent re-sleep keeps shield-touch
/// and battery-pull-after-soft-off feel equivalent to "I didn't
/// wake the device" — predictable, not surprising.
async fn unexpected_wake_resleep(mut flash: Flash) -> ! {
    defmt::info!(
        "ui: unexpected wake (soft-off intent set, no live press, no VBUS) → silent re-sleep"
    );
    save_soft_off_intent(&mut flash, true).await;
    board::power::enter_system_off()
}

// ── USB-plug brief wake ─────────────────────────────────────────

/// Boot-time branch for `WakeSource::UsbPlug`.  Renders a single
/// "Charging" frame using the just-sampled battery state, holds it
/// for ~2 s, then puts the panel back to sleep and re-enters
/// `board::power::enter_system_off()`.  Diverges.
///
/// What's missing vs. a full boot (intentional):
///   - SD's `recover_pending_panic` doesn't run.  A panic that
///     happened in the previous user session was already recorded
///     to the panic ring on the way down; this path doesn't need
///     to log anything new and the About screen the user *will*
///     see (next time they wake with Center) handles it.
///   - No flash setup, no settings restore, no task spawning.  We
///     have ~2 s of work to do and then the chip goes back to
///     sleep — keeping the boot lean is what makes "USB plug → 2 s
///     frame → off" cheap in terms of average current.
///   - No `ui_render_task` / `ui_state_loop` / link runtime.  Frame
///     is built + flushed inline; the joystick stays unread
///     because input has no role on this path.
///
/// The radio is left in whatever state the prior soft-off landed
/// it in — `link_runtime::handle_*_shutdown` puts it in SLEEP
/// (~160 nA) before parking, and the SX1262 retains state across
/// the MCU's System OFF wake (shared 3.3 V rail, no reset pulse
/// from `build_resources`'s `Level::High` NRESET init).
async fn usb_wake_charging_frame(
    mut display: board::Display,
    mut backlight: Output<'static>,
    mut battery_mon: board::battery::BatteryMonitor,
    mut flash: Flash,
) -> ! {
    // Sample once so the frame shows actual mV / %.  Single SAADC
    // round is ~200 µs.
    let mv = battery_mon.sample().await;
    let status = osrf_ui::BatteryStatus::from_reading(mv, true, CHEMISTRY);
    defmt::info!(
        "usb-wake: battery {=u16} mV ({=u8} %)",
        status.voltage_mv,
        status.percent
    );

    display.init().await;

    // Build the frame.  Title + a centred reading + the standard
    // BatteryIndicator widget so the operator's eye lands on the
    // same icon they're used to from the title-bar.
    let mut widgets: WidgetList = WidgetList::new();
    let _ = widgets.push(Widget::Title(short_str::<24>("Charging")));
    let mut mv_text: heapless::String<24> = heapless::String::new();
    use core::fmt::Write as _;
    let _ = write!(&mut mv_text, "{} mV", status.voltage_mv);
    let _ = widgets.push(Widget::Text { row: 2, text: mv_text });
    let mut pct_text: heapless::String<24> = heapless::String::new();
    let _ = write!(&mut pct_text, "{}%", status.percent);
    let _ = widgets.push(Widget::Text { row: 3, text: pct_text });
    let _ = widgets.push(Widget::BatteryIndicator {
        voltage_mv: status.voltage_mv,
        percent: status.percent,
        plugged_in: true,
    });

    let mut renderer = osrf_ui::Renderer::new();
    // SAFETY: same FRAMEBUFFER borrow as the normal-boot path, but
    // we're on the USB-wake branch so the normal path's own borrow
    // never runs.  Single-owner across this boot.
    let fb: &'static mut board::framebuffer::Framebuffer =
        unsafe { &mut *core::ptr::addr_of_mut!(FRAMEBUFFER) };
    let scan = osrf_ui::ScanState::default();
    let _ = renderer.render(&widgets, &scan, fb);
    display.flush(fb).await;
    backlight.set_low(); // active-LOW = on

    // Hold the frame visible.
    Timer::after_secs(2).await;

    // Teardown: same order as `enter_soft_off`, minus the
    // SHUTDOWN-to-link-runtime + POWER_OFF_DISPLAY signals (no
    // tasks running on this path).  Backlight first, then panel
    // VDD gate, then System OFF.
    backlight.set_high();
    display.power_off().await;

    // Re-set the flash-backed intent so the *next* boot (whether
    // via clean SENSE wake, USB plug, or brown-out) is recognised
    // as "we meant to be off."  enter_system_off itself also sets
    // the RAM wakeflag — flash + RAM together cover both clean
    // and brown-out wake paths.
    save_soft_off_intent(&mut flash, true).await;
    defmt::info!("usb-wake: charging frame done — re-entering System OFF");
    board::power::enter_system_off()
}

/// Tiny helper: build a fixed-size `heapless::String` from a
/// `&'static str`.  Copy of the `s()` helper used elsewhere in the
/// renderer, scoped to this module to avoid leaking it through
/// `core/ui`'s public surface.
fn short_str<const N: usize>(literal: &'static str) -> heapless::String<N> {
    let mut out = heapless::String::new();
    let _ = out.push_str(literal);
    out
}

// ── Deep soft-off ───────────────────────────────────────────────

/// Tear the device down to sub-µA System OFF.  Diverges; the only
/// path out is the chip resetting via the SENSE wake on the Center
/// joystick pin (configured by [`board::power::enter_system_off`]).
///
/// Order of operations:
///   1. Light the panel back up if it had auto-slept, so the user
///      sees the goodbye frame.
///   2. Render a reason-specific goodbye (`build_power_off_screen`).
///   3. Hold for ~2 s, petting the WDT through the wait.  Long enough
///      to read; short enough that low-battery doesn't burn what little
///      runtime is left.
///   4. For [`POWEROFF_REASON_LOW_BATTERY`]: push a
///      `low-battery shutdown` record to the panic ring so next boot's
///      About shows the cause.  Operator-initiated soft-off is normal
///      user flow and gets no ring entry — keeping the ring focused on
///      faults / unexpected exits.
///   5. Backlight off (active HIGH disables).
///   6. Signal [`SHUTDOWN`] — the link-runtime task picks it up,
///      runs `all_notes_off()` (RX), parks the radio, then drops
///      the SX1262 to SLEEP (~160 nA).
///   7. Signal [`POWER_OFF_DISPLAY`] — `ui_render_task` runs
///      `display.power_off()` (DISPOFF + SLPIN + VTFT gate high),
///      then idles in WDT-pet mode.
///   8. ~250 ms cooldown so the other tasks land their teardown work
///      before we cut peripheral power.  Petting the local WDT
///      through the wait.
///   9. `board::power::enter_system_off()` — VEXT low, SENSE = Low on
///      P0_13 (Center), `sd_power_system_off` SVC.  Never returns.
async fn enter_soft_off(
    reason: u8,
    backlight: &mut Output<'static>,
    wdt: &mut WatchdogHandle,
    flash: &mut Flash,
    widgets: &mut WidgetList,
    state: &UiState,
    display_on: bool,
) -> ! {
    let reason_label = match reason {
        POWEROFF_REASON_OPERATOR => "operator",
        POWEROFF_REASON_LOW_BATTERY => "low-battery",
        POWEROFF_REASON_WIRED_USB_LOST => "wired-usb-lost",
        _ => "unknown",
    };
    defmt::warn!("ui: deep soft-off ({}) — rendering goodbye", reason_label);

    // 1) Wake the panel if it had auto-slept.
    if !display_on {
        backlight.set_low();
    }

    // 2) Goodbye frame.  Different copy per reason — low-battery
    //    nudges the user toward the charger; operator confirms the
    //    shutdown happened so they know to stop waiting for it.
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

    // 3) Hold the goodbye visible for ~1 s.  Long enough to register
    //    visually, short enough not to feel like the unit is stuck —
    //    the operator already confirmed, they want it to *go off*.
    //    Pets every 250 ms keep the WDT comfortably fed (5 s
    //    timeout).
    for _ in 0..4 {
        wdt.pet();
        Timer::after_millis(250).await;
    }

    // 4) Audit-trail entry for low-battery only.
    if reason == POWEROFF_REASON_LOW_BATTERY {
        osrf_panic_log::push(
            flash,
            board::storage::PANIC_RING_RANGE,
            0,
            b"low-battery shutdown",
        )
        .await;
    }

    // 5) Backlight off — TFT will follow in step 7 when the render
    //    task gates VDD; backlight first means no brief "lit blank
    //    panel" flash during the controller's sleep handshake.
    backlight.set_high();

    // 6) Link runtime teardown.  The link task picks up `SHUTDOWN`,
    //    runs all-notes-off (RX) / radio park / `set_sleep` and
    //    finally idles forever.
    SHUTDOWN.signal();

    // 7) Display teardown.  The render task picks up
    //    `POWER_OFF_DISPLAY`, runs `display.power_off()`, then sits
    //    petting `wdt_render`.
    POWER_OFF_DISPLAY.signal(());

    // 8) Cooldown.  Gives the link + render tasks ~250 ms to land
    //    their teardown — link runtime's blink alone is ~720 ms
    //    but we don't need to wait for the LED light show, only for
    //    the SPI traffic to drain (`set_sleep` is a single 2-byte
    //    SPI command, ~µs).  The display's SLPIN sequence is ~5 ms
    //    plus the VTFT-gate raise.  250 ms is generous and still
    //    well inside the WDT budget.
    wdt.pet();
    Timer::after_millis(250).await;
    wdt.pet();

    // 9) Persist the soft-off intent.  RAM wakeflag (set inside
    //    enter_system_off) handles clean SENSE wakes; this flash
    //    flag handles wakes-via-brown-out where RAM is wiped (USB
    //    plug-in events have been observed to trigger this on the
    //    T114 — TP4054 charging-mode transient and/or shield ESD).
    //    Flash write is ~30 ms; fits comfortably in the WDT budget.
    save_soft_off_intent(flash, true).await;
    wdt.pet();

    // 10) Enter System OFF.  Diverges.
    defmt::info!("ui: entering System OFF — wake on Center press");
    board::power::enter_system_off()
}

/// Push the reason-specific goodbye widgets for [`enter_soft_off`].
/// Kept out of `enter_soft_off` so the same data flow (Title + two
/// Text rows + Footer hint) is easy to read in one place.
fn build_power_off_goodbye(reason: u8, out: &mut WidgetList) {
    use heapless::String;
    let title: String<24> = String::try_from("Powering off").unwrap_or_default();
    let _ = out.push(Widget::Title(title));

    match reason {
        POWEROFF_REASON_LOW_BATTERY => {
            let _ = out.push(Widget::Text {
                row: 2,
                text: String::try_from("Battery low").unwrap_or_default(),
            });
            let _ = out.push(Widget::Text {
                row: 3,
                text: String::try_from("Plug in to charge").unwrap_or_default(),
            });
        }
        POWEROFF_REASON_WIRED_USB_LOST => {
            let _ = out.push(Widget::Text {
                row: 2,
                text: String::try_from("USB disconnected").unwrap_or_default(),
            });
            let _ = out.push(Widget::Text {
                row: 3,
                text: String::try_from("Plug back in to wake").unwrap_or_default(),
            });
        }
        _ => {
            // Operator (or any unforeseen value) — the safe default
            // is "tell the user how to bring it back."
            let _ = out.push(Widget::Text {
                row: 2,
                text: String::try_from("Goodnight").unwrap_or_default(),
            });
            let _ = out.push(Widget::Text {
                row: 3,
                text: String::try_from("Press Center to wake").unwrap_or_default(),
            });
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────

/// Translate `osrf-link-runtime`'s `LinkStats` snapshot into the
/// `osrf-ui` `LinkStatus` shape that the renderer expects.
fn link_status_from_stats(s: &osrf_link_runtime::LinkStats) -> LinkStatus {
    LinkStatus {
        up: s.link_up,
        last_rssi_dbm: s.last_rssi_dbm.map(|r| r.clamp(i8::MIN as i16, i8::MAX as i16) as i8),
        recent_loss_pct: s.recent_loss_pct,
        total_accepted: s.total_accepted,
        stuck_recoveries: s.stuck_recoveries,
    }
}

/// Build a [`LinkConfig`] from the UI's [`Settings`].  Today only
/// `frequency_hz` and `tx_power_dbm` flow through; the rest stays at
/// `default_915()` values.
fn link_config_from(settings: &Settings) -> LinkConfig {
    let mut c = LinkConfig::default_915();
    c.frequency_hz = settings.current_channel().frequency_khz * 1000;
    c.tx_power_dbm = settings.tx_power_dbm;
    c
}

/// Pull two random bytes from SD's RNG and pack into a `u16`.  Used
/// once at boot for the link-layer `boot_counter`.  M7 will replace
/// with a flash-persisted counter.
fn read_random_u16() -> u16 {
    let mut bytes = [0u8; 2];
    let ret = board::softdevice::rand_bytes(&mut bytes);
    if ret != 0 {
        defmt::warn!(
            "sd_rand_application_vector_get returned {=u32}; using fallback boot_counter",
            ret
        );
        return 0;
    }
    u16::from_be_bytes(bytes)
}

/// Build the frequency list (Hz) for a band plan, in channel-index
/// order.  Used to seed [`ScanController::start`] when the user
/// enters the Scan screen.
fn collect_scan_frequencies(plan: BandPlan, out: &mut [u32; MAX_SCAN_CHANNELS]) -> usize {
    let max_idx = max_channel_index(plan);
    let n = (max_idx as usize + 1).min(MAX_SCAN_CHANNELS);
    for i in 0..n {
        out[i] = band_plan_channel(plan, i as u8).frequency_khz * 1000;
    }
    n
}

// ── Settings persistence (M7) ───────────────────────────────────
//
// Each `Settings` field gets its own [`sequential-storage`] key in
// the Settings flash region (defined in `boards/t114/src/storage.rs`).
// Per-field keys mean an Apply* command only rewrites the changed
// field — the others stay where they are.  That keeps wear-leveling
// even across rapid same-field edits (e.g. spinning a channel
// selector through 24 values writes 24 channel records but doesn't
// touch the band-plan / power / key records).
//
// Schema (key → value type):
//   KEY_CHANNEL       → u32     channel index in the active band plan
//   KEY_BAND_PLAN     → u32     index into `BAND_PLANS`
//   KEY_TX_POWER      → i32     dBm, range MIN_TX_POWER_DBM..=MAX_TX_POWER_DBM
//   KEY_ACTIVE_KEY_FP → u32     fingerprint, 0 = "no key" (== `None`)
//   KEY_SOFT_OFF_INTENT → u32   1 if the last UI tick before this boot
//                               called enter_soft_off; 0 otherwise.
//                               Survives brown-out + battery-pull (unlike
//                               the RAM-based wakeflag), which is what
//                               makes the brief-charging-frame work when
//                               USB plug causes an ESD-or-brownout reset
//                               instead of a clean SENSE wake.
//
// We use u32/i32 even for fields that fit in a smaller type — it
// makes the sequential-storage `Value` impl trivial (built in for
// primitive ints) and the wear cost is negligible at our write rate.

const KEY_CHANNEL: u8 = 0;
const KEY_BAND_PLAN: u8 = 1;
const KEY_TX_POWER: u8 = 2;
const KEY_ACTIVE_KEY_FP: u8 = 3;
const KEY_SOFT_OFF_INTENT: u8 = 4;

/// Scratch buffer size for sequential-storage's record assembly.
/// 64 B is comfortable for any of our values (max is a u32 ≈ 4 B
/// payload plus a couple of bytes of overhead).
const PERSIST_BUF_LEN: usize = 64;

/// Read all `Settings` fields from flash and apply to `settings`.
/// Fields not present in flash (first-boot case, or partial corruption)
/// are left at whatever default the caller seeded `settings` with.
/// Errors are logged but not propagated — we treat persistence
/// failures as "fall back to defaults," not as fatal.
async fn load_settings(flash: &mut Flash, settings: &mut Settings) {
    let mut buf = [0u8; PERSIST_BUF_LEN];
    let mut cache = NoCache::new();
    let range = board::storage::SETTINGS_RANGE;

    match map::fetch_item::<u8, u32, _>(flash, range.clone(), &mut cache, &mut buf, &KEY_CHANNEL)
        .await
    {
        Ok(Some(v)) => settings.channel = v as u8,
        Ok(None) => defmt::info!("persist: no stored channel, using default"),
        Err(e) => defmt::warn!("persist: load channel failed: {:?}", defmt::Debug2Format(&e)),
    }
    match map::fetch_item::<u8, u32, _>(flash, range.clone(), &mut cache, &mut buf, &KEY_BAND_PLAN)
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
        Err(e) => defmt::warn!("persist: load band_plan failed: {:?}", defmt::Debug2Format(&e)),
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

async fn save_channel(flash: &mut Flash, ch: u8) {
    let mut buf = [0u8; PERSIST_BUF_LEN];
    let mut cache = NoCache::new();
    let v = ch as u32;
    if let Err(e) = map::store_item::<u8, u32, _>(
        flash,
        board::storage::SETTINGS_RANGE,
        &mut cache,
        &mut buf,
        &KEY_CHANNEL,
        &v,
    )
    .await
    {
        defmt::warn!("persist: save channel failed: {:?}", defmt::Debug2Format(&e));
    }
}

async fn save_band_plan(flash: &mut Flash, plan: BandPlan) {
    let mut buf = [0u8; PERSIST_BUF_LEN];
    let mut cache = NoCache::new();
    let v = band_plan_index(plan) as u32;
    if let Err(e) = map::store_item::<u8, u32, _>(
        flash,
        board::storage::SETTINGS_RANGE,
        &mut cache,
        &mut buf,
        &KEY_BAND_PLAN,
        &v,
    )
    .await
    {
        defmt::warn!(
            "persist: save band_plan failed: {:?}",
            defmt::Debug2Format(&e)
        );
    }
}

async fn save_tx_power(flash: &mut Flash, dbm: i8) {
    let mut buf = [0u8; PERSIST_BUF_LEN];
    let mut cache = NoCache::new();
    let v = dbm as i32;
    if let Err(e) = map::store_item::<u8, i32, _>(
        flash,
        board::storage::SETTINGS_RANGE,
        &mut cache,
        &mut buf,
        &KEY_TX_POWER,
        &v,
    )
    .await
    {
        defmt::warn!(
            "persist: save tx_power failed: {:?}",
            defmt::Debug2Format(&e)
        );
    }
}

// ── Panic recovery (M7) ─────────────────────────────────────────
//
// The panic handler stages the panic into a `.uninit` buffer in
// the board crate (see `boards/t114/src/panic_record.rs`) and
// soft-resets.  The next boot reads the staged record before SD
// enable, copies the message + reset reason into flash via
// `sequential-storage::queue` (panic-ring region in `storage.rs`),
// and surfaces a one-line summary on RTT.  The About-screen
// rendering of the recovered panic is a downstream extension to
// the UI core.

/// Boot-time check for a staged panic from the prior boot.  If
/// present: log it, push to the panic-ring flash region, then
/// return so normal boot continues.  Reset reason is read +
/// cleared at boot regardless (`RESETREAS` accumulates flags
/// across resets if we don't clear it).
async fn recover_pending_panic(flash: &mut Flash) {
    // SAFETY: called exactly once per boot, from `run()` which is
    // itself called exactly once per binary lifetime.  No other
    // code reads the staging buffer.
    //
    // `take_reset_reason` reads-and-clears via SD so each boot's
    // value reflects only that boot's reset cause.  Without the
    // clear, DOG / SREQ etc accumulate across boots and the
    // post-WDT-fire "dog && !staged → log task hang" branch would
    // either fire spuriously (DOG bit set from a prior boot, this
    // boot was clean) or never (DOG bit already set and we don't
    // know if it's new).
    let reset_reas = board::panic_record::take_reset_reason();
    let staged = unsafe { board::panic_record::take_panic_record() };

    if let Some(record) = staged {
        let msg_len = (record.message_len as usize).min(board::panic_record::PANIC_MSG_LEN);
        let msg_bytes = &record.message[..msg_len];
        let msg_str = core::str::from_utf8(msg_bytes).unwrap_or("(non-utf8 panic message)");
        defmt::warn!(
            "recovered panic from prior boot (reset_reas={=u32:#x}): {}",
            reset_reas,
            msg_str
        );
        osrf_panic_log::push(flash, board::storage::PANIC_RING_RANGE, reset_reas, msg_bytes).await;
    } else if reset_reas != 0 {
        // No staged panic but RESETREAS has flags.  Distinguish the
        // common cases for the boot log; flags accumulate (we don't
        // clear because SD-restricted), so this is "since flash, at
        // least one reset has been from <X>" rather than per-boot
        // precise — combined with the panic-magic check above it's
        // still actionable.
        let dog = reset_reas & board::panic_record::reset_reason::DOG != 0;
        let sreq = reset_reas & board::panic_record::reset_reason::SREQ != 0;
        let pin = reset_reas & board::panic_record::reset_reason::RESETPIN != 0;
        let lockup = reset_reas & board::panic_record::reset_reason::LOCKUP != 0;
        let off = reset_reas & board::panic_record::reset_reason::OFF != 0;
        defmt::info!(
            "boot reset_reas={=u32:#x} (no staged panic — dog={} sreq={} pin={} lockup={} off={})",
            reset_reas,
            dog,
            sreq,
            pin,
            lockup,
            off,
        );
        if dog {
            // Watchdog reset without a staged panic = a task hung
            // long enough for the WDT to fire on its own.  Persist
            // a "watchdog-hang" record to the panic ring so the
            // About screen can surface it the same way it shows
            // panics.  Message is generic since we don't know
            // which task hung — diagnosing that would need
            // per-task counters in the panic ring.
            osrf_panic_log::push(
                flash,
                board::storage::PANIC_RING_RANGE,
                reset_reas,
                b"watchdog: task hung",
            )
            .await;
        }
    }
}

// ── Panic handler ───────────────────────────────────────────────
//
// Production handler: stage the panic into the `.uninit` buffer in
// the board crate, then trigger a software reset.  The next boot
// recovers + writes to flash + reports.  Replaces `panic-probe`'s
// "log + halt forever" pattern so a panic during a gig reboots
// the unit cleanly instead of bricking it until manual power-cycle.
//
// During development (probe attached): the `defmt::error!` call
// inside this handler emits the panic message via RTT before the
// reset hits, so dev sessions see the same info `panic-probe`
// would have printed.

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write as _;

    // Mask interrupts so nothing else runs while we're staging.
    cortex_m::interrupt::disable();

    // Format the panic into a small SliceWriter targeting a stack
    // buffer.  Truncates silently at PANIC_MSG_LEN — large panic
    // messages get clipped rather than dropped entirely.
    let mut buf = [0u8; board::panic_record::PANIC_MSG_LEN];
    let written = {
        struct SliceWriter<'a> {
            buf: &'a mut [u8],
            n: usize,
        }
        impl<'a> core::fmt::Write for SliceWriter<'a> {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let bytes = s.as_bytes();
                let take = bytes.len().min(self.buf.len() - self.n);
                self.buf[self.n..self.n + take].copy_from_slice(&bytes[..take]);
                self.n += take;
                Ok(())
            }
        }
        let mut w = SliceWriter { buf: &mut buf, n: 0 };
        let _ = write!(&mut w, "{}", info);
        w.n
    };

    // Stage the record into the cross-reset buffer.  Direct pointer
    // writes — the buffer is MaybeUninit, we initialise it here.
    // SAFETY: panic handler runs to completion; no other code is
    // executing concurrently (interrupts disabled above).
    unsafe {
        let pending_ptr = core::ptr::addr_of_mut!(board::panic_record::PANIC_PENDING)
            as *mut board::panic_record::PanicStaging;
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!((*pending_ptr).message_len),
            written as u32,
        );
        core::ptr::copy_nonoverlapping(
            buf.as_ptr(),
            core::ptr::addr_of_mut!((*pending_ptr).message) as *mut u8,
            buf.len(),
        );
        // Magic last — readers gate on this, so writing it last
        // means a partially-staged record is never seen as valid.
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!((*pending_ptr).magic),
            board::panic_record::PANIC_MAGIC,
        );
    }

    // Emit the panic message via defmt-rtt for any attached probe.
    // This may or may not actually drain to the host before reset;
    // we don't depend on it.
    defmt::error!("PANIC: {}", defmt::Display2Format(info));

    // Tiny delay so the RTT write has a chance to flush.  100k
    // cycles at 64 MHz = ~1.5 ms, plenty.
    cortex_m::asm::delay(100_000);

    // Soft reset.  Next boot will recover and persist the staged
    // record.
    cortex_m::peripheral::SCB::sys_reset()
}

async fn save_active_key(flash: &mut Flash, fp: Option<u32>) {
    let mut buf = [0u8; PERSIST_BUF_LEN];
    let mut cache = NoCache::new();
    let v = fp.unwrap_or(0);
    if let Err(e) = map::store_item::<u8, u32, _>(
        flash,
        board::storage::SETTINGS_RANGE,
        &mut cache,
        &mut buf,
        &KEY_ACTIVE_KEY_FP,
        &v,
    )
    .await
    {
        defmt::warn!("persist: save key_fp failed: {:?}", defmt::Debug2Format(&e));
    }
}

/// Persist the "we are about to enter (or just entered) soft-off"
/// intent.  Written from `enter_soft_off` + `usb_wake_charging_frame`
/// just before `board::power::enter_system_off`, cleared at boot
/// after `detect_wake_source` has had a chance to read it.  Flash-
/// backed instead of RAM-backed because USB-plug events sometimes
/// brown-out the chip on this hardware (Vbat sag as TP4054 switches
/// charging modes, or ESD on the shield) — RAM wakeflag is wiped,
/// but flash survives, so the boot still recognises "we meant to be
/// off and there's a USB present" and lands on the charging frame
/// instead of a full Idle boot.
async fn save_soft_off_intent(flash: &mut Flash, intent: bool) {
    let mut buf = [0u8; PERSIST_BUF_LEN];
    let mut cache = NoCache::new();
    let v: u32 = if intent { 1 } else { 0 };
    if let Err(e) = map::store_item::<u8, u32, _>(
        flash,
        board::storage::SETTINGS_RANGE,
        &mut cache,
        &mut buf,
        &KEY_SOFT_OFF_INTENT,
        &v,
    )
    .await
    {
        defmt::warn!(
            "persist: save soft_off_intent failed: {:?}",
            defmt::Debug2Format(&e)
        );
    }
}

/// Read the persisted soft-off intent flag.  Returns `false` on
/// missing / corrupt records — safer to drop into a normal boot than
/// to falsely re-sleep on first-ever boot.
async fn load_soft_off_intent(flash: &mut Flash) -> bool {
    let mut buf = [0u8; PERSIST_BUF_LEN];
    let mut cache = NoCache::new();
    match map::fetch_item::<u8, u32, _>(
        flash,
        board::storage::SETTINGS_RANGE,
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
