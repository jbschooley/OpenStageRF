// SPDX-License-Identifier: AGPL-3.0-or-later

//! Edge-wake 5-way joystick driver.
//!
//! Sleeps until any pin produces a falling edge (via GPIOTE on the
//! nRF52, or the equivalent on other HALs).  After a press is
//! confirmed, sleeps until either release (rising edge) or the
//! long-press threshold elapses, whichever comes first.  Idle CPU
//! cost is essentially zero — the executor is parked the entire
//! time the user isn't touching the joystick.
//!
//! ## State machine
//!
//! Three states, transitioned by `next_event`:
//!
//! - **Idle** — no press in progress.  Wait for any pin to fall.
//!   Validate the press hasn't immediately bounced back (one short
//!   re-check after [`DEBOUNCE_DURATION`]).  Emit `Press(dir)` and
//!   move to [`InternalState::PressedAwaitingLong`].
//! - **PressedAwaitingLong** — the initial press has been emitted.
//!   Wait for either the pin to rise (release) or
//!   [`crate::LONG_PRESS_THRESHOLD`] to elapse from press time.
//!   On release: back to Idle, no event.  On timer:
//!   emit `LongPress(dir)` and move to [`InternalState::LongPressed`].
//! - **LongPressed** — the long-press has been emitted; just wait
//!   for release.  On release: back to Idle, no event.
//!
//! ## Multi-press
//!
//! While a press is in progress (PressedAwaitingLong / LongPressed),
//! other pins' edges are ignored.  Realistic for joysticks (one
//! direction at a time); avoids the complexity of independent
//! per-direction state machines.  If a future use case needs
//! per-direction concurrency, switch to the per-pin task pattern
//! (one Embassy task per pin pushing to a shared `Channel`).
//!
//! ## "Already pressed at startup"
//!
//! `wait_for_falling_edge` does not fire if the pin is already low
//! when the call begins.  If the user is holding a button when
//! `next_event` is first called, that initial press is missed.
//! `next_event` checks all five pin levels at entry on Idle and
//! synthesises a Press for any that are already low — closing this
//! corner case at the cost of one quick poll per Idle entry.

use embassy_futures::select::{select, select4, Either, Either4};
use embassy_time::{Duration, Instant, Timer};
use embedded_hal::digital::InputPin;
use embedded_hal_async::digital::Wait;

use crate::{idx_to_direction, Direction, JoystickEvent, LONG_PRESS_THRESHOLD};

/// Time we wait after a falling edge before re-checking the pin.
/// If the pin has bounced back HIGH within this window, we treat
/// the edge as a glitch and re-arm.  20 ms is comfortably past
/// typical mechanical-switch bounce (5–10 ms).
pub const DEBOUNCE_DURATION: Duration = Duration::from_millis(20);

/// Auto-repeat interval for held directional events.  After the
/// initial delay (see [`AUTO_REPEAT_INITIAL_DELAY`]) holding Up /
/// Down / Left / Right continues to fire synthetic `Press(dir)`
/// events at this cadence so list scrolling and the Scan screen's
/// horizontal cursor feel like a typamatic keyboard.  **Center
/// does not auto-repeat** — its long-press is the universal
/// "go home" action and emitting periodic Press(Center) events
/// afterwards would re-enter MainMenu repeatedly.
pub const AUTO_REPEAT_INTERVAL: Duration = Duration::from_millis(100);

/// Grace period between `LongPress(dir)` firing and the **first**
/// auto-repeat `Press(dir)` tick.  Matches the keyboard-typamatic
/// convention of "initial delay > inter-repeat interval" so a
/// long-press that intentionally transitions screens (e.g.
/// `LongPress(Left)` → `PowerOffConfirm`) doesn't immediately fire
/// a stray `Press(dir)` on the new screen while the user is still
/// releasing.  Users who actually want to auto-scroll hold past
/// this and inter-repeat takes over at [`AUTO_REPEAT_INTERVAL`].
///
/// 500 ms matches the long-press threshold itself — total
/// press-to-first-repeat is 1 s, which is comfortably past any
/// reasonable "hold to gesture then release" cycle.
pub const AUTO_REPEAT_INITIAL_DELAY: Duration = Duration::from_millis(500);

/// Internal state machine — see module docs.
enum InternalState {
    Idle,
    PressedAwaitingLong {
        idx: usize,
        press_start: Instant,
    },
    /// Long-press has fired.  For Up / Down (`auto_repeat = true`)
    /// the driver continues to emit synthetic `Press` events every
    /// [`AUTO_REPEAT_INTERVAL`] until the user releases.  For other
    /// directions (`auto_repeat = false`) the driver simply waits
    /// for release without further events.
    LongPressed {
        idx: usize,
        auto_repeat: bool,
        next_repeat_at: Instant,
    },
}

/// Edge-wake joystick driver.
pub struct Joystick5Way<U, D, L, R, C> {
    up: U,
    down: D,
    left: L,
    right: R,
    center: C,
    state: InternalState,
}

impl<U, D, L, R, C> Joystick5Way<U, D, L, R, C>
where
    U: Wait + InputPin,
    D: Wait + InputPin,
    L: Wait + InputPin,
    R: Wait + InputPin,
    C: Wait + InputPin,
{
    /// Wrap the five pins.  Each pin must already be configured as
    /// input with a pull-up.
    pub fn new(up: U, down: D, left: L, right: R, center: C) -> Self {
        Self {
            up,
            down,
            left,
            right,
            center,
            state: InternalState::Idle,
        }
    }

    /// Reset state — useful after a UI mode switch where we want to
    /// ignore whatever the user is currently holding.
    pub fn reset(&mut self) {
        self.state = InternalState::Idle;
    }

    /// Wait for and return the next [`JoystickEvent`].  Releases
    /// produce no event (they only return us to Idle for the next
    /// press cycle).
    pub async fn next_event(&mut self) -> JoystickEvent {
        loop {
            match self.state {
                InternalState::Idle => {
                    // Cover the "already-held at startup" case: if
                    // any pin is already LOW, treat as just-pressed
                    // without waiting for an edge.
                    let pre_pressed = self.first_pressed_pin();
                    let idx = match pre_pressed {
                        Some(idx) => idx,
                        None => self.wait_any_falling_edge().await,
                    };

                    // Confirm the press hasn't bounced back HIGH.
                    Timer::after(DEBOUNCE_DURATION).await;
                    if !self.is_low(idx) {
                        // Bounce — re-arm.
                        continue;
                    }

                    // Confirmed press.
                    let dir = idx_to_direction(idx);
                    self.state = InternalState::PressedAwaitingLong {
                        idx,
                        press_start: Instant::now(),
                    };
                    return JoystickEvent::Press(dir);
                }

                InternalState::PressedAwaitingLong { idx, press_start } => {
                    let long_at = press_start + LONG_PRESS_THRESHOLD;
                    let outcome =
                        select(self.wait_rising_edge(idx), Timer::at(long_at)).await;
                    match outcome {
                        Either::First(_) => {
                            // Released before long-press threshold.
                            self.state = InternalState::Idle;
                            // No event for release; loop and wait
                            // for the next press.
                            continue;
                        }
                        Either::Second(_) => {
                            // Long-press threshold elapsed.  For
                            // Up / Down, transition to auto-repeat
                            // mode so the user can hold to scroll
                            // a list quickly; for other directions,
                            // the long-press is the only event and
                            // we wait silently for release.
                            let auto_repeat = matches!(
                                idx_to_direction(idx),
                                Direction::Up
                                    | Direction::Down
                                    | Direction::Left
                                    | Direction::Right
                            );
                            self.state = InternalState::LongPressed {
                                idx,
                                auto_repeat,
                                // First repeat is delayed extra-long; subsequent
                                // ticks use AUTO_REPEAT_INTERVAL (see the LongPressed
                                // arm below).  See AUTO_REPEAT_INITIAL_DELAY docs.
                                next_repeat_at: Instant::now() + AUTO_REPEAT_INITIAL_DELAY,
                            };
                            return JoystickEvent::LongPress(idx_to_direction(idx));
                        }
                    }
                }

                InternalState::LongPressed {
                    idx,
                    auto_repeat,
                    next_repeat_at,
                } => {
                    if auto_repeat {
                        // Wait for either release OR the next
                        // auto-repeat tick.
                        let outcome = select(
                            self.wait_rising_edge(idx),
                            Timer::at(next_repeat_at),
                        )
                        .await;
                        match outcome {
                            Either::First(_) => {
                                self.state = InternalState::Idle;
                                continue;
                            }
                            Either::Second(_) => {
                                // Tick — emit synthetic Press and
                                // reschedule the next repeat.
                                self.state = InternalState::LongPressed {
                                    idx,
                                    auto_repeat,
                                    next_repeat_at: Instant::now()
                                        + AUTO_REPEAT_INTERVAL,
                                };
                                return JoystickEvent::Press(idx_to_direction(idx));
                            }
                        }
                    } else {
                        // No auto-repeat — just wait for release.
                        self.wait_rising_edge(idx).await;
                        self.state = InternalState::Idle;
                        continue;
                    }
                }
            }
        }
    }

    // ── Internal helpers ────────────────────────────────────────────

    /// Wait for a falling edge on **any** of the five pins.  Returns
    /// the index of whichever fired first.  Disjoint field-borrows
    /// (no `&mut self`) so all five futures can coexist.
    async fn wait_any_falling_edge(&mut self) -> usize {
        let Self {
            up,
            down,
            left,
            right,
            center,
            ..
        } = self;
        let result = select(
            select4(
                up.wait_for_falling_edge(),
                down.wait_for_falling_edge(),
                left.wait_for_falling_edge(),
                right.wait_for_falling_edge(),
            ),
            center.wait_for_falling_edge(),
        )
        .await;
        match result {
            Either::First(Either4::First(_)) => crate::IDX_UP,
            Either::First(Either4::Second(_)) => crate::IDX_DOWN,
            Either::First(Either4::Third(_)) => crate::IDX_LEFT,
            Either::First(Either4::Fourth(_)) => crate::IDX_RIGHT,
            Either::Second(_) => crate::IDX_CENTER,
        }
    }

    /// Wait for a rising edge on the specific pin identified by `idx`.
    /// Used to detect release of an already-pressed direction.
    async fn wait_rising_edge(&mut self, idx: usize) {
        match idx {
            crate::IDX_UP => {
                let _ = self.up.wait_for_rising_edge().await;
            }
            crate::IDX_DOWN => {
                let _ = self.down.wait_for_rising_edge().await;
            }
            crate::IDX_LEFT => {
                let _ = self.left.wait_for_rising_edge().await;
            }
            crate::IDX_RIGHT => {
                let _ = self.right.wait_for_rising_edge().await;
            }
            crate::IDX_CENTER => {
                let _ = self.center.wait_for_rising_edge().await;
            }
            _ => {}
        }
    }

    /// Snapshot one pin's level (true = LOW = pressed) without
    /// waiting.
    fn is_low(&mut self, idx: usize) -> bool {
        match idx {
            crate::IDX_UP => self.up.is_low().unwrap_or(false),
            crate::IDX_DOWN => self.down.is_low().unwrap_or(false),
            crate::IDX_LEFT => self.left.is_low().unwrap_or(false),
            crate::IDX_RIGHT => self.right.is_low().unwrap_or(false),
            crate::IDX_CENTER => self.center.is_low().unwrap_or(false),
            _ => false,
        }
    }

    /// Find the index of the first pin currently reading LOW
    /// (already-pressed-at-startup detection).  Returns `None` if
    /// all are HIGH.
    fn first_pressed_pin(&mut self) -> Option<usize> {
        for i in 0..crate::N {
            if self.is_low(i) {
                return Some(i);
            }
        }
        None
    }
}

// Suppress `unused` warning on `Direction` import in this module.
#[allow(dead_code)]
fn _direction_link() -> Direction {
    Direction::Center
}
