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

/// Top-level menu, entered from Idle on Center/Right.  When
/// adding new submenus, declare a `static FOO_MENU: MenuNode`
/// and reference it from here (or any deeper parent) via
/// [`ItemAction::Submenu`].
pub static MAIN_MENU: MenuNode = MenuNode {
    title: "Menu",
    items: &[
        MenuItem { label: "Channel",    action: ItemAction::List(ListKind::Channel) },
        MenuItem { label: "Band Plan",  action: ItemAction::List(ListKind::BandPlan) },
        MenuItem { label: "TX Power",   action: ItemAction::Value(ValueKind::TxPower) },
        // Hidden until AEAD lands (Stage 3 in ROADMAP.md) — KeySelect
        // and the KeyStore still exist and work, but exposing them in
        // the UI is misleading while there's no actual encryption.
        // MenuItem { label: "Key",        action: ItemAction::List(ListKind::Key) },
        MenuItem { label: "Link Stats", action: ItemAction::Custom(ScreenId::LinkStats) },
        MenuItem { label: "About",      action: ItemAction::Custom(ScreenId::About) },
    ],
};

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
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            screen: ScreenId::Idle,
            current_menu: &MAIN_MENU,
            cursor: 0,
            scroll_offset: 0,
            edit_mode: false,
            edit_buffer: 0,
            nav_stack: Vec::new(),
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
                self.enter_menu(&MAIN_MENU);
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

    fn go_home(&mut self) {
        self.screen = ScreenId::Idle;
        self.current_menu = &MAIN_MENU;
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
            self.current_menu = &MAIN_MENU;
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
            _ => 0,
        };
        self.cursor = cursor;
        self.scroll_offset = cursor.saturating_sub(VISIBLE_LIST_ROWS - 1);
        self.edit_buffer = match screen {
            ScreenId::PowerSelect => settings.tx_power_dbm as i32,
            _ => 0,
        };
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
        ScreenId::Idle => build_idle(settings, keys, status, out),
        ScreenId::Menu => build_menu(state, out),
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

fn build_idle(settings: &Settings, keys: &KeyStore, status: &LinkStatus, out: &mut WidgetList) {
    let _ = keys;
    out.push(Widget::Title(s("OpenStageRF"))).ok();
    let link_text = if status.up { s("Link: UP") } else { s("Link: LOST") };
    out.push(Widget::LinkStatus {
        row: 1,
        up: status.up,
        text: link_text,
    })
    .ok();
    let ch = settings.current_channel();
    // Plan name + channel label.
    let mut row2: String<24> = String::new();
    let _ = write!(&mut row2, "{} {}", settings.band_plan.info().label, ch.label);
    out.push(Widget::Text { row: 2, text: row2 }).ok();
    // Frequency.
    let mut row3: String<24> = String::new();
    let _ = write!(&mut row3, "{}", ch.format_frequency());
    out.push(Widget::Text { row: 3, text: row3 }).ok();
    // TX power.
    let mut row4: String<24> = String::new();
    let _ = write!(&mut row4, "TX +{} dBm", settings.tx_power_dbm);
    out.push(Widget::Text { row: 4, text: row4 }).ok();
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
            current_menu: &MAIN_MENU,
            nav_stack: Vec::new(),
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
            current_menu: &MAIN_MENU,
            nav_stack: Vec::new(),
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
            current_menu: &MAIN_MENU,
            nav_stack: Vec::new(),
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
            band_plan: BandPlan::Dense,    // 8 channels
            channel: 7,
            ..Settings::default()
        };
        let keys = KeyStore::new();
        // Navigate to BandPlanSelect (index 1 in main menu).
        state.handle_event(&mut settings, &keys, press(Direction::Down));
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
            current_menu: &MAIN_MENU,
            nav_stack: Vec::new(),
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
            current_menu: &MAIN_MENU,
            nav_stack: Vec::new(),
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
        let link_stats_idx = MAIN_MENU
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
}
