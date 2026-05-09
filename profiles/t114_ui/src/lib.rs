// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

//! Milestone 6 UI smoke test, T114 deployment.  Shared between
//! the [`ui_tx`](../bin/ui_tx.rs) and [`ui_rx`](../bin/ui_rx.rs)
//! binaries — each picks a [`Role`] and calls [`run`].
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
//! Two cooperating tasks:
//!   - `joystick_task` waits for joystick events and feeds them
//!     into a [`Channel`].
//!   - The main task awaits the channel, runs `UiState::handle_event`,
//!     re-renders, and logs any `Command` for debugging (no live
//!     link wiring yet — that lands when we plumb a config-update
//!     signal into `osrf-link-runtime`).

use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer};
use osrf_app_midi_node::{run_rx, run_tx, LinkConfig, UartMidiSink, UartMidiSource};
use osrf_board_t114 as board;
use osrf_driver_input_joystick5way::{Joystick5Way, JoystickEvent};
use osrf_link_runtime::LinkStatsCell;
use osrf_ui::{
    build_screen, KeyStore, LinkStatus, Renderer, Role, ScreenId, Settings, UiState, WidgetList,
    MAX_SCAN_CHANNELS,
};

use board::embassy_nrf::gpio::{Input, Pull};
use board::framebuffer::Framebuffer;

/// Joystick pin types per `boards/t114/src/lib.rs::joystick`.
type Joystick = Joystick5Way<
    Input<'static>,
    Input<'static>,
    Input<'static>,
    Input<'static>,
    Input<'static>,
>;

/// Input event channel — joystick task pushes, main task pops.
/// Capacity 8: more than enough for human input rates.
static EVENT_CHAN: Channel<CriticalSectionRawMutex, JoystickEvent, 8> = Channel::new();

/// Cross-task shared link-runtime stats.  `run_rx` / `run_tx` write
/// counters + RSSI + link-up here on every loop iteration; the UI
/// loop snapshots the latest values each render and turns them into
/// a `LinkStatus` for the Idle / Link Stats screens.
static STATS: LinkStatsCell = LinkStatsCell::new();

/// 64 KB in-RAM framebuffer the renderer paints into (sync via
/// `DrawTarget`).  After each render we hand it to
/// `board::Display::flush(...).await`, which streams the dirty box to
/// the panel via async SPI — yielding the executor between DMA bursts
/// so `run_rx` can keep up with the SX1262 RX FIFO.  Single-instance,
/// owned by `run()`; `static mut` because `Framebuffer::new()` is
/// `const fn` so the array goes in BSS rather than blowing the stack
/// at startup.
static mut FRAMEBUFFER: Framebuffer = Framebuffer::new();

/// Bring up display + joystick + UI state machine for the given
/// link [`Role`] and run the main loop forever.  Called from each
/// binary's `#[embassy_executor::main]` after the `pre_init`
/// bootloader hand-off.
pub async fn run(spawner: Spawner, role: Role) -> ! {
    defmt::info!("ui (T114, {:?}): bringing up SD + display + joystick + link", role);

    // Order: `embassy_nrf::init()` (inside `board::resources()`)
    // **must** come before `Softdevice::enable()` — SD claims CLOCK +
    // POWER on activation; embassy can no longer configure those
    // afterwards.  See `boards/t114/src/softdevice.rs` module docs
    // for the full SD setup contract.
    let mut r = board::resources();
    let sd = board::softdevice::enable();
    spawner.spawn(board::softdevice::run(sd).expect("alloc softdevice run task"));

    // ── Display ─────────────────────────────────────────────────────────────
    let mut display = r.display;
    display.init().await;
    // SAFETY: `run()` is called exactly once per binary lifetime
    // (from `#[embassy_executor::main]`), so this is the only place
    // FRAMEBUFFER is borrowed.
    let fb: &'static mut Framebuffer = unsafe { &mut *core::ptr::addr_of_mut!(FRAMEBUFFER) };

    // ── Joystick ────────────────────────────────────────────────────────────
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

    // ── UI state ────────────────────────────────────────────────────────────
    let mut state = UiState::with_role(role);
    let mut settings = Settings::default();
    // Placeholder keys, hidden from MAIN_MENU until AEAD lands.
    let mut keys = KeyStore::new();
    let _ = keys.add("Studio A", 0x111111);
    let _ = keys.add("Backup", 0x222222);
    let mut widgets: WidgetList = WidgetList::new();
    let mut renderer = Renderer::new();

    // Initial paint — `STATS` is empty so this looks like the
    // pre-runtime stub did, but every subsequent render snapshots
    // fresh stats inside `ui_loop`.  Order: paint into `fb` (sync),
    // then flush the dirty region to the panel via async SPI, then
    // turn the backlight on — so the user never sees the panel's
    // power-on RAM contents.
    let initial_status = link_status_from_stats(&STATS.get());
    build_screen(&state, &settings, &keys, &initial_status, &mut widgets);
    let _ = renderer.render(&widgets, &state.scan, fb);
    display.flush(fb).await;
    r.display_backlight.set_low(); // backlight on (active LOW)
    defmt::info!("ui ready: role={:?} screen={:?}", role, state.screen);

    // ── Link runtime ───────────────────────────────────────────────────────
    // Fixed initial config — derived from `Settings::default()` at boot.
    // `Command::Apply*` from the UI doesn't yet retune the radio (live
    // reconfig is increment 4 of M6); for now `LinkConfig` matches
    // `default_915` and stays that way for the run.
    let config = link_config_from(&settings);

    // Run the UI loop and the link runtime concurrently in this task
    // via `embassy_futures::join`.  Both halves are infinite (`-> !`),
    // so the join itself never resolves — exactly what we want.
    match role {
        Role::Rx => {
            let mut sink = UartMidiSink::new(r.midi_uart);
            join(
                ui_loop(
                    &mut display,
                    fb,
                    &mut state,
                    &mut settings,
                    &keys,
                    &mut widgets,
                    &mut renderer,
                ),
                run_rx(&mut r.radio0, &mut r.status_led, &mut sink, &config, &STATS),
            )
            .await;
        }
        Role::Tx => {
            // Boot counter goes into the high 16 bits of the link-layer
            // `seq` and MUST change across resets so the receiver's
            // replay window doesn't reject our new low-seq packets as
            // ancient duplicates.  Pull via SD's RNG SVC — M7 will
            // replace this with a flash-persisted counter.
            let boot_counter = read_random_u16();
            defmt::info!("boot_counter = {} (random per-boot)", boot_counter);
            let mut source = UartMidiSource::new(r.midi_uart);
            join(
                ui_loop(
                    &mut display,
                    fb,
                    &mut state,
                    &mut settings,
                    &keys,
                    &mut widgets,
                    &mut renderer,
                ),
                run_tx(
                    &mut r.radio0,
                    &mut r.status_led,
                    &mut source,
                    boot_counter,
                    &config,
                    &STATS,
                ),
            )
            .await;
        }
    }
    // Both halves of `join` return `!`, so we never get here.
    loop {}
}

/// UI event loop, factored out of `run()` so the radio runtime can
/// run concurrently in the same task via `embassy_futures::join`.
/// Snapshots [`STATS`] each render pass and translates it into a
/// `LinkStatus` for the Idle / Link Stats screens.  Returns `!` —
/// runs forever.
///
/// Render cadence is the scan tick (~3 Hz) on every screen.  This
/// used to be capped at 2 Hz on idle screens because the sync display
/// SPI blocked `run_rx` long enough to drop 5-12 % of inbound packets
/// — but [`board::Display::flush`] is now async, so the executor
/// yields between SPI bursts and the rate-limit hack is gone.
async fn ui_loop(
    display: &mut board::Display,
    fb: &mut Framebuffer,
    state: &mut UiState,
    settings: &mut Settings,
    keys: &KeyStore,
    widgets: &mut WidgetList,
    renderer: &mut Renderer,
) -> ! {
    let scan_tick = Duration::from_millis(300);

    loop {
        let next_tick = Timer::after(scan_tick);
        match select(EVENT_CHAN.receive(), next_tick).await {
            Either::First(event) => {
                if let Some(cmd) = state.handle_event(settings, keys, event) {
                    defmt::info!("ui command: {:?}", cmd);
                    // TODO (M6 increment 4): push the new LinkConfig
                    // to the runtime via Signal<LinkConfig>.
                }
            }
            Either::Second(()) => {
                if state.screen == ScreenId::Scan {
                    let mut buf = [0i16; MAX_SCAN_CHANNELS];
                    let n = state.scan.channel_count as usize;
                    synth_scan_pass(&mut buf[..n]);
                    state.apply_scan_pass(&buf[..n]);
                }
            }
        }

        // Render every iteration — the renderer's per-widget content
        // diff makes idle frames near-free (nothing changes → empty
        // dirty box → flush returns immediately), and the async SPI
        // path means even a full ScanGraph repaint doesn't starve
        // `run_rx`.
        let status = link_status_from_stats(&STATS.get());
        build_screen(state, settings, keys, &status, widgets);
        let _ = renderer.render(widgets, &state.scan, fb);
        display.flush(fb).await;
    }
}

/// Translate `osrf-link-runtime`'s `LinkStats` snapshot into the
/// `osrf-ui` `LinkStatus` shape that the renderer expects.  Most
/// fields map 1:1; `recent_loss_pct` is left `None` for now since
/// the runtime doesn't currently expose a sliding-window loss
/// percentage in the cell (it logs one to RTT but doesn't store it).
fn link_status_from_stats(s: &osrf_link_runtime::LinkStats) -> LinkStatus {
    LinkStatus {
        up: s.link_up,
        // SX1262 RSSI values comfortably fit in i8 (-120..-10 dBm
        // range); clamp at conversion time to be safe.
        last_rssi_dbm: s.last_rssi_dbm.map(|r| r.clamp(i8::MIN as i16, i8::MAX as i16) as i8),
        recent_loss_pct: s.recent_loss_pct,
        total_accepted: s.total_accepted,
        stuck_recoveries: s.stuck_recoveries,
    }
}

/// Build a [`LinkConfig`] from the UI's [`Settings`].  Today only
/// `frequency_hz` and `tx_power_dbm` flow through — the rest stays at
/// `default_915()` values.  When live reconfig lands (M6 increment 4)
/// this is what `Command::Apply*` translates into.
fn link_config_from(settings: &Settings) -> LinkConfig {
    let mut c = LinkConfig::default_915();
    c.frequency_hz = settings.current_channel().frequency_khz * 1000;
    c.tx_power_dbm = settings.tx_power_dbm;
    c
}

/// Pull two random bytes from SD's RNG and pack into a `u16`.  Used
/// once at boot for the link-layer `boot_counter` (high 16 bits of
/// the 48-bit `seq`).  Replace with a flash-persisted counter in M7.
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

/// Stub scanner: synthesize per-channel noise floors that move
/// over time so the Scan screen visibly updates without a real
/// radio.  Each channel oscillates around its own baseline at a
/// distinct phase, with one occasional spike per pass to exercise
/// the peak-tick render path.  Replace with a real
/// `link_runtime.scan_step(...)` call once live config-update
/// plumbing lands.
fn synth_scan_pass(out: &mut [i16]) {
    let n = out.len();
    let t = Instant::now().as_millis() as i32;
    let spike_target = (t / 600) as usize % n.max(1);
    for (i, slot) in out.iter_mut().enumerate() {
        let phase = ((t / 50) + (i as i32) * 30) % 240;
        let tri = if phase < 120 { phase } else { 240 - phase };
        let baseline = -100 - (i as i32 % 5);
        let mut dbm = baseline + (tri / 10);
        if spike_target == i {
            dbm += 18;
        }
        *slot = dbm as i16;
    }
}

#[embassy_executor::task]
async fn joystick_task(mut js: Joystick) {
    loop {
        let ev = js.next_event().await;
        EVENT_CHAN.send(ev).await;
    }
}

