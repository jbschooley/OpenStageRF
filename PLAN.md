# OpenStageRF — First Prototype Plan

Concrete plan to ship Stage 1 (per [ROADMAP.md](ROADMAP.md)): 1× DX-LR30 TX, 1× DX-LR30 RX, one-way packetized MIDI over GFSK at 902–928 MHz US ISM, no diversity, no encryption initially, Rust + embassy on STM32F103.

This file is the build playbook. Architectural commitments are in [README.md](README.md); on-air protocol is in [protocols/midi_packet_v1/SPEC.md](protocols/midi_packet_v1/SPEC.md); UI is in [docs/ui_design.md](docs/ui_design.md).

## Confirmed design decisions

These were settled in conversation before code starts:

1. **Nonce scheme:** AEAD nonce = `[device_id:4][direction:1][session_seq:4][boot_counter:2]` (12 B for ChaCha20-Poly1305) or with extra reserved padding (13 B for AES-CCM). `boot_counter` persists to flash on each device boot; `session_seq` lives in RAM and resets per boot.
2. **Replay window:** sliding window of 64 packets at the receiver, tracked as a bitmap. Accepts any seq within `[last_seq − 64, last_seq + 64]` not previously seen.
3. **Heartbeat / watchdog:** TX sends a heartbeat packet every 20 ms (50 Hz) during musical idle. RX fires all-notes-off after 200 ms of no packets (≈10 missed heartbeats).
4. **Channel plan:** 16 default channels at 500 kHz spacing in the LoRa-quiet zone (915.5 – 923.0 MHz). 8 additional "LoRa-shared" channels available outside the quiet zone (902–915, 923–928 MHz). All centers align with the LoRaWAN 200 kHz grid.
5. **TX power:** default +10 dBm (10 mW). User-selectable: 0, +5, +10, +15, +20 dBm.
6. **Persistence layout (DX-LR30, 64 KB flash):**
   - 56 KB firmware
   - 4 KB settings (channel, power, active key slot, UI prefs)
   - 4 KB key store (~56 keys; each entry is ~72 bytes including names and `sequential-storage` overhead — see key entry layout below)
   - boot counter lives in settings page (1 write per boot)
   - Key entry layout (61 bytes raw): `local_slot: u8` (UI identifier) + `cipher_id: u8` + `key_bytes: [u8; 32]` + `tx_nonce_counter: u64` + `name: [u8; 16]` + `key_fp: [u8; 3]` (SHA-256(cipher_id ‖ key_bytes)[0..3], cached at provisioning time). Names are stored on **all** hardware — including DX-LR30.
7. **Logging:** `defmt` + RTT via probe-rs. Feature-gate `defmt` so future Zephyr port can substitute Zephyr logging.
8. **Error handling:** `Result`-typed APIs throughout. Top-level handler logs + resets on critical errors (radio init failure, flash failure). Minimize panics; never `unwrap()` outside known-safe contexts.
9. **Crate naming:** `osrf-` prefix on every workspace crate (`osrf-link`, `osrf-radio-sx126x`, etc.).
10. **License headers:** `// SPDX-License-Identifier: AGPL-3.0-or-later` on every source file. Enforced via `cargo deny` in CI.
11. **Test strategy:** ~85% of tests run in Docker / GitHub Actions (unit, property, mock-radio integration, cross-compile, lints, license audit). HIL tests (real RF, end-to-end latency, real persistence cycles) deferred until self-hosted hardware runner is set up.
12. **Audio extensibility:** the v1 transport envelope handles audio later via new `event_type` values. No protocol redesign needed for v3 audio.
13. **Key fingerprint on wire:** the wire format uses a 3-byte `key_fp` field (SHA-256(cipher_id ‖ key_bytes)[0..3]) instead of a locally-assigned `key_id`. This makes key matching device-independent — two devices holding the same key material compute the same fingerprint regardless of which local slot they assigned it to, so TX/RX key sets do not need to be ordered identically. `key_fp = 0x000000` is the sentinel for no-encryption mode. With 16.7 M possible values, fingerprint collision probability is ~0.009% across a 56-key store (birthday problem). Locally each device still assigns a slot number for UI display purposes; the fingerprint is precomputed at provisioning time and cached in the key entry.

## Milestones

### Milestone 0 — toolchain & workspace skeleton (1–2 days)

**Goal:** every developer can flash the DX-LR30 and see RTT logs.

- [x] Install Rust stable + `thumbv7m-none-eabi` target (`rustup target add thumbv7m-none-eabi`)
- [x] Install `probe-rs` (`cargo install probe-rs-tools --locked` → probe-rs 0.31.0 at `~/.cargo/bin`)
- [x] Set up Cargo workspace skeleton:
  - root `Cargo.toml` with `[workspace]` and member list
  - `Cargo.lock` checked in (this is a binary project)
  - `rust-toolchain.toml` pinned to 1.95.0
  - `.cargo/config.toml` with default `[target.thumbv7m-none-eabi]` runner = `probe-rs run --chip STM32F103C8`
  - `[alias] xtask = ...` so `cargo xtask build <profile>` works
- [x] Initial crates: `osrf-link`, `osrf-protocols-midi-v1`, `osrf-crypto`, `osrf-radio-sx126x`, `osrf-board-dx-lr30`, `osrf-port-embassy-stm32`, `osrf-app-midi-node`, `osrf-xtask`
- [x] `osrf-xtask` reads `profiles/<name>/profile.yaml` and shells out to `cargo build --target thumbv7m-none-eabi -p osrf-app-midi-node --features <board>`
- [x] `SPDX-License-Identifier: AGPL-3.0-or-later` headers on every source file; `deny.toml` created for cargo-deny (install `cargo install cargo-deny` to enforce in CI)
- [ ] First flash: blink LED on DX-LR30 + RTT log viewed via `probe-rs attach` — **requires hardware** (binary `target/thumbv7m-none-eabi/debug/embassy_dx_lr30` is built and ready)

**Exit criteria:** `cargo run -p osrf-app-midi-node --target thumbv7m-none-eabi --features dx_lr30` flashes the board and shows logs.

### Milestone 1 — schematic verification + hardware bring-up (3–5 days)

**Goal:** all peripherals respond. Pinmap is no longer TBD.

- [x] DX-LR30 schematic verified; pin assignments live in `boards/dx_lr30/src/lib.rs` (module-per-peripheral)
- [x] T114 schematic verified (Heltec Mesh Node T114 v2.0); pin assignments in `boards/t114/src/lib.rs`
- [x] `osrf-board-dx-lr30`: HSI+PLL clock config (64 MHz, no external crystal); `boards/dx_lr30/src/clocks.rs`
- [x] Smoke test `examples/smoke.rs` for both boards — LED, SX1262 reset/BUSY/DIO1/CS, MIDI UART init.
      Run: `cargo run --example smoke -p osrf-board-dx-lr30 --target thumbv7m-none-eabi`
      Run: `cargo run --example smoke -p osrf-board-t114    --target thumbv7em-none-eabihf`
- [ ] Wire ST-Link SWD: SWDIO, SWCLK, GND, optionally NRST
- [ ] Wire Adafruit MIDI FeatherWing on the chosen board's MIDI UART pins
- [ ] (RX side only) wire I²C OLED (DX-LR30) or onboard ST7789 (T114) + buttons
- [ ] Run `smoke` example on each board; fix any pin mismatches found

**Exit criteria:** schematic-verified pinmap committed; all GPIOs respond on real hardware.

### Milestone 2 — SX1262 driver (5–10 days)

**Goal:** packets travel between two SX1262-equipped boards (DX-LR30 ↔ DX-LR30, or DX-LR30 ↔ T114) over the air.

- [x] Survey: `lora-phy` (LoRa-only on SX126x backend), `tweedegolf/sx126x` (no async, no GFSK, no DIO2 RF switch), `BroderickCarlin/sx1262` (full GFSK, async, DIO2 switch). See conversation log for full report.
- [x] Decision: wrap `sx1262 = "0.3"` with our own `osrf-radio-sx126x` thin async layer
- [x] `osrf-radio-sx126x` wrapper — compiles for both targets, with and without `defmt`:
  - `Sx1262Radio<Spi, Dio1, Reset, Switch>` — generic over `embedded-hal-async` `SpiDevice` + `Wait`-able DIO1 + `OutputPin` reset + a `RfSwitchControl` impl
  - `RfSwitchControl` trait with two impls: `Dio2RfSwitch` (T114, calls `SetDio2AsRfSwitchCtrl(true)` once during init) and `PinRfSwitch<Txen, Rxen>` (DX-LR30, toggles GPIOs around tx/rx)
  - `init`, `set_frequency`, `set_modulation_gfsk`, `set_packet_format`, `set_tx_power`, `tx`, `rx_continuous` — all async
  - `RxPacket { len, rssi_dbm, crc_ok }` — no SNR (SX1262 only reports SNR for LoRa; FSK uses RssiSync from `GetPacketStatus`)
- [ ] **Board-side reset gate:** the wrapper does not own `DelayNs`, so the board's `Resources` builder must pulse NRESET low ≥100 µs and wait ≥10 ms before calling `radio.init().await`. Add this to each board crate's `resources()` constructor when the radio gets wired in.
- [ ] Wire `radio0` field into `Resources` on both `boards/dx_lr30/` and `boards/t114/` — uses `PinRfSwitch` on DX-LR30, `Dio2RfSwitch` on T114
- [ ] Bench test: TX board sends `[0xDE 0xAD 0xBE 0xEF]` once per second at 915 MHz / 300 kbps GFSK; RX board logs received bytes + RSSI via RTT.

**Exit criteria:** RX reliably receives TX's test packet at 5 m line of sight, RSSI logged is reasonable (-50 to -70 dBm).

**Known upstream caveats** (worth tracking, not blocking):
- `sx1262 = "0.3.0"` lags master — `PacketParams` is a raw `[u8; 9]` here vs. typed enums on master. Wrapper hand-encodes the byte layout. If we hit issues, switch to a git dep at master.
- Upstream `Status::from_bytes(...).unwrap()` panics on weird reserved bits in the chip's status response. Not a problem under nominal operation but worth knowing if we see mysterious crashes.

### Milestone 3 — DIN MIDI parser and I/O (3–5 days)

**Goal:** MIDI events flow in and out of the MCU correctly.

- [x] `osrf-midi-din` parser crate (`drivers/midi/din/`) — `MidiParser` consumes a 31250 baud byte stream and emits typed `MidiEvent` values.  17 host-side unit tests cover the state-machine edge cases (running status, real-time interruption, SysEx with embedded real-time, malformed-status-during-SysEx, undefined system bytes).
- [x] MIDI byte parser state machine:
  - status bytes (0x80–0xFF) vs data bytes (0x00–0x7F)
  - running status (data bytes following a status byte reuse the previous status)
  - real-time messages (0xF8–0xFF) can interrupt other messages without affecting parser state
  - SysEx (0xF0…0xF7), streamed via `ParseResult::SysExByte(u8)` between `MidiEvent::SysExStart` and `SysExEnd` (consumer accumulates if it cares)
- [x] Output shape: `MidiEvent::{NoteOff, NoteOn, PolyAftertouch, ControlChange, ProgramChange, ChannelAftertouch, PitchBend, TimeCodeQuarterFrame, SongPosition, SongSelect, TuneRequest, TimingClock, Start, Continue, Stop, ActiveSensing, SystemReset, SysExStart, SysExEnd}`.  SysEx body is **streamed**, not buffered into a `SysExFragment` — that decision keeps the parser allocation-free and pushes the buffer-size policy to the consumer.
- [x] Board-agnostic bench app `osrf-app-midi-bench` (`apps/midi_bench/`) generic over `embedded_io_async::{Read, Write}`: `run_rx` parses + logs every event, `run_tx` arpeggiates C major using running status with interleaved real-time clock.
- [x] DX-LR30 board crate exposes `Resources::midi_uart` as `BufferedUart<'static>` over USART3 PB10/PB11.  **DMA conflict resolution**: USART3's hardwired DMA channels (DMA1_CH2/CH3) collide with SPI1's allocation for the SX1262, so the MIDI UART runs interrupt-driven via `BufferedUart` — at 31250 baud the per-byte interrupt is trivial.
- [x] T114 board crate exposes `Resources::midi_uart` as `BufferedUarte<'static>` over UARTE1 P0_09/P0_10 (consumes TIMER1 + PPI_CH0/CH1 + PPI_GROUP0 for the buffered driver's idle-detect machinery).  Plain `Uarte` only implements `embedded_io_async::Write`, not `Read`, so `BufferedUarte` is the right choice anyway.
- [x] Profile binaries: `dx_lr30_midi_{rx,tx}`, `t114_midi_{rx,tx}` (the latter pair gated with optional `usb-log` feature mirroring the radio bench profiles).
- [ ] Bench test on TX side: connect keyboard MIDI OUT to FeatherWing → DX-LR30; play notes; log parsed events via RTT.
- [ ] Bench test on RX side: synthesize a stream of `MidiEvent::NoteOn`/`NoteOff` in firmware, push out FeatherWing UART, verify on a synth.

**Exit criteria:** keyboard-played MIDI parses to correct events including pitch bend, sustain, real-time clock; firmware-generated MIDI plays back on a synth without artifacts.

### Milestone 4 — on-air protocol v1 + link layer (5–7 days)

**Goal:** end-to-end MIDI over the wireless link, no encryption yet.

- [ ] `osrf-protocols-midi-v1`: implement encode/decode per [SPEC.md](protocols/midi_packet_v1/SPEC.md). Start with `key_id = 0x00` (no encryption) only.
- [ ] `osrf-link`:
  - `LinkSender`: takes `MidiEvent`, encodes to packet, hands to radio. Generates seq numbers via `(boot_counter, session_seq)`.
  - `LinkReceiver`: receives raw packet from radio, validates radio-level CRC, decodes, runs replay window check (64-packet sliding window with bitmap), emits dedup'd `MidiEvent` to consumer.
  - `Watchdog`: timer that fires after 200 ms of no received packets; emits `LinkLost` event to consumer.
  - `Heartbeat`: timer that emits `MidiEvent::Heartbeat` every 20 ms when no other event has been sent.
- [ ] `osrf-app-midi-node` TX role: read MIDI from FeatherWing → `LinkSender` → radio
- [ ] `osrf-app-midi-node` RX role: radio → `LinkReceiver` → on `LinkLost`, send all-notes-off; on event, write to FeatherWing UART
- [ ] Tests in mock-radio harness: dedup correctness, replay rejection, watchdog firing, heartbeat timing.

**Exit criteria:** TX and RX boards transmit MIDI events end-to-end. Power off TX while a chord is held → RX receives all-notes-off within 250 ms.

### Milestone 5 — end-to-end live test + latency measurement (2–3 days)

**Goal:** measured latency, validated stage scenario.

- [ ] Hardware setup: keyboard → DX-LR30 TX, DX-LR30 RX → synth/computer. Battery-power TX side; USB-power RX side.
- [ ] Latency measurement: trigger oscilloscope on a known MIDI byte (e.g. Note On status byte 0x90) on the input UART, capture corresponding output byte on the output UART. Measure delta. Document target (<3 ms RF transit) vs measured.
- [ ] Range test: walk away with TX, log RSSI on RX. Document range at which CRC errors begin.
- [ ] Stress test: play a busy MIDI sequence (60 events/sec), verify no missed/garbled events for 10 minutes.
- [ ] Document any anomalies: stuck notes, clock jitter, dropouts, etc.

**Exit criteria:** all latency / range / stress numbers documented in `docs/v1_test_results.md`.

### Milestone 6 — UI on RX side (5–7 days)

**Goal:** RX has a usable on-device interface for channel/power configuration.

- [ ] `osrf-driver-display-ssd1306`: I²C OLED, basic monospace text rendering (8×8 font, 16 cols × 8 rows on 128×64 display)
- [ ] `osrf-driver-input-joystick5way`: 5-way joystick, debounced (~20 ms), generates `JoystickEvent::{Up, Down, Left, Right, Center, LongPress(Direction)}`
- [ ] `osrf-ui` crate with screen state machine and the screens defined in [docs/ui_design.md](docs/ui_design.md)
- [ ] Embassy task layout: `ui_render` (low priority, 30 Hz cap), `ui_input` (medium priority, 100 Hz polling), `ui_state` (medium priority, event-driven)

**Exit criteria:** all screens render, joystick navigates correctly, channel and power changes apply live (without reboot).

### Milestone 7 — persistence (3–5 days)

**Goal:** settings survive reboot. Boot counter increments correctly.

- [ ] Flash partition layout per *Confirmed design decisions* above
- [ ] `sequential-storage` integration over a defined flash region for the settings page and a separate region for the key store
- [ ] Boot counter: read on boot, increment, write back. One flash write per boot.
- [ ] Settings (channel, power, active key slot, ui prefs): read on boot, written when UI changes confirm
- [ ] Key store: stub for v1 (only no-encryption mode, `key_fp=0x0000`, used); structure ready for Stage 3 encryption work

**Exit criteria:** power-cycle the device, settings retained; boot counter visible in `[About]` screen and increments on each boot.

## Total estimate

**Single-developer first prototype: 4–6 weeks**, plus 1–2 weeks if `osrf-radio-sx126x` is written from scratch instead of integrating an existing crate.

## Out of scope for first prototype

- Diversity (Stage 2)
- Second platform port to T114 (Stage 2.5)
- Encryption / authentication (Stage 3)
- BLE config (Stage 4 / v2 board)
- Audio (Stage 5 / v3 board)
- Multi-band switching (just 902–928 MHz US 915 ISM)
- All advanced UI features (pairing screens, key import flows)

## Embassy → Zephyr portability — guardrails to enforce during development

Per README Decision #10, the portability boundary must be defended *as code is written*, not after the fact. Concrete rules:

- `osrf-link`, `osrf-protocols-*`, `osrf-crypto`, `osrf-driver-*` crates may **only** depend on:
  - `embedded-hal` and `embedded-hal-async` traits
  - `embedded-storage` and `embedded-storage-async` traits
  - project-internal trait crates (e.g. `osrf-platform` for `MonotonicClock` etc.)
  - pure-Rust no_std utilities (`heapless`, `bitflags`, RustCrypto crates, `defmt` behind feature flag)
- `osrf-board-*` and `osrf-port-*` crates are the **only** places that import vendor HALs (`embassy-stm32`, `embassy-nrf`, etc.)
- Time/delay: never call `embassy_time::Timer` from outside a port crate. Inside `osrf-link`, use a `MonotonicClock` trait.
- Logging: use `defmt` macros directly during development, gate behind `cfg(feature = "defmt")` for crates intended for both embassy and Zephyr builds.
- Async runtime entry: `apps/midi_node/src/bin/embassy_dx_lr30.rs` (and similar) is per-platform. Shared async logic lives in `apps/midi_node/src/lib.rs` and is called from each entry binary.

If a driver or core file ever has to import `embassy-*` directly, that's a portability bug — fix at the trait boundary.

## CI / automation pipeline (to set up alongside Milestone 0)

GitHub Actions workflows:

- `.github/workflows/check.yml` (every PR + push):
  - `cargo fmt --check`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo deny check` (license + security audit)
  - `cargo build --target thumbv7m-none-eabi -p osrf-app-midi-node --features dx_lr30`
  - (later) other build targets: nRF52840, nRF5340 host
- `.github/workflows/test.yml` (every PR + push):
  - `cargo test` for all host-runnable crates (`osrf-link`, `osrf-protocols-*`, `osrf-crypto`, etc.)
  - mock-radio integration tests
- `.github/workflows/hil.yml` (manual / nightly, self-hosted runner with hardware): deferred — set up after Milestone 5 has been bench-validated and the hardware test fixture exists.

## Open hardware questions for the user

These should be settled before or during Milestone 1:

- Where will the user get the DX-LR30 schematic for pinmap verification?
- Are both DX-LR30 boards already in hand, or do they need to be ordered?
- Adafruit MIDI FeatherWing — already on hand or ordered?
- I²C OLED + joystick — already on hand or which exact part numbers?
- ST-Link debugger model and any cabling already prepared?

These don't block Milestone 0 (toolchain) but block Milestone 1 (bring-up).
