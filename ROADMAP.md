# OpenStageRF Roadmap

The active project state, architecture, and design decisions are in [README.md](README.md). This file covers the planned trajectory beyond v1.

## Prototype Stages

### Stage 1 — basic link (current focus, v1)
See [First Edition Target](README.md#first-edition-target) in the README. 1× DX-LR30 TX, 1× DX-LR30 RX, one-way packetized MIDI over GFSK at ~915 MHz, no diversity, no encryption. Built on Rust + embassy via `embassy-stm32`.

### Stage 2 — diversity (UART slave first)
Add a second DX-LR30 to the receive end as a **UART slave**: it runs nearly the same firmware as the master, receives RF independently, and forwards `RxReport` frames (seq, RSSI, payload) to the master over UART. Master runs the dedupe/arbitration logic.

Why UART-slave before dual-SPI on one MCU:
- both boards run nearly identical code; no radio-driver hacks
- diversity arbitration is developed in isolation on the master
- the slave can sit physically apart for real spatial diversity
- the dual-SPI implementation gets done once, on the v2 custom board

Dual-SPI (both SX1262s on one MCU's SPI bus) is also a supported profile and becomes the default on the v2 custom board.

### Stage 2.5 — second platform (Heltec T114 / nRF52840)
Port the firmware to **Heltec T114** (nRF52840 + SX1262) via `embassy-nrf`. This is where the multi-vendor portability boundary gets validated: the same `core/`, `drivers/`, `protocols/` crates compile against a different HAL with only board and port code changing. Anything that doesn't compile cleanly is a portability bug to fix in core, not in the port.

T114 also unlocks experimentation with hardware AES-CCM (native to the nRF CCM peripheral) and lets BLE config/pairing development start in parallel with the radio link, since `embassy-nrf` + `nrf-softdevice` is a mature Rust BLE path.

### Stage 3 — encryption + authentication
Add AEAD with a sequence-number nonce. Default cipher: ChaCha20-Poly1305 (works on every chip; F103 has no crypto hardware). On targets with AES acceleration (nRF52/53 CCM peripheral, others), AES-128-CCM is selectable and faster. Replay protection comes from the AEAD nonce; tamper detection from the auth tag. RustCrypto crates (`chacha20poly1305`, `ccm`) are well-audited and `no_std` compatible. See *Encryption* and *Key distribution* in the README.

**AES-256 roadmap note:** AES-256-CCM is planned as a future selectable cipher (`cipher_id = AES_256_CCM`). It is hardware-accelerated on nRF5340 (CryptoCell CC312 supports AES-256) and will be a software fallback via RustCrypto on all other targets (including nRF52840, whose hardware CCM peripheral is 128-bit only). AES-256 adds no wire-format changes — it slots in as another `cipher_id` value in the local key entry. Deferred until Stage 4 (nRF5340 hardware) so the hardware path is tested first; software path can land earlier if there is demand.

### Stage 4 — v2 custom MIDI board (nRF5340 + 2× SX1262)
Spin a custom **nRF5340 + 2× SX1262** board, MIDI-only scope: dual-core M33 (app + net), CryptoCell CC312 hardware crypto (AES-CCM, AES-256, SHA-256), native BLE 5.3 (used for config/pairing only), both radios on shared SPI for true diversity, smaller form factor for keytar-mount. DIN MIDI in/out + opto-isolator front-end. Pre-certified Nordic modules (Raytac MDBT53, Fanstel BT5340) provide the cleanest FCC modular-approval path.

Stage 4 also decides the band question: stay on 902–928 if it holds up live, or add a 470–510 MHz SKU/profile (SX1268) for users in noisier ISM environments.

### Beyond v2

- channel scan, frequency diversity, mobile configurator app

## Potential future platforms

These were considered earlier and may return as community ports, but are not on the active roadmap:

- **STM32WBA5x** — capable hardware, but BLE-in-Rust support is currently rough (no mature stack equivalent to `nrf-softdevice`). Consider revisiting if TrouBLE or vendor Rust BLE matures.
- **TI CC1352R / CC1354R10** — strong hardware. No embassy HAL today. Either a community-built embassy HAL or a parallel Zephyr+C implementation could enable it.

### What "porting to C" means

If a contributor wants to support TI silicon (or anything else without an embassy HAL) via Zephyr+C, that's not a mechanical port of the Rust code — it's a **parallel C implementation that shares the on-air protocol spec and crypto test vectors but no source code**. Concretely, the C side would re-implement `core/link/`, `protocols/`, `crypto/`, and the radio driver from scratch in C against Zephyr's APIs. Devices running the Rust implementation and the Zephyr+C implementation interoperate over the air (same packet format, same AEAD, same key model); they do not share code. Analogous to BlueZ vs BlueDroid for Bluetooth — same protocol, separate codebases. Maintenance burden is real: every protocol change happens twice. Worth doing only if a contributor wants to own the C variant.
