// SPDX-License-Identifier: AGPL-3.0-or-later
#![cfg_attr(not(test), no_std)]

//! Display-agnostic UI core for OpenStageRF.
//!
//! Three layers, all independent of the actual display hardware:
//!
//! 1. [`Settings`] — pure data: channel, TX power, key slot, etc.
//!    Adding a new editable setting is one field added here plus
//!    one `Selector` widget added to the relevant screen builder.
//! 2. [`UiState`] — the screen state machine: which `ScreenId` is
//!    active, where the cursor is, are we in edit mode, the
//!    in-progress numeric edit buffer.
//!    [`UiState::handle_event`] consumes a [`JoystickEvent`] from
//!    the joystick driver, mutates state, optionally mutates
//!    settings, optionally returns a [`Command`] for the host (e.g.
//!    "apply this channel change to the live link").
//! 3. [`build_screen`] — given `(state, settings, link_status)`,
//!    fills a [`WidgetList`] with [`Widget`] values describing what
//!    should appear on screen.  No pixel writes; that's the
//!    renderer's job.
//!
//! The renderer (separate module / crate) iterates the widget list
//! and paints each variant to a `DrawTarget`.  Same widgets render
//! the same way on monochrome OLED and colour TFT — colour-only
//! affordances (red `LinkLost` banner) live behind type bounds in
//! the renderer, not here.
//!
//! ## Testing
//!
//! The state machine is host-testable: feed events into
//! [`UiState::handle_event`] and assert on the resulting state and
//! commands.  See `lib.rs` test module.

use core::fmt::Write as _;
use heapless::{String, Vec};

pub use osrf_driver_input_joystick5way::{Direction, JoystickEvent};

pub mod band_plan;
pub mod key_store;
pub mod render;

pub use band_plan::{
    channel as band_plan_channel, max_channel_index, BandPlan, BandPlanInfo, ChannelInfo,
    BAND_PLANS,
};
pub use key_store::{KeyEntry, KeyStore, MAX_KEY_NAME, MAX_KEYS};
pub use render::{render, Renderer};

// ── Settings ────────────────────────────────────────────────────────────────

/// Editable + persistable settings exposed by the UI.  All changes
/// flow through [`UiState::handle_event`]; the host can serialise
/// [`Settings`] to flash on every change confirmation (M7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Settings {
    /// Active band plan — selects which set of channels the user
    /// can pick from on the ChannelSelect screen.  Different
    /// plans have different channel layouts (e.g. for
    /// coordination with Sennheiser or Shure pro gear).  See
    /// [`band_plan`] for available plans.
    pub band_plan: BandPlan,
    /// Active channel index within the current band plan.  The
    /// resolved frequency is `band_plan_channel(band_plan, channel).frequency_khz`.
    pub channel: u8,
    /// SX1262 TX power in dBm.  Range −9..=+22 per the radio's
    /// spec; UI clamps to that.
    pub tx_power_dbm: i8,
    /// Active encryption key as a 24-bit fingerprint into the
    /// runtime [`KeyStore`].  `None` means "no encryption" — the
    /// UI's `Open` pseudo-entry on the KeySelect screen.  The
    /// link layer's `key_fp` header field is `0` when this is
    /// `None`, otherwise the low 24 bits of this value.
    pub active_key_fp: Option<u32>,
}

/// Minimum TX power (per SX1262 spec).
pub const MIN_TX_POWER_DBM: i8 = -9;
/// Maximum TX power (per SX1262 spec).
pub const MAX_TX_POWER_DBM: i8 = 22;

impl Default for Settings {
    fn default() -> Self {
        Self {
            band_plan: BandPlan::Ism915,
            channel: 0,
            tx_power_dbm: 22,
            active_key_fp: None,
        }
    }
}

impl Settings {
    /// Resolve the current channel to its [`ChannelInfo`] (label +
    /// frequency).  Helper used by both the renderer and the link-
    /// runtime adapter.
    pub fn current_channel(&self) -> ChannelInfo {
        band_plan_channel(self.band_plan, self.channel)
    }
}

// ── Link status (read-only, fed by the link runtime) ────────────────────────

/// Snapshot of the link runtime state for the UI to display.  The
/// host updates this from the run_tx/run_rx loop and the UI reads
/// it on every render — no mutation from the UI side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct LinkStatus {
    /// Watchdog says the peer is alive.
    pub up: bool,
    /// Last packet RSSI in dBm, if any packet has been received.
    pub last_rssi_dbm: Option<i8>,
    /// Recent packet loss as a percentage (0..=100).
    pub recent_loss_pct: Option<u8>,
    /// Total accepted packets since boot.
    pub total_accepted: u32,
    /// Total stuck-note recoveries fired (heartbeat-state failsafe).
    pub stuck_recoveries: u32,
}

impl Default for LinkStatus {
    fn default() -> Self {
        Self {
            up: false,
            last_rssi_dbm: None,
            recent_loss_pct: None,
            total_accepted: 0,
            stuck_recoveries: 0,
        }
    }
}

// ── Screen IDs ──────────────────────────────────────────────────────────────

/// Logical screens, mirroring `docs/ui_design.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum ScreenId {
    /// Default screen after boot / quick-action exit.
    Idle,
    /// A menu of items.  *Which* menu is held in
    /// [`UiState::current_menu`] — Menu is a generic container,
    /// content is data-driven from a [`MenuNode`].
    Menu,
    /// Channel scanner — bar graph of per-channel current and
    /// peak-since-open noise floor.  Center applies the cursor's
    /// channel as the new active channel.
    Scan,
    /// Channel selector — scrollable list of channels in the
    /// current band plan, each showing label + frequency.
    ChannelSelect,
    /// Band-plan selector — pick which named channel layout the
    /// device uses (e.g. ISM 915 default, Sennheiser-compat,
    /// Shure-compat).
    BandPlanSelect,
    /// Crypto key slot selector (stub for v1).
    KeySelect,
    /// TX power selector — numeric edit-buffer pattern.
    PowerSelect,
    /// Live link statistics (read-only).
    LinkStats,
    /// Firmware version + boot counter.
    About,
}

impl ScreenId {
    /// Human-readable label for a screen — used as the row text in
    /// MainMenu and as the title bar on each submenu.
    pub fn label(&self) -> &'static str {
        match self {
            ScreenId::Idle => "Idle",
            ScreenId::Menu => "Menu",
            ScreenId::Scan => "Scan",
            ScreenId::ChannelSelect => "Channel",
            ScreenId::BandPlanSelect => "Band Plan",
            ScreenId::KeySelect => "Key",
            ScreenId::PowerSelect => "TX Power",
            ScreenId::LinkStats => "Link Stats",
            ScreenId::About => "About",
        }
    }
}

// ── Menu tree ───────────────────────────────────────────────────────────────
//
// Menus are pure data: a [`MenuNode`] is a title + a list of
// [`MenuItem`]s, each of which carries a label and an
// [`ItemAction`].  The state machine walks this tree generically
// — adding a new submenu means adding a `static FOO_MENU:
// MenuNode = ...` and a `MenuItem` referencing it from a parent,
// no match-arm edits.
//
// Custom screens (Idle, LinkStats, About) sit outside the tree;
// menus link to them via [`ItemAction::Custom(ScreenId::...)`]
// and the per-screen `handle_*` / `build_*` functions take over
// once entered.

/// One row in a menu — a display label plus what activating it does.
#[derive(Debug, Clone, Copy)]
pub struct MenuItem {
    pub label: &'static str,
    pub action: ItemAction,
}

/// What pressing Center / Right on a menu row does.
#[derive(Debug, Clone, Copy)]
pub enum ItemAction {
    /// Descend into a child menu.  The state machine pushes the
    /// current frame and switches `current_menu` to the new node.
    Submenu(&'static MenuNode),
    /// Open a list-select screen of the given kind (Channel,
    /// Band Plan, Key).
    List(ListKind),
    /// Open a value-edit screen of the given kind (TX Power).
    Value(ValueKind),
    /// Open a custom screen by ID — escape hatch for screens that
    /// don't fit the list-or-value mould (LinkStats readout, About,
    /// future status displays).
    Custom(ScreenId),
}

/// A menu — a title (drawn as the screen's title bar) and a
/// static list of items.  Submenus link here via
/// [`ItemAction::Submenu`].
#[derive(Debug)]
pub struct MenuNode {
    pub title: &'static str,
    pub items: &'static [MenuItem],
}

/// Which side of the link this firmware is running.  Set once at
/// boot per profile binary; the UI core branches on it for the
/// menu (TX Power vs Link Stats) and the Idle screen (TX power
/// readout vs RSSI / link-up indicator).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Role {
    /// Transmitter — owns TX Power, can't observe link liveness
    /// (no ACK in v1).  No KeyStore-AEAD on TX path until Stage 3.
    Tx,
    /// Receiver — owns the watchdog / Link Stats; TX Power is
    /// meaningless (RX doesn't transmit).
    Rx,
}

impl Role {
    /// The MAIN_MENU appropriate for this role.  Used by the UI
    /// core to pick which top-level menu Idle → Menu enters.
    pub fn main_menu(self) -> &'static MenuNode {
        match self {
            Role::Tx => &MAIN_MENU_TX,
            Role::Rx => &MAIN_MENU_RX,
        }
    }

    /// Idle-screen banner suffix — `"TX"` or `"RX"`.
    pub fn label(self) -> &'static str {
        match self {
            Role::Tx => "TX",
            Role::Rx => "RX",
        }
    }
}

/// TX-side top menu: channel + scan + band plan + TX power +
/// about.  No Link Stats (v1 has no ACK channel; the TX has no
/// way to observe whether RX is alive).
pub static MAIN_MENU_TX: MenuNode = MenuNode {
    title: "Menu",
    items: &[
        MenuItem { label: "Channel",    action: ItemAction::List(ListKind::Channel) },
        MenuItem { label: "Scan",       action: ItemAction::Custom(ScreenId::Scan) },
        MenuItem { label: "Band Plan",  action: ItemAction::List(ListKind::BandPlan) },
        MenuItem { label: "TX Power",   action: ItemAction::Value(ValueKind::TxPower) },
        // Key entry hidden until AEAD lands (Stage 3 in ROADMAP.md).
        // MenuItem { label: "Key",        action: ItemAction::List(ListKind::Key) },
        MenuItem { label: "About",      action: ItemAction::Custom(ScreenId::About) },
    ],
};

/// RX-side top menu: channel + scan + band plan + link stats +
/// about.  No TX Power (RX doesn't transmit).
pub static MAIN_MENU_RX: MenuNode = MenuNode {
    title: "Menu",
    items: &[
        MenuItem { label: "Channel",    action: ItemAction::List(ListKind::Channel) },
        MenuItem { label: "Scan",       action: ItemAction::Custom(ScreenId::Scan) },
        MenuItem { label: "Band Plan",  action: ItemAction::List(ListKind::BandPlan) },
        // MenuItem { label: "Key",        action: ItemAction::List(ListKind::Key) },
        MenuItem { label: "Link Stats", action: ItemAction::Custom(ScreenId::LinkStats) },
        MenuItem { label: "About",      action: ItemAction::Custom(ScreenId::About) },
    ],
};

// ── Scan state ──────────────────────────────────────────────────────────────

/// Maximum number of channels the scan screen can track at once.
/// Sized for the Wide 200 kHz plan (131 channels) with headroom.
/// Plans with fewer channels pad and use only the first
/// `channel_count` slots.  Memory cost is `4 * MAX_SCAN_CHANNELS`
/// bytes per [`ScanState`] (current + peak as `i16`).
pub const MAX_SCAN_CHANNELS: usize = 144;

/// Sentinel for "no RSSI sample yet" in [`ScanState`].  The
/// renderer treats this as "draw nothing" for that channel.
pub const SCAN_NO_DATA: i16 = i16::MIN;

/// Per-channel scan data: current noise-floor RSSI and peak (max)
/// observed since this scan session began.  Profile drives one
/// pass at a time via [`UiState::apply_scan_pass`]; reset happens
/// automatically on entering [`ScreenId::Scan`].
#[derive(Debug, Clone)]
pub struct ScanState {
    /// Current noise floor in dBm per channel index in the active
    /// band plan.  `SCAN_NO_DATA` until the first pass populates.
    pub current_dbm: [i16; MAX_SCAN_CHANNELS],
    /// Peak (highest) noise floor observed per channel since the
    /// last reset.  `SCAN_NO_DATA` until populated; never decays.
    pub peak_dbm: [i16; MAX_SCAN_CHANNELS],
    /// Number of valid entries (entries `0..channel_count` are
    /// meaningful; the rest are stale padding).  Set on entry.
    pub channel_count: u8,
}

impl Default for ScanState {
    fn default() -> Self {
        Self {
            current_dbm: [SCAN_NO_DATA; MAX_SCAN_CHANNELS],
            peak_dbm: [SCAN_NO_DATA; MAX_SCAN_CHANNELS],
            channel_count: 0,
        }
    }
}

impl ScanState {
    /// Reset all per-channel data and bind the scan to a band
    /// plan's channel count.  Called on entering the Scan screen.
    pub fn reset(&mut self, plan: BandPlan) {
        let n = plan.info().channels.len().min(MAX_SCAN_CHANNELS);
        for i in 0..MAX_SCAN_CHANNELS {
            self.current_dbm[i] = SCAN_NO_DATA;
            self.peak_dbm[i] = SCAN_NO_DATA;
        }
        self.channel_count = n as u8;
    }
}

// ── UiState ────────────────────────────────────────────────────────────────

/// Maximum navigation stack depth.  Each `enter()` pushes one
/// [`NavFrame`]; `pop_nav()` restores the top frame.  Sized for
/// plausible UI depth on a small device — Idle → MainMenu →
/// submenu → sub-submenu fits in 3, leaving room.  Pushes past
/// the limit are silently dropped (the user gets a one-frame
/// shorter back-trail; no panic).
pub const MAX_NAV_DEPTH: usize = 4;

/// One frame on the navigation stack — captures enough to
/// restore a parent screen exactly: which screen, which menu
/// (if it was a Menu screen), cursor, scroll.
#[derive(Debug, Clone, Copy)]
pub struct NavFrame {
    pub screen: ScreenId,
    /// Menu pointer at the time of push.  Always present (even
    /// for non-Menu screens it holds the most-recent menu we
    /// were on, so popping back to a non-menu state still
    /// leaves `current_menu` sensible).
    pub menu: &'static MenuNode,
    pub cursor: u8,
    pub scroll: u8,
}

/// Active UI state.  Held statically for the lifetime of the
/// program; `handle_event` mutates it in place.
///
/// Not `defmt::Format` (`heapless::Vec` doesn't impl it) and
/// not `PartialEq` (the `&'static MenuNode` field complicates
/// derive); callers print individual fields and never compare
/// whole states.
#[derive(Debug, Clone)]
pub struct UiState {
    /// Which side of the link this firmware is running.  Picked
    /// at construction by the profile binary
    /// ([`UiState::with_role`]).  Selects the top-level menu and
    /// drives the Idle screen's content.
    pub role: Role,
    /// Current screen.
    pub screen: ScreenId,
    /// Currently active menu definition.  Meaningful when
    /// [`Self::screen`] is `ScreenId::Menu`; otherwise it's the
    /// menu we were last on (so popping back into a menu screen
    /// has the right context).
    pub current_menu: &'static MenuNode,
    /// Cursor index on the current screen.  For list-based screens
    /// (ChannelSelect, BandPlanSelect, Menu) this is the index of
    /// the selected list item.  For value-edit screens
    /// (PowerSelect, KeySelect) this is unused (always 0).
    pub cursor: u8,
    /// Vertical scroll offset for list-based screens.  The list
    /// item at cursor index `cursor` should be visible — the
    /// state machine adjusts `scroll_offset` to keep that
    /// invariant when the cursor moves past the visible window.
    pub scroll_offset: u8,
    /// True while the user is editing a numeric value (TX power,
    /// key slot).  Up / Down adjust [`UiState::edit_buffer`];
    /// Center applies and exits; Left cancels.  Always false on
    /// list-based screens — those apply on Center directly.
    pub edit_mode: bool,
    /// Numeric edit buffer used while `edit_mode` is true.
    /// Initialised from the relevant settings field on entry into
    /// edit mode; written back on Center confirm.  Held as an `i32`
    /// so it can carry a TX-power negative range without overflow.
    pub edit_buffer: i32,
    /// Parent navigation stack.  Top of stack is the immediate
    /// parent of [`Self::screen`]; popping restores that parent
    /// with its cursor + scroll preserved.  Empty at boot (Idle
    /// is the root and has no parent).
    pub nav_stack: Vec<NavFrame, MAX_NAV_DEPTH>,
    /// Channel-scan state.  Meaningful when [`Self::screen`] is
    /// `ScreenId::Scan`; reset on every entry to that screen.
    /// Updated by the profile's scan loop via
    /// [`Self::apply_scan_pass`] after each completed pass.
    pub scan: ScanState,
}

impl Default for UiState {
    fn default() -> Self {
        // Role::Rx is the default; profile binaries that want a
        // TX UI use [`UiState::with_role(Role::Tx)`] instead.
        Self {
            role: Role::Rx,
            screen: ScreenId::Idle,
            current_menu: Role::Rx.main_menu(),
            cursor: 0,
            scroll_offset: 0,
            edit_mode: false,
            edit_buffer: 0,
            nav_stack: Vec::new(),
            scan: ScanState::default(),
        }
    }
}

impl UiState {
    /// Construct a UiState bound to the given side of the link.
    /// Picks the role-appropriate top menu and Idle layout.
    pub fn with_role(role: Role) -> Self {
        Self {
            role,
            current_menu: role.main_menu(),
            ..Self::default()
        }
    }
}

/// Number of list items visible on one screen of a list-based
/// selector (after the title row, before the footer).  At
/// FONT_9X18 with 19 px row pitch on a 240×135 panel, we fit 5
/// body rows.  Cursor outside this window scrolls the list.
pub const VISIBLE_LIST_ROWS: u8 = 5;

/// Side-effect command emitted by [`UiState::handle_event`] when a
/// setting change should be propagated to the host (live-applied to
/// the link runtime, persisted, etc.).  The UI itself never
/// directly drives the radio — it just emits these commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Command {
    /// Channel index within the current band plan changed; host
    /// should resolve to a frequency via [`band_plan_channel`] and
    /// re-tune the radio.
    ApplyChannel(u8),
    /// Band plan changed; host should re-resolve the current
    /// channel within the new plan and re-tune.
    ApplyBandPlan(BandPlan),
    /// TX power changed; host should re-set on the radio.
    ApplyTxPower(i8),
    /// Active key changed.  `None` = no encryption (link layer
    /// emits `key_fp = 0`); `Some(fp)` = use the key with that
    /// fingerprint (host looks it up in [`KeyStore`]).
    ApplySetActiveKey(Option<u32>),
}

impl UiState {
    /// Process one [`JoystickEvent`].  Mutates self and possibly
    /// `settings`; returns a [`Command`] iff a setting confirmation
    /// fired.
    ///
    /// State machine, cross-cutting:
    /// * Long-press Center from anywhere → Idle (universal "go home").
    ///   Exception: from Idle, long-press Center → ChannelSelect
    ///   (per `docs/ui_design.md`'s quick-action shortcut).
    /// * Per-screen handling below.
    pub fn handle_event(
        &mut self,
        settings: &mut Settings,
        keys: &KeyStore,
        event: JoystickEvent,
    ) -> Option<Command> {
        // Universal: long-press Center.  Quick-action from Idle goes
        // to ChannelSelect; from anywhere else goes home.
        if matches!(event, JoystickEvent::LongPress(Direction::Center)) {
            if self.screen == ScreenId::Idle {
                self.enter(ScreenId::ChannelSelect, settings, keys);
            } else {
                self.go_home();
            }
            return None;
        }

        // Per-screen dispatch.
        match self.screen {
            ScreenId::Idle => self.handle_idle(event, settings, keys),
            ScreenId::Menu => self.handle_menu(event, settings, keys),
            ScreenId::Scan => self.handle_scan(event, settings),
            ScreenId::ChannelSelect => self.handle_list_select(event, settings, keys, ListKind::Channel),
            ScreenId::BandPlanSelect => {
                self.handle_list_select(event, settings, keys, ListKind::BandPlan)
            }
            ScreenId::PowerSelect => {
                self.handle_value_select(event, settings, ValueKind::TxPower)
            }
            ScreenId::KeySelect => self.handle_list_select(event, settings, keys, ListKind::Key),
            ScreenId::LinkStats | ScreenId::About => self.handle_readonly(event),
        }
    }

    fn handle_idle(
        &mut self,
        event: JoystickEvent,
        _settings: &Settings,
        _keys: &KeyStore,
    ) -> Option<Command> {
        match event {
            JoystickEvent::Press(Direction::Center)
            | JoystickEvent::Press(Direction::Right) => {
                self.enter_menu(self.role.main_menu());
            }
            _ => {}
        }
        None
    }

    /// Generic menu handler — drives any [`MenuNode`] held in
    /// `self.current_menu`.  Up / Down move the cursor (with
    /// scroll bookkeeping); Left pops; Center / Right dispatch
    /// the current item's [`ItemAction`].
    fn handle_menu(
        &mut self,
        event: JoystickEvent,
        settings: &Settings,
        keys: &KeyStore,
    ) -> Option<Command> {
        match event {
            JoystickEvent::Press(Direction::Up) => {
                self.cursor = self.cursor.saturating_sub(1);
                if self.cursor < self.scroll_offset {
                    self.scroll_offset = self.cursor;
                }
            }
            JoystickEvent::Press(Direction::Down) => {
                let max = (self.current_menu.items.len() as u8).saturating_sub(1);
                self.cursor = (self.cursor + 1).min(max);
                if self.cursor >= self.scroll_offset + VISIBLE_LIST_ROWS {
                    self.scroll_offset = self.cursor + 1 - VISIBLE_LIST_ROWS;
                }
            }
            JoystickEvent::Press(Direction::Left) => {
                self.pop_nav();
            }
            JoystickEvent::Press(Direction::Right)
            | JoystickEvent::Press(Direction::Center) => {
                if let Some(item) = self.current_menu.items.get(self.cursor as usize) {
                    let action = item.action;
                    match action {
                        ItemAction::Submenu(node) => self.enter_menu(node),
                        ItemAction::List(kind) => self.enter(kind.screen(), settings, keys),
                        ItemAction::Value(kind) => self.enter(kind.screen(), settings, keys),
                        ItemAction::Custom(s) => self.enter(s, settings, keys),
                    }
                }
            }
            _ => {}
        }
        None
    }

    /// Channel-scan handler.  Joystick is reoriented for this
    /// screen since channels are laid out horizontally:
    ///
    /// - **Left / Right** scroll the cursor between channels
    ///   (auto-repeat on hold for fast traversal of the 87- and
    ///   131-channel dense plans).
    /// - **Down** pops back to the parent menu (Up is left as a
    ///   no-op so users don't accidentally exit while reaching
    ///   for the scroll keys).
    /// - **Center** applies the cursor's channel as the new
    ///   active channel and **stays on Scan** so the user can
    ///   keep watching the floor or pick a different channel.
    fn handle_scan(
        &mut self,
        event: JoystickEvent,
        settings: &mut Settings,
    ) -> Option<Command> {
        let max_idx = self.scan.channel_count.saturating_sub(1);
        match event {
            JoystickEvent::Press(Direction::Left) => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            JoystickEvent::Press(Direction::Right) => {
                self.cursor = (self.cursor + 1).min(max_idx);
            }
            JoystickEvent::Press(Direction::Center) => {
                let new_v = self.cursor.min(max_channel_index(settings.band_plan));
                let changed = settings.channel != new_v;
                settings.channel = new_v;
                return if changed {
                    Some(Command::ApplyChannel(new_v))
                } else {
                    None
                };
            }
            JoystickEvent::Press(Direction::Down) => {
                self.pop_nav();
            }
            // Up: intentionally no-op.  Long-press Center
            // (universal "go home") still works.
            _ => {}
        }
        None
    }

    /// Update the scan table with the result of one full pass.
    /// `rssi[i]` is the noise floor (mean dBm) for channel `i` in
    /// the active band plan.  Peak per channel is `max(prev_peak,
    /// rssi[i])`.  Profile calls this after every completed
    /// scan_step.  Safe to call when not on the Scan screen
    /// (no-op on the screen, but the data persists into the next
    /// entry only if you don't reset — and we always reset on
    /// entry, so this is fine).
    pub fn apply_scan_pass(&mut self, rssi: &[i16]) {
        let n = (rssi.len()).min(self.scan.channel_count as usize);
        for i in 0..n {
            self.scan.current_dbm[i] = rssi[i];
            if rssi[i] > self.scan.peak_dbm[i] {
                self.scan.peak_dbm[i] = rssi[i];
            }
        }
    }

    fn handle_list_select(
        &mut self,
        event: JoystickEvent,
        settings: &mut Settings,
        keys: &KeyStore,
        kind: ListKind,
    ) -> Option<Command> {
        let max_idx = kind.max_index(settings, keys);
        match event {
            JoystickEvent::Press(Direction::Up) => {
                self.cursor = self.cursor.saturating_sub(1);
                if self.cursor < self.scroll_offset {
                    self.scroll_offset = self.cursor;
                }
            }
            JoystickEvent::Press(Direction::Down) => {
                self.cursor = (self.cursor + 1).min(max_idx);
                if self.cursor >= self.scroll_offset + VISIBLE_LIST_ROWS {
                    self.scroll_offset = self.cursor + 1 - VISIBLE_LIST_ROWS;
                }
            }
            JoystickEvent::Press(Direction::Center)
            | JoystickEvent::Press(Direction::Right) => {
                let cmd = kind.commit(self.cursor, settings, keys);
                self.pop_nav();
                return cmd;
            }
            JoystickEvent::Press(Direction::Left) => {
                self.pop_nav();
            }
            _ => {}
        }
        None
    }

    fn handle_value_select(
        &mut self,
        event: JoystickEvent,
        settings: &mut Settings,
        kind: ValueKind,
    ) -> Option<Command> {
        match event {
            JoystickEvent::Press(Direction::Center) => {
                if self.edit_mode {
                    // Confirm the buffered value into settings + emit
                    // the apply command for the host.
                    let cmd = kind.commit(self.edit_buffer, settings);
                    self.edit_mode = false;
                    return cmd;
                } else {
                    // Enter edit mode.
                    self.edit_mode = true;
                    self.edit_buffer = kind.read(settings);
                }
            }
            JoystickEvent::Press(Direction::Up) => {
                if self.edit_mode {
                    self.edit_buffer = kind.clamp(self.edit_buffer + 1);
                }
            }
            JoystickEvent::Press(Direction::Down) => {
                if self.edit_mode {
                    self.edit_buffer = kind.clamp(self.edit_buffer - 1);
                }
            }
            JoystickEvent::Press(Direction::Left) => {
                if self.edit_mode {
                    // Cancel — discard buffer, stay on screen.
                    self.edit_mode = false;
                } else {
                    self.pop_nav();
                }
            }
            _ => {}
        }
        None
    }

    fn handle_readonly(&mut self, event: JoystickEvent) -> Option<Command> {
        match event {
            JoystickEvent::Press(Direction::Left) => self.pop_nav(),
            _ => {}
        }
        None
    }

    /// Reset to the Idle screen with a fresh nav stack and cursor —
    /// the universal "go home" action.  Bound to long-press Center
    /// from inside `handle_event`, and also called from profiles'
    /// inactivity-timeout paths to bring the user back to Idle after
    /// a stretch of no input.
    pub fn go_home(&mut self) {
        self.screen = ScreenId::Idle;
        self.current_menu = self.role.main_menu();
        self.cursor = 0;
        self.scroll_offset = 0;
        self.edit_mode = false;
        self.edit_buffer = 0;
        self.nav_stack.clear();
    }

    /// Pop one frame off the nav stack and become that screen,
    /// restoring its cursor + scroll + menu pointer.  If the
    /// stack is empty (e.g. entered a screen directly without
    /// going through `enter()`), fall back to Idle.
    fn pop_nav(&mut self) {
        if let Some(frame) = self.nav_stack.pop() {
            self.screen = frame.screen;
            self.current_menu = frame.menu;
            self.cursor = frame.cursor;
            self.scroll_offset = frame.scroll;
        } else {
            self.screen = ScreenId::Idle;
            self.current_menu = self.role.main_menu();
            self.cursor = 0;
            self.scroll_offset = 0;
        }
        self.edit_mode = false;
        self.edit_buffer = 0;
    }

    /// Push current frame and switch to a child [`MenuNode`].
    /// Used by [`ItemAction::Submenu`] dispatch and the
    /// Idle → MainMenu transition.
    fn enter_menu(&mut self, node: &'static MenuNode) {
        let _ = self.nav_stack.push(NavFrame {
            screen: self.screen,
            menu: self.current_menu,
            cursor: self.cursor,
            scroll: self.scroll_offset,
        });
        self.screen = ScreenId::Menu;
        self.current_menu = node;
        self.cursor = 0;
        self.scroll_offset = 0;
        self.edit_mode = false;
        self.edit_buffer = 0;
    }

    /// Push current frame and switch to a non-menu screen
    /// (list-select, value-edit, or a custom screen).  For
    /// list-based screens, positions the cursor on the
    /// currently-active entry.
    fn enter(&mut self, screen: ScreenId, settings: &Settings, keys: &KeyStore) {
        let _ = self.nav_stack.push(NavFrame {
            screen: self.screen,
            menu: self.current_menu,
            cursor: self.cursor,
            scroll: self.scroll_offset,
        });

        self.screen = screen;
        self.edit_mode = false;
        self.edit_buffer = 0;
        let cursor = match screen {
            ScreenId::ChannelSelect => settings.channel,
            ScreenId::BandPlanSelect => band_plan_index(settings.band_plan) as u8,
            ScreenId::KeySelect => active_key_cursor(settings.active_key_fp, keys),
            // Scan starts with the cursor on the currently-active
            // channel — same UX as ChannelSelect.
            ScreenId::Scan => settings.channel,
            _ => 0,
        };
        self.cursor = cursor;
        self.scroll_offset = cursor.saturating_sub(VISIBLE_LIST_ROWS - 1);
        self.edit_buffer = match screen {
            ScreenId::PowerSelect => settings.tx_power_dbm as i32,
            _ => 0,
        };
        // Scan: reset peak-since-open table on every entry so a
        // fresh scan session always starts from no data.
        if screen == ScreenId::Scan {
            self.scan.reset(settings.band_plan);
        }
    }
}

/// Resolve the active key fingerprint to a cursor index in the
/// key list.  Index 0 is the synthetic "Open" entry; indices 1..
/// correspond to entries returned by `KeyStore::sorted_into`.
fn active_key_cursor(active_fp: Option<u32>, keys: &KeyStore) -> u8 {
    let Some(fp) = active_fp else {
        return 0; // Open
    };
    let mut buf: [KeyEntry; MAX_KEYS] = core::array::from_fn(|_| KeyEntry {
        fingerprint: 0,
        name: String::new(),
    });
    let sorted = keys.sorted_into(&mut buf);
    sorted
        .iter()
        .position(|e| e.fingerprint == (fp & 0x00FF_FFFF))
        .map(|i| (i + 1) as u8)
        .unwrap_or(0)
}

/// Find the index of a [`BandPlan`] in [`BAND_PLANS`].  Used by
/// `enter` to position the cursor on the currently-active plan.
fn band_plan_index(plan: BandPlan) -> usize {
    BAND_PLANS
        .iter()
        .position(|p| *p == plan)
        .unwrap_or(0)
}

/// Which list a list-based screen is selecting from.  Public so
/// menu definitions can refer to it via [`ItemAction::List`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    Channel,
    BandPlan,
    Key,
}

impl ListKind {
    /// The [`ScreenId`] this list opens on.
    pub fn screen(self) -> ScreenId {
        match self {
            ListKind::Channel => ScreenId::ChannelSelect,
            ListKind::BandPlan => ScreenId::BandPlanSelect,
            ListKind::Key => ScreenId::KeySelect,
        }
    }

    fn max_index(&self, settings: &Settings, keys: &KeyStore) -> u8 {
        match self {
            ListKind::Channel => max_channel_index(settings.band_plan),
            ListKind::BandPlan => (BAND_PLANS.len() as u8).saturating_sub(1),
            // Key list = 1 (Open) + however many real keys are stored.
            ListKind::Key => keys.len() as u8,
        }
    }

    fn commit(&self, cursor: u8, settings: &mut Settings, keys: &KeyStore) -> Option<Command> {
        match self {
            ListKind::Channel => {
                let new_v = cursor.min(max_channel_index(settings.band_plan));
                let changed = settings.channel != new_v;
                settings.channel = new_v;
                if changed {
                    Some(Command::ApplyChannel(new_v))
                } else {
                    None
                }
            }
            ListKind::BandPlan => {
                let new_plan = BAND_PLANS
                    .get(cursor as usize)
                    .copied()
                    .unwrap_or(BandPlan::Ism915);
                let changed = settings.band_plan != new_plan;
                settings.band_plan = new_plan;
                // Switching plan may push the active channel out of
                // range — clamp it.
                let max = max_channel_index(new_plan);
                if settings.channel > max {
                    settings.channel = max;
                }
                if changed {
                    Some(Command::ApplyBandPlan(new_plan))
                } else {
                    None
                }
            }
            ListKind::Key => {
                // Cursor 0 = Open (no encryption); cursor 1.. = a
                // real key, looked up in the sorted view of
                // `keys`.
                let new_fp = if cursor == 0 {
                    None
                } else {
                    let mut buf: [KeyEntry; MAX_KEYS] = core::array::from_fn(|_| KeyEntry {
                        fingerprint: 0,
                        name: String::new(),
                    });
                    let sorted = keys.sorted_into(&mut buf);
                    sorted.get((cursor - 1) as usize).map(|e| e.fingerprint)
                };
                let changed = settings.active_key_fp != new_fp;
                settings.active_key_fp = new_fp;
                if changed {
                    Some(Command::ApplySetActiveKey(new_fp))
                } else {
                    None
                }
            }
        }
    }
}

/// Which numeric setting we're editing on a value-edit screen.
/// Only TX power uses this pattern in v1; channel, band plan,
/// and key slot are list-based and use [`ListKind`].  Public so
/// menu definitions can refer to it via [`ItemAction::Value`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    TxPower,
}

impl ValueKind {
    /// The [`ScreenId`] this value-edit opens on.
    pub fn screen(self) -> ScreenId {
        match self {
            ValueKind::TxPower => ScreenId::PowerSelect,
        }
    }

    fn read(&self, settings: &Settings) -> i32 {
        match self {
            ValueKind::TxPower => settings.tx_power_dbm as i32,
        }
    }

    fn clamp(&self, v: i32) -> i32 {
        match self {
            ValueKind::TxPower => v.clamp(MIN_TX_POWER_DBM as i32, MAX_TX_POWER_DBM as i32),
        }
    }

    fn commit(&self, buf: i32, settings: &mut Settings) -> Option<Command> {
        let v = self.clamp(buf);
        match self {
            ValueKind::TxPower => {
                let new_v = v as i8;
                let changed = settings.tx_power_dbm != new_v;
                settings.tx_power_dbm = new_v;
                if changed {
                    Some(Command::ApplyTxPower(new_v))
                } else {
                    None
                }
            }
        }
    }
}

// ── Widgets ─────────────────────────────────────────────────────────────────

/// Maximum widgets emitted per screen.  Sized for the busiest
/// screen (LinkStats) plus headroom.
pub const MAX_WIDGETS: usize = 12;

/// Vector of widgets describing the current screen.  Renderer
/// consumes this; UI core fills it via [`build_screen`].
pub type WidgetList = Vec<Widget, MAX_WIDGETS>;

/// One UI element on a screen.  All variants describe **what** to
/// render, not how — the renderer chooses font, position, colour,
/// etc., based on display capabilities.
///
/// Layout convention: rows are 0-indexed from the top.  The
/// renderer maps row indices to pixel y-coordinates per its own
/// font / spacing.  The 16-col × 8-row grid in `docs/ui_design.md`
/// is the mono baseline; colour TFT renders the same grid scaled.
///
/// `PartialEq + Eq` so the renderer can do content-level diff
/// against the previous frame's widget list and skip rows that
/// haven't changed (no clear, no redraw, no flicker).
///
/// `defmt::Format` is intentionally not derived because
/// `heapless::String` doesn't implement it.  If you need to log a
/// widget, format its content fields manually.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Widget {
    /// Title bar at the top (row 0).  Renderer typically draws this
    /// inverted or with an underline.
    Title(String<24>),
    /// Plain text on a specific row.
    Text { row: u8, text: String<24> },
    /// Selectable label + value pair.  `selected` highlights the
    /// row (cursor mark); `active` flags this row as the
    /// currently-applied setting (regardless of cursor).
    /// `editing` indicates the value is being modified (renderer
    /// brackets the value).
    ///
    /// `label` and `value` are sized to comfortably fit any plan
    /// or channel name we're likely to show.  heapless::String
    /// silently rejects writes that would overflow capacity (the
    /// string ends up empty rather than truncated), so the size
    /// must accommodate the longest label we might emit.
    Selector {
        row: u8,
        label: String<16>,
        value: String<16>,
        /// Cursor is on this row.  Renders a `>` mark.
        selected: bool,
        /// This row's value matches the currently-applied
        /// setting (e.g. on ChannelSelect, the row whose channel
        /// matches `settings.channel`).  Renders a `*` mark in a
        /// separate column from the cursor so the user can see
        /// both "what's active" and "what I'm hovering" at once.
        active: bool,
        /// User is in edit mode for this value.  Renders the
        /// value with `[brackets]`.
        editing: bool,
    },
    /// Footer hint (typically the bottom row), shows joystick
    /// shortcut help.
    Footer(String<24>),
    /// Status indicator for the link.  `up=true` is "good";
    /// renderer typically draws as a coloured dot or text colour.
    LinkStatus { row: u8, up: bool, text: String<24> },
    /// Channel-scan graph marker.  Carries only what changes per
    /// frame at small cost; the (much larger) per-channel RSSI
    /// arrays live in [`UiState::scan`] and are passed to the
    /// renderer alongside the widget list — keeping them out of
    /// the widget enum avoids ballooning the WidgetList by ~14 KB
    /// (every slot grows to the largest variant).  Renderer
    /// draws the entire panel for this widget.
    ScanGraph {
        /// Number of valid channel entries (matches
        /// `state.scan.channel_count`).
        channel_count: u8,
        /// Cursor index — which channel is highlighted.
        cursor: u8,
        /// Active-channel index (the one `settings.channel`
        /// currently points to).  Marker row shows the active
        /// stripe under this column.
        active: u8,
        /// Pre-formatted title shown above the bars (cursor
        /// channel label + frequency + current/peak dBm).
        title: String<32>,
    },
}

/// Build the widget tree for the current screen.  Clears `out`
/// and fills it with the appropriate widgets.  Takes
/// `&KeyStore` so KeySelect can render the runtime-mutable key
/// list; callers that aren't using encryption can pass an empty
/// store via `&KeyStore::new()`.
pub fn build_screen(
    state: &UiState,
    settings: &Settings,
    keys: &KeyStore,
    status: &LinkStatus,
    out: &mut WidgetList,
) {
    out.clear();
    match state.screen {
        ScreenId::Idle => build_idle(state, settings, keys, status, out),
        ScreenId::Menu => build_menu(state, out),
        ScreenId::Scan => build_scan(state, settings, out),
        ScreenId::ChannelSelect => build_channel_select(state, settings, out),
        ScreenId::BandPlanSelect => build_band_plan_select(state, settings, out),
        ScreenId::PowerSelect => build_value_select(
            state,
            settings,
            "TX Power",
            ValueKind::TxPower,
            out,
        ),
        ScreenId::KeySelect => build_key_select(state, settings, keys, out),
        ScreenId::LinkStats => build_link_stats(settings, status, out),
        ScreenId::About => build_about(out),
    }
}

fn build_idle(
    state: &UiState,
    settings: &Settings,
    keys: &KeyStore,
    status: &LinkStatus,
    out: &mut WidgetList,
) {
    let _ = keys;
    // Title carries the role suffix ("OpenStageRF TX" / "RX") so
    // the user can tell at a glance which side they're holding.
    let mut title: String<24> = String::new();
    let _ = write!(&mut title, "OpenStageRF {}", state.role.label());
    out.push(Widget::Title(title)).ok();
    let ch = settings.current_channel();
    // Row 1: link status on RX (watchdog tracks heartbeats);
    // TX power on TX (the most operationally relevant value to
    // see at a glance — dialing it down at FOH is common).
    match state.role {
        Role::Rx => {
            let link_text = if status.up { s("Link: UP") } else { s("Link: LOST") };
            out.push(Widget::LinkStatus {
                row: 1,
                up: status.up,
                text: link_text,
            })
            .ok();
        }
        Role::Tx => {
            let mut row1: String<24> = String::new();
            let _ = write!(&mut row1, "TX +{} dBm", settings.tx_power_dbm);
            out.push(Widget::Text { row: 1, text: row1 }).ok();
        }
    }
    // Plan name + channel label.
    let mut row2: String<24> = String::new();
    let _ = write!(&mut row2, "{} {}", settings.band_plan.info().label, ch.label);
    out.push(Widget::Text { row: 2, text: row2 }).ok();
    // Frequency.
    let mut row3: String<24> = String::new();
    let _ = write!(&mut row3, "{}", ch.format_frequency());
    out.push(Widget::Text { row: 3, text: row3 }).ok();
    // Row 4: RSSI on RX (only meaningful here); blank on TX
    // until we have something else worth showing (battery,
    // packet rate, etc.).
    if state.role == Role::Rx {
        let mut row4: String<24> = String::new();
        match status.last_rssi_dbm {
            Some(r) => {
                let _ = write!(&mut row4, "RSSI {} dBm", r);
            }
            None => {
                let _ = write!(&mut row4, "RSSI --");
            }
        }
        out.push(Widget::Text { row: 4, text: row4 }).ok();
    }
    out.push(Widget::Footer(s("Center: menu"))).ok();
}

/// Generic menu renderer — walks `state.current_menu.items`,
/// emitting a `Selector` widget per visible row.  Same code
/// renders the top-level menu and any future submenus.
fn build_menu(state: &UiState, out: &mut WidgetList) {
    let menu = state.current_menu;
    let mut title: String<24> = String::new();
    let _ = write!(&mut title, "{}", menu.title);
    out.push(Widget::Title(title)).ok();
    let total = menu.items.len() as u8;
    let start = state.scroll_offset;
    let end = (start + VISIBLE_LIST_ROWS).min(total);
    for (visible_idx, list_idx) in (start..end).enumerate() {
        let item = menu.items[list_idx as usize];
        let mut label: String<16> = String::new();
        let _ = write!(&mut label, "{}", item.label);
        out.push(Widget::Selector {
            row: 1 + visible_idx as u8,
            label,
            value: String::new(),
            selected: state.cursor == list_idx,
            active: false, // menu items are navigation, not state
            editing: false,
        })
        .ok();
    }
    out.push(Widget::Footer(s("Left: back"))).ok();
}

/// Build the Scan screen — a single [`Widget::ScanGraph`] carrying
/// the per-channel RSSI table, cursor, active index, and a
/// pre-formatted title line for the cursor's channel.
fn build_scan(state: &UiState, settings: &Settings, out: &mut WidgetList) {
    let info = settings.band_plan.info();
    let cursor = (state.cursor as usize).min(info.channels.len().saturating_sub(1));
    let cur_ch = info.channels[cursor];
    let cur_rssi = state.scan.current_dbm[cursor];
    let peak_rssi = state.scan.peak_dbm[cursor];

    let mut title: String<32> = String::new();
    // "Ch01 903.000  -85/-78"  — channel label + freq + cur/peak.
    // SCAN_NO_DATA prints as "--".
    let _ = write!(&mut title, "{} {} ", cur_ch.label, cur_ch.format_frequency());
    if cur_rssi == SCAN_NO_DATA {
        let _ = write!(&mut title, "--");
    } else {
        let _ = write!(&mut title, "{}", cur_rssi);
    }
    let _ = title.push('/');
    if peak_rssi == SCAN_NO_DATA {
        let _ = write!(&mut title, "--");
    } else {
        let _ = write!(&mut title, "{}", peak_rssi);
    }

    out.push(Widget::ScanGraph {
        channel_count: state.scan.channel_count,
        cursor: state.cursor,
        active: settings.channel,
        title,
    })
    .ok();
}

fn build_channel_select(
    state: &UiState,
    settings: &Settings,
    out: &mut WidgetList,
) {
    let info = settings.band_plan.info();
    out.push(Widget::Title(s("Channel"))).ok();
    let total = info.channels.len() as u8;
    // Visible window: scroll_offset .. scroll_offset+VISIBLE_LIST_ROWS.
    let start = state.scroll_offset;
    let end = (start + VISIBLE_LIST_ROWS).min(total);
    for (visible_idx, list_idx) in (start..end).enumerate() {
        let ch = info.channels[list_idx as usize];
        let mut label: String<16> = String::new();
        let _ = write!(&mut label, "{}", ch.label);
        let mut value: String<16> = String::new();
        let _ = write!(&mut value, "{}", ch.format_frequency());
        out.push(Widget::Selector {
            row: 1 + visible_idx as u8,
            label,
            value,
            selected: state.cursor == list_idx,
            active: list_idx == settings.channel,
            editing: false,
        })
        .ok();
    }
    out.push(Widget::Footer(s("Cen=apply  L=back"))).ok();
}

fn build_band_plan_select(
    state: &UiState,
    settings: &Settings,
    out: &mut WidgetList,
) {
    out.push(Widget::Title(s("Band Plan"))).ok();
    let total = BAND_PLANS.len() as u8;
    let start = state.scroll_offset;
    let end = (start + VISIBLE_LIST_ROWS).min(total);
    for (visible_idx, list_idx) in (start..end).enumerate() {
        let plan = BAND_PLANS[list_idx as usize];
        let info = plan.info();
        let mut label: String<16> = String::new();
        let _ = write!(&mut label, "{}", info.label);
        out.push(Widget::Selector {
            row: 1 + visible_idx as u8,
            label,
            value: String::new(), // active marker shows in prefix instead
            selected: state.cursor == list_idx,
            active: plan == settings.band_plan,
            editing: false,
        })
        .ok();
    }
    out.push(Widget::Footer(s("Cen=apply  L=back"))).ok();
}

fn build_key_select(
    state: &UiState,
    settings: &Settings,
    keys: &KeyStore,
    out: &mut WidgetList,
) {
    out.push(Widget::Title(s("Key"))).ok();

    // Materialise the sorted key list once into a stack buffer.
    let mut buf: [KeyEntry; MAX_KEYS] = core::array::from_fn(|_| KeyEntry {
        fingerprint: 0,
        name: String::new(),
    });
    let sorted = keys.sorted_into(&mut buf);

    // Total list = 1 (Open) + sorted real keys.
    let total = (1 + sorted.len()) as u8;
    let start = state.scroll_offset;
    let end = (start + VISIBLE_LIST_ROWS).min(total);

    for (visible_idx, list_idx) in (start..end).enumerate() {
        let row = 1 + visible_idx as u8;
        let selected = state.cursor == list_idx;

        let mut label: String<16> = String::new();
        let mut value: String<16> = String::new();
        let is_active;

        if list_idx == 0 {
            // "Open" pseudo-entry — no fingerprint.
            let _ = label.push_str("Open");
            let _ = value.push_str("------");
            is_active = settings.active_key_fp.is_none();
        } else {
            let entry = &sorted[(list_idx - 1) as usize];
            for c in entry.name.chars().take(MAX_KEY_NAME) {
                let _ = label.push(c);
            }
            let fp_str = entry.format_fingerprint();
            let _ = value.push_str(fp_str.as_str());
            is_active = settings.active_key_fp == Some(entry.fingerprint);
        }

        out.push(Widget::Selector {
            row,
            label,
            value,
            selected,
            active: is_active,
            editing: false,
        })
        .ok();
    }
    out.push(Widget::Footer(s("Cen=apply  L=back"))).ok();
}

fn build_value_select(
    state: &UiState,
    settings: &Settings,
    title: &'static str,
    kind: ValueKind,
    out: &mut WidgetList,
) {
    out.push(Widget::Title(s(title))).ok();
    let displayed = if state.edit_mode {
        state.edit_buffer
    } else {
        kind.read(settings)
    };
    let mut value: String<16> = String::new();
    match kind {
        ValueKind::TxPower => {
            // Show explicit sign for clarity.
            if displayed >= 0 {
                let _ = write!(&mut value, "+{} dBm", displayed);
            } else {
                let _ = write!(&mut value, "{} dBm", displayed);
            }
        }
    }
    let mut label: String<16> = String::new();
    let _ = write!(&mut label, "{}", title);
    out.push(Widget::Selector {
        row: 2,
        label,
        value,
        selected: true,
        active: false, // value-edit screens have only one row; "active" is implicit
        editing: state.edit_mode,
    })
    .ok();
    let footer = if state.edit_mode {
        s("Up/Dn  Cen=ok  L=cancel")
    } else {
        s("Center: edit")
    };
    out.push(Widget::Footer(footer)).ok();
}

fn build_link_stats(settings: &Settings, status: &LinkStatus, out: &mut WidgetList) {
    out.push(Widget::Title(s("Link Stats"))).ok();
    out.push(Widget::LinkStatus {
        row: 1,
        up: status.up,
        text: if status.up { s("UP") } else { s("LOST") },
    })
    .ok();
    let mut row: u8 = 2;
    let ch = settings.current_channel();
    {
        let mut t: String<24> = String::new();
        let _ = write!(
            &mut t,
            "{} {}  +{}dBm",
            settings.band_plan.info().label,
            ch.label,
            settings.tx_power_dbm
        );
        out.push(Widget::Text { row, text: t }).ok();
        row += 1;
    }
    {
        let mut t: String<24> = String::new();
        let _ = write!(&mut t, "{}", ch.format_frequency());
        out.push(Widget::Text { row, text: t }).ok();
        row += 1;
    }
    if let Some(rssi) = status.last_rssi_dbm {
        let mut t: String<24> = String::new();
        let _ = write!(&mut t, "RSSI {} dBm", rssi);
        out.push(Widget::Text { row, text: t }).ok();
        row += 1;
    }
    if let Some(loss) = status.recent_loss_pct {
        let mut t: String<24> = String::new();
        let _ = write!(&mut t, "Loss {}%", loss);
        out.push(Widget::Text { row, text: t }).ok();
        row += 1;
    }
    {
        let mut t: String<24> = String::new();
        let _ = write!(&mut t, "Pkts {}", status.total_accepted);
        out.push(Widget::Text { row, text: t }).ok();
        row += 1;
    }
    if status.stuck_recoveries > 0 {
        let mut t: String<24> = String::new();
        let _ = write!(&mut t, "Stuck recov {}", status.stuck_recoveries);
        out.push(Widget::Text { row, text: t }).ok();
    }
    out.push(Widget::Footer(s("Left: back"))).ok();
}

fn build_about(out: &mut WidgetList) {
    out.push(Widget::Title(s("About"))).ok();
    out.push(Widget::Text {
        row: 1,
        text: s("OpenStageRF v0.1"),
    })
    .ok();
    out.push(Widget::Text {
        row: 2,
        text: s("github.com/..."),
    })
    .ok();
    out.push(Widget::Footer(s("Left: back"))).ok();
}

/// Tiny helper: build a fixed-size [`String`] from a `&'static str`.
fn s<const N: usize>(literal: &'static str) -> String<N> {
    let mut out: String<N> = String::new();
    let _ = out.push_str(literal);
    out
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn press(d: Direction) -> JoystickEvent {
        JoystickEvent::Press(d)
    }
    fn long(d: Direction) -> JoystickEvent {
        JoystickEvent::LongPress(d)
    }

    #[test]
    fn idle_to_main_menu_on_center() {
        let mut state = UiState::default();
        let mut settings = Settings::default(); let keys = KeyStore::new();
        let cmd = state.handle_event(&mut settings, &keys, press(Direction::Center));
        assert_eq!(state.screen, ScreenId::Menu);
        assert_eq!(state.cursor, 0);
        assert_eq!(cmd, None);
    }

    #[test]
    fn idle_long_press_center_jumps_to_channel_select() {
        let mut state = UiState::default();
        let mut settings = Settings::default(); let keys = KeyStore::new();
        state.handle_event(&mut settings, &keys, long(Direction::Center));
        assert_eq!(state.screen, ScreenId::ChannelSelect);
    }

    #[test]
    fn long_press_center_from_submenu_returns_to_idle() {
        let mut state = UiState {
            screen: ScreenId::PowerSelect,
            cursor: 0,
            scroll_offset: 0,
            edit_mode: false,
            edit_buffer: 0,
            role: Role::Rx,
            current_menu: &MAIN_MENU_RX,
            nav_stack: Vec::new(),
            scan: ScanState::default(),
        };
        let mut settings = Settings::default(); let keys = KeyStore::new();
        state.handle_event(&mut settings, &keys, long(Direction::Center));
        assert_eq!(state.screen, ScreenId::Idle);
    }

    #[test]
    fn main_menu_navigation() {
        let mut state = UiState {
            screen: ScreenId::Menu,
            cursor: 0,
            scroll_offset: 0,
            edit_mode: false,
            edit_buffer: 0,
            role: Role::Rx,
            current_menu: &MAIN_MENU_RX,
            nav_stack: Vec::new(),
            scan: ScanState::default(),
        };
        let mut settings = Settings::default(); let keys = KeyStore::new();
        state.handle_event(&mut settings, &keys, press(Direction::Down));
        assert_eq!(state.cursor, 1);
        state.handle_event(&mut settings, &keys, press(Direction::Down));
        assert_eq!(state.cursor, 2);
        state.handle_event(&mut settings, &keys, press(Direction::Up));
        assert_eq!(state.cursor, 1);
        // Up at top stops at 0.
        state.handle_event(&mut settings, &keys, press(Direction::Up));
        state.handle_event(&mut settings, &keys, press(Direction::Up));
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn main_menu_enters_submenu_on_center() {
        let mut state = UiState {
            screen: ScreenId::Menu,
            cursor: 0, // ChannelSelect
            scroll_offset: 0,
            edit_mode: false,
            edit_buffer: 0,
            role: Role::Rx,
            current_menu: &MAIN_MENU_RX,
            nav_stack: Vec::new(),
            scan: ScanState::default(),
        };
        let mut settings = Settings::default(); let keys = KeyStore::new();
        state.handle_event(&mut settings, &keys, press(Direction::Center));
        assert_eq!(state.screen, ScreenId::ChannelSelect);
    }

    #[test]
    fn channel_select_list_navigation_and_apply() {
        // Enter ChannelSelect from MainMenu.
        let mut state = UiState {
            screen: ScreenId::Menu,
            cursor: 0, // ChannelSelect (first menu item)
            ..UiState::default()
        };
        let mut settings = Settings::default(); let keys = KeyStore::new();
        state.handle_event(&mut settings, &keys, press(Direction::Center));
        assert_eq!(state.screen, ScreenId::ChannelSelect);
        // Cursor pre-positioned on currently-selected channel (0).
        assert_eq!(state.cursor, 0);

        // Move down twice → cursor=2.
        state.handle_event(&mut settings, &keys, press(Direction::Down));
        state.handle_event(&mut settings, &keys, press(Direction::Down));
        assert_eq!(state.cursor, 2);
        // Settings unchanged until apply.
        assert_eq!(settings.channel, 0);

        // Apply with Center → channel=2, command emitted, returns home.
        let cmd = state.handle_event(&mut settings, &keys, press(Direction::Center));
        assert_eq!(settings.channel, 2);
        assert_eq!(cmd, Some(Command::ApplyChannel(2)));
        assert_eq!(state.screen, ScreenId::Menu);
    }

    #[test]
    fn channel_select_left_cancels_without_applying() {
        // Navigate Idle → MainMenu → ChannelSelect so the nav
        // stack records the parent frames; otherwise pop_nav
        // falls all the way back to Idle.
        let mut state = UiState::default();
        let mut settings = Settings { channel: 0, ..Settings::default() };
        let keys = KeyStore::new();
        state.handle_event(&mut settings, &keys, press(Direction::Center));
        state.handle_event(&mut settings, &keys, press(Direction::Center));
        assert_eq!(state.screen, ScreenId::ChannelSelect);
        // Move cursor, then back out.
        state.handle_event(&mut settings, &keys, press(Direction::Down));
        state.handle_event(&mut settings, &keys, press(Direction::Left));
        assert_eq!(state.screen, ScreenId::Menu);
        assert_eq!(settings.channel, 0, "channel preserved on Left");
    }

    #[test]
    fn channel_select_clamps_at_max() {
        let mut state = UiState {
            screen: ScreenId::ChannelSelect,
            cursor: 0,
            ..UiState::default()
        };
        let mut settings = Settings::default(); let keys = KeyStore::new();
        let max = max_channel_index(settings.band_plan);
        for _ in 0..(max as u32 + 5) {
            state.handle_event(&mut settings, &keys, press(Direction::Down));
        }
        assert_eq!(state.cursor, max);
    }

    #[test]
    fn band_plan_select_changes_plan_and_clamps_channel() {
        // Start with band plan that has many channels and channel
        // index near the top, then switch to a plan with fewer
        // channels.  channel should clamp.
        let mut state = UiState {
            screen: ScreenId::Menu,
            cursor: 0,
            ..UiState::default()
        };
        let mut settings = Settings {
            band_plan: BandPlan::DenseLo,  // 87 channels
            channel: 7,
            ..Settings::default()
        };
        let keys = KeyStore::new();
        // Navigate to BandPlanSelect — find its row at runtime so
        // this tracks MAIN_MENU edits (e.g. inserting Scan).
        let band_idx = MAIN_MENU_RX
            .items
            .iter()
            .position(|i| matches!(i.action, ItemAction::List(ListKind::BandPlan)))
            .expect("Band Plan must be in MAIN_MENU") as u8;
        for _ in 0..band_idx {
            state.handle_event(&mut settings, &keys, press(Direction::Down));
        }
        state.handle_event(&mut settings, &keys, press(Direction::Center));
        assert_eq!(state.screen, ScreenId::BandPlanSelect);
        // Find Shure plan (4 channels, index 2).
        let shure_idx = BAND_PLANS.iter().position(|p| *p == BandPlan::Shure).unwrap() as u8;
        // Move cursor to Shure.
        let cur_plan_idx = band_plan_index(settings.band_plan) as u8;
        if cur_plan_idx < shure_idx {
            for _ in 0..(shure_idx - cur_plan_idx) {
                state.handle_event(&mut settings, &keys, press(Direction::Down));
            }
        } else {
            for _ in 0..(cur_plan_idx - shure_idx) {
                state.handle_event(&mut settings, &keys, press(Direction::Up));
            }
        }
        assert_eq!(state.cursor, shure_idx);
        // Apply.
        let cmd = state.handle_event(&mut settings, &keys, press(Direction::Center));
        assert_eq!(cmd, Some(Command::ApplyBandPlan(BandPlan::Shure)));
        assert_eq!(settings.band_plan, BandPlan::Shure);
        // Channel clamped from 7 to Shure's max (3).
        assert_eq!(settings.channel, max_channel_index(BandPlan::Shure));
    }

    #[test]
    fn tx_power_edit_handles_negative() {
        let mut state = UiState {
            screen: ScreenId::PowerSelect,
            cursor: 0,
            scroll_offset: 0,
            edit_mode: false,
            edit_buffer: 0,
            role: Role::Rx,
            current_menu: &MAIN_MENU_RX,
            nav_stack: Vec::new(),
            scan: ScanState::default(),
        };
        let mut settings = Settings { tx_power_dbm: 0, ..Settings::default() }; let keys = KeyStore::new();
        state.handle_event(&mut settings, &keys, press(Direction::Center));
        for _ in 0..15 {
            state.handle_event(&mut settings, &keys, press(Direction::Down));
        }
        // Clamps at MIN_TX_POWER_DBM = -9.
        assert_eq!(state.edit_buffer, MIN_TX_POWER_DBM as i32);
        let cmd = state.handle_event(&mut settings, &keys, press(Direction::Center));
        assert_eq!(settings.tx_power_dbm, MIN_TX_POWER_DBM);
        assert_eq!(cmd, Some(Command::ApplyTxPower(MIN_TX_POWER_DBM)));
    }

    #[test]
    fn unchanged_value_emits_no_command() {
        // TX power: enter, don't change, confirm.  Should not emit.
        let mut state = UiState {
            screen: ScreenId::PowerSelect,
            ..UiState::default()
        };
        let mut settings = Settings { tx_power_dbm: 22, ..Settings::default() }; let keys = KeyStore::new();
        state.handle_event(&mut settings, &keys, press(Direction::Center));
        // Edit-mode now, edit_buffer = 22.  Don't change anything,
        // confirm.
        let cmd = state.handle_event(&mut settings, &keys, press(Direction::Center));
        assert_eq!(cmd, None);
    }

    #[test]
    fn channel_select_unchanged_emits_no_command() {
        let mut state = UiState {
            screen: ScreenId::ChannelSelect,
            cursor: 2,
            ..UiState::default()
        };
        let mut settings = Settings { channel: 2, ..Settings::default() }; let keys = KeyStore::new();
        // Cursor already on currently-active channel; Center applies
        // but value is unchanged.
        let cmd = state.handle_event(&mut settings, &keys, press(Direction::Center));
        assert_eq!(cmd, None);
    }

    #[test]
    fn build_idle_screen_includes_link_status() {
        let state = UiState::default();
        let settings = Settings::default();
        let keys = KeyStore::new();
        let mut status = LinkStatus::default();
        status.up = true;
        let mut widgets: WidgetList = WidgetList::new();
        build_screen(&state, &settings, &keys, &status, &mut widgets);
        assert!(widgets
            .iter()
            .any(|w| matches!(w, Widget::LinkStatus { up: true, .. })));
        assert!(widgets.iter().any(|w| matches!(w, Widget::Title(_))));
    }

    #[test]
    fn build_main_menu_has_correct_cursor_highlight() {
        let mut state = UiState {
            screen: ScreenId::Menu,
            cursor: 2,
            scroll_offset: 0,
            edit_mode: false,
            edit_buffer: 0,
            role: Role::Rx,
            current_menu: &MAIN_MENU_RX,
            nav_stack: Vec::new(),
            scan: ScanState::default(),
        };
        let settings = Settings::default();
        let keys = KeyStore::new();
        let status = LinkStatus::default();
        let mut widgets: WidgetList = WidgetList::new();
        build_screen(&state, &settings, &keys, &status, &mut widgets);
        // Selectors should have selected=true exactly once, on row 1+cursor.
        let selected_count = widgets
            .iter()
            .filter(|w| matches!(w, Widget::Selector { selected: true, .. }))
            .count();
        assert_eq!(selected_count, 1);
        // Row of the selected widget should be 1 + cursor.
        for w in widgets.iter() {
            if let Widget::Selector { row, selected: true, .. } = w {
                assert_eq!(*row, 1 + state.cursor);
            }
        }
        let _ = &mut state;
    }

    #[test]
    fn back_from_submenu_restores_main_menu_cursor() {
        // Navigate Idle → MainMenu, scroll cursor onto Link Stats,
        // enter it, press Left.  Cursor should land back on Link
        // Stats, not jump to 0.  (Index resolved at runtime so this
        // tracks MAIN_MENU edits — e.g. hiding Key shifts the row.)
        let mut state = UiState::default();
        let mut settings = Settings::default();
        let keys = KeyStore::new();
        let link_stats_idx = MAIN_MENU_RX
            .items
            .iter()
            .position(|i| matches!(i.action, ItemAction::Custom(ScreenId::LinkStats)))
            .expect("Link Stats must be in MAIN_MENU") as u8;
        state.handle_event(&mut settings, &keys, press(Direction::Center));
        for _ in 0..link_stats_idx {
            state.handle_event(&mut settings, &keys, press(Direction::Down));
        }
        assert_eq!(state.cursor, link_stats_idx);
        state.handle_event(&mut settings, &keys, press(Direction::Center));
        assert_eq!(state.screen, ScreenId::LinkStats);
        state.handle_event(&mut settings, &keys, press(Direction::Left));
        assert_eq!(state.screen, ScreenId::Menu);
        assert_eq!(state.cursor, link_stats_idx);
    }

    /// Helper: navigate Idle → MainMenu → Scan via the menu so
    /// the nav stack is correctly populated.  Returns the
    /// MainMenu row index Scan was on (so callers can verify
    /// Down-back lands there).
    fn enter_scan(state: &mut UiState, settings: &mut Settings, keys: &KeyStore) -> u8 {
        let scan_idx = MAIN_MENU_RX
            .items
            .iter()
            .position(|i| matches!(i.action, ItemAction::Custom(ScreenId::Scan)))
            .expect("Scan must be in MAIN_MENU") as u8;
        state.handle_event(settings, keys, press(Direction::Center));
        for _ in 0..scan_idx {
            state.handle_event(settings, keys, press(Direction::Down));
        }
        state.handle_event(settings, keys, press(Direction::Center));
        assert_eq!(state.screen, ScreenId::Scan);
        scan_idx
    }

    #[test]
    fn scan_apply_updates_settings_and_stays() {
        // Enter Scan, simulate a few passes (to seed peaks),
        // scroll cursor with Right, hit Center.  Should emit
        // ApplyChannel with the cursor's index, settings.channel
        // updated, user stays on Scan (Down is the way out).
        let mut state = UiState::default();
        let mut settings = Settings::default();
        let keys = KeyStore::new();
        let scan_idx = enter_scan(&mut state, &mut settings, &keys);
        assert_eq!(state.cursor, 0);
        assert_eq!(state.scan.channel_count, max_channel_index(BandPlan::Ism915) + 1);

        let pass1: heapless::Vec<i16, MAX_SCAN_CHANNELS> =
            (0..state.scan.channel_count).map(|_| -110i16).collect();
        state.apply_scan_pass(&pass1);
        let mut pass2: heapless::Vec<i16, MAX_SCAN_CHANNELS> = pass1.clone();
        pass2[3] = -70;
        state.apply_scan_pass(&pass2);
        assert_eq!(state.scan.current_dbm[3], -70);
        assert_eq!(state.scan.peak_dbm[3], -70);
        state.apply_scan_pass(&pass1);
        assert_eq!(state.scan.current_dbm[3], -110);
        assert_eq!(state.scan.peak_dbm[3], -70);

        // Scroll cursor right to ch5 and apply; stays on Scan.
        for _ in 0..5 {
            state.handle_event(&mut settings, &keys, press(Direction::Right));
        }
        assert_eq!(state.cursor, 5);
        let cmd = state.handle_event(&mut settings, &keys, press(Direction::Center));
        assert_eq!(cmd, Some(Command::ApplyChannel(5)));
        assert_eq!(settings.channel, 5);
        assert_eq!(state.screen, ScreenId::Scan, "stays on scan after apply");
        assert_eq!(state.cursor, 5, "cursor stays put");
        // Down to exit.
        state.handle_event(&mut settings, &keys, press(Direction::Down));
        assert_eq!(state.screen, ScreenId::Menu);
        assert_eq!(state.cursor, scan_idx);
    }

    #[test]
    fn scan_down_cancels_without_applying() {
        let mut state = UiState::default();
        let mut settings = Settings { channel: 2, ..Settings::default() };
        let keys = KeyStore::new();
        enter_scan(&mut state, &mut settings, &keys);
        // Cursor on active channel (2).  Scroll right 3, then Down.
        for _ in 0..3 {
            state.handle_event(&mut settings, &keys, press(Direction::Right));
        }
        assert_eq!(state.cursor, 5);
        let cmd = state.handle_event(&mut settings, &keys, press(Direction::Down));
        assert!(cmd.is_none());
        assert_eq!(settings.channel, 2, "channel preserved on Down-back");
        assert_eq!(state.screen, ScreenId::Menu);
    }

    #[test]
    fn scan_left_right_scroll_cursor() {
        // Confirm Left and Right move the scan cursor (and Up
        // does nothing).
        let mut state = UiState::default();
        let mut settings = Settings::default();
        let keys = KeyStore::new();
        enter_scan(&mut state, &mut settings, &keys);
        for _ in 0..4 {
            state.handle_event(&mut settings, &keys, press(Direction::Right));
        }
        assert_eq!(state.cursor, 4);
        state.handle_event(&mut settings, &keys, press(Direction::Left));
        assert_eq!(state.cursor, 3);
        // Up is a no-op.
        state.handle_event(&mut settings, &keys, press(Direction::Up));
        assert_eq!(state.cursor, 3);
        assert_eq!(state.screen, ScreenId::Scan);
        // Left at 0 saturates.
        for _ in 0..10 {
            state.handle_event(&mut settings, &keys, press(Direction::Left));
        }
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn tx_role_uses_tx_menu_no_link_stats() {
        let state = UiState::with_role(Role::Tx);
        assert_eq!(state.role, Role::Tx);
        assert!(core::ptr::eq(state.current_menu, &MAIN_MENU_TX));
        assert!(MAIN_MENU_TX
            .items
            .iter()
            .any(|i| matches!(i.action, ItemAction::Value(ValueKind::TxPower))));
        assert!(!MAIN_MENU_TX
            .items
            .iter()
            .any(|i| matches!(i.action, ItemAction::Custom(ScreenId::LinkStats))));
    }

    #[test]
    fn rx_role_uses_rx_menu_no_tx_power() {
        let state = UiState::with_role(Role::Rx);
        assert_eq!(state.role, Role::Rx);
        assert!(core::ptr::eq(state.current_menu, &MAIN_MENU_RX));
        assert!(MAIN_MENU_RX
            .items
            .iter()
            .any(|i| matches!(i.action, ItemAction::Custom(ScreenId::LinkStats))));
        assert!(!MAIN_MENU_RX
            .items
            .iter()
            .any(|i| matches!(i.action, ItemAction::Value(ValueKind::TxPower))));
    }

    #[test]
    fn build_idle_title_includes_role() {
        let mut widgets: WidgetList = WidgetList::new();
        let settings = Settings::default();
        let keys = KeyStore::new();
        let status = LinkStatus::default();

        let rx = UiState::with_role(Role::Rx);
        build_screen(&rx, &settings, &keys, &status, &mut widgets);
        let rx_title = widgets.iter().find_map(|w| match w {
            Widget::Title(t) => Some(t.as_str()),
            _ => None,
        });
        assert_eq!(rx_title, Some("OpenStageRF RX"));

        let tx = UiState::with_role(Role::Tx);
        build_screen(&tx, &settings, &keys, &status, &mut widgets);
        let tx_title = widgets.iter().find_map(|w| match w {
            Widget::Title(t) => Some(t.as_str()),
            _ => None,
        });
        assert_eq!(tx_title, Some("OpenStageRF TX"));
    }

    #[test]
    fn scan_resets_peak_table_on_each_entry() {
        let mut state = UiState::default();
        let mut settings = Settings::default();
        let keys = KeyStore::new();
        enter_scan(&mut state, &mut settings, &keys);
        let pass: heapless::Vec<i16, MAX_SCAN_CHANNELS> =
            (0..state.scan.channel_count).map(|_| -50i16).collect();
        state.apply_scan_pass(&pass);
        assert_eq!(state.scan.peak_dbm[0], -50);
        // Down-back, then re-enter; peak table should be cleared.
        state.handle_event(&mut settings, &keys, press(Direction::Down));
        state.handle_event(&mut settings, &keys, press(Direction::Center));
        assert_eq!(state.scan.peak_dbm[0], SCAN_NO_DATA);
        assert_eq!(state.scan.current_dbm[0], SCAN_NO_DATA);
    }
}
