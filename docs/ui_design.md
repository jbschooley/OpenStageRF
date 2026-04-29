# OpenStageRF — UI Design

On-device UI for the receiver-side OpenStageRF unit. Designed for an I²C OLED (default: 128×64 SSD1306-class) plus a 5-way joystick (up, down, left, right, center push) wired to GPIOs.

This file specifies screen layouts, navigation flow, and implementation expectations. It is intentionally hardware-light and platform-agnostic — the same UI logic runs on any board with a comparable display + 5-way input.

## Hardware assumptions

- **Display:** 128×64 OLED, I²C, monochrome. SSD1306 or SH1106 controller (driver crate `osrf-driver-display-ssd1306`). Updates over a separate I²C bus from the radio's SPI bus (per README Decision #8) so display traffic never blocks radio handling.
- **Input:** 5-way joystick on 5 GPIOs, internal pull-up enabled, joystick pulls each pin to ground when actuated. Joystick directions: Up, Down, Left, Right, Center (push). Driver crate `osrf-driver-input-joystick5way` debounces (~20 ms) and emits events.
- **No additional buttons assumed.** v1 UI must be fully navigable with the 5-way alone. Future hardware variants may add a power button or dedicated function buttons; UI design is forward-compatible but doesn't depend on them.

## Joystick mapping (universal)

| Direction | Default action |
|---|---|
| **Up** | Move cursor / selector up; increase value in edit mode |
| **Down** | Move cursor / selector down; decrease value in edit mode |
| **Left** | Back / cancel / exit edit mode without applying |
| **Right** | Enter submenu / forward; same as Center on simple list screens |
| **Center (push)** | Confirm / select / apply |
| **Center (long-press, ≥1 s)** | Quick-action: from Idle, jumps directly to Channel Select; from any submenu, returns to Idle |

Long-press is handled by the input driver: a hold past the threshold emits `JoystickEvent::LongPress(Center)`.

## Screen states

```
                    ┌───────────────┐
            ┌──────►│    [Idle]     │◄─────────┐
            │       └──────┬────────┘          │
            │   center│   ▲ long-press from any
            │            │                     │
            │            ▼                     │
            │     ┌────────────────┐           │
            │ left│   [MainMenu]   │           │
            └─────┤                │           │
                  └──────┬─────────┘           │
                  right or center               │
            ┌──────┬─────┴─────┬────────┬─────┐│
            ▼      ▼           ▼        ▼     ▼│
  [ChannelSelect][KeySelect][PowerSelect][LinkStats][About]
            │       │        │            │      │
            └───────┴────────┴────────────┴──────┘
                       (Left → MainMenu;
                        Center on a confirmable screen → applies + returns to Idle)
```

State transitions are driven entirely by joystick events. Each state owns its own render + input handling.

## Screen layouts

All screens use a monospace 8×8 font, giving a 16-column × 8-row grid on a 128×64 display.

### Idle screen

The default screen shown after boot or after a successful menu action. Always reachable via long-press Center from any submenu.

```
┌────────────────┐
│OpenStageRF v0.1│
│                │
│Ch 14 915.0 MHz │
│Key 01 Band Main│
│Crypto: AES-CCM │
│RSSI: -68 dBm   │
│Bat 87%  Lk OK  │
│[●] menu        │
└────────────────┘
```

Live updates:
- RSSI / link state: every 100 ms
- Battery: every 5 s
- Channel / key / crypto: only on config change

### Main menu

Reached from Idle by Center press.

```
┌────────────────┐
│Menu            │
│                │
│▶Channel    14  │
│ Key        01  │
│ TX Power +10dB │
│ Link Stats     │
│ About          │
│[◀]back [▶]ent  │
└────────────────┘
```

Cursor indicator (`▶`) marks the current selection. Up/Down move the cursor; Right or Center enters the submenu; Left returns to Idle.

Trailing summary on each row shows the current value (channel number, key id, etc.) so users can see configuration at a glance without diving into each submenu.

### Channel Select

```
┌────────────────┐
│Channel  ★=now  │
│                │
│ 12  916.0 MHz  │
│ 13  916.5 MHz  │
│★14  917.0 MHz  │
│▶15  917.5 MHz  │
│ 16  918.0 MHz  │
│[●]apply [◀]bk  │
└────────────────┘
```

The list scrolls within a 5-row window centered on the cursor. The currently-active channel is marked with `★` regardless of where the cursor is. Up/Down moves the cursor; Center applies the highlighted channel (becomes the new `★`) and returns to Idle; Left cancels and returns to Main Menu.

Channels enumerated from the configured channel plan:
- Channels 1–16: LoRa-quiet zone (915.5 – 923.0 MHz at 500 kHz spacing)
- Channels 17–24: LoRa-shared bands (902–915 and 923–928 MHz at 1 MHz spacing)

### Key Select

```
┌────────────────┐
│Key      ★=now  │
│                │
│★01 Band Main   │
│   AES-CCM 128  │
│ 02 Backup      │
│   ChaCha20Poly │
│▶00 No Crypto   │
│[●]apply [◀]bk  │
└────────────────┘
```

Each key entry takes 2 rows (key_id + name on one row, cipher on the next). Cursor moves between entries (i.e. by 2 rows per Up/Down). Selecting `00 No Crypto` (key_id=0x00) disables encryption — sends packets in cleartext per the protocol spec.

If the on-device key name table is not loaded (no desktop config / no BLE pairing yet), names default to `(unnamed)` and only the key_id and cipher show.

### Power Select

```
┌────────────────┐
│TX Power        │
│                │
│  0 dBm  (1 mW) │
│ +5 dBm  (3 mW) │
│★+10 dBm (10 mW)│
│▶+15 dBm (32 mW)│
│+20 dBm (100mW) │
│[●]apply [◀]bk  │
└────────────────┘
```

Five fixed values per the README's Multi-band design decision. Center applies and returns to Idle.

### Link Stats (read-only)

```
┌────────────────┐
│Link Stats      │
│                │
│RSSI:    -68 dBm│
│SNR:      9.5 dB│
│Last pkt:  12ms │
│Lost: 3/1248    │
│Uptime: 0:12:34 │
│[◀]back         │
└────────────────┘
```

Updates at 5 Hz. Stats reset on link re-establishment after `LinkLost`. Useful for tuning antenna placement during setup.

### About (read-only)

```
┌────────────────┐
│About           │
│                │
│v0.1.0 dx_lr30  │
│Build: a3f9c1d  │
│Boot count: 47  │
│Device ID:      │
│ B7C49D3F       │
│[◀]back         │
└────────────────┘
```

Build hash from the git commit at compile time (`env!("VERGEN_GIT_SHA")` or similar). Boot count from the persisted counter. Device ID is the lower 4 bytes of the chip UUID, hex-formatted.

## State machine implementation

```rust
enum Screen {
    Idle,
    MainMenu { cursor: u8 },
    ChannelSelect { cursor: u8 },
    KeySelect { cursor: u8 },
    PowerSelect { cursor: u8 },
    LinkStats,
    About,
}

enum UiEvent {
    Joystick(JoystickEvent),
    LinkStateChanged,
    BatteryUpdated,
    Tick,                       // periodic redraw / animation tick
}
```

State transitions handled in a single function `fn handle(state: &mut Screen, event: UiEvent) -> UiAction`, where `UiAction` is one of `None`, `RedrawRequest`, `ApplySetting(...)`, `Persist`, etc.

## Embassy task layout

Three tasks, all on the same executor:

| Task | Priority | Tick rate | Responsibility |
|---|---|---|---|
| `ui_input` | medium | 100 Hz polling | Read joystick GPIOs, debounce, emit `JoystickEvent`s into a channel |
| `ui_state` | medium | event-driven | Consume `JoystickEvent`s + tick events, update `Screen`, push `RedrawRequest`s and config-change events to `settings_writer` |
| `ui_render` | low | up to 30 Hz, gated by `RedrawRequest` | Draw current `Screen` to OLED via I²C |

The radio task (link layer) runs on the same executor at higher priority. Embassy's cooperative scheduler ensures the radio task isn't blocked by UI work — each task awaits independently and yields between operations.

**No I²C or display work in any radio path or IRQ.** Screen updates only happen in `ui_render`, which is the lowest-priority task.

## Error / status overlays

Some events require interrupting the current screen to show a status:

- `LinkLost`: small banner overlay on Idle screen ("LINK LOST — sending all-notes-off"). Cleared automatically when link is re-established.
- `KeyAuthFailure` (Stage 3+): toast notification ("Auth failed — wrong key?"). Auto-dismisses after 3 s.
- `FlashWriteFailed`: persistent banner until reboot. Indicates settings can't be saved.
- `BatteryLow` (<15%): blinking indicator on Idle. Below 5%: full-screen warning.

Overlays are rendered on top of the current screen state without changing it.

## Boot sequence

1. **Splash** (200 ms): "OpenStageRF v0.1.0" centered. Visible long enough for the user to see the firmware loaded; not held longer than necessary.
2. **Self-test** (variable, typically <500 ms):
   - Radio init — show "Radio: OK" or "Radio: FAILED" with a hard fault on failure.
   - Flash storage — show "Storage: OK" or status.
   - Key store — show "Keys: N loaded".
3. **Idle** screen, with link search animation until first packet received.

If self-test fails on the radio, the device sits on the failure screen indefinitely (or attempts a watchdog reset after 5 s). User-actionable: reboot, check antenna, etc.

## Future-proofing notes

These are not required for v1 but the layout should not preclude them:

- **Pairing screen** (Stage 4 / v2 BLE config): a separate screen for "Pair via BLE" → discovery list → confirm. Slot reserved in Main Menu.
- **Diagnostic / spectrum scan screen** (Beyond v2): visualize RSSI per channel as a small bar graph. Useful when 902–928 ISM is congested.
- **Audio profile selection** (v3): replace "Channel" + "Key" with a richer "Mode" submenu that selects (audio profile, channel, key) as a bundle. v3 has more configuration surface.
- **Battery indicator** is shown but battery monitoring isn't wired up in v1 (DX-LR30 is bench-powered for the prototype). Reserve the screen real estate; report "Bat: ??" until ADC is set up.

## Display/input library choices

- `osrf-driver-display-ssd1306`: built on `embedded-graphics` for text rendering. Use a small bitmap font (FONT_8X8 or similar). Avoid heavy graphics primitives — overkill for this UI and consume CPU.
- `osrf-driver-input-joystick5way`: pure GPIO-poll, no library needed. Internal pull-ups enabled in board init.

Both crates depend only on `embedded-hal` traits — they're board-agnostic and will work on any platform that exposes I²C and GPIO via embedded-hal.
