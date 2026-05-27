// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

//! T114 UI deployment — wiring shell.
//!
//! Picks the per-build configuration (battery chemistry, power
//! policy, role / TX source) and bundles the board's concrete
//! peripherals with the board-agnostic [`osrf_app_ui_runtime`]
//! loop bodies.  All UI / battery / soft-off / settings-persistence
//! logic lives in the app crate — this profile is the embassy task
//! plumbing + the production panic-staging handler.
//!
//! # Task topology
//!
//! | Task             | Executor            | Priority | Body lives in            |
//! |------------------|---------------------|----------|--------------------------|
//! | `softdevice run` | thread (main)       | (SD)     | nrf-softdevice           |
//! | `joystick`       | thread (main)       | low      | profile (drives driver)  |
//! | `ui_render`      | thread (main)       | low      | profile (concrete display)|
//! | **main** task    | thread (main)       | low      | `osrf_app_ui_runtime::ui_state_loop` |
//! | `battery`        | thread (main)       | low      | `osrf_app_ui_runtime::battery_loop`  |
//! | `link_runtime`   | interrupt executor  | **P2**   | `osrf_app_midi_node::run_*`          |
//!
//! See `PLAN.md` § Milestone 6 for the task-split rationale and
//! § Milestone 8 for the soft-off / wake / power-policy story.

use embassy_executor::{InterruptExecutor, Spawner};
use embassy_futures::select::{select3, Either3};
use embassy_time::Timer;
use osrf_app_link_bench::synthetic::ScenarioSource;
use osrf_app_midi_node::{
    run_rx, run_rx_diversity, run_rx_secondary, run_tx, AeadConfig, AeadUpdate, CipherId, Direction,
    DiversityRxChannel, LinkConfig, LinkConfigSignal, UartMidiSink, UartMidiSource,
};
use osrf_app_ui_runtime as app;
use osrf_board_t114 as board;
use osrf_driver_input_joystick5way::Joystick5Way;
use osrf_ui::{
    BandPlan, BatteryChemistry, BatteryStatus, KeyStore, PowerPolicy, Renderer, Role, Settings,
    UiState, Widget, WidgetList,
};

use board::embassy_nrf::gpio::{Input, Output, Pull};
use board::embassy_nrf::interrupt::{self, InterruptExt, Priority};
use board::embassy_nrf::wdt::{Config as WdtConfig, Watchdog, WatchdogHandle};
use board::framebuffer::Framebuffer;

// ── Profile-level configuration ─────────────────────────────────
//
// Battery chemistry and power policy are passed into `run()` from the
// build-time profile (`configs/<name>.toml` → `battery` / `power_policy`),
// not hardcoded here.  See `configs/README.md`.

// ── AEAD keys: baked at build time from the key file ─────────────
//
// Until Stage 4 BLE provisioning lands there's no on-device way to
// transfer a key between paired units, so the keys are compiled in.
// `build.rs` reads `<workspace>/osrf-keys.toml` (override path with the
// `OSRF_KEYS_FILE` env var) and generates `KEY_SHARED` / `KEY_SHARED_NAME`
// / `KEY_TX_ONLY` / `KEY_TX_ONLY_NAME` / `TEST_DEVICE_ID` into
// `$OUT_DIR/keys.rs`, included below.  With no key file the build falls
// back to the historical TEST keys ([0x42;32]/[0x99;32]) and prints a
// `cargo:warning` — see `build.rs`.
//
// Both paired units must build with the **same `shared` key** (same bytes →
// same fingerprint → packets accepted).  TX registers both keys so the
// operator can flip between them in the Key menu; RX registers only
// `KEY_SHARED`, so picking `TX-Only` on TX demonstrates the `KeyFpMismatch`
// rejection path on RX.  `TEST_DEVICE_ID` ties the AEAD nonce to a fixed
// device id so paired units agree without exchanging FICR.DEVICEID
// out-of-band (Stage 4 will switch to real per-device ids + an allowlist).
include!(concat!(env!("OUT_DIR"), "/keys.rs"));

/// Cipher used for every baked key.  (The key file doesn't pick a cipher
/// per key yet; ChaCha20-Poly1305 works on every target.)
const KEY_CIPHER: CipherId = CipherId::ChaCha20Poly1305;

/// Build the AEAD context for a given key.  Cipher, device_id and
/// direction are profile-wide.
fn ctx_for_key(key: [u8; 32]) -> AeadConfig {
    AeadConfig {
        cipher: KEY_CIPHER,
        key,
        device_id: KEY_DEVICE_ID,
        direction: Direction::TxToRx,
    }
}

fn fp_for_key(key: &[u8; 32]) -> u32 {
    osrf_app_midi_node::osrf_crypto::fingerprint(KEY_CIPHER, key) & 0x00FF_FFFF
}

/// Resolve the UI-selected key fingerprint to an AEAD config.  Used by
/// **both** roles now that the whole keyring is registered on each end:
///
/// * `None` (operator picked **Open**) → plaintext, no encryption.
/// * `Some(fp)` matching a baked key → strict: encrypt/decrypt with that
///   key only, reject plaintext.
/// * `Some(fp)` matching nothing we hold → refuse everything (defensive;
///   the menu only offers fingerprints we registered).
fn aead_resolver(active_fp: Option<u32>) -> AeadUpdate {
    match active_fp.map(|fp| fp & 0x00FF_FFFF) {
        None => AeadUpdate {
            aead: None,
            allow_open: true,
        },
        Some(fp) => {
            for (_name, key) in BAKED_KEYS {
                if fp_for_key(key) == fp {
                    return AeadUpdate {
                        aead: Some(ctx_for_key(*key)),
                        allow_open: false,
                    };
                }
            }
            AeadUpdate {
                aead: None,
                allow_open: false,
            }
        }
    }
}

/// Which `MidiSource` flavour the TX-role build drives the runtime
/// with.  `Uart` reads real DIN MIDI from the FeatherWing UART
/// (production path).  `Scenario` runs the synthetic burst-pattern
/// source for stress tests.  Ignored for `Role::Rx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxSource {
    Uart,
    Scenario,
}

// ── Memory + executor wiring ────────────────────────────────────

/// 64 KB in-RAM framebuffer the renderer paints into.  Lives in BSS
/// because `Framebuffer::new()` is `const fn` — avoids a stack-
/// allocated 64 KB which would blow embassy's task pool.  Borrowed
/// exactly once at boot.
static mut FRAMEBUFFER: Framebuffer = Framebuffer::new();

/// Interrupt-driven executor running the link-runtime task at P2
/// (P0/P1/P4 are SD-reserved).  Bound to `SWI0_EGU0` so radio
/// IRQ → packet handling preempts UI rendering on the main task.
static EXECUTOR_LINK: InterruptExecutor = InterruptExecutor::new();

/// Diversity handoff (RX builds only): the secondary radio's drain task
/// (`link_rx_secondary_task`) pushes frames here; the consumer
/// (`link_rx_diversity_task`) reads them. Unused on single-radio builds.
static RADIO1_CH: DiversityRxChannel = DiversityRxChannel::new();
/// Live `LinkConfig` forward from the consumer loop to the secondary radio's
/// task, so a UI channel change retunes both radios.
static SECONDARY_CFG: LinkConfigSignal = LinkConfigSignal::new();

#[cortex_m_rt::interrupt]
#[allow(non_snake_case)]
unsafe fn EGU0_SWI0() {
    EXECUTOR_LINK.on_interrupt()
}

// ── Watchdog adapter ────────────────────────────────────────────

/// Newtype implementing the app crate's [`app::Watchdog`] trait
/// over the board's concrete `WatchdogHandle`.  Lives here so the
/// app crate stays HAL-agnostic.
struct ProfileWdt(WatchdogHandle);

impl app::Watchdog for ProfileWdt {
    fn pet(&mut self) {
        self.0.pet();
    }
}

// ── Battery sampler adapter ─────────────────────────────────────

struct ProfileBattery(board::battery::BatteryMonitor);

impl app::BatterySampler for ProfileBattery {
    async fn sample_mv(&mut self) -> u16 {
        self.0.sample().await
    }
    fn vbus_present(&self) -> bool {
        board::battery::vbus_present()
    }
}

/// Function-pointer wrapper so we can pass `vbus_present` into the
/// app's `ui_state_loop` (Wired-mode polling).
fn vbus_present_fn() -> bool {
    board::battery::vbus_present()
}

// ── Joystick type alias ─────────────────────────────────────────

type Joystick =
    Joystick5Way<Input<'static>, Input<'static>, Input<'static>, Input<'static>, Input<'static>>;

// ── Watchdog timing ─────────────────────────────────────────────

/// Hardware-watchdog timeout in 32 768 Hz ticks (5 seconds).
const WDT_TIMEOUT_TICKS: u32 = 5 * 32_768;

// ── Public API ──────────────────────────────────────────────────

/// Bring up the board, dispatch the wake path, spawn all tasks,
/// run the UI state machine forever.  Called from each binary's
/// `#[embassy_executor::main]`.
/// `diversity` (RX only): when `true` and `role == Role::Rx`, bring up the
/// second SX1262 (radio1, on SPI3) and run the receiver with dual-radio
/// receive diversity via `run_rx_diversity`.  Ignored for `Role::Tx`
/// (single-radio TX).  Single-radio builds pass `false` and never claim
/// SPI3 / the radio1 header pins.
///
/// `band_plans` is this build's Band Plan menu list (resolved from the
/// profile's `band_plans = [...]` against the `band_plans/` registry).  It
/// also fixes the default/clamp: a fresh device boots on `band_plans[0]`,
/// and a persisted plan outside this list (e.g. after reflashing across
/// bands) is snapped back to `band_plans[0]`.
pub async fn run(
    spawner: Spawner,
    role: Role,
    tx_source: TxSource,
    diversity: bool,
    band_plans: &'static [BandPlan],
    power_policy: PowerPolicy,
    chemistry: BatteryChemistry,
    name: &'static str,
) -> ! {
    // Clear DEMCR — see `t114_dap_idle_freeze.md` memory note.
    // Without this, transient HardFaults halt the core forever
    // post-`cargo run` once the probe is detached.
    const DEMCR: *mut u32 = 0xE000_EDFC as *mut u32;
    unsafe { core::ptr::write_volatile(DEMCR, 0) };

    // Emulated-System-OFF post-`cargo run` is documented in
    // `t114_emulated_system_off.md`.  No software workaround;
    // power-cycle once after each flash before testing soft-off.

    defmt::info!(
        "ui (T114, {:?}): bringing up SD + display + joystick + link",
        role
    );

    // embassy_nrf::init (inside board::resources) must precede SD
    // enable — SD claims CLOCK + POWER on activation.
    //
    // Diversity (RX only) additionally constructs radio1 on SPI3.  Gated
    // on `role == Rx` so a TX build never claims the second radio's
    // peripheral/pins.  `radio1` is threaded to the Rx spawn below.
    let want_diversity = diversity && matches!(role, Role::Rx);
    let (r, radio1) = if want_diversity {
        let (r, r1) = board::resources_with_diversity();
        (r, Some(r1))
    } else {
        (board::resources(), None)
    };

    // Identify the RAM-side wake signal before SD enable — direct
    // register reads, SD-safe, and keeps Center-press race latency
    // low.  The destructive `wakeflag::take` happens here.
    let early_wake = board::power::detect_wake_source();
    defmt::info!("ui: early_wake = {:?}", early_wake);

    let sd = board::softdevice::enable();
    spawner.spawn(board::softdevice::run(sd).expect("alloc softdevice run task"));

    // Brief settle so the SD's first event-loop tick lands before
    // we take Flash.  Insurance against taking Flash mid-startup.
    Timer::after_millis(10).await;
    let mut flash = board::storage::flash(sd);

    // Boot dispatch — see the per-chemistry/policy/wake matrix
    // in `osrf_app_ui_runtime` docs and the comments below.
    let flash_intent = app::load_soft_off_intent(&mut flash, board::storage::SETTINGS_RANGE).await;
    let vbus_at_boot = board::battery::vbus_present();

    // Wired-mode short-circuit: any wake path → Idle (the 10 s
    // grace timer in `ui_state_loop` handles the rest).
    if matches!(power_policy, PowerPolicy::Wired) {
        defmt::info!(
            "ui: Wired policy → Idle (VBUS={}, early_wake={:?})",
            vbus_at_boot,
            early_wake
        );
    } else {
        match early_wake {
            board::power::WakeSource::CenterPress => {
                defmt::info!("ui: wake = CenterPress → Idle");
            }
            board::power::WakeSource::UsbPlug if vbus_at_boot => {
                defmt::info!("ui: wake = UsbPlug + VBUS → charging frame");
                usb_wake_charging_frame(r.display, r.display_backlight, r.battery, chemistry).await;
            }
            board::power::WakeSource::UsbPlug => {
                app::unexpected_wake_resleep(board::power::enter_system_off);
            }
            board::power::WakeSource::ColdBoot if flash_intent && vbus_at_boot => {
                defmt::info!(
                    "ui: wake = ColdBoot + flash_intent + VBUS → charging frame \
                     (probable brown-out from USB plug-in)"
                );
                usb_wake_charging_frame(r.display, r.display_backlight, r.battery, chemistry).await;
            }
            board::power::WakeSource::ColdBoot if flash_intent => {
                app::unexpected_wake_resleep(board::power::enter_system_off);
            }
            board::power::WakeSource::ColdBoot => {
                defmt::info!("ui: wake = ColdBoot (no flash_intent) → Idle");
            }
        }
    }

    // Committed to Idle.  Clear flash flag if it was set — see
    // app::load_soft_off_intent docs for the lifecycle.
    if flash_intent {
        app::save_soft_off_intent(&mut flash, board::storage::SETTINGS_RANGE, false).await;
    }

    // Recover any panic staged by the prior boot.  Idempotent — a
    // clean boot here is a no-op.  Both the panic-staging
    // `#[panic_handler]` and this recovery fn live in the board
    // crate (gated behind the `panic-stage` Cargo feature).
    board::panic_record::recover_pending_panic(&mut flash).await;
    let mut last_panic_msg =
        osrf_panic_log::read_latest(&mut flash, board::storage::PANIC_RING_RANGE).await;

    // ── Display init + initial paint ──────────────────────────────
    let mut display = r.display;
    let mut backlight = r.display_backlight;
    display.init().await;
    // SAFETY: only place we borrow FRAMEBUFFER.
    let fb: &'static mut Framebuffer = unsafe { &mut *core::ptr::addr_of_mut!(FRAMEBUFFER) };

    let mut state = UiState::with_role_bands(role, band_plans, name);
    let mut settings = Settings::default();
    // Boot default = this build's first band; a fresh device comes up on a
    // band its radio can actually tune.
    settings.band_plan = band_plans[0];
    app::load_settings(&mut flash, board::storage::SETTINGS_RANGE, &mut settings).await;
    // Snap a persisted plan that isn't in this build's list back to the
    // default — covers reflashing a device across bands (the stored global
    // index could otherwise resolve to a plan this radio can't tune).
    if !band_plans.contains(&settings.band_plan) {
        settings.band_plan = band_plans[0];
        settings.channel = 0;
    }
    let mut keys = KeyStore::new();
    app::load_keys(&mut flash, board::storage::KEY_STORE_RANGE, &mut keys).await;

    // Register the whole baked keyring in the runtime keystore so every
    // key shows up as a selectable entry in the UI's Key list — on both
    // TX and RX (either end can select any key; both must select the same
    // one to talk).  `keys.add` caps at `MAX_KEYS`; extras are dropped.
    for (name, key) in BAKED_KEYS {
        let fp = fp_for_key(key);
        if keys.find(fp).is_none() && keys.add(name, KEY_CIPHER, fp).is_err() {
            defmt::warn!("t114_ui: keystore full; key {} not registered", name);
        }
    }

    // Boot-default key: if nothing is persisted yet, select the key file's
    // `active` entry (the first key by default) so a freshly-flashed device
    // comes up encrypted rather than in the clear.  Once the operator picks
    // a key (or Open) in the menu, that choice persists and wins here.
    if settings.active_key_fp.is_none() {
        if let Some(idx) = ACTIVE_KEY_IDX {
            settings.active_key_fp = Some(fp_for_key(&BAKED_KEYS[idx].1));
        }
    }

    // Single `aead_resolver` (top-level fn) for both roles.  Live key
    // changes from the menu are applied via `ui_state_loop` →
    // `AEAD_UPDATES` → the runtime's top-of-loop `try_take`; no reboot.
    let initial_update = aead_resolver(settings.active_key_fp);
    let initial_aead = initial_update.aead;
    let initial_allow_open = initial_update.allow_open;
    defmt::info!(
        "t114_ui: boot AEAD aead={=bool} allow_open={=bool}",
        initial_aead.is_some(),
        initial_allow_open,
    );
    let mut widgets: WidgetList = WidgetList::new();
    let mut renderer = Renderer::new();

    // Random per-boot 16-bit session ID — shown on About; reused
    // as the link-layer `boot_counter` for TX.
    let session_id = read_random_u16();
    defmt::info!("session_id = 0x{=u16:04X} (random per-boot)", session_id);

    let initial_status = app::link_status_from_stats(&app::STATS.get());
    let initial_about = app::about_data(
        session_id,
        &last_panic_msg,
        concat!("v", env!("CARGO_PKG_VERSION")),
        board::GIT_HASH,
    );
    osrf_ui::build_screen(
        &mut state,
        &settings,
        &keys,
        &initial_status,
        &initial_about,
        &mut widgets,
    );
    let initial_battery = critical_section::with(|cs| app::BATTERY.borrow(cs).get());
    let _ = widgets.push(Widget::BatteryIndicator {
        voltage_mv: initial_battery.voltage_mv,
        percent: initial_battery.percent,
        plugged_in: initial_battery.plugged_in,
    });
    let _ = renderer.render(&widgets, &state.scan, fb);
    display.flush(fb).await;
    backlight.set_low(); // active LOW = on
    defmt::info!("ui ready: role={:?} screen={:?}", role, state.screen);

    // ── Joystick spawn ────────────────────────────────────────────
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
    spawner.spawn(battery_task(r.battery, chemistry).expect("alloc battery_task"));

    // ── Hardware watchdog ─────────────────────────────────────────
    // Done late in boot so the slow startup steps (display rail
    // warmup, SD enable, initial flash reads) don't trip the WDT
    // before any task is alive to pet it.
    let mut wdt_config = WdtConfig::default();
    wdt_config.timeout_ticks = WDT_TIMEOUT_TICKS;
    let (_wdt, [wdt_main, wdt_render]) =
        Watchdog::try_new(r.wdt, wdt_config).expect("WDT already configured differently");

    spawner.spawn(ui_render_task(display, fb, renderer, wdt_render).expect("alloc ui_render_task"));

    // ── link_runtime on its own interrupt executor at P2 ─────────
    let config = app::link_config_from(&settings);

    let irq = interrupt::EGU0_SWI0;
    irq.set_priority(Priority::P2);
    let spawner_link = EXECUTOR_LINK.start(irq);

    match role {
        Role::Rx => {
            let sink = UartMidiSink::new(r.midi_uart);
            match radio1 {
                Some(radio1) => {
                    defmt::info!("ui: RX receive-diversity ON (radio0 + radio1 on SPI3)");
                    // Producer: radio1 drains into RADIO1_CH on its own task.
                    spawner_link.spawn(
                        link_rx_secondary_task(radio1, config)
                            .expect("alloc link_rx_secondary_task"),
                    );
                    // Consumer: radio0 + the shared decode/stats loop.
                    spawner_link.spawn(
                        link_rx_diversity_task(
                            r.radio0,
                            r.status_led,
                            sink,
                            config,
                            initial_aead,
                            initial_allow_open,
                        )
                        .expect("alloc link_rx_diversity_task"),
                    );
                }
                None => {
                    spawner_link.spawn(
                        link_rx_task(
                            r.radio0,
                            r.status_led,
                            sink,
                            config,
                            initial_aead,
                            initial_allow_open,
                        )
                        .expect("alloc link_rx_task"),
                    );
                }
            }
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
                            initial_aead,
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
                            initial_aead,
                        )
                        .expect("alloc link_tx_scenario_task"),
                    );
                }
            }
        }
    }

    // Drop neopixel — already parked Low.  Leaking keeps the pin
    // held; dropping it would float.
    core::mem::forget(r.neopixel_parked);

    // ── Main task body = UI state loop ───────────────────────────
    app::ui_state_loop(
        &mut backlight,
        &mut flash,
        ProfileWdt(wdt_main),
        &mut state,
        settings,
        keys,
        &mut widgets,
        session_id,
        &mut last_panic_msg,
        concat!("v", env!("CARGO_PKG_VERSION")),
        board::GIT_HASH,
        chemistry,
        power_policy,
        board::storage::SETTINGS_RANGE,
        board::storage::PANIC_RING_RANGE,
        vbus_present_fn,
        board::power::enter_system_off,
        aead_resolver,
    )
    .await
}

// ── Tasks ────────────────────────────────────────────────────────

/// Renderer task — awaits a fresh frame on [`app::FRAME`], paints
/// it into the framebuffer, flushes the dirty region to the panel.
/// On [`app::POWER_OFF_DISPLAY`]: runs `display.power_off()` then
/// idles in WDT-pet mode.
#[embassy_executor::task]
async fn ui_render_task(
    mut display: board::Display,
    fb: &'static mut Framebuffer,
    mut renderer: Renderer,
    mut wdt: WatchdogHandle,
) -> ! {
    loop {
        match select3(
            app::FRAME.wait(),
            Timer::after_secs(app::WDT_RENDER_IDLE_PET_S),
            app::POWER_OFF_DISPLAY.wait(),
        )
        .await
        {
            Either3::First(frame) => {
                let _ = renderer.render(&frame.widgets, &frame.scan, fb);
                display.flush(fb).await;
            }
            Either3::Second(()) => {
                // No frame in the idle-pet window — display is off
                // or ui_state is quiet.  Pet and re-wait.
            }
            Either3::Third(()) => {
                display.power_off().await;
                defmt::info!("ui_render: display powered off — entering pet-only idle");
                loop {
                    wdt.pet();
                    Timer::after_secs(app::WDT_RENDER_IDLE_PET_S).await;
                }
            }
        }
        wdt.pet();
    }
}

#[embassy_executor::task]
async fn link_rx_task(
    mut radio0: board::Radio0,
    mut status_led: Output<'static>,
    mut sink: UartMidiSink<board::MidiUart>,
    config: LinkConfig,
    aead: Option<AeadConfig>,
    allow_open: bool,
) -> ! {
    run_rx(
        &mut radio0,
        &mut status_led,
        &mut sink,
        &config,
        &app::STATS,
        Some(&app::CONFIG_UPDATES),
        Some(&app::SCAN),
        Some(&app::SHUTDOWN),
        aead,
        allow_open,
        Some(&app::AEAD_UPDATES),
    )
    .await
}

/// Receive-diversity **consumer** (on-board radio0): runs its own receive
/// plus the shared decode/dedup/stats, consuming the secondary radio's
/// frames from `RADIO1_CH`.  Same UI-signal wiring as the single-radio task;
/// a live channel change is forwarded to the secondary via `SECONDARY_CFG`.
/// Paired with [`link_rx_secondary_task`].
#[embassy_executor::task]
async fn link_rx_diversity_task(
    mut radio0: board::Radio0,
    mut status_led: Output<'static>,
    mut sink: UartMidiSink<board::MidiUart>,
    config: LinkConfig,
    aead: Option<AeadConfig>,
    allow_open: bool,
) -> ! {
    run_rx_diversity(
        &mut radio0,
        RADIO1_CH.receiver(),
        &SECONDARY_CFG,
        &mut status_led,
        &mut sink,
        &config,
        &app::STATS,
        Some(&app::CONFIG_UPDATES),
        Some(&app::SCAN),
        Some(&app::SHUTDOWN),
        aead,
        allow_open,
        Some(&app::AEAD_UPDATES),
    )
    .await
}

/// Receive-diversity **producer** (SPI3 radio1, DX-LR30): drains its radio
/// into `RADIO1_CH`. Lives in its own task so its `rx_recv` is never
/// cancelled → DIO1 IRQ always cleared → no GPIOTE-PORT spurious-wake storm.
/// Retunes when the consumer forwards a config change via `SECONDARY_CFG`.
#[embassy_executor::task]
async fn link_rx_secondary_task(mut radio1: board::Radio1, config: LinkConfig) -> ! {
    run_rx_secondary(&mut radio1, &config, Some(&SECONDARY_CFG), RADIO1_CH.sender()).await
}

#[embassy_executor::task]
async fn link_tx_uart_task(
    mut radio0: board::Radio0,
    mut status_led: Output<'static>,
    mut source: UartMidiSource<board::MidiUart>,
    boot_counter: u16,
    config: LinkConfig,
    aead: Option<AeadConfig>,
) -> ! {
    run_tx(
        &mut radio0,
        &mut status_led,
        &mut source,
        boot_counter,
        &config,
        &app::STATS,
        Some(&app::CONFIG_UPDATES),
        Some(&app::SCAN),
        Some(&app::SHUTDOWN),
        aead,
        Some(&app::AEAD_UPDATES),
    )
    .await
}

#[embassy_executor::task]
async fn link_tx_scenario_task(
    mut radio0: board::Radio0,
    mut status_led: Output<'static>,
    boot_counter: u16,
    config: LinkConfig,
    aead: Option<AeadConfig>,
) -> ! {
    let mut source = ScenarioSource::new();
    run_tx(
        &mut radio0,
        &mut status_led,
        &mut source,
        boot_counter,
        &config,
        &app::STATS,
        Some(&app::CONFIG_UPDATES),
        Some(&app::SCAN),
        Some(&app::SHUTDOWN),
        aead,
        Some(&app::AEAD_UPDATES),
    )
    .await
}

#[embassy_executor::task]
async fn joystick_task(mut js: Joystick) {
    loop {
        let ev = js.next_event().await;
        app::EVENT_CHAN.send(ev).await;
    }
}

#[embassy_executor::task]
async fn battery_task(monitor: board::battery::BatteryMonitor, chemistry: BatteryChemistry) -> ! {
    app::battery_loop(ProfileBattery(monitor), chemistry).await
}

// ── USB-wake charging frame (uses concrete display) ─────────────

/// Boot-time branch for `WakeSource::UsbPlug` + VBUS-present.
/// Renders a single "Charging" frame using the just-sampled battery
/// state, holds it for 2 s, then puts the panel back to sleep and
/// re-enters `board::power::enter_system_off()`.  Diverges.
async fn usb_wake_charging_frame(
    mut display: board::Display,
    mut backlight: Output<'static>,
    mut battery_mon: board::battery::BatteryMonitor,
    chemistry: BatteryChemistry,
) -> ! {
    // Sample once so the frame shows real mV / %.
    let mv = battery_mon.sample().await;
    let status = BatteryStatus::from_reading(mv, true, chemistry);
    defmt::info!(
        "usb-wake: battery {=u16} mV ({=u8} %)",
        status.voltage_mv,
        status.percent
    );

    display.init().await;

    let mut widgets: WidgetList = WidgetList::new();
    app::build_charging_frame(status, &mut widgets);

    let mut renderer = Renderer::new();
    // SAFETY: same FRAMEBUFFER borrow as the normal-boot path, but
    // we're on the USB-wake branch so the normal path's borrow
    // never runs.  Single-owner across this boot.
    let fb: &'static mut Framebuffer = unsafe { &mut *core::ptr::addr_of_mut!(FRAMEBUFFER) };
    let scan = osrf_ui::ScanState::default();
    let _ = renderer.render(&widgets, &scan, fb);
    display.flush(fb).await;
    backlight.set_low();

    Timer::after_secs(2).await;

    // Teardown — backlight first, then panel VDD gate.  No flash
    // write: we got here because flash_intent was true and `run()`
    // never cleared it (Idle-fall-through is the only clear path).
    backlight.set_high();
    display.power_off().await;
    defmt::info!("usb-wake: charging frame done — re-entering System OFF");
    board::power::enter_system_off()
}

// ── Helpers ──────────────────────────────────────────────────────

/// Pull two random bytes from SD's RNG and pack into a `u16`.
/// Used once at boot for the session ID + TX boot-counter.
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

// Panic staging + recovery now live in
// `boards/t114/src/panic_record.rs`, gated behind the board crate's
// `panic-stage` Cargo feature (enabled in this profile's
// `Cargo.toml`).
