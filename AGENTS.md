# AGENTS.md — orientation for coding agents

A focused brief for LLM coding agents working on this repo.  Read [README.md](README.md) for the project pitch and [PLAN.md](PLAN.md) for the milestone roadmap.  This file is the operational manual: how to build, how to flash, how to read the diagnostic output, and what's load-bearing that isn't obvious from the code.

## What this project is, in one paragraph

OpenStageRF is firmware for low-latency wireless MIDI over sub-GHz radio, written in Rust on `embassy` (async, no_std).  The first-edition target hardware is the **Heltec T114 v2.1** — nRF52840 MCU, SX1262 radio (in the LR1262 module), and a built-in 1.14″ 240×135 ST7789 TFT.  US 902-928 MHz ISM band, GFSK modulation at 300 kbps tuned for latency rather than range.  The link layer (`core/link`) does sequence numbers, dedup, and prioritised retransmits.  The link runtime (`core/link_runtime`) wires the link layer to the radio driver and MIDI source/sink.  Above that is a UI core (`core/ui`) that's `DrawTarget`-generic so it can run on the T114's TFT today and an OLED on the DX-LR30 later.  A SoftDevice (S140 v6.1.1) sits underneath providing critical-section + DC-DC + RNG; BLE is reserved for a future milestone.

## Workspace layout

The repo is a Cargo workspace.  The shape:

```
apps/         — high-level firmware "roles" (link_bench, midi_node)
profiles/     — concrete build configurations: board × role × features
boards/       — hardware pin maps and HAL setup (t114, dx_lr30)
core/         — board-agnostic logic (link, link_runtime, ui)
drivers/      — peripheral drivers (radio/sx126x, midi, input)
protocols/    — frozen on-air packet formats
docs/         — design docs and reliability notes
tools/        — host-side utilities
```

The mental model: a *profile* binary in `profiles/<name>/` (e.g. `t114_link_rx`) brings up the board (via the `boards/` crate), constructs a MIDI source/sink (from `apps/link_bench` or `apps/midi_node`), and runs the link runtime from `core/link_runtime`.  Everything else is supporting infrastructure.

## The target-triple gotcha (read this before doing anything)

The workspace's `.cargo/config.toml` sets a default target of `thumbv7m-none-eabi` (Cortex-M3, for the DX-LR30 / STM32F103).  **The T114 needs `thumbv7em-none-eabihf` (Cortex-M4F with hard-float).**  If you just `cargo build -p osrf-profile-t114-ui`, it'll happily compile against the wrong target, produce a wrong-arch binary, and you'll waste time wondering why memory layout numbers look strange.

**Always pass `--target thumbv7em-none-eabihf` for T114 builds.**  Example:

```
cargo build --release --target thumbv7em-none-eabihf -p osrf-profile-t114-ui --bin ui_rx
```

If you forget, you'll typically notice because BSS doesn't include statics you know you allocated (like the 64 KB framebuffer).  `arm-none-eabi-size` on the binary will reveal a smaller-than-expected total.

## Building, flashing, running

### `cargo build` — just compile

```
cargo build --release --target thumbv7em-none-eabihf -p <crate> --bin <binary>
```

Produces an ELF at `target/thumbv7em-none-eabihf/release/<binary>`.

### `cargo run` — compile, flash, stream defmt-rtt

The cargo runner for the embedded targets is configured to `probe-rs run --chip nRF52840_xxAA --rtt-scan-memory` (see `.cargo/config.toml`).  So:

```
cargo run --release --target thumbv7em-none-eabihf -p osrf-profile-t114-ui --bin ui_rx
```

…flashes the connected board via SWD probe and streams defmt-rtt to the terminal.  Ctrl-C to stop and detach.

### To capture a log

```
cargo run --release --target thumbv7em-none-eabihf -p osrf-profile-t114-ui --bin ui_rx 2>&1 | tee link_rxNN.log
```

`2>&1` folds stderr (build messages, probe diagnostics) into stdout; `tee` writes a copy to the file.  The convention in this repo is `link_rxNN.log` numbered sequentially for diagnostic captures.

### Flashing without a probe (UF2 + Adafruit bootloader)

The T114 ships with an Adafruit-fork bootloader (oltaco/Adafruit_nRF52_Bootloader_OTAFIX) that boots into UF2 mass-storage mode on a double-tap of the reset button.  To flash via USB:

```
python3 tools/uf2conv.py target/thumbv7em-none-eabihf/release/<binary> -c -b 0x26000 -f 0xADA52840 -o firmware.uf2
# then drag-drop firmware.uf2 onto the board's UF2 drive
```

The `0x26000` start address is the post-SoftDevice app location.  See `boards/t114/memory.x` for the layout.

## Profiles and what each one is for

The repo has many profiles.  The relevant T114 ones, grouped by purpose:

**Bring-up / smoke tests:**
- `t114_blink` — LED toggle, sanity-check the toolchain works.

**Radio bring-up (no link layer):**
- `t114_radio_tx`, `t114_radio_rx` — bare-metal SX1262 driver test, no MIDI.
- `t114_tx_basic`, `t114_rx_basic` — adds packet framing.

**Link layer with synthetic MIDI (the bench):**
- `t114_link_tx` — TX with `ScenarioSource` cycling scale / chord / glissando / pitch wheel / etc.
- `t114_link_rx` — RX with `DefmtLogSink` that logs each MIDI event over RTT.
- These default to 915 MHz, no UI, fastest path to test the link-layer end-to-end.

**Real MIDI over DIN (FeatherWing):**
- `t114_midi_node_tx` — reads DIN MIDI IN, transmits.
- `t114_midi_node_rx` — receives, drives DIN MIDI OUT to a synth.

**UI builds (M6 deliverables):**
- `t114_ui` profile, three binaries:
  - `ui_rx` — UI + link RX + UART MIDI sink (production RX path).
  - `ui_tx` — UI + link TX + UART MIDI source (production TX path).
  - `ui_bench_tx` — UI + link TX + **synthetic** `ScenarioSource` from `link_bench`.  Use this when you want to stress-test the link with the UI active.

**Other:**
- `t114_ui_demo` — older display smoke test, predates the full UI.
- `t114_midi_tx`, `t114_midi_rx` — pre-link-layer MIDI tests, mostly legacy.

DX-LR30 profiles exist (`dx_lr30_*`) but are mostly inactive — see README.md for why.

## The current default test recipe

For end-to-end testing of "is the link working":

1. **TX side** — synthetic burst-pattern source (no real MIDI gear required):
   ```
   cargo run --release --target thumbv7em-none-eabihf -p osrf-profile-t114-ui --bin ui_bench_tx
   ```
   Wait for `link TX:` and `ui_bench_tx: synthetic scenario source running` in the log, then disconnect the probe.  The board keeps running on battery.

2. **RX side** — capture log:
   ```
   cargo run --release --target thumbv7em-none-eabihf -p osrf-profile-t114-ui --bin ui_rx 2>&1 | tee link_rxNN.log
   ```

3. Both boards default to channel 1 (903 MHz).  If you want a cleaner channel, navigate UI → Channel on each and pick the same one (916 MHz has been verified as a good mid-band choice in residential settings; 903 MHz suffers from smart-meter interference).

## Reading the diagnostic output

The RX runtime emits two lines per second when a link is active:

```
RX last1s: pkts=A/B loss=X.Y% midi_ev=N hb=M drop=D crc_err=C | total: pkts=... midi_ev=... ...
RX prof: gap_ms <2=N0 <12=N1 <25=N2 <50=N3 <100=N4 <250=N5 >=250=N6 | err crc=X crc-early=Y unex-irq=Z spi=A bus=B other=C
```

**The `RX last1s` line** counts the rolling 1 s window plus running totals.  `pkts=A/B` is "accepted / expected" (expected derived from TX packet_seq).  `loss=X.Y%` is the fraction missed.

**The `RX prof` line** is the gap histogram and error counts:

- `<2 ms` — burst-clustered packets (back-to-back).
- `<12 ms` — heartbeat-cadence and normal-burst spacing.  Most counts live here in healthy operation.
- `<25 ms` — one heartbeat missed (1-2 packets lost in a row).
- `<50 ms` — two heartbeats missed (multi-packet loss event).
- `<100 ms`, `<250 ms`, `>=250 ms` — progressively larger gaps; indicate sustained interference, shadowing, or link loss.
- `err crc` — packets that arrived complete but failed CRC.
- `err crc-early` — chip reported a CRC error before a complete frame assembled.
- `err unex-irq` — chip returned an unexpected IRQ pattern.  **Nonzero means scheduling/state-management problem, not RF.**
- `err spi`, `err bus`, `err other` — radio driver errors.  Should always be zero.

The histogram resets each 1 s window.  Error counters accumulate.

A healthy run looks like: nearly everything in `<12`, zero or low counts in `<25`/`<50`, zero in `<100`+ except during walks / link drops, and all `err *` columns at zero (except small `err crc` accumulating slowly from background RF noise).

## Host tests, hardware tests

- `core/ui` has ~30 host-side unit tests (`cargo test -p osrf-ui`).  These run on the dev machine, no hardware needed.
- `core/link` has host tests for the dedup, watchdog, and queue logic.
- Most other crates are `no_std` and don't have host tests; they're exercised through the profile binaries on real hardware.
- A `cargo test --workspace` from the repo root will pull in some host-incompatible crates and fail — this is a known limitation of the workspace shape and not something to fix idly.

When making changes, the build-everything check is:

```
for p in t114_blink t114_link_rx t114_link_tx t114_midi_node_rx t114_midi_node_tx t114_radio_rx t114_radio_tx t114_rx_basic t114_rx_diversity t114_tx_basic t114_ui t114_ui_demo; do
  cargo build --release --target thumbv7em-none-eabihf -p osrf-profile-${p//_/-} 2>&1 | tail -2 | head -1
done
```

(Cargo can compile each profile in parallel; the loop is sequential for clarity.)

## Memory layout and the SoftDevice

`boards/t114/memory.x`:

```
MBR  0x0000  – 0x1000
SD   0x1000  – 0x26000   (S140 v6.1.1)
App  0x26000 – 0xED000   (FLASH)
RAM  ORIGIN = 0x200032D8, LENGTH = 0x3CD28
```

The RAM origin is whatever SD reports it needs for the configured BLE profile (currently minimal: LF clock only, no advertising).  If you change SD configuration, the runtime will panic at `Softdevice::enable` time with a message naming the required ram_start.  Bump `memory.x` to that value.

App-side IRQs run at priority **P2** (SD reserves P0/P1/P4).  See `boards/t114/src/softdevice.rs` for the priority-lowering logic and why each peripheral interrupt has to be set explicitly.

## Common gotchas

- **Cargo features are crate-wide.** You can't have two binaries in the same crate built with different features in one `cargo build`.  If you want feature-divergent binaries (e.g., one with `bench-source`), use separate binaries that pull in different deps via `required-features`, and invoke them with explicit `--bin` per build.
- **`UartMidiSource::new(midi_uart)` consumes the UART.**  If you're adding a code path that doesn't use the UART, structure to avoid constructing `UartMidiSource` at all; don't try to "leak it."  The `TxSource` enum in `profiles/t114_ui/src/lib.rs` is the example of how to gate this.
- **`embassy-nrf` peripherals are move-only.**  Once you build a `Spim` from `p.SPI2`, that token is consumed.  Resource bundles in `boards/t114/src/lib.rs` capture all this at boot.
- **defmt's `{:?}` requires `Format` impls all the way down.**  Generic error types parameterized by `<Reset, Switch>` etc. break this — the `RadioError` printout in `run_rx` handles each variant explicitly rather than `{:?}`-ing the whole thing.

## Where to look for context on recent changes

- **PLAN.md** — milestone-level status.  Items marked `[x]` are done; `[ ]` are open.  M6 is the current frontier as of this writing.
- **Recent log files** (`link_rxNN.log`) — captured RTT output from real hardware runs.  Useful for grounding any claim about RX/TX behaviour in actual measurements rather than guesses.
- **Memory entries** (under `~/.claude/projects/.../memory/`, if you have auto-memory configured) — durable facts about user preferences, prior debugging incidents, and architectural rationale that aren't captured in code or commit messages.

## Things that look weird but are intentional

- **`static mut FRAMEBUFFER: Framebuffer = Framebuffer::new();`** in `profiles/t114_ui/src/lib.rs` — 64 KB BSS allocation accessed via `addr_of_mut!`.  This is the correct pattern for placing a large const-initialised struct in BSS without needing `static_cell` and without putting it on the stack.
- **`scan.set_frequency_fast` skips `CalibrateImage`** — within a single FCC band (902-928 MHz here), one calibration during init is enough.  The fast path saves ~3-4 ms per channel during scan-mode sweeps, which is dominant in scan loop cost.
- **`pre_init` is a no-op** — `bootloader_handoff` exists for back-compat with binaries that still call it, but SD's reset handler now runs before app `pre_init` and has already configured VTOR + interrupt forwarding.  Adding logic to `pre_init` will break SD.
- **`config_updates: Option<&LinkConfigSignal>`** and **`scan: Option<&ScanController>`** in `run_rx` / `run_tx` — these are `None` for static-config profiles (link bench, midi node) and `Some(&...)` only for the UI profile.  The check is a single `Option::is_some` per loop iteration; cost is negligible.

## What's solid, what's tentative

**Solid (don't refactor without strong reason):**
- The link-layer dedup / replay-window / packet_seq logic in `core/link`.
- The link-runtime `select4` shape in `run_rx` and `run_tx`.
- The framebuffer + async flush design for the display.
- The `LinkStatsCell` / `LinkConfigSignal` / `ScanController` shared-state plumbing.

**Tentative or open:**
- Embassy task split (currently using `embassy_futures::join` to colocate UI loop + link runtime in one task) — see PLAN.md.
- BLE config import — not yet implemented; SD is enabled but unused for BLE.
- Persistence (settings + boot counter survive reboot) — M7 work.
- Encryption (AEAD on the link layer) — pre-AEAD link runtime works fine; adding it is in M7 territory.
- FCC compliance (current GFSK config is not strictly §15.247 compliant; documented in conversation logs).
