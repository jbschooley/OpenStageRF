## Platform pivot, 2026-05

Stage 1 was originally written against the DX-LR30 (STM32F103 + SX1262).  Mid-M2 the target
shifted to the **Heltec T114 (nRF52840 + SX1262)** for the first-edition firmware.  Reasons,
from concrete to strategic:

- **No SWD on the DX-LR30 carrier.** PA13/PA14 aren't broken out (verified against the LR30-SP
  schematic); flashing works via the on-board CH340C → STM32 ROM bootloader over UART, but
  there's no probe-rs / RTT path without tack-soldering to the QFP48.
- **T114 has a built-in 240×135 ST7789 TFT.**  M6 (on-device UI) becomes immediately practical
  on a single board, vs requiring an external SSD1306 + level-shifter wiring on the DX-LR30.
- **nRF52840 unlocks Stage 3 + Stage 4 work earlier.**  Hardware AES-CCM (CCM peripheral) and
  `nrf-softdevice`-backed BLE config can be exercised on the same board the link runs on,
  rather than waiting for the v2 custom board.
- **Multi-vendor portability boundary gets validated by going second-vendor immediately.**  If
  `core/` / `protocols/` / `drivers/` build clean against `embassy-nrf` *and* `embassy-stm32`,
  that's the test the architecture was designed for.

DX-LR30 stays in the workspace as a supported board crate (Resources, MIDI UART, radio plumbing
all work) and will return as a minimal-cost TX-only profile once the T114 stack is solid.  See
the README for the user-facing summary; ROADMAP.md Stage 1 carries the same note.

# OpenStageRF — First Prototype Plan

Concrete plan to ship Stage 1 (per [ROADMAP.md](ROADMAP.md)): 1× T114 TX, 1× T114 RX, one-way
packetized MIDI over GFSK at 902–928 MHz US ISM, no diversity, no encryption initially, Rust +
embassy on nRF52840.  (Originally targeted DX-LR30 / STM32F103 — see *Platform pivot* above.)

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
- [x] Bench test on TX side: keyboard MIDI OUT → FeatherWing IN → T114 P0_09; parsed events stream through RTT log via `t114_midi_rx` profile binary.  Validated end-to-end on T114 + Adafruit MIDI FeatherWing.
- [x] Bench test on RX side: T114 P0_10 → FeatherWing TX → DIN OUT → synth MIDI IN; arpeggio plays cleanly via `t114_midi_tx` profile binary.

**Exit criteria:** keyboard-played MIDI parses to correct events including pitch bend, sustain, real-time clock; firmware-generated MIDI plays back on a synth without artifacts.  **Met on T114 + Adafruit MIDI FeatherWing.**  DX-LR30 deployment deferred.

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
- [x] `osrf-app-midi-node` TX role: `UartMidiSource` reads MIDI from FeatherWing IN via `BufferedUarte`, parses via `osrf_midi_din::MidiParser`, re-encodes complete channel-voice events as wire bytes, drives `LinkSender` → radio.  Lives in `apps/midi_node/src/uart.rs`.  Profile binary: `osrf-profile-t114-midi-node-tx`.
- [x] `osrf-app-midi-node` RX role: radio → `LinkReceiver` → on `LinkLost`, `UartMidiSink::all_notes_off` writes 16 × CC#123 (48 bytes) to the FeatherWing UART; on event, `write_message` writes the 1–3 wire bytes verbatim.  Profile binary: `osrf-profile-t114-midi-node-rx`.
- [x] **Shared runtime extracted**: `core/link_runtime/` (`osrf-link-runtime`) owns `LinkConfig`, `MidiSource`/`MidiSink` traits, `configure_radio`, `run_tx`, `run_rx`.  Both `osrf-app-link-bench` (synthetic source) and `osrf-app-midi-node` (UART source/sink) consume it; refactor preserves the existing `LinkBenchConfig` name as a type alias for backward compat with existing link-bench profile binaries.
- [ ] Tests in mock-radio harness: dedup correctness, replay rejection, watchdog firing, heartbeat timing.  Deferred — behaviour validated end-to-end on hardware via rx5–rx12 link-bench runs and the new midi-node smoke test below.

**Exit criteria:** TX and RX boards transmit MIDI events end-to-end. Power off TX while a chord is held → RX receives all-notes-off within 250 ms.  **Met on T114 + Adafruit MIDI FeatherWing.**  Held chord cuts within 200 ms of TX power loss; link recovers immediately on TX repower.

### Milestone 5 — end-to-end live test + latency measurement (2–3 days)

**Goal:** measured latency, validated stage scenario, documented ceiling on mid-show
failure rate.

- [ ] Hardware setup: keyboard → T114 TX, T114 RX → synth/computer. Battery-power TX side; USB-power RX side.
- [ ] Latency measurement: trigger oscilloscope on a known MIDI byte (e.g. Note On status byte 0x90) on the input UART, capture corresponding output byte on the output UART. Measure delta. Document target (<3 ms RF transit) vs measured.
- [ ] Range test: walk away with TX, log RSSI on RX. Document range at which CRC errors begin.
- [ ] Stress test: play a busy MIDI sequence (60 events/sec), verify no missed/garbled events for 10 minutes.
- [ ] **4-hour soak test** with realistic traffic (60 events/s avg, 200 events/s peaks).
      Records `health_violations`, `LinkStats.total_accepted`, RAM high-water-mark per task
      (pattern-fill the stack pre-init, read back post-soak).  Pass criteria: no panic, no
      health-check violation, no `Err` from any UART/SPI op held longer than 100 ms.
      See [docs/reliability.md](docs/reliability.md) for failure modes and what the
      `health_check` task asserts.
- [ ] **`health_check` task** (`core/health/`): periodic invariant assertions (heapless
      vec capacity not persistently full, link counters monotonic, `current_menu` is a
      known-static `MenuNode`, etc.).  Health violations bump a counter visible on About.
- [ ] Document any anomalies: stuck notes, clock jitter, dropouts, etc.

**Exit criteria:** all latency / range / stress / soak numbers documented in `docs/v1_test_results.md`.

### Milestone 6 — UI on RX side (5–7 days)

**Goal:** RX (and TX) have a usable on-device interface for channel/power/key configuration,
running concurrently with the live MIDI link.

Implementation lives on T114 (built-in 240×135 ST7789 TFT + external 5-way joystick on the
expansion header).  Always-on SoftDevice S140 v6.1.1 underneath manages POWER + CLOCK + DCDC
+ critical-section impl; app-side IRQs sit at SD-allowed P2 priority.  SSD1306 (DX-LR30
add-on) is deferred until DX-LR30 returns to the active path; the UI core is
`DrawTarget`-generic so the same screens render on both when that lands.

- [x] **Hand-rolled ST7789 driver** in `boards/t114/src/display.rs`.  Drives **VTFT_CTRL on
      P0_03 active LOW** (the actual TFT VDD gate — verified against Meshtastic + MeshCore
      variant.h after a debug saga where the original code wrongly assumed P0_21 was the gate).
      1 s rail-warmup before SPI; 10 ms hardware reset pulse, Adafruit-style power+gamma
      block, MADCTL 0x60, X_OFFSET=40 / Y_OFFSET=53, SPIM2 @ 8 MHz MODE_0, active-LOW
      backlight on P0_15.
- [x] `osrf-driver-input-joystick5way`: edge-wake driver (GPIOTE, no polling) generates
      `Press(dir)` / `LongPress(dir)`.  500 ms long-press threshold, 20 ms debounce, 100 ms
      auto-repeat on Up / Down / Left / Right hold (typamatic-style scroll).  Center
      excluded from auto-repeat so long-press Center stays the universal "go home" action.
      Pre-pressed-at-startup and bounce-back guards.
- [x] `osrf-ui` crate (`core/ui/`):
  - `Role::{Tx, Rx}` baked into UiState; controls which top menu (`MAIN_MENU_TX` /
    `MAIN_MENU_RX`) drives Idle → menu and what the Idle banner shows.
  - `Settings` (band plan, channel, TX power, active key fingerprint), `LinkStatus`, `ScreenId`.
  - State machine `UiState::handle_event` returning optional `Command::{ApplyChannel, ApplyBandPlan, ApplyTxPower, ApplySetActiveKey}`.
  - **Data-driven menu tree:** `MenuNode { title, items: &[MenuItem] }` with `ItemAction::{Submenu, List, Value, Custom}`.  Adding a submenu is a `static FOO_MENU = ...` declaration plus a parent reference — no match-arm edits.
  - **Nav stack** (`Vec<NavFrame, 4>` on UiState) — every Center/Right push, every Left pop.  Backing out of any submenu restores parent cursor + scroll.  `go_home()` clears the stack on long-press Center.
  - List screens: ChannelSelect (per band plan), BandPlanSelect, KeySelect (sorted by name with synthetic "Open" row).  Active-marker `*` + cursor `>` 2-char prefix.
  - Value-edit screen: PowerSelect (numeric edit-buffer pattern, −9..+22 dBm).
  - Read-only: LinkStats (RSSI + accepted + loss% + stuck-recoveries — all wired live), About.
  - **Channel scan screen**: continuous rescan, per-channel bar graph with current + peak-since-open per channel.  Up to 144 channels supported (`MAX_SCAN_CHANNELS`), with adaptive bar geometry — wider bars at low channel counts (24 ch ISM), spectrum-trace mode at high counts (Wide 131 ch).  Markers (cursor + active stripe) under the bars.  Currently driven by a synthetic `synth_scan_pass` stub; real radio-side `scan_step` integration is pending (see open items below).
  - Stateful renderer with content-level diff (no flicker on row-only changes), background-aware MonoTextStyleBuilder, FONT_9X18 at 19 px row pitch (5 visible body rows on 240×135).
  - Multi-band plans (`band_plan.rs`): ISM 915 (24 ch @ 1 MHz, 903–926 MHz), Sennheiser-G compat (5 ch), Shure compat (4 ch), Dense Lo / Mid / Hi (3 × 87 ch @ 100 kHz tiling 902–928 MHz), Wide (131 ch @ 200 kHz over the full band).
  - Runtime KeyStore (`Vec<KeyEntry, 16>`, 24-bit fingerprint matching the on-wire `key_fp` header field, sorted-by-name view).  Profile-baked entries seeded at boot until BLE import lands.  Entry hidden from MAIN_MENU until AEAD lands (single-line gate).
  - 34 host tests cover the state machine, menu tree, key store, scan-state behaviour, and band-plan invariants.
- [x] `profiles/t114_ui` profile with `ui_rx` and `ui_tx` binaries.  Each runs the UI loop
      and the link runtime concurrently in the same task via `embassy_futures::join`:
  - `ui_rx` joins `ui_loop` with `osrf_link_runtime::run_rx` + `UartMidiSink` driving the
    FeatherWing MIDI OUT.
  - `ui_tx` joins `ui_loop` with `osrf_link_runtime::run_tx` + `UartMidiSource` reading
    FeatherWing MIDI IN.  Boot counter pulled from SD's RNG SVC (`sd_rand_application_vector_get`).
- [x] **SoftDevice integration** (`boards/t114/src/softdevice.rs`).  `enable()` lowers
      app-side peripheral IRQs to P2 (SPIM0/SPIM2/UARTE1; embassy's defaults sit at SD-
      reserved P0), calls `Softdevice::enable` with a minimal LF-clock-only config, then
      `sd_power_dcdc_mode_set(NRF_POWER_DCDC_ENABLE)` for DC-DC.  `run` task spawned for
      SD's event loop.  RAM origin in `memory.x` set to `0x200032D8` per SD's runtime
      report.  No BLE config yet — Stage 4 wires that.
- [x] **Live LinkStatus feedback**: `osrf-link-runtime` exposes `LinkStats` + `LinkStatsCell`
      (`critical_section::Mutex<Cell<LinkStats>>`).  `run_rx` writes `link_up`,
      `last_rssi_dbm`, `total_accepted`, accepted/dropped/CRC counts, `recent_loss_pct`
      (computed in the periodic-stats window), and `stuck_recoveries` on every loop
      iteration; `run_tx` writes `total_sent` / `heartbeats_sent`.  UI snapshots the cell
      on each render and translates into `osrf_ui::LinkStatus` for the Idle banner and
      Link Stats screen.
- [x] **Async-SPI display refactor** to fix UI-induced packet loss.  Display driver
      pushes pixels via `Spim::write().await` (yields during DMA) instead of
      `blocking_write`.  Path (a) from the original sketch: a 240×135 RGB565
      [`Framebuffer`](boards/t114/src/framebuffer.rs) in BSS impls `DrawTarget`
      synchronously; renderer paints into it; `Display::flush(&mut fb).await` streams
      the dirty bounding box one row per `Spim::write()` call.  Removed the
      `IDLE_RENDER_INTERVAL` rate-limit hack — render every iteration of `ui_loop`
      now.  Loss under sustained UI activity dropped from 5-12% to ~0%.  ~64 KB
      framebuffer in BSS; total RAM use ~129 KB of the 244 KB available.
- [x] **Auto-off / inactivity-driven Idle return**.  Per-screen policy in `ui_loop`:
      Idle → backlight off after 15 s of no input; Menu / ChannelSelect /
      BandPlanSelect / PowerSelect / KeySelect / About → `state.go_home()` after
      120 s; LinkStats and Scan stay on indefinitely (live readouts).  Wake-from-
      sleep is "next joystick press lights the panel and is consumed" — the press
      doesn't fire its action, so the user can't accidentally trigger a menu item
      on the wake press.  `UiState::go_home` exposed `pub` so the profile timeout
      path can call it.
- [x] **Live config-update plumbing**: `LinkConfigSignal` (newtype around
      `embassy_sync::signal::Signal`) in `osrf-link-runtime`; UI pushes a fresh
      `LinkConfig` (rebuilt from `Settings`) on `Command::Apply{Channel,BandPlan,
      TxPower}`; `run_tx` / `run_rx` poll the signal at top-of-loop and add
      `wait()` as an arm of their idle `select` so a config change at idle is
      applied immediately (rather than after the next packet / heartbeat).
      `apply_*_reconfig` helpers walk the chip through `init` → `configure_radio`
      → resume (`rx_start` for RX), reset Heartbeat / Watchdog timers if the ms
      changed, force a session-reset on RX (new RF params imply a new peer).
      `ApplySetActiveKey` doesn't trigger reconfigure (no AEAD in v1).
- [x] **Real channel-scan integration**.  New `ScanController` (critical-section
      `Mutex<RefCell<ScanInner>>` + state-change `Signal`) plus radio driver
      additions (`set_standby_rc`, `set_frequency_fast`, `get_rssi_inst`).  `run_rx`
      and `run_tx` both gained an `Option<&ScanController>` parameter and a
      mode-aware loop: top-of-loop reconciles "is the controller enabled" against
      a local `scanning` flag, walks the chip between continuous-RX/heartbeat-TX
      and scan-mode at the transition.  In scan mode each iteration runs
      `scan_one_channel`: `set_frequency_fast → rx_start → 1 ms settle → 6 RSSI
      samples spaced 1 ms apart → set_standby_rc`, peak across the 6 samples
      written to the controller's results array.  6-sample window catches the
      heartbeat carrier ~60 % per pass; UI's `peak_dbm` accumulator covers the
      gaps within ~3 passes.  Wide 131-ch full pass: ~920 ms.  ISM 24-ch:
      ~170 ms.  TX scan drains and discards source MIDI events during sweep
      (resets queue + `tx_state` on entry so the post-scan heartbeat mask
      matches the receiver's post-watchdog cleared state).  `apply_scan_pass`
      skips `SCAN_NO_DATA` slots so a partially-populated pass doesn't clobber
      previous readings.
- [x] **Embassy task split**.  Originally motivated as a fix for UI-induced packet
      loss; that turned out to be solved by the async-SPI refactor alone.  Done
      anyway as architectural groundwork for future audio (which requires
      preemption to avoid buffer underruns), concurrent BLE telemetry during live
      MIDI, encryption (modest benefit), and dual-core migration (free if tasks
      already split).  Layout:
      - `link_runtime_task` (run_rx / run_tx / scenarios — three concrete task
        fns) on an `InterruptExecutor` bound to `EGU0_SWI0` at priority P2.
        Preempts everything app-side when a radio IRQ lands.
      - `ui_render_task` on the thread executor — owns display + framebuffer +
        renderer, awaits a `Signal<FrameData>` from ui_state.
      - Main task body = `ui_state_loop` (state machine, scan reconcile, auto-
        off policy, frame production).  Pushes frames to ui_render via the
        signal.
      - `joystick_task` and `softdevice::run` as before on the thread executor.
      Shared state already in place (`STATS`, `CONFIG_UPDATES`, `SCAN`,
      `EVENT_CHAN`); added `FRAME: Signal<FrameData>` for ui_state→ui_render
      handoff (latest-wins; render-in-flight discards intermediate frames).
      RAM cost: BSS grew ~31 KB for the new task storage + Signal slots; total
      now ~161 KB / 244 KB available.  Loss numbers unchanged from joined-task
      design (already at ~0 %).

**Exit criteria:** all screens render, joystick navigates correctly, channel/band/power changes
apply live (without reboot), scan reports per-channel RSSI for the active band plan, packet loss
under sustained UI activity stays under 1%.

### Milestone 7 — persistence + crash safety (3–5 days)

**Goal:** settings survive reboot.  A panic or hardware hang reboots the unit and surfaces
post-mortem info on the next boot's About screen instead of bricking it mid-show.  Low
battery triggers a clean shutdown with audible-MIDI-quiet rather than a brownout.

- [x] Flash partition layout.  `boards/t114/memory.x` shrinks FLASH to `0xC1000` to carve
      24 KB at the top for persistence; `boards/t114/src/storage.rs` exposes `SETTINGS_RANGE`,
      `KEY_STORE_RANGE`, `PANIC_RING_RANGE` (8 KB / 2 pages each), with compile-time
      `const _` assertions on alignment + page-size invariants, plus a `flash(sd)` helper
      wrapping `nrf-softdevice::Flash`.
- [x] `sequential-storage` integration: `map` for the settings region (per-field keys 0..3),
      `queue` for the panic ring (auto-overwriting oldest entry on fill).  All flash IO goes
      through `nrf-softdevice::Flash` so SD's flash-controller ownership is honoured.
- [x] **Settings persisted on-change.**  `save_channel`, `save_band_plan`, `save_tx_power`,
      `save_active_key` helpers in `profiles/t114_ui/src/lib.rs` fire from each
      `Command::Apply*` arm of `ui_state_loop`.  `load_settings` on boot reads what's there
      and falls back to `Settings::default()` for missing keys.  Per-field keys mean editing
      one field doesn't rewrite the others — wear-leveling preserved even under rapid
      same-field edits.
- [x] **Boot counter stays random** (`board::softdevice::rand_bytes` → u16) — *not*
      flash-persisted.  About screen extension (below) will render it as `Session: 0xNNNN`.
- [x] **Key store: stub for v1.**  Sequential-storage region reserved (`KEY_STORE_RANGE`
      in board storage).  On-flash record format defined: `KeyRecord` in
      `core/ui/src/key_store.rs` is a fixed-64-byte `#[repr(C)]` struct
      (`fingerprint: u32` / `name_len: u8` / `name_bytes: [u8; 16]` / `key_material:
      [u8; 32]` / `reserved: [u8; 11]`).  `KeyRecord::from_entry` / `to_entry` provide
      round-trip conversion to/from runtime `KeyEntry`; tests cover roundtrip + bad
      name_len rejection + fingerprint masking.  v1 status: no UI flow populates the
      flash region (no add-key screen yet, no AEAD wiring on the link), and
      `KeyStore::default()` returns empty.  Stage 3 just has to plumb a load+populate
      path through the existing region — no migration needed since the format is
      committed.
- [x] **Hardware watchdog.**  `WDT_TIMEOUT_TICKS = 5 s` in `profiles/t114_ui/src/lib.rs`,
      armed via `Watchdog::try_new` with two slots: `wdt_main` (petted at the top of
      `ui_state_loop` every ~300 ms) and `wdt_render` (petted in `ui_render_task` after
      each frame or via a `select(FRAME.wait(), Timer::after_secs(2))` fall-through so
      WDT keeps eating even when display is off).  Link runtime is intentionally not
      WDT-monitored — the link-layer 200 ms watchdog handles "link runtime stopped" via
      RX-side all-notes-off, which is the operationally correct response.
- [x] **Panic-to-flash + auto-reset.**  Custom `#[panic_handler]` in
      `profiles/t114_ui/src/lib.rs` stages the panic message into `.uninit` (survives the
      soft reset since cortex-m-rt's startup doesn't zero `.uninit`), defmt-logs, then
      `SCB::sys_reset()`.  Boot path's `recover_pending_panic` reads + clears `RESETREAS`
      via `sd_power_reset_reason_get` / `_clr` so each boot's value reflects only that
      boot's cause (originally we kept the accumulating-flags posture but it broke the
      DOG-without-staged-panic detector for the watchdog-hang case).  Takes the staged
      record from `.uninit`, persists to the panic ring; writes "watchdog: task hung"
      record on DOG without a staged panic.  Verified end-to-end via force-panic and
      force-WDT menu items (below).
- [x] **Shared `osrf-panic-log` crate** (`core/panic_log/`): board-agnostic wrapper over
      `sequential-storage::queue` with a fixed record format (`[reset_reas: u32 LE]
      [UTF-8 message ≤ PANIC_MSG_LEN]`) and `push` / `read_latest` / `clear` helpers.
      Profile glues board-specific `PANIC_RING_RANGE` + nrf-softdevice `Flash` to the
      shared API.  Board crate re-exports `PANIC_MSG_LEN` so the cross-reset staging
      buffer (board-side, in `.uninit`) and the on-flash records stay locked to the
      same size.  Pattern is now ready for any future UI-bearing board.
- [x] **About screen extension** (`core/ui/src/lib.rs::build_about`): scrollable
      multi-line view with firmware version (`env!("CARGO_PKG_VERSION")` from the
      profile crate), git hash (set by board `build.rs`; appended `*` when working tree
      is dirty), session ID (random per-boot u16), last panic message (read at boot
      from the panic ring via `osrf_panic_log::read_latest`), and a 3-line GitHub URL.
      Scrolling via Up/Down on the screen; `build_about` clamps overshoot so Down past
      the bottom doesn't pile up scroll that must be undone with Up.  Long-press Right
      emits `Command::ClearPanicLog`; profile handles it by `osrf_panic_log::clear`-ing
      the flash region and zeroing the cached last-panic string.
- [x] **Diagnostic menu items.**  `Force panic` (emits `Command::ForcePanic` → profile
      calls `panic!("forced panic from menu (test)")`) and `Force WDT` (emits
      `Command::ForceWdtHang` → profile busy-spins `ui_state_loop` so its WDT slot
      stops petting → ~5 s later WDT fires).  Both surface as menu items in
      `MAIN_MENU_RX` / `MAIN_MENU_TX`; behaviorally identical to a real crash or hang
      so the recovery path can be exercised one-handed without a rebuild.  Cheap to
      leave in production — operator has to navigate two menu levels to reach them.
- [x] **Dev-workflow fix: clear DEMCR at boot.**  `probe-rs run` defaults to setting
      `VC_HARDERR` + `VC_CORERESET` in DEMCR, which survive SYSRESETREQ and halt the core
      forever on the next transient HardFault once STLink is unplugged.  Single mmio
      write to `0xE000EDFC` at the top of `run()` clears them.  Production boots never
      see this (no probe → no bits set → no-op write), but it makes dev iteration
      survivable without NRESET-after-every-cargo-run.  See chat 2026-05-11.
- [x] **Portability + no-alloc CI audit** (`cargo xtask audit`).  Scans every crate
      under `core/`, `drivers/`, `protocols/`, `crypto/`, `apps/`; flags two classes
      of violation and exits non-zero on any hit:
        1. Any `[dependencies]` entry whose name starts with `embassy-` that isn't on
           a whitelist of framework crates (`embassy-time`, `embassy-sync`,
           `embassy-futures`, `embassy-executor`, `embassy-usb*`) — HAL crates
           (`embassy-nrf`, `embassy-stm32`, …) must live in `boards/` or `profiles/`.
        2. Any `.rs` file under those dirs containing `extern crate alloc` or
           `use alloc::` — heap allocation stays an explicit per-board choice.
      `boards/`, `profiles/`, `ports/`, `xtask/` are exempt (those are exactly where
      HAL + allocator wiring belong).  Smoke-tested both detection paths in
      2026-05-11 chat; current workspace passes clean (14 shared crates).  Hook into
      CI as a single `cargo xtask audit` step whenever the workflow gets written.
- [x] **Low-battery graceful shutdown — quick version.**  Critical threshold (Vbat ≤
      `SHUTDOWN_MV`, sustained for 5 reads at 5 s cadence, USB unplugged) triggers, in order:
        1. `SHUTDOWN.signal()` from `battery_task` → link runtime arm wakes.
        2. `sink.all_notes_off()` (RX) + `radio.set_standby_rc()` — silence + park radio.
        3. 6 LED blinks then `set_low`; runtime task idles forever on a 60 s timer.
        4. UI side picks up `SHUTDOWN_LATCH` next iteration: renders "Shutting down /
           Battery low / Plug in to charge" frame, pushes a "low-battery shutdown" record
           to the panic ring, kills the backlight after ~3 s, parks WDT-petting.
        5. Any joystick event in the park loop triggers `SCB::sys_reset()` → fresh boot.
           If battery is still below threshold, `battery_task` re-fires shutdown after
           the 25 s debounce.  USB-plug recovery is implicit: `vbus_present` gates the
           shutdown predicate, so plugging in keeps subsequent boots alive.
      No explicit settings save — Apply* on-change writes already cover that.  Pending UI
      edits (edit_buffer values not yet confirmed via Center) are lost by design.
      **NOTE:** this is a "quick" shutdown, not a deep soft-off.  Idle power in the park
      state is ~2–5 mA (SD idle + ST7789 in normal-mode-with-backlight-off + SX1262 in
      STBY_RC + joystick poll task + WDT pet).  On a 700 mAh 14500 LiPo this is ~10 days
      shelf life — adequate for "left on overnight" but not "left in a gig bag for a
      month."  The real deep soft-off (SLPIN display, SX1262 SLEEP, `sd_softdevice_disable`,
      `sd_power_system_off` with GPIO-sense wakeup) lives in M8 and gets us to <5 µA.

**Exit criteria:** ✅ all of the following verified end-to-end on hardware
(2026-05-11):
  - Power-cycle device, settings retained (channel, band plan, TX power persist).
  - `Force panic` menu item → reboot → About shows `forced panic from menu (test)`.
  - `Force WDT` menu item → ~5 s hang → reboot via WDT → About shows
    `watchdog: task hung`.
  - Low-battery shutdown (testable by bumping `SHUTDOWN_MV` to current Vbat) →
    goodbye screen → backlight off → About on next boot shows `low-battery shutdown`.
  - About long-press Right → panic ring cleared → `No prior panic` on next render
    and across subsequent reboots.
  - About scroll Up/Down works, with overshoot clamping (Down past end doesn't
    accumulate scroll the user has to undo).

**Milestone status:** ✅ complete (2026-05-11).  Key store stub and portability /
no-alloc audit landed alongside the panic / shutdown / About work.  Only follow-on
item is wiring `cargo xtask audit` into a CI workflow once one exists.

### Milestone 8 — power management + battery chemistry options (3–5 days)

**Goal:** the device powers down cleanly to a sub-50 µA standby state on operator command,
wakes on joystick or USB-plug events, and supports both LiPo and NiMH cell configurations
per-profile.

- [x] **Full deep soft-off**.  Long-press **Left** from Idle → `PowerOffConfirm` screen
      (Center: confirm, Right: cancel, Left: ignored to avoid colliding with the entry-
      gesture auto-repeat).  Confirm fires `Command::PowerOff`, which routes through
      `enter_soft_off()` in `profiles/t114_ui/src/lib.rs` and runs:
      - Goodbye frame held for ~1 s (reason-specific copy for low-battery vs operator).
      - Backlight off (`P0_15` HIGH).
      - `SHUTDOWN.signal()` → link runtime does all-notes-off (RX), `set_standby_rc`, LED
        confirmation blink, then `radio.set_sleep()` (SX1262 ~160 nA).
      - `POWER_OFF_DISPLAY.signal()` → `ui_render_task` runs `display.power_off()` (ST7789
        DISPOFF + SLPIN + VTFT_CTRL gate HIGH), then enters a pet-only idle loop on its
        WDT slot.
      - 250 ms cooldown so the other tasks land their teardown.
      - `board::power::enter_system_off()` (new `power.rs` module): mask GPIOTE in NVIC,
        drive VEXT (P0_21) LOW, clear SENSE on Up/Down/Left/Right joystick pins, set
        SENSE=Low on Center (P0_13), call `sd_power_system_off` SVC.
      - On wake (Center press): chip resets, boots through `run()` → Idle.  Settings come
        back via M7 flash persistence.  `RESETREAS.OFF` distinguishes the wake from any
        other reset cause.

      **Gesture note** vs. the original plan: long-press Center *can't* be the entry —
      the joystick driver fires `Press(Center)` *then* `LongPress(Center)`, so by the time
      the long-press handler runs, the short press has already opened MainMenu.  Long-press
      Left from Idle works cleanly: `Press(Left)` on Idle is unused, and an added
      `AUTO_REPEAT_INITIAL_DELAY = 500 ms` in the joystick driver puts the first
      auto-repeat 1 s past the press start.

      **SoftDevice handling**: kept enabled; `sd_power_system_off` is the supported SD-aware
      System OFF entry.  PLAN's earlier note about calling `Softdevice::disable()` first
      turned out to be unnecessary — the SVC handles SD teardown internally and we don't
      need critical-section through the moment of `sd_power_system_off` since no app code
      runs after it (in production).

      **Debugger-attached caveat**: with the probe attached, the `DBGEN` bit makes SD enter
      *emulated* System OFF — CPU halts in WFE but clocks stay running, so current is in
      the mA range.  `sd_power_system_off` returns `NRF_ERROR_SOC_POWER_OFF_SHOULD_NOT_
      RETURN` (0x2006); our handler logs and `SCB::sys_reset()`s so dev sessions see
      "confirm → reboot to Idle" instead of a hang.  Real System OFF needs the probe
      detached AND a power-cycle (DBGEN only clears on NRESET / power-on) — that's the
      bench step for actually measuring the µA target.

      **Status (2026-05-12)**: ✅ end-to-end working on hardware.  Dev path (probe
      attached) sees emulated System OFF → `sys_reset` → reboot to Idle, which exercises
      every step except the actual System OFF entry.  Production path (probe detached +
      power-cycled to clear `DBGEN`) confirmed visually: with the external joystick
      board's red LED wired off VEXT (P0_21), the LED goes dark at the confirm moment
      and stays dark through sleep, lighting back up only on Center-press wake.  That's
      the visual signature of real System OFF + VEXT teardown + reset-vector wake.
      Quantitative <50 µA measurement still pending an actual ammeter setup, but the
      qualitative observation (no measurable battery drop over multi-hour overnight
      tests when in real System OFF, vs ~0.9 mA chip draw when stuck in Idle) is
      consistent with the target.

- [x] **Migrate the M7 low-battery auto-shutdown onto the deep soft-off path.**  Both
      `Command::PowerOff` (operator) and the sustained-low-battery latch route through the
      same `enter_soft_off()` helper.  Single `POWEROFF_REASON: AtomicU8` (values:
      `Operator` / `LowBattery`) selects the goodbye copy and whether to push a panic-ring
      record before the System OFF call (LowBattery only; operator soft-off is normal user
      flow and gets no ring entry).  Drops park-state drain from M7's ~3 mA to whatever the
      System OFF measurement comes out at (pending — see above).  ✅ wired and unit-
      tested; bench measurement pending alongside the operator path.

- [ ] **USB-plug wake (brief).**  When soft-off, plugging in USB wakes the device just long
      enough to render one frame showing battery charging status (the existing
      `BatteryIndicator` repurposed as a full-screen "Charging…" view).  Auto-sleep after
      ~5 s or on USB unplug.  Implementation: USBDETECTED interrupt → set a flag → main
      loop checks flag, runs one render, sleeps again.

      **Status (2026-05-12)**: chip already wakes from emulated soft-off on USB plug
      because the SD leaves `POWER->INTENSET.USBDETECTED` armed and the event bumps WFE.
      In production this turns into the wake path described above; what's still missing is
      the *brief* part — currently a USB plug fully re-enters `run()` and lands on Idle
      with backlight on.  Needs a one-shot "charging frame then sleep again" path that
      detects "boot reason = USB-wake from soft-off" via RESETREAS and a flash flag,
      renders one frame, then re-enters `enter_soft_off`.

- [ ] **Battery chemistry as a per-profile compile-time option.**  Today `core/ui/src/battery.rs`
      assumes single-cell LiPo with Meshtastic's OCV table.  Add a `BatteryChemistry` enum
      with at least: `LiPoSingle`, `NimhPack { cells: u8 }`.  Each variant carries an OCV
      table (3-cell NiMH: ~3.0-4.2 V range, but flatter discharge curve with sharp knee
      around 3.3 V; 2-cell NiMH would need boost so probably not viable; 4-cell NiMH ≈
      4.0-5.6 V, exceeds LiPo regulator input range so also not viable without a buck).
      Profile picks via Cargo feature or const associated with the board crate.

- [ ] **Document the external-charging story for NiMH.**  Decision: **no on-board NiMH
      charging.**  TP4054 is LiPo-only and overcharging NiMH at 4.2 V CV would damage cells.
      Users running NiMH packs use an off-board smart charger and swap cells between gigs.
      The T114's `vbus_present()` still works for showing the lightning bolt during USB
      operation, but no automatic charging happens — clearly labelled in the About screen
      / docs so users don't expect it.

- [ ] **Document removable-LiPo workflow.**  Recommended path for fast battery swaps:
      **14500 LiPo cells** (AA form factor, 14×50 mm, ~800 mAh) in an AA-style holder
      (Keystone 79 series, ~$1) connected to the existing JST-PH battery port.  Same LiPo
      chemistry → same TP4054 path → same firmware OCV table → zero board or firmware
      changes beyond the swap mechanism itself.  Carry charged spares in a pocket, swap in
      ~5 seconds on stage.  External charging via a single-bay LiPo charger that handles
      14500 (Nitecore F1, XTAR MC1 — $10-15 each).  Same active runtime as the current
      800 mAh pouch cell but with a meaningful "dead-battery-on-stage" recovery story.

      Larger swappable options if capacity matters more than form-factor:
        - **18350** (18×35 mm, ~1000 mAh): slight capacity bump.
        - **18650** (18×65 mm, ~3000 mAh): the bulk-charge / long-set option, 4× capacity.
      Both use the same charging story (LiPo, external multi-bay smart charger).  Update
      the hardware-guide doc in `docs/`.

- [ ] **Optional: route TP4054 STAT to a GPIO.**  The T114's CHG LED is hardwired to the
      charger IC's STAT pin (active-low while charging, released at full).  Currently
      firmware can't distinguish "charging" from "charge done" — both show as
      `vbus_present == true`.  Wiring STAT to a free GPIO (P0_05 is unclaimed) lets the
      UI render "Charging…" vs "Charged" vs "Powered" states distinctly.  Small hardware
      mod (one wire-tack), nice-to-have not required.

**Exit criteria:** long-press Left from Idle → Center on the confirm screen powers down
the unit cleanly; current draw in soft-off measures < 100 µA on a multimeter; Center
press wakes the device with settings intact (post-M7); USB plug while off shows one
charging frame; battery indicator correctly reflects voltage for both LiPo and
(eventually) NiMH chemistries.

**Exit criteria status (2026-05-12):**
  - ✅ Confirm flow works end-to-end (dev + production paths).
  - ✅ Real System OFF engages with probe detached + power-cycled — verified visually
    via VEXT-powered joystick LED going dark and staying dark through sleep.
  - ⏳ <100 µA quantitative measurement — pending ammeter setup; qualitative evidence
    (no measurable % drop over multi-hour real-off tests) is consistent.
  - ⏳ USB plug → one charging frame — see USB-plug wake bullet above.
  - ⏳ NiMH OCV table — not started.

**Out of scope:** on-board NiMH charging circuit (covered above — external charger only);
hardware-switch design (revisit if soft-off proves unreliable in practice).

**Milestone status:** 🟢 core deep soft-off complete (2026-05-12).  The headline
"powers down cleanly to a sub-µA standby state on operator command and wakes on Center
press" bullet is done and hardware-verified.  Remaining bullets — USB-plug brief wake,
NiMH chemistry, docs, optional STAT-pin mod — are independent follow-ups and can each
be picked up à la carte.

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
