# OpenStageRF Roadmap

The active project state, architecture, and design decisions are in [README.md](README.md). This file covers the planned trajectory beyond v1.

## Prototype Stages

### Stage 1 — basic link (current focus, v1)
See [First Edition Target](README.md#first-edition-target) in the README.  **As of 2026-05 the
first-edition target is the Heltec T114 (nRF52840 + SX1262) rather than the originally planned
DX-LR30** — see the README for why.  Stage 1 deliverable: 1× T114 TX, 1× T114 RX, one-way
packetized MIDI over GFSK at ~915 MHz, no diversity, no encryption, on-device UI with channel +
band-plan + TX-power configuration and runtime key-store scaffolding.  Built on Rust + embassy
via `embassy-nrf`.

DX-LR30 (STM32F103) remains in the workspace as a supported board crate; it'll come back as a
minimal-cost TX-only profile once the T114 stack is solid (link + UI + persistence).  Other
boards (CC1352R, STM32WBA, future custom designs) are **community ports unless the maintainer
needs them** — the active path is T114 → v2 nRF5340 board.

**Driver:** `drivers/radio/sx126x` wraps the upstream `sx1262` crate (BroderickCarlin) with a thin async layer + `RfSwitchControl` trait. Two impls cover the boards we care about: `Dio2RfSwitch` (T114, internal RF switch via SX1262 DIO2) and `PinRfSwitch` (DX-LR30, external TXEN/RXEN GPIOs). See PLAN.md Milestone 2 for the API.

**SX1262 GFSK rate limit (300 kbps):** the SX1262 silicon caps GFSK at 300 kbps per datasheet. This is fine for MIDI (a few kbps of sustained traffic, lots of headroom for retries/diversity).

### Stage 2 — diversity (dual-SPI on T114, active)
**As of 2026-05 the active diversity path is dual-SPI on a single T114**, not the
UART-slave approach originally planned here. A second SX1262 (a header-wired
DX-LR30-900M22S) sits on the nRF52840's dedicated SPI3 bus alongside the on-board
LR1262 on TWISPI0. Both radios listen on the same channel and feed one shared
`LinkReceiver`; whichever demodulates a packet first is accepted, and the receiver's
existing packet-`seq` replay window discards the duplicate from the other radio. That
replay window **is** the diversity arbitration — no separate dedupe layer was needed.

Why the switch to dual-SPI-first:
- the dedupe/arbitration logic already existed in `core/link` (replay windows), so
  diversity reduced to plumbing a second radio into the verified RX loop
- both SX1262s are the same concrete type on the T114 (`Spim<'static>` erases the
  peripheral instance), so `run_rx_diversity` reuses the single-radio runtime with no
  new monomorphisation or driver hacks
- it lands real diversity on the *current* first-edition hardware (a custom T114 carrier
  PCB) rather than waiting for the v2 board
- the DX-LR30 drives its RF switch from DIO2 and its TCXO from DIO3 exactly like the
  on-board module, so it uses the same `Dio2RfSwitch` impl with zero driver changes

Implementation (landed pre-hardware, pending PCB bring-up):
- board: `osrf_board_t114::resources_with_diversity()` builds `radio1` on SPI3 from the
  `dual_spi_diff_bus_radio1` pinout; single-radio profiles drop the `Radio1Tokens` and
  never claim SPI3 or the radio1 header pins
- runtime: `osrf_link_runtime::run_rx_diversity` wraps the verified `run_rx_inner` with an
  optional second radio; the single-radio `run_rx` path is behaviour-identical (the second
  receive future is `pending()`)
- profile: `profiles/t114_link_rx_diversity`
- v1 scope: fixed config — live reconfig / channel-scan / AEAD-key updates stay
  single-radio (those arms touch only the primary radio and would desync the secondary's
  channel)

The **UART-slave** variant (a second board forwarding `RxReport` frames over UART, for
true spatial separation) remains a valid future option — the arbitration logic is
transport-independent, so it would reuse the same shared-receiver merge. Dual-SPI also
becomes the default on the v2 custom board.

### Stage 2.5 — second platform (Heltec T114 / nRF52840)
Port the firmware to **Heltec T114** (nRF52840 + SX1262) via `embassy-nrf`. This is where the multi-vendor portability boundary gets validated: the same `core/`, `drivers/`, `protocols/` crates compile against a different HAL with only board and port code changing. Anything that doesn't compile cleanly is a portability bug to fix in core, not in the port.

T114 also unlocks experimentation with hardware AES-CCM (native to the nRF CCM peripheral) and lets BLE config/pairing development start in parallel with the radio link, since `embassy-nrf` + `nrf-softdevice` is a mature Rust BLE path.

### Stage 3 — encryption + authentication
Add AEAD with a sequence-number nonce. Default cipher: ChaCha20-Poly1305 (works on every chip; F103 has no crypto hardware). On targets with AES acceleration (nRF52/53 CCM peripheral, others), AES-128-CCM is selectable and faster. Replay protection comes from the AEAD nonce; tamper detection from the auth tag. RustCrypto crates (`chacha20poly1305`, `ccm`) are well-audited and `no_std` compatible. See *Encryption* and *Key distribution* in the README.

**Status (2026-05): on-air complete on T114.** `osrf-crypto` ships both ciphers behind a unified `CipherId` dispatch; `LinkSender::with_aead` / `LinkReceiver::with_aead` plug into the link layer with the auth tag appended after the body; nonce derivation matches the PLAN.md scheme (`[device_id:4][direction:1][session_seq:4][boot_counter:2]` with zero-padding for AES-CCM's 13-byte nonce); flash key-store persistence is wired (`apps/ui_runtime::{load_keys, save_key, remove_key}` over `sequential-storage::map` at the 8 KB `KEY_STORE_RANGE`).  Hardware verification: paired `t114_link_tx` / `t114_link_rx` profiles ran 6913 MIDI events under ChaCha20-Poly1305 with 0 missed notes, 0 AEAD failures, 11 background CRC errors across 27,524 packets (0.04%).

**Deferred to Stage 4:**
- Hardware AES via SoftDevice SVCs.  The plumbing exists in `osrf-crypto`'s `aes-hw-sd` feature (gated off on every shipping profile) but doesn't fit our current task topology — the link runtime runs at IRQ priority P2 and SD's SVCall handler is at priority 4, so `sd_ecb_block_encrypt` from the link path HardFaults.  Software AES is invisible in practice (~100 µs/packet).  Re-enabling needs either a lower link-runtime priority (loses radio-IRQ preemption), thread-mode deferral, or Stage 4 hardware with a different SD priority story.
- Multi-key keyring on RX.  Today's receiver holds one `AeadContext`; "Auto" mode can't decrypt multiple ciphers simultaneously.  Operator workaround: pick the same cipher on both ends.  Proper fix is a half-day receiver refactor.
- DX-LR30 AEAD (no RNG peripheral on STM32F103; needs a fallback-entropy decision).
- On-device key generation UI.  Until Stage 4 BLE lands there's no out-of-band way to share a freshly-generated key with the peer, so the menu item stays hidden — keys for testing live in `apps/link_bench::test_aead_chacha()` (a `[0x42; 32]` stub) or get hardcoded per-profile.  The Idle screen surfaces the active key by name (or `Open` is silent — only encrypted modes show a `Key: …` row).

**AES-256 roadmap note:** AES-256-CCM is planned as a future selectable cipher (`cipher_id = AES_256_CCM`). It is hardware-accelerated on nRF5340 (CryptoCell CC312 supports AES-256) and will be a software fallback via RustCrypto on all other targets (including nRF52840, whose hardware CCM peripheral is 128-bit only). AES-256 adds no wire-format changes — it slots in as another `cipher_id` value in the local key entry. Deferred until Stage 4 (nRF5340 hardware) so the hardware path is tested first; software path can land earlier if there is demand.

### Stage 4 — v2 custom MIDI board (nRF5340 + 2× SX1262)
Spin a custom **nRF5340 + 2× SX1262** board, MIDI-only scope: dual-core M33 (app + net), CryptoCell CC312 hardware crypto (AES-CCM, AES-256, SHA-256), native BLE 5.3 (used for config/pairing only), both radios on shared SPI for true diversity, smaller form factor for keytar-mount. DIN MIDI in/out + opto-isolator front-end. Pre-certified Nordic modules (Raytac MDBT53, Fanstel BT5340) provide the cleanest FCC modular-approval path.

Stage 4 also decides the band question: stay on 902–928 if it holds up live, or add a 470–510 MHz SKU/profile (SX1268) for users in noisier ISM environments.

### Beyond v2

- frequency diversity, mobile configurator app

(Channel-scan was originally listed here; pulled forward into Stage 1 / Milestone 6 in PLAN.md
since it's a pure UI + thin link-runtime feature and the most useful diagnostic for venue
coordination on the multi-band-plan UI.)

## Potential future platforms

These were considered earlier and may return as community ports, but are not on the active roadmap:

- **STM32WBA5x** — capable hardware, but BLE-in-Rust support is currently rough (no mature stack equivalent to `nrf-softdevice`). Consider revisiting if TrouBLE or vendor Rust BLE matures.
- **TI CC1352R / CC1354R10** — strong hardware. No embassy HAL today. Either a community-built embassy HAL or a parallel Zephyr+C implementation could enable it.

### What "porting to C" means

If a contributor wants to support TI silicon (or anything else without an embassy HAL) via Zephyr+C, that's not a mechanical port of the Rust code — it's a **parallel C implementation that shares the on-air protocol spec and crypto test vectors but no source code**. Concretely, the C side would re-implement `core/link/`, `protocols/`, `crypto/`, and the radio driver from scratch in C against Zephyr's APIs. Devices running the Rust implementation and the Zephyr+C implementation interoperate over the air (same packet format, same AEAD, same key model); they do not share code. Analogous to BlueZ vs BlueDroid for Bluetooth — same protocol, separate codebases. Maintenance burden is real: every protocol change happens twice. Worth doing only if a contributor wants to own the C variant.
