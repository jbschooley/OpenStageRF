# OpenStageRF — Reliability and Mid-Show Failure Modes

Anything that takes either side of the link offline mid-show is a P0 failure: a stuck note,
silent IEM, or dead mic during a downbeat is much worse than a 30-second total reboot before
soundcheck.  This doc enumerates the failure modes we care about, what we do at runtime to
detect / recover, and what we do offline to prevent regressions.

It cuts across milestones: live-link concerns mostly belong to **M5 (latency + soak)**,
runtime mitigations to **M7 (persistence + boot path)**, and CI invariants to the workflow
files alongside Milestone 0's existing `check.yml`.

## Failure modes

What can take a unit down between songs:

1. **Heapless capacity exhaustion.**  We are `no_std` + no-alloc — every queue, string, and
   buffer is a `heapless::Vec<T, N>` or `String<N>` with a fixed `N`.  `Vec::push` returns
   `Err` when full; we use `.ok()` in many places (UI nav stack, widget list, MIDI buffers)
   to silently drop overflow.  In normal operation `N` is generous; under load (a CC sweep,
   a SysEx burst, a held chord with sustained vibrato) any task that lets `Err` accumulate
   instead of draining can wedge.  **A "leak" in this codebase looks like a queue that fills
   and never drains, not a heap that grows.**
2. **Stack overflow.**  Each Embassy task has a fixed stack.  An async function that holds a
   large `[u8; N]` across an `await` lands the buffer on the task's frame; one accidentally
   large frame plus one nested call can hit the guard.  No alloc means no stack grows on
   demand.
3. **Embassy executor stall.**  A future that never `.await`s yields the executor; a tight
   loop in any task starves every other task on the same priority.  Symptom: link goes
   silent but the device looks alive (LED still toggling on a separate timer task).
4. **Panic.**  Today every profile uses `panic_probe`, which on a debug-probe target prints
   the panic message over RTT and *halts* the core.  On stage there's no probe attached, so
   that's effectively a brick until someone power-cycles.
5. **Hardware MCU lockup.**  PLL drop, brown-out under PA load, errata not yet seen.  No
   software path catches this; only a hardware watchdog reboot does.
6. **Flash write failure during a settings save.**  Currently no settings are persisted at
   runtime, but once M7 lands, a partial write that corrupts the settings page should not
   prevent boot.  A/B partition (or `sequential-storage`'s natural append-only behavior) is
   what protects against this.
7. **Radio driver wedge.**  SX1262 occasionally needs `SetStandby` / `ClearIrq` / re-init
   sequences that the upstream `sx1262 = 0.3` crate didn't expose; our hand-rolled driver
   has the five-step recovery encoded (see `memory/sx1262_handroll.md`), but a *new* wedge
   pattern would still need a watchdog-driven kick.
8. **MIDI sink buffer overflow on RX.**  If the receiver's UART TX can't drain fast enough
   (BPM-by-BPM CC bursts at 31250 baud is tight), the link runtime currently uses
   `BufferedUarte` which silently drops past its capacity.

## Runtime mitigations

What we ship in the firmware so that one bad packet, one runaway task, or one chip glitch
doesn't end the show.

### Hardware watchdog (M7)
Every profile arms the nRF52840 hardware WDT (`embassy_nrf::wdt`) with a generous reload
window — 1 s on the link tasks, 2 s on the UI task — and feeds it from a single
`watchdog_feeder` task that itself awaits a `Signal` kicked by every other task once per
loop.  If any monitored task wedges, the WDT fires within its window and the chip resets.
Reset cause is read on next boot via `RESETREAS` and logged to the panic-record region (see
below).  STM32F103 (DX-LR30) uses `IWDG` with the same pattern.

### Panic-to-flash + auto-reset (M7)
Replace `panic_probe` (in production builds) with a custom panic handler that:

1. Captures `PanicInfo`'s file/line/message into a small `[u8; 64]` plus the `RESETREAS`
   value, the firmware git hash, and the boot counter.
2. Writes the record to a dedicated flash page (one slot per boot, ring buffer of 8) using
   the same `embedded-storage` API we'll use for settings.
3. Triggers a soft reset (`cortex_m::peripheral::SCB::sys_reset()`).

`profiles/*` keep `panic_probe` for the `dev` build (RTT halt is what you want with a probe
attached); the `release` build uses the persistent panic handler.  Boot path reads the
panic-record ring on init and surfaces "last panic at <file:line>" on the About screen.

### Periodic self-checks (M5)
Once per second, a `health_check` task asserts invariants that should always hold:

- `nav_stack.len() <= MAX_NAV_DEPTH`
- Every `heapless::Vec` we own has `len() < capacity()` *or* the over-capacity event has been
  observed-and-cleared (capacity exhaustion is OK if drained promptly; persistent fullness
  is the leak).
- `LinkStats.total_accepted` is monotonic and `recent_loss_pct` is in `0..=100`.
- The UI's `current_menu` pointer is one of the known-static `MenuNode`s.

A failed assertion increments a `health_violations` counter (visible on About) and, if it
fails N times consecutively, panics — which fires the panic-to-flash + reset path above.
The point is to *find* the leak in soak tests by tripping the assertion early, not just
catch it on stage.

### Link-side liveness
The existing `WatchdogTimer` in `core/link/` (200 ms RX-side, fires all-notes-off on
expiry) is the *link-level* watchdog and stays as-is.  The hardware WDT above is one layer
out; both run independently.

## Offline / pre-release checks

What we do before a unit goes on stage.

### Soak tests (M5 deliverable)
Add to PLAN.md M5: a 4-hour soak run on the bench with realistic MIDI traffic (60 events/s
average, 200 events/s peaks during chord changes) that:

- Records `health_violations`, `link_stats.total_accepted`, panic-record count over time.
- Asserts no panic, no health-check violation, no `Err` from any UART/SPI op held longer
  than 100 ms.
- Documents stable RAM high-water-mark (per task) — currently this needs a pre-init pattern
  fill of the stack regions plus a post-soak readback; a small xtask helper extracts the
  numbers.

Pass criteria documented in `docs/v1_test_results.md` alongside the latency / range
numbers.

### CI invariants (alongside `.github/workflows/check.yml`)
- `cargo build --release` for every profile must produce a binary; we already have this.
- A custom `xtask audit` that fails CI if:
  - any crate in `core/`, `drivers/`, `protocols/` directly depends on an `embassy-*` HAL
    crate (portability boundary, README Decision #10).
  - any `.rs` file outside `boards/`/`profiles/` uses `extern crate alloc` or
    `alloc::*` imports (no_alloc invariant).
  - any task spawned by `#[embassy_executor::task]` declares a stack reservation past a
    documented budget.  We don't have this number yet — first soak test produces it; CI
    enforces it after that.
- `cargo deny` already catches license + advisory-DB issues.

### Manual pre-show checklist (lives in `docs/build_guides/`)
Before flashing a show-day unit:

- Boot, observe Idle screen for ≥ 5 s, no panic banner, RSSI sane.
- Walk through every menu screen and back out — exercises the nav stack and confirms
  rendering at each depth.
- Trigger a held chord on TX while toggling TX power — RX should not lose state through
  the reconfigure (M6 deliverable).
- Power-cycle TX while a chord is held — RX must all-notes-off within 250 ms (M4 already
  exit-criteria; this is the smoke test that proves it on the unit going on stage).

## Where each piece lives

| Mitigation                          | Crate / file                                | Milestone |
|-------------------------------------|---------------------------------------------|-----------|
| Hardware WDT arming + feeder        | `boards/t114/src/wdt.rs` (new)              | M7        |
| Panic-to-flash + auto-reset         | `core/panic/` (new)                         | M7        |
| `health_check` task + invariants    | `core/health/` (new)                        | M5        |
| Soak harness + RAM watermark xtask  | `xtask/src/soak.rs` (new)                   | M5        |
| Portability + no-alloc audit        | `xtask/src/audit.rs` (new)                  | M0 follow-up |
| Link-level RX watchdog (existing)   | `core/link/src/watchdog.rs`                 | M4 (done) |
