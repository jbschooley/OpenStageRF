# OpenStageRF — UI Design

On-device UI for the receiver-side OpenStageRF unit. Designed against `embedded-graphics`' `DrawTarget` trait so the same UI logic renders to either a monochrome I²C OLED or a colour SPI TFT, plus a 5-way joystick (up, down, left, right, center push) wired to GPIOs (or, where the board has fewer buttons, a smaller input set — see input variants below).

This file specifies screen layouts, navigation flow, and implementation expectations. It is intentionally hardware-light and platform-agnostic — the same UI code runs on any board with an `embedded-graphics` `DrawTarget` plus enough input surface to navigate the menu tree.

## Hardware assumptions

The UI targets two concrete display classes; designs should look correct on both.

| Class | Default board | Bus | Resolution | Colour | Driver |
|---|---|---|---|---|---|
| Mono OLED | DX-LR30 (external add-on) | I²C @ 400 kHz | 128×64 | `BinaryColor` | upstream `ssd1306` crate, async via embedded-hal-async |
| Colour TFT | T114 (built-in) | SPI @ 8 MHz | 240×135 | `Rgb565` | upstream `mipidsi` crate, ST7789 controller |

**Why both:**
- DX-LR30 has no built-in display; the canonical add-on in the bench-test ecosystem is a 128×64 SSD1306 I²C OLED. Cheap, ubiquitous, low-power, monochrome.
- The Heltec T114 has a built-in 1.14" ST7789 TFT (RGB565, 240×135) wired to SPI1 (TWISPI1) plus DC/CS/RESET/backlight GPIOs. Using anything else on T114 would mean ignoring hardware that's already populated and powered through `VEXT_ENABLE` (P0_21).

**Layout approach:** all UI code targets `D: DrawTarget` where `D::Color: From<embedded_graphics::pixelcolor::BinaryColor>`. The UI uses two abstract colours — foreground and background — represented as `BinaryColor::On` and `BinaryColor::Off`. On the mono OLED these map directly; on the colour TFT, `On → white, Off → black` via `From<BinaryColor> for Rgb565`. The colour TFT therefore renders the same screens but with much more headroom: the 16-column × 8-row mono layout below maps to the colour display with comfortable padding. Future colour-only embellishments (e.g. red `LinkLost` banner, green `Lk OK`) can be guarded behind a `D::Color: From<embedded_graphics::pixelcolor::Rgb565>` bound and become no-ops on mono.

**Resolution and font:** the canonical mono layout is 16-col × 8-row at 8×8 pixels per glyph (128×64). On the colour TFT we render the same 16-col × 8-row grid scaled 2×, leaving border whitespace. This keeps the UI code identical and avoids per-board asset paths. A future "colour-rich" UI variant is possible but is not the v1 target — v1 prioritises identical behaviour across boards.

- **Bus separation:** display traffic never shares the radio's SPI bus (per README Decision #8). On DX-LR30 the OLED is on I²C1 (PB6/PB7) — the radio is on SPI1. On T114 the TFT is on TWISPI1 (P1_08/P1_09) — the radio is on TWISPI0. Display redraws cannot block radio handling.

- **Input:** 5-way joystick on 5 GPIOs, internal pull-up enabled, joystick pulls each pin to ground when actuated. Joystick directions: Up, Down, Left, Right, Center (push). Driver crate `osrf-driver-input-joystick5way` debounces (~20 ms) and emits events.
  - DX-LR30: matches the design — pads on the expansion header take a 5-way joystick module. Pins per `boards/dx_lr30/src/lib.rs::joystick`.
  - T114: the Heltec board itself only has a single user button (P1_10), but our deployment adds a 5-way joystick on header pins. Default pin assignment in `boards/t114/src/lib.rs::joystick` (P0_08/P0_00/P0_01/P1_11/P1_04 — GPIO header pins that don't collide with the dual-radio diversity pinout). Profiles where the joystick lives elsewhere on the header override that module.
  - **Reduced-input fallback:** for any board where the full 5-way isn't wired (or for a barebones v1 deployment), the input driver can degrade to single-button mode using the always-present `button_user`: short-press = Center, long-press = back / Idle. This is enough for a one-deep menu hierarchy. Used when `Resources::joystick` is `None` or absent.

- **No additional buttons assumed.** v1 UI must be fully navigable with the 5-way (or single-button fallback) alone. Future hardware variants may add a power button or dedicated function buttons; UI design is forward-compatible but doesn't depend on them.

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
| `ui_render` | low | up to 30 Hz, gated by `RedrawRequest` | Draw current `Screen` to the board's `DrawTarget` (I²C or SPI; whichever the board exposes) |

The radio task (link layer) runs on the same executor at higher priority. Embassy's cooperative scheduler ensures the radio task isn't blocked by UI work — each task awaits independently and yields between operations.

**No display-bus work in any radio path or IRQ.** Screen updates only happen in `ui_render`, which is the lowest-priority task. Whether the bus is I²C (DX-LR30) or SPI (T114) is invisible to the rest of the system — `ui_render` only touches the board's exposed `DrawTarget`.

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

**Display drivers (use upstream crates, no need to write our own):**

- **Mono I²C OLED (DX-LR30 add-on, future v2 RX):** the upstream [`ssd1306`](https://crates.io/crates/ssd1306) crate. Implements `embedded-graphics`' `DrawTarget<Color = BinaryColor>` directly; supports SSD1306 and SH1106 controllers. Async via embedded-hal-async (its `BufferedGraphicsModeAsync`). 128×64 the default; the same crate handles 128×32 and 64×48 if a smaller display gets used.
- **Colour SPI TFT (T114 built-in):** the upstream [`mipidsi`](https://crates.io/crates/mipidsi) crate. Supports ST7789 (T114), ILI9341, ST7735, GC9A01, etc. Implements `DrawTarget<Color = Rgb565>`. SPI bus via `display-interface-spi`.

The board crate's `Resources` struct exposes whichever driver is appropriate for that board, with a concrete type. UI code is generic over `DrawTarget` so it doesn't care which.

**Font:** small bitmap, 8×8 (e.g. `embedded_graphics::mono_font::ascii::FONT_6X10` or `FONT_8X13_BOLD`). Avoid heavy graphics primitives — overkill for this UI and they consume CPU on slow buses. The colour TFT has 16-bit pixels and ~390 KB per full frame; partial redraws (only changed regions) are essential.

**Input:**

- `osrf-driver-input-joystick5way`: pure GPIO-poll, no library needed. Internal pull-ups enabled in board init. Used on boards with a 5-way joystick wired to GPIO — DX-LR30 (expansion header), T114 (header pins, see `boards/t114/src/lib.rs::joystick`), and the future v2 custom board.
- `osrf-driver-input-button1`: degraded subset for deployments that don't wire the full joystick (or for the bare Heltec T114 with only its built-in user button). Same `JoystickEvent` output enum, but only emits `Center` / `LongPress(Center)` events. The UI state machine handles the reduced navigation: long-press = "back / cancel" instead of dedicated Left.

Both input crates depend only on `embedded-hal` GPIO traits — board-agnostic. A given board crate can expose both `joystick` and `button_user` modules; the profile picks which input driver to instantiate based on what's actually wired in that deployment.

**Why not write our own display driver:** SSD1306 and ST7789 are extremely well-trodden in the Rust embedded ecosystem; their upstream drivers are mature, support `embedded-graphics` natively, and have async backends. Writing our own would just be NIH.
