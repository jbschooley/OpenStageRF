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
- [x] First flash on **DX-LR30**: blink running, GPIO toggle confirmed end-to-end.
      SWD pins (PA13/PA14) are not broken out on the LR30-SP carrier, so flashing
      uses the on-board CH340C USB-C → STM32 ROM UART bootloader via `stm32flash`
      (BOOT0/NRST driven by RTS#/DTR# through the auto-reset transistor pair).
      Working command: `stm32flash -w bin -v -g 0x08000000 -i '-rts,dtr,-dtr:rts,-dtr' /dev/tty.usbserial-…`.
      Bring-up gotcha: the on-board status LED (LED2 on PC13 through R2 = 4.7 KΩ)
      draws only ~64 µA — invisibly dim and apparently dead on the bring-up unit.
      `profiles/dx_lr30_blink` now drives PB0 (H3 pin 8) for an external LED +
      bypasses `board::resources()` (which inits SPI/USART/I²C and could hang
      with no peripherals connected); both choices documented in the profile's
      main.rs and reverted once peripherals are validated.
      RTT logs deferred until SWD wiring is added (would require tack-soldering
      to PA13/PA14 on the QFP48).
- [x] First flash on **T114** — `profiles/t114_blink` blinks the green LED on P1_03 at 1 Hz.
      Flashing path is **UF2 over the Heltec `ht-n5262 0.9.0` bootloader** (double-tap
      reset → drag-drop onto T114BOOT volume).  Required ~6 hours of bring-up work because
      the bootloader hand-off has a stack of hardware quirks that are *not* documented in
      Heltec's docs:
        1. **No SoftDevice on this unit.** The bootloader's `is_sd_existed()` returns false,
           so user app must be at `0x1000` (MBR_SIZE), not `0x26000` (SD_SIZE) as every
           Adafruit/Meshtastic doc claims for factory T114s.  `boards/t114/memory.x` FLASH
           ORIGIN = `0x00001000`.  UF2 conversion: `python uf2conv.py firmware.bin -c -b 0x1000 -f 0xADA52840`.
        2. **`#[cortex_m_rt::pre_init]` must call `osrf_board_t114::bootloader_handoff()`**:
           VTOR relocation (cortex-m-rt does NOT do this), NVIC ICER/ICPR clear, RTC
           INTENCLR/EVTENCLR/CLEAR, all-TIMERs stop+shutdown, GPIOTE config clear, PPI CHENCLR,
           LFCLK STOP.  Without each piece, different things break in different combinations
           (LED stuck driven by leftover GPIOTE, embassy time driver wedged at first `.await`,
           executor frozen by stray USBD interrupts firing through cortex-m-rt's
           `DefaultHandler` infinite-loop).
        3. **`mipidsi::Builder::init()` hangs** on this hardware — every other piece of
           `build_resources` (SX1262 SPI, UARTE1, raw TWISPI1 SPI write, 120 ms
           `Delay::delay_ms`) verified working in isolation via stepwise diagnostic.  Display
           field removed from `Resources` until smoke-test development debugs it.
           `profiles/t114_ui_demo` is broken with a TODO header until then.

**Exit criteria:** `cargo run -p osrf-app-midi-node --target thumbv7m-none-eabi --features dx_lr30` flashes the board and shows logs. **Met for T114** via `firmware.uf2` flow; deferred for DX-LR30 until USART1-via-CH340C logging is wired (see Milestone 1).

### Milestone 1 — schematic verification + hardware bring-up (3–5 days)

**Goal:** all peripherals respond. Pinmap is no longer TBD.

- [x] DX-LR30 schematic verified; pin assignments live in `boards/dx_lr30/src/lib.rs` (module-per-peripheral)
- [x] T114 schematic verified (Heltec Mesh Node T114 v2.0); pin assignments in `boards/t114/src/lib.rs`
- [x] `osrf-board-dx-lr30`: HSI+PLL clock config (64 MHz, no external crystal); `boards/dx_lr30/src/clocks.rs`
- [x] Smoke test `examples/smoke.rs` for both boards — LED, SX1262 reset/BUSY/DIO1/CS, MIDI UART init.
      Run: `cargo run --example smoke -p osrf-board-dx-lr30 --target thumbv7m-none-eabi`
      Run: `cargo run --example smoke -p osrf-board-t114    --target thumbv7em-none-eabihf`
- [x] **T114 SWD wired** (ST-Link V2 → SWDIO/SWCLK/RST/GND test points on the back of the PCB)
- [x] **T114 smoke run on real hardware** — all checks PASS:
      LED P1_03 toggles, button P1_10 reads released (pull-up OK), VEXT P0_21 toggles,
      SX1262 BUSY=false post-reset (chip alive and in standby), DIO1=false (idle as expected),
      CS toggles cleanly.  Embassy time driver matches wall-clock to 0.0006% over 200 ms = LFRC fine.
- [ ] **DX-LR30 SWD: NOT POSSIBLE on this carrier.** PA13/SWDIO and PA14/SWCLK are not broken
      out to either H3 or H4 expansion header (verified 2026-05-04 against
      `LR30-SP PCBA schematic diagram.pdf`).  Tack-soldering to the QFP48 chip pads is the only
      SWD path.  Two practical alternatives:
        a. Re-route logs via the on-board CH340C USART1 bridge (PA9/PA10 → USB-Serial).  The
           board crate already exposes `dx_lr30::debug_uart` for this.  Requires adding
           defmt-over-UART (e.g. `defmt-bbq` + a USART1 forwarder task) or routing `log::*`
           through the bridge similar to T114's `usb-log` feature.  No SWD probe needed.
        b. Ship without runtime logs; trust visual / multimeter / scope verification of the
           pinmap (reasonable since the same Resources builder works on T114 and the schematic
           pin assignments are committed).
- [ ] Wire Adafruit MIDI FeatherWing on the chosen board's MIDI UART pins
- [ ] (RX side only) wire I²C OLED (DX-LR30) or onboard ST7789 (T114) + buttons
- [ ] Run `smoke` example on each board; fix any pin mismatches found

**Infrastructure changes during M1 bring-up:**
- `.cargo/config.toml`: added `[env] DEFMT_LOG = "info"` workspace-wide.  Without this, defmt
  filters log macros to no-ops at compile time and RTT shows nothing despite a valid SEGGER
  control block.  Verified by inspecting RTT WrOff register on the running chip.
- `.cargo/config.toml`: T114 runner uses `--rtt-scan-memory` (probe-rs 0.31 default symbol
  lookup misses our control block at 0x20000010 since `memory.x` puts RAM origin at 0x20000008).
- `Embed.toml`: added for cargo-embed as a fallback RTT viewer; same chip + scan-memory config.
- `osrf-board-t114::bootloader_handoff()`: VTOR + NVIC + RTC/TIMER/GPIOTE/PPI/LFCLK teardown
  every UF2-flashed binary must call from `#[pre_init]`.  Hardware-specific to the Heltec
  `ht-n5262 0.9.0` bootloader hand-off; not something C/Arduino would avoid.
- `boards/t114/memory.x`: FLASH ORIGIN = `0x00001000` (this T114 unit has no SoftDevice; the
  bootloader's `is_sd_existed()` returns false → user app at MBR_SIZE, not SD_SIZE).
- `Resources::display` removed from T114; `mipidsi::Builder::init()` hangs after every other
  build_resources step (radio SPI, UART, raw TWISPI1 SPI write, 120 ms `Delay::delay_ms`)
  verified in isolation.  Deferred to whenever UI work resumes.  `profiles/t114_ui_demo` is
  intentionally broken with a TODO header until we revisit.

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
- [x] **Board-side reset gate:** the wrapper now owns the reset pulse + post-reset wait via `embassy_time::Timer`.  Boards just hand a high-idle reset pin to `Sx1262Radio::new()`.
- [x] Wire `radio0` field into `Resources` on both `boards/dx_lr30/` and `boards/t114/` — uses `PinRfSwitch` on DX-LR30, `Dio2RfSwitch` on T114.  T114 also passes BUSY (P0_17).
- [x] Bench test PASSED: TX T114 sends `[0xDE 0xAD 0xBE 0xEF, seq:u32]` once per second at 915 MHz / 300 kbps GFSK / +14 dBm; RX T114 logs `RX #N: len=8 rssi=-39dBm bytes=[0xde, 0xad, 0xbe, 0xef, 0x00, 0x00, 0x00, 0xNN]` per packet with monotonic sequence numbers.

**Exit criteria MET (2026-05-04):** RX receives every TX packet at desk-distance, RSSI -39 to -40 dBm.

**Massive sx1262 driver pivot during M2 bring-up:**
The `sx1262 = "0.3"` crate had two bugs that made it unusable on the Heltec T114:
1. `Status::from_bytes(...).unwrap()` panicked on `cmd_status` values 0 (Reserved) and 1 (RFU), which the chip returns in normal operation.  We were chasing "cmd_status=5 = Failure to execute" for hours; the real chip state was hidden by parser panics.
2. No exposure of raw register access → couldn't apply the mandatory **TxClampConfig** workaround (datasheet §15.2: `REG[0x08D8] |= 0x1E` after `SetPaConfig`).

Replaced with hand-rolled raw SPI command layer in `drivers/radio/sx126x/src/lib.rs`.  ~700 lines, no external SX1262 crate dependency.  Five non-obvious things the chip needs that we discovered the hard way:

1. `SetDio3AsTcxoCtrl(1.8 V, 5 ms)` before any RF / calibration — Heltec T114's LR1262 module wires DIO3 to power the TCXO; without this, PLL never locks and every TX rejects with `cmd_status=5`.
2. `SetRegulatorMode(DC-DC)` — chip defaults to LDO-only after POR; PA browns out the LDO at +14 dBm and up.
3. `SetRxTxFallbackMode(FS = 0x40)` — after TX_DONE, chip auto-enters FS (PLL locked, PA off).
4. **Do NOT call `SetStandby(RC)` after TX_DONE.**  Empirically (every-other-TX failure pattern), explicit standby leaves the chip in a sub-state that rejects the next SetTx for ~3 seconds.  Let fallback mode handle the post-TX state.
5. `TxClampConfig` workaround per datasheet §15.2 — `REG[0x08D8] |= 0x1E` after `SetPaConfig`.

`SetDio2AsRfSwitchCtrl` must be called LAST (after all RF config) per RadioLib pattern.  See `memory/sx1262_handroll.md` for the full working init order.

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

- [x] `osrf-protocols-midi-v1`: wire-format encode/decode per [SPEC.md](protocols/midi_packet_v1/SPEC.md), `key_fp = 0x000000` (no-crypto) path only.
  - Public API: `Header { ver, key_fp, seq, event_type }` (with `make_seq(boot_counter, session_seq)` packer + `boot_counter()`/`session_seq()` accessors), `FragState`, `Body<'a>` (Heartbeat / MidiMessage / SysExFragment / Unknown for forward-compat), `Packet<'a>`, `EncodeError`, `DecodeError`.
  - Free functions: `wire_len(body)`, `encode(out, header, body) -> usize`, `decode(buf) -> Packet`.
  - Encode/decode are tag-agnostic — they only frame the wire format.  AEAD-using callers (future): `encode` writes header + plaintext, caller hands `out[..HEADER_LEN]` (AAD) and `out[HEADER_LEN..n]` (plaintext) to a cipher, then appends the tag past `out[..n]`.
  - `#![cfg_attr(not(test), no_std)]` + `[lib] test = true` — host tests intentionally enabled here despite the project-wide `test = false` convention, because wire-format correctness is the kind of thing that benefits enormously from unit tests.
  - **23 host-side tests pass**, covering: round-trips for all body variants (Heartbeat / 1-,2-,3-byte MIDI / all four FragState values / Unknown event types), explicit byte-level wire-layout assertion (canary against accidental wire breaks), seq packing/unpacking, header AAD layout, all error paths (truncation, wrong version, reserved 0x00 event_type, invalid MIDI length on encode + decode, buffer too small, seq overflow, invalid fragstate, empty SysEx body on encode + decode), spec size table sanity (`HEADER_LEN`, NoteOn = 14 bytes in `none` mode).
  - Compiles clean for both embedded targets (`thumbv7m-none-eabi`, `thumbv7em-none-eabihf`).
- [x] `osrf-link` data plane (`core/link/src/lib.rs`):
  - `ReplayWindow` — 64-bit sliding-window bitmap keyed on the full 48-bit `seq` (boot_counter ⊕ session_seq).  Forward jumps shift the bitmap; backwards-within-window are accepted once and rejected on replay; too-old (distance ≥ 64) are rejected.
  - `LinkSender::{new, no_crypto, encode}` — owns `(boot_counter, session_seq, key_fp)`, calls `proto::encode`, advances `session_seq` per call, errors on overflow.
  - `LinkReceiver::{new, no_crypto, process}` — calls `proto::decode`, drops on `key_fp` mismatch, drops on replay-window rejection, accepts otherwise.  Returns `RxOutcome::{Accept(Packet), Drop(KeyFpMismatch | Replay)}`.
  - **16 host tests pass** covering: first-packet accept, replay rejection, strictly-increasing accept, out-of-order within window, too-old boundary (distance 64 vs 63), far-forward jump resets bitmap, short-forward keeps history, boot-counter jump treated as forward, sender seq increment + overflow, sender header layout, receiver accept/replay/key-mismatch/decode-error, receiver out-of-order accept-then-replay.
  - Compiles clean for both embedded targets.
- [x] `osrf-link` timer plane:
  - `WatchdogTimer::{new, kick, wait}` — receiver-side, default 200 ms.  Composed with the radio's `rx_continuous` future via `embassy_futures::select`; each accepted packet kicks the watchdog, expiry surfaces `LinkLost` to the app (which translates to all-notes-off on every channel for the MIDI consumer).
  - `HeartbeatTimer::{new, note_send, wait}` — transmitter-side, default 20 ms (10× safety margin against the 200 ms RX watchdog).  Composed with the inbound MIDI source via `select`; any send (MIDI event or heartbeat) defers the next heartbeat.
  - Timer types use `embassy_time::{Instant, Duration, Timer::at}` directly; deadline-based design means kicks before the future resolves are guaranteed to delay (no staleness window).
  - Behaviour validated end-to-end on hardware (Phase 4); host-side mock-time tests deferred since the deadline math is trivial and the await behavior is what actually matters for link liveness.
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
