// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![allow(async_fn_in_trait)]

//! Debounced 5-way joystick driver.
//!
//! Five pull-up GPIO inputs (one per direction), each pulled to
//! ground by the joystick when actuated.  The driver consumes pin
//! events and emits debounced [`JoystickEvent`] values via
//! `next_event`.
//!
//! ## Two implementations
//!
//! - [`Joystick5Way`] (re-exported from [`edge`]) — **edge-wake,
//!   recommended.**  Sleeps until a pin actually changes; idle CPU
//!   cost is essentially zero.  Required for battery operation.
//!   Generic over `embedded_hal::digital::InputPin +
//!   embedded_hal_async::digital::Wait`.
//!
//! - [`polling::Joystick5WayPolling`] — fallback / sanity-check
//!   implementation.  Polls all five pins every 5 ms.  Simple and
//!   deterministic but keeps the executor warm 200×/sec — fine on
//!   USB, expensive on battery.  Generic over `InputPin` only.
//!
//! Both implementations share the [`Direction`] enum and
//! [`JoystickEvent`] variants, so swapping between them is
//! transparent to consumer code.
//!
//! ## Long-press
//!
//! After a press is debounced and `Press(dir)` is emitted, the
//! direction is monitored for [`LONG_PRESS_THRESHOLD`] continuous
//! hold time.  If reached, `LongPress(dir)` fires once, after which
//! holding longer produces no further events until release.  The
//! threshold is per-press-cycle: each fresh press resets it.
//!
//! ## Multi-press
//!
//! Both implementations track a single press at a time.  Realistic
//! for a 5-way joystick (one direction at a time); avoids the
//! complexity of independent per-direction state machines.

pub mod edge;
pub mod polling;

pub use edge::Joystick5Way;
pub use polling::Joystick5WayPolling;

use embassy_time::Duration;

// ── Shared types and constants ──────────────────────────────────────────────

/// One direction on the joystick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
    Center,
}

/// Event emitted by `next_event`.  Releases produce no event — they
/// only return the driver to its idle state for the next press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum JoystickEvent {
    /// A direction has just been pressed (debounced).  Fires once
    /// per press cycle, at the moment the press is confirmed.
    Press(Direction),
    /// A direction has been held continuously for at least
    /// [`LONG_PRESS_THRESHOLD`].  Fires once per press cycle, after
    /// the initial `Press(dir)` event.  Holding longer produces no
    /// further events until release.
    LongPress(Direction),
}

/// Continuous press time after which `LongPress(dir)` is emitted.
pub const LONG_PRESS_THRESHOLD: Duration = Duration::from_millis(500);

// ── Internal helpers shared by both implementations ─────────────────────────

pub(crate) const N: usize = 5;
pub(crate) const IDX_UP: usize = 0;
pub(crate) const IDX_DOWN: usize = 1;
pub(crate) const IDX_LEFT: usize = 2;
pub(crate) const IDX_RIGHT: usize = 3;
pub(crate) const IDX_CENTER: usize = 4;

pub(crate) fn idx_to_direction(i: usize) -> Direction {
    match i {
        IDX_UP => Direction::Up,
        IDX_DOWN => Direction::Down,
        IDX_LEFT => Direction::Left,
        IDX_RIGHT => Direction::Right,
        IDX_CENTER => Direction::Center,
        _ => unreachable!(),
    }
}
