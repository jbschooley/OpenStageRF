// SPDX-License-Identifier: AGPL-3.0-or-later

//! Polling-based 5-way joystick driver.
//!
//! Polls all five pins at a fixed cadence (default 5 ms) and
//! debounces in software.  Simple and deterministic, but keeps the
//! executor warm 200×/sec even when nothing is happening — fine for
//! USB-powered devices, **expensive on battery**.  See [`crate::edge`]
//! for the edge-wake variant that sleeps until something changes.
//!
//! Kept in the crate as a fallback / sanity-check implementation.

use embassy_time::{Instant, Timer};
use embedded_hal::digital::InputPin;

use crate::{idx_to_direction, Direction, JoystickEvent, IDX_CENTER, N};

/// Polling interval — how often we sample all five inputs.
pub const POLL_INTERVAL_MS: u64 = 5;

/// Number of consecutive stable LOW samples required before a press
/// is registered.  At [`POLL_INTERVAL_MS`] = 5 ms, a value of 4
/// means 20 ms of stable contact — comfortably past typical
/// mechanical-switch bounce (5–10 ms on most parts).
pub const DEBOUNCE_SAMPLES: u8 = 4;

/// Per-direction tracker.  Held inside [`Joystick5WayPolling`] across
/// calls to `next_event` so press / long-press state survives the
/// iteration.
#[derive(Default, Clone, Copy)]
struct DirectionState {
    /// Currently considered pressed (after debounce, before release).
    pressed: bool,
    /// Consecutive stable LOW samples observed during current debounce.
    stable_low: u8,
    /// Whether the initial `Press(dir)` has been emitted yet for this
    /// press cycle.  Reset to `false` on release.
    press_emitted: bool,
    /// When the current press cycle started (after debounce).  `None`
    /// while not pressed.
    press_start: Option<Instant>,
    /// Whether `LongPress(dir)` has been emitted yet for this press
    /// cycle.  Prevents repeat emissions while the user keeps holding.
    long_press_emitted: bool,
}

/// Polling driver.  Construct with [`Joystick5WayPolling::new`] and
/// consume events by repeatedly calling `next_event`.
pub struct Joystick5WayPolling<U, D, L, R, C> {
    up: U,
    down: D,
    left: L,
    right: R,
    center: C,
    states: [DirectionState; N],
}

impl<U, D, L, R, C> Joystick5WayPolling<U, D, L, R, C>
where
    U: InputPin,
    D: InputPin,
    L: InputPin,
    R: InputPin,
    C: InputPin,
{
    /// Wrap the five pins.  Each pin must already be configured as
    /// input with a pull-up; the driver does not configure pin mode
    /// itself (that's HAL-specific).
    pub fn new(up: U, down: D, left: L, right: R, center: C) -> Self {
        Self {
            up,
            down,
            left,
            right,
            center,
            states: [DirectionState::default(); N],
        }
    }

    /// Wait for and return the next debounced [`JoystickEvent`].
    pub async fn next_event(&mut self) -> JoystickEvent {
        loop {
            Timer::after_millis(POLL_INTERVAL_MS).await;
            let raw = [
                self.up.is_low().unwrap_or(false),
                self.down.is_low().unwrap_or(false),
                self.left.is_low().unwrap_or(false),
                self.right.is_low().unwrap_or(false),
                self.center.is_low().unwrap_or(false),
            ];

            let now = Instant::now();
            for (i, &is_pressed) in raw.iter().enumerate() {
                if let Some(event) = update_direction(&mut self.states[i], i, is_pressed, now) {
                    return event;
                }
            }
        }
    }

    /// Reset internal state — clears any in-progress debounce and
    /// long-press tracking.
    pub fn reset(&mut self) {
        self.states = [DirectionState::default(); N];
    }
}

/// Update one direction's state and emit an event if a transition
/// occurred this poll.
fn update_direction(
    s: &mut DirectionState,
    idx: usize,
    is_pressed: bool,
    now: Instant,
) -> Option<JoystickEvent> {
    if !is_pressed {
        s.pressed = false;
        s.stable_low = 0;
        s.press_emitted = false;
        s.press_start = None;
        s.long_press_emitted = false;
        return None;
    }

    if !s.pressed {
        s.stable_low = s.stable_low.saturating_add(1);
        if s.stable_low >= DEBOUNCE_SAMPLES {
            s.pressed = true;
            s.press_emitted = true;
            s.press_start = Some(now);
            return Some(JoystickEvent::Press(idx_to_direction(idx)));
        }
        return None;
    }

    if !s.long_press_emitted {
        if let Some(start) = s.press_start {
            if now.duration_since(start) >= crate::LONG_PRESS_THRESHOLD {
                s.long_press_emitted = true;
                let _ = idx;
                let _ = IDX_CENTER;
                let _ = Direction::Center;
                return Some(JoystickEvent::LongPress(idx_to_direction(idx)));
            }
        }
    }
    None
}
