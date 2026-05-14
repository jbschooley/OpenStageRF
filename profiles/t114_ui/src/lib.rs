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
    run_rx, run_tx, AeadConfig, AeadUpdate, CipherId, Direction, LinkConfig, UartMidiSink,
    UartMidiSource,
};
use osrf_app_ui_runtime as app;
use osrf_board_t114 as board;
use osrf_driver_input_joystick5way::Joystick5Way;
use osrf_ui::{
    BatteryChemistry, BatteryStatus, KeyStore, PowerPolicy, Renderer, Role, Settings, UiState,
    Widget, WidgetList,
};

use board::embassy_nrf::gpio::{Input, Output, Pull};
use board::embassy_nrf::interrupt::{self, InterruptExt, Priority};
use board::embassy_nrf::wdt::{Config as WdtConfig, Watchdog, WatchdogHandle};
use board::framebuffer::Framebuffer;

// ── Profile-level configuration ─────────────────────────────────

/// Battery chemistry for this build.  Stock T114 ships with a
/// single-cell LiPo pouch; swap to `BatteryChemistry::NimhPack
/// { cells: 3 }` if you've replaced the cell with a 3-AA NiMH
/// holder.  See `docs/hardware_guides/battery_options.md` for the
/// full per-chemistry hardware-mod story.
const CHEMISTRY: BatteryChemistry = BatteryChemistry::LiPoSingle;

/// Power policy.  `Battery` (default) = handheld with explicit
/// user control over on/off.  `Wired` = permanent-install: device
/// tracks the host's USB power and auto-soft-offs ~10 s after USB
/// is lost.
const POWER_POLICY: PowerPolicy = PowerPolicy::Wired;

// ── Stage 3: hardcoded AEAD test keys (paired-units testing) ──────
//
// Until Stage 4 BLE provisioning lands there's no out-of-band way to
// transfer a key between paired units, so testing AEAD on the full
// UI profile uses these compiled-in keys.  TX gets BOTH so the
// operator can flip between them in the Key menu; RX gets only
// `KEY_SHARED` — `KEY_TX_ONLY` is intentionally missing on RX so
// you can demonstrate the rejection path by picking it on TX and
// watching RX log `KeyFpMismatch`.
//
// Same bytes on both ends → same fingerprint → packets accepted.
// Different bytes → `RxDrop::KeyFpMismatch` (or `AeadFail` if
// fingerprints happen to collide).

const KEY_SHARED: [u8; 32] = [0x42; 32];
const KEY_SHARED_NAME: &str = "Shared";
const KEY_SHARED_AES: [u8; 32] = [0x55; 32];
const KEY_SHARED_AES_NAME: &str = "Shared (AES)";
const KEY_TX_ONLY: [u8; 32] = [0x99; 32];
const KEY_TX_ONLY_NAME: &str = "TX-Only";
/// Default cipher for the `KEY_SHARED` / `KEY_TX_ONLY` entries
/// (software ChaCha20-Poly1305 on the nRF52840 — no hardware
/// support).  `KEY_SHARED_AES` uses [`CipherId::Aes128Ccm`] so the
/// `aes-hw-sd` feature's hardware AES path (`sd_ecb_block_encrypt`
/// SVC) gets exercised when the operator picks it.
const TEST_CIPHER: CipherId = CipherId::ChaCha20Poly1305;
const TEST_CIPHER_AES: CipherId = CipherId::Aes128Ccm;
/// Hardcoded device_id so paired units agree without exchanging
/// FICR.DEVICEID values out-of-band.  Stage 4 / multi-device
/// deployments will switch this to `board::device_id::device_id()`
/// + an RX-side allowlist.
const TEST_DEVICE_ID: u32 = 0x0000_0001;

/// Build the AEAD context for a given hardcoded key + cipher.
/// `device_id` and `direction` are profile-wide constants.
fn ctx_for_key(key: [u8; 32], cipher: CipherId) -> AeadConfig {
    AeadConfig {
        cipher,
        key,
        device_id: TEST_DEVICE_ID,
        direction: Direction::TxToRx,
    }
}

fn fp_for_key(key: &[u8; 32], cipher: CipherId) -> u32 {
    osrf_app_midi_node::osrf_crypto::fingerprint(cipher, key) & 0x00FF_FFFF
}

/// Resolver for the **TX** side: operator selects a key in the UI →
/// link sender encrypts subsequent packets with it.  Picking Open
/// drops to plaintext.  An unknown fingerprint (shouldn't happen
/// since the keystore only contains the keys we know) falls back to
/// plaintext as a safe default.
fn tx_aead_resolver(active_fp: Option<u32>) -> AeadUpdate {
    let shared_fp = fp_for_key(&KEY_SHARED, TEST_CIPHER);
    let shared_aes_fp = fp_for_key(&KEY_SHARED_AES, TEST_CIPHER_AES);
    let tx_only_fp = fp_for_key(&KEY_TX_ONLY, TEST_CIPHER);
    let masked = active_fp.map(|fp| fp & 0x00FF_FFFF);
    match masked {
        None => AeadUpdate {
            aead: None,
            allow_open: true,
        },
        Some(fp) if fp == shared_fp => AeadUpdate {
            aead: Some(ctx_for_key(KEY_SHARED, TEST_CIPHER)),
            allow_open: false,
        },
        Some(fp) if fp == shared_aes_fp => AeadUpdate {
            aead: Some(ctx_for_key(KEY_SHARED_AES, TEST_CIPHER_AES)),
            allow_open: false,
        },
        Some(fp) if fp == tx_only_fp => AeadUpdate {
            aead: Some(ctx_for_key(KEY_TX_ONLY, TEST_CIPHER)),
            allow_open: false,
        },
        Some(_) => AeadUpdate {
            aead: None,
            allow_open: true,
        },
    }
}

/// Resolver for the **RX** side.  RX only holds the `Shared` key
/// material; that's the only fingerprint it can decrypt.
///
/// * Open / Auto (`active_fp = None`): permissive — accept the
///   `Shared` key OR plaintext.  No filter.
/// * Specific selection matching `Shared`: strict — accept that
///   fingerprint only, reject plaintext.
/// * Specific selection that doesn't match anything we hold:
///   refuse everything (RX shouldn't actually let this state happen
///   in the menu, but the resolver stays defensive).
fn rx_aead_resolver(active_fp: Option<u32>) -> AeadUpdate {
    let shared_fp = fp_for_key(&KEY_SHARED, TEST_CIPHER);
    let shared_aes_fp = fp_for_key(&KEY_SHARED_AES, TEST_CIPHER_AES);
    let masked = active_fp.map(|fp| fp & 0x00FF_FFFF);
    match masked {
        None => AeadUpdate {
            // Auto/Open mode falls back to whichever key the operator
            // had picked previously (or defaults to `Shared` ChaCha
            // on a never-configured boot).  Multi-key keyring is a
            // follow-up; today RX can only have one cipher armed
            // simultaneously.
            aead: Some(ctx_for_key(KEY_SHARED, TEST_CIPHER)),
            allow_open: true,
        },
        Some(fp) if fp == shared_fp => AeadUpdate {
            aead: Some(ctx_for_key(KEY_SHARED, TEST_CIPHER)),
            allow_open: false,
        },
        Some(fp) if fp == shared_aes_fp => AeadUpdate {
            aead: Some(ctx_for_key(KEY_SHARED_AES, TEST_CIPHER_AES)),
            allow_open: false,
        },
        Some(_) => AeadUpdate {
            aead: None,
            allow_open: false,
        },
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
pub async fn run(spawner: Spawner, role: Role, tx_source: TxSource) -> ! {
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
    let r = board::resources();

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
    if matches!(POWER_POLICY, PowerPolicy::Wired) {
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
                usb_wake_charging_frame(r.display, r.display_backlight, r.battery).await;
            }
            board::power::WakeSource::UsbPlug => {
                app::unexpected_wake_resleep(board::power::enter_system_off);
            }
            board::power::WakeSource::ColdBoot if flash_intent && vbus_at_boot => {
                defmt::info!(
                    "ui: wake = ColdBoot + flash_intent + VBUS → charging frame \
                     (probable brown-out from USB plug-in)"
                );
                usb_wake_charging_frame(r.display, r.display_backlight, r.battery).await;
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

    let mut state = UiState::with_role(role);
    let mut settings = Settings::default();
    app::load_settings(&mut flash, board::storage::SETTINGS_RANGE, &mut settings).await;
    let mut keys = KeyStore::new();
    app::load_keys(&mut flash, board::storage::KEY_STORE_RANGE, &mut keys).await;

    // Register the hardcoded test keys in the runtime keystore so
    // the UI's Key list shows them as selectable entries.
    //
    // Per-role registration:
    //   * Both TX and RX get `Shared` (ChaCha20) and `Shared (AES)`
    //     so the operator can switch ciphers while still talking to
    //     the same peer — useful for exercising the hardware-AES
    //     path independently from the software-ChaCha path.
    //   * TX additionally gets `TX-Only` so picking it on TX
    //     reproduces the documented mismatch case (RX doesn't have
    //     this key → `RxDrop::KeyFpMismatch` on every packet).
    let shared_fp = fp_for_key(&KEY_SHARED, TEST_CIPHER);
    if keys.find(shared_fp).is_none() {
        let _ = keys.add(KEY_SHARED_NAME, TEST_CIPHER, shared_fp);
    }
    let shared_aes_fp = fp_for_key(&KEY_SHARED_AES, TEST_CIPHER_AES);
    if keys.find(shared_aes_fp).is_none() {
        let _ = keys.add(KEY_SHARED_AES_NAME, TEST_CIPHER_AES, shared_aes_fp);
    }
    if matches!(role, Role::Tx) {
        let tx_only_fp = fp_for_key(&KEY_TX_ONLY, TEST_CIPHER);
        if keys.find(tx_only_fp).is_none() {
            let _ = keys.add(KEY_TX_ONLY_NAME, TEST_CIPHER, tx_only_fp);
        }
    }

    // Pick the per-role resolver + compute the boot-time AEAD config
    // from the persisted `active_key_fp`.  Subsequent operator key
    // changes are applied live by `ui_state_loop` → `AEAD_UPDATES`
    // → the runtime's top-of-loop `try_take`; no reboot needed.
    let aead_resolver: fn(Option<u32>) -> AeadUpdate = match role {
        Role::Tx => tx_aead_resolver,
        Role::Rx => rx_aead_resolver,
    };
    let initial_update = aead_resolver(settings.active_key_fp);
    let initial_aead = initial_update.aead;
    let initial_allow_open = initial_update.allow_open;

    // RX-side keyring: full set of keys the receiver has material
    // for, so Auto/Open mode can decrypt any of them.  Built once
    // at boot — operator key changes only flip the strict filter
    // (see `rx_aead_resolver`).
    let mut rx_keyring: heapless::Vec<AeadConfig, { osrf_app_midi_node::MAX_RX_KEYS }> =
        heapless::Vec::new();
    let _ = rx_keyring.push(ctx_for_key(KEY_SHARED, TEST_CIPHER));
    let _ = rx_keyring.push(ctx_for_key(KEY_SHARED_AES, TEST_CIPHER_AES));
    let rx_initial_filter = initial_aead.as_ref().map(osrf_app_midi_node::aead_fp);

    defmt::info!(
        "t114_ui: boot AEAD aead={=bool} allow_open={=bool} rx_keyring={=usize}",
        initial_aead.is_some(),
        initial_allow_open,
        rx_keyring.len(),
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
    spawner.spawn(battery_task(r.battery).expect("alloc battery_task"));

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
            spawner_link.spawn(
                link_rx_task(
                    r.radio0,
                    r.status_led,
                    sink,
                    config,
                    rx_keyring,
                    rx_initial_filter,
                    initial_allow_open,
                )
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
        CHEMISTRY,
        POWER_POLICY,
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
    keyring: heapless::Vec<AeadConfig, { osrf_app_midi_node::MAX_RX_KEYS }>,
    initial_filter: Option<osrf_app_midi_node::KeyFp>,
    initial_allow_open: bool,
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
        keyring,
        initial_filter,
        initial_allow_open,
        Some(&app::AEAD_UPDATES),
    )
    .await
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
async fn battery_task(monitor: board::battery::BatteryMonitor) -> ! {
    app::battery_loop(ProfileBattery(monitor), CHEMISTRY).await
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
) -> ! {
    // Sample once so the frame shows real mV / %.
    let mv = battery_mon.sample().await;
    let status = BatteryStatus::from_reading(mv, true, CHEMISTRY);
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
