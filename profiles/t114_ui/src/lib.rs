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
use osrf_ui::battery::NO_BATTERY_MV;
use osrf_ui::{
    band_plan_channel, band_plan_index, build_screen, max_channel_index, BandPlan, BatteryStatus,
    SHUTDOWN_MV,
    Command, KeyStore, LinkStatus, Renderer, Role, ScanState, ScreenId, Settings, UiState, Widget,
    WidgetList, BAND_PLANS, MAX_SCAN_CHANNELS,
};
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
/// `SHUTDOWN_LATCH` flag is needed for the ui_state_loop side.
static SHUTDOWN: ShutdownSignal = ShutdownSignal::new();

/// Latched shutdown flag for the UI side.  Set alongside
/// [`SHUTDOWN`] in `battery_task`; polled by `ui_state_loop`'s loop
/// tick.  Separate from `SHUTDOWN` because `Signal::wait` consumes
/// the value (single-consumer), and we want both runtime and UI to
/// observe the same shutdown event.  Polling is fine here — the
/// ui_state_loop tick is 300 ms, which is well within the
/// shutdown-budget of "user sees goodbye frame before the chip
/// browns out."  Never cleared.
static SHUTDOWN_LATCH: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

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

    defmt::info!(
        "ui (T114, {:?}): bringing up SD + display + joystick + link",
        role
    );

    // Order: `embassy_nrf::init()` (inside `board::resources()`)
    // **must** come before `Softdevice::enable()` — SD claims CLOCK +
    // POWER on activation; embassy can no longer configure those
    // afterwards.
    let r = board::resources();
    let sd = board::softdevice::enable();
    spawner.spawn(board::softdevice::run(sd).expect("alloc softdevice run task"));

    // Brief settling delay so the SD's first event-loop tick has
    // happened before we take Flash.  Cheap insurance against
    // taking Flash mid-SD-startup.
    Timer::after_millis(10).await;
    let mut flash = board::storage::flash(sd);

    // Recover any panic staged by the prior boot (if any).  Reads
    // and clears RESETREAS, takes the staged record from .uninit,
    // logs + persists to the panic-ring flash region.  Idempotent:
    // a clean cold boot here is a no-op.
    recover_pending_panic(&mut flash).await;

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

    let initial_status = link_status_from_stats(&STATS.get());
    build_screen(&state, &settings, &keys, &initial_status, &mut widgets);
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
            // Boot counter goes into the high 16 bits of the link-
            // layer `seq` and MUST change across resets so RX's replay
            // window doesn't reject post-reboot low-seq packets as
            // ancient duplicates.  Pull via SD's RNG SVC; flash-
            // persisted in M7.
            let boot_counter = read_random_u16();
            defmt::info!("boot_counter = {} (random per-boot)", boot_counter);
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
    )
    .await
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
#[embassy_executor::task]
async fn ui_render_task(
    mut display: board::Display,
    fb: &'static mut Framebuffer,
    mut renderer: Renderer,
    mut wdt: WatchdogHandle,
) -> ! {
    loop {
        match select(FRAME.wait(), Timer::after_secs(WDT_RENDER_IDLE_PET_S)).await {
            Either::First(frame) => {
                let _ = renderer.render(&frame.widgets, &frame.scan, fb);
                display.flush(fb).await;
            }
            Either::Second(()) => {
                // No frame arrived in the idle-pet window — display
                // is off or ui_state is quiet.  Nothing to render;
                // just fall through to pet the WDT and re-wait.
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
        let status = BatteryStatus::from_reading(mv, plugged_in);
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
            && mv >= NO_BATTERY_MV
            && mv <= SHUTDOWN_MV;
        if shutdown_eligible {
            shutdown_run = shutdown_run.saturating_add(1);
            if !shutdown_fired && shutdown_run >= SHUTDOWN_BATTERY_SUSTAINED_SAMPLES {
                defmt::warn!(
                    "battery shutdown threshold: {=u16} mV sustained for {=u32} samples — signalling SHUTDOWN",
                    mv,
                    shutdown_run
                );
                SHUTDOWN.signal();
                SHUTDOWN_LATCH.store(true, core::sync::atomic::Ordering::Release);
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

    loop {
        // Pet the WDT at every loop iteration top.  Cadence is the
        // scan_tick (~300 ms) plus whatever flash-write time an
        // Apply* command added — comfortably under the 5 s timeout.
        // Bursts of joystick events may pet faster than that.
        wdt.pet();

        // Low-battery shutdown: `battery_task` set the latch after
        // sustained sub-shutdown voltage.  Render a goodbye frame,
        // log a "low-battery shutdown" record to the panic ring
        // (audit trail — useful when the user's first question
        // after powering on a dead board is "did it crash or just
        // run out?"), then drop into a WDT-petting park loop.  We
        // don't `loop {}` outright because the WDT would reset us
        // back to here on a sustained-low boot — wasted power on
        // the boot cycle.
        if SHUTDOWN_LATCH.load(core::sync::atomic::Ordering::Acquire) {
            defmt::warn!("ui: low-battery shutdown acknowledged — rendering goodbye frame");
            // Light the panel back up if it had auto-slept, so the
            // user sees the goodbye.  No need to track `display_on`
            // afterwards — the park loop never returns to the
            // top-of-loop policy code.
            if !display_on {
                backlight.set_low();
            }
            widgets.clear();
            let _ = widgets.push(Widget::Title(heapless::String::try_from("Shutting down").unwrap_or_default()));
            let _ = widgets.push(Widget::Text {
                row: 2,
                text: heapless::String::try_from("Battery low").unwrap_or_default(),
            });
            let _ = widgets.push(Widget::Text {
                row: 3,
                text: heapless::String::try_from("Plug in to charge").unwrap_or_default(),
            });
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
            push_panic_record(flash, 0, b"low-battery shutdown").await;
            // Show the goodbye for ~3 s before killing the panel.
            // WDT slot pets the timer in this task, so we use short
            // sub-pets to keep it fed.
            for _ in 0..10 {
                wdt.pet();
                Timer::after_millis(300).await;
            }
            backlight.set_high();
            // Park loop — pet the WDT, but also consume joystick
            // events so a deliberate user interaction can recover.
            // Any event triggers a full reset; if the battery is
            // still below threshold, `battery_task` re-fires the
            // shutdown on the next boot.  Acceptable trade: a
            // briefly-revived UI on a dying cell is better than a
            // bricked-looking unit that needs NRESET.  Drains the
            // event channel first so any presses queued during the
            // goodbye render don't immediately reboot us.
            while EVENT_CHAN.try_receive().is_ok() {}
            loop {
                wdt.pet();
                match select(EVENT_CHAN.receive(), Timer::after_secs(1)).await {
                    Either::First(_) => {
                        defmt::info!("ui: joystick wake from shutdown — rebooting");
                        cortex_m::peripheral::SCB::sys_reset();
                    }
                    Either::Second(()) => {}
                }
            }
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
            build_screen(state, &settings, &keys, &status, widgets);
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
//
// We use u32/i32 even for fields that fit in a smaller type — it
// makes the sequential-storage `Value` impl trivial (built in for
// primitive ints) and the wear cost is negligible at our write rate.

const KEY_CHANNEL: u8 = 0;
const KEY_BAND_PLAN: u8 = 1;
const KEY_TX_POWER: u8 = 2;
const KEY_ACTIVE_KEY_FP: u8 = 3;

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

/// Append a single panic / shutdown record to the panic-ring flash
/// region.  Format: `[reset_reason: u32 LE][message: UTF-8 bytes]`,
/// pushed onto a `sequential-storage::queue`.  Pops are not
/// performed today — the queue auto-overwrites the oldest entry
/// once it fills (via `push`'s `allow_overwrite=true`), giving us
/// the last ~30 panics naturally.
async fn push_panic_record(flash: &mut Flash, reset_reas: u32, message: &[u8]) {
    use sequential_storage::queue;

    let mut cache = NoCache::new();
    let mut record = [0u8; 4 + board::panic_record::PANIC_MSG_LEN];
    record[0..4].copy_from_slice(&reset_reas.to_le_bytes());
    let msg_len = message.len().min(board::panic_record::PANIC_MSG_LEN);
    record[4..4 + msg_len].copy_from_slice(&message[..msg_len]);

    if let Err(e) = queue::push(
        flash,
        board::storage::PANIC_RING_RANGE,
        &mut cache,
        &record[..4 + msg_len],
        true,
    )
    .await
    {
        defmt::warn!(
            "persist: panic-ring push failed: {:?}",
            defmt::Debug2Format(&e)
        );
    }
}

/// Boot-time check for a staged panic from the prior boot.  If
/// present: log it, push to the panic-ring flash region, then
/// return so normal boot continues.  Reset reason is read +
/// cleared at boot regardless (`RESETREAS` accumulates flags
/// across resets if we don't clear it).
async fn recover_pending_panic(flash: &mut Flash) {
    // SAFETY: called exactly once per boot, from `run()` which is
    // itself called exactly once per binary lifetime.  No other
    // code reads the staging buffer.
    let reset_reas = unsafe { board::panic_record::read_reset_reason() };
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
        push_panic_record(flash, reset_reas, msg_bytes).await;
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
        defmt::info!(
            "boot reset_reas={=u32:#x} (no staged panic — dog={} sreq={} pin={} lockup={})",
            reset_reas,
            dog,
            sreq,
            pin,
            lockup,
        );
        if dog {
            // Watchdog reset without a staged panic = a task hung
            // long enough for the WDT to fire on its own.  Persist
            // a "watchdog-hang" record to the panic ring so the
            // About screen can surface it the same way it shows
            // panics.  Message is generic since we don't know
            // which task hung — diagnosing that would need
            // per-task counters in the panic ring.
            push_panic_record(flash, reset_reas, b"watchdog: task hung").await;
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
