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
use osrf_link_runtime::{LinkConfigSignal, LinkStatsCell, ScanController};
use osrf_ui::{
    band_plan_channel, build_screen, max_channel_index, BandPlan, Command, KeyStore, LinkStatus,
    Renderer, Role, ScanState, ScreenId, Settings, UiState, WidgetList, MAX_SCAN_CHANNELS,
};

use board::embassy_nrf::gpio::{Input, Output, Pull};
use board::embassy_nrf::interrupt::{self, InterruptExt, Priority};
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
    let settings = Settings::default();
    let mut keys = KeyStore::new();
    let _ = keys.add("Studio A", 0x111111);
    let _ = keys.add("Backup", 0x222222);
    let mut widgets: WidgetList = WidgetList::new();
    let mut renderer = Renderer::new();

    let initial_status = link_status_from_stats(&STATS.get());
    build_screen(&state, &settings, &keys, &initial_status, &mut widgets);
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

    // ── ui_render task — owns display + fb + renderer ──────────────
    spawner.spawn(ui_render_task(display, fb, renderer).expect("alloc ui_render_task"));

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
    ui_state_loop(&mut backlight, &mut state, settings, keys, &mut widgets).await
}

// ── Tasks ───────────────────────────────────────────────────────

/// Renderer task — awaits a fresh [`FrameData`] on [`FRAME`], paints
/// it into the framebuffer, and flushes the dirty region to the panel
/// via async SPI.  Owns the display, framebuffer, and renderer for
/// the lifetime of the program; nothing else writes to them.
///
/// When the panel is "off" (backlight high), ui_state simply doesn't
/// signal new frames — so this task sleeps on `FRAME.wait()` until the
/// user wakes the display.  No need for an explicit on/off path here.
#[embassy_executor::task]
async fn ui_render_task(
    mut display: board::Display,
    fb: &'static mut Framebuffer,
    mut renderer: Renderer,
) -> ! {
    loop {
        let frame = FRAME.wait().await;
        let _ = renderer.render(&frame.widgets, &frame.scan, fb);
        display.flush(fb).await;
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
                        Command::ApplyChannel(_)
                        | Command::ApplyBandPlan(_)
                        | Command::ApplyTxPower(_) => {
                            CONFIG_UPDATES.signal(link_config_from(&settings));
                        }
                        Command::ApplySetActiveKey(_) => {
                            // No-op until AEAD lands.
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
