# OpenStageRF

Open-source firmware platform for low-latency wireless MIDI (and experimental audio later) over sub-GHz radio. Designed for live performance reliability — built around a packet-radio link with sequence numbers, duplicate suppression for critical events, and a watchdog all-notes-off on link loss.

## First Edition Target

- **Board:** DX-LR30 (STM32F103C8T6 + SX1262, detachable radio module)
- **Band:** US 902–928 MHz ISM (unlicensed)
- **Modulation:** GFSK — chasing low latency, not LoRa range
- **Scope:** one-way MIDI link, single radio per node, no diversity, no BLE
- **UI (RX side, optional):** external I²C OLED + GPIO buttons
- **MIDI front end:** external DIN opto-isolator (e.g. Adafruit MIDI FeatherWing for prototype)
- **Language / framework:** Rust + [embassy](https://embassy.dev) (async, no_std, multi-vendor via `embedded-hal`)

The firmware is structured to grow into more boards, radios, and feature profiles, but the initial focus is making one rock-solid configuration before broadening.

## Prototype Stages

### Stage 1 — basic link
1× DX-LR30 TX, 1× DX-LR30 RX. One-way packetized MIDI over GFSK at ~915 MHz, no diversity, no encryption. Goal: prove latency, range, and packet reliability end-to-end with real instruments. Built on Rust + embassy via `embassy-stm32`. (See *First Edition Target* above.)

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
Add AEAD with a sequence-number nonce. Default cipher: ChaCha20-Poly1305 (works on every chip; F103 has no crypto hardware). On targets with AES acceleration (nRF52/53 CCM peripheral, others), AES-128-CCM is selectable and faster. Replay protection comes from the AEAD nonce; tamper detection from the auth tag. RustCrypto crates (`chacha20poly1305`, `ccm`) are well-audited and `no_std` compatible. See *Encryption* and *Key distribution* below.

### Stage 4 — v2 custom MIDI board (nRF5340 + 2× SX1262)
Spin a custom **nRF5340 + 2× SX1262** board, MIDI-only scope: dual-core M33 (app + net), CryptoCell CC312 hardware crypto (AES-CCM, AES-256, SHA-256), native BLE 5.3 (used for config/pairing only — not for audio), both radios on shared SPI for true diversity, smaller form factor for keytar-mount. DIN MIDI in/out + opto-isolator front-end. Pre-certified Nordic modules (Raytac MDBT53, Fanstel BT5340) provide the cleanest FCC modular-approval path.

Stage 4 also decides the band question: stay on 902–928 if it holds up live, or add a 470–510 MHz SKU/profile (SX1268) for users in noisier ISM environments.

### Stage 5 — v3 audio expansion board (nRF5340 + 2× Si4463)
Spin a second custom board for the audio tier. Same MCU and architectural pattern as v2, but with three deliberate differences:

- **Radios swap SX1262 → Si4463 / Si4464.** SX1262 caps at 300 kbps, which is fine for MIDI but locks out every audio profile. Si4463 does up to 1 Mbps GFSK / ~2 Mbps in 4-GFSK with the same SPI-controlled architecture; a `radio-si446x` driver crate slots in alongside `radio-sx126x`. Both radios on shared SPI, exactly the same dual-radio diversity pattern as v2.
- **Audio I/O front-end added.** I²S codec IC (TLV320AIC3204-class), instrumentation-grade headphone amp for IEM output, balanced mic preamp + phantom power for mic input, balanced 1/4″ TRS for instrument input. Dedicated audio rail with low-noise LDOs separated from the digital supply.
- **Default modulation: 4-GFSK** for audio profiles. Si4463 supports 4-(G)FSK natively — same chip, configuration change. Halves the per-link RF channel width vs 2-GFSK at the same audio bitrate, bringing OpenStageRF within ~2–3× of pro mid-tier spectrum density. Sensitivity penalty (~3–5 dB) is irrelevant at typical IEM/mic ranges. MIDI builds on this hardware can stay on 2-GFSK for max sensitivity if needed.

v3 inherits everything else from v2: BLE config/pairing, AES-128-CCM via CryptoCell, key store, link layer, true diversity arbitration. The only new code is the audio driver crate, codec crates, and the Si4463 driver. Audio profiles (IMA ADPCM stereo, PCM 24k stereo, PCM 48k mono, etc.) all run on this hardware with **true 2-radio diversity** because the radio bandwidth is enough to carry the entire audio stream on a single channel — both radios listen to the same channel and packet selection runs identically to the MIDI tier.

Why two boards (v2 MIDI + v3 audio) rather than one combined board: keeping v2 MIDI-only ships the more impactful product first (open wireless MIDI exists, audio doesn't), avoids the audio analog front-end complexity slowing v2, and lets v3 build on a proven v2 codebase rather than debugging audio + RF + protocol simultaneously.

### Beyond v2

- channel scan, frequency diversity, mobile configurator app
- audio (see *Audio capability tiers* below)

Audio is deferred until the MIDI link is solid live. SX1262-class radios are bandwidth-starved for any audio target.

### Audio capability tiers

Wireless audio breaks into two architecturally distinct tiers reachable from this project, plus one explicitly out of scope. **2.4 GHz / BLE Audio is not a goal of this project** — sub-GHz spectrum is a hard requirement for stage propagation.

#### Tier 2 — Pro wireless audio (SLX-D / ULX-D / EW-D class) — *active roadmap, lands on v3 hardware*

**Target:** point-to-point mics, instruments, and IEMs at 2–5 ms end-to-end latency, sub-GHz spectrum, full-band audio, true 2-radio diversity at full audio quality.

**Hardware: v3 board (nRF5340 + 2× Si4463 + audio front-end).** Audio profiles do not run on the v2 MIDI board — SX1262 caps at 300 kbps, below every audio bitrate in the table below. The Si4463-based v3 board (see *Stage 5* above) is the audio-tier target.

**Default modulation: 4-GFSK.** Si4463 supports 4-(G)FSK natively, halving per-link RF channel width vs 2-GFSK at the same bitrate. This brings per-link RF channel widths to ~250–700 kHz depending on profile — within ~2–3× of pro mid-tier spectrum density (ULX-D ~170 kHz, SLX-D ~350 kHz, EW-D ~300 kHz). Sensitivity penalty (~3–5 dB vs 2-GFSK) is irrelevant at typical IEM/mic stage ranges.

**On-air bandwidth reality:** SLX-D / ULX-D / EW-D / EW-DX all advertise "24-bit / 48 kHz" at their analog interfaces, but that's not the over-the-air rate. Channel grids and channel densities (e.g. ULX-D fitting ~47 channels into one 8 MHz TV channel, EW-D's 600 kHz equidistant spacing) imply **~250–600 kbps per link** on-air after a low-latency proprietary codec compresses ~3–5×. They run π/4-DQPSK-class modulation in narrow channels; we run 4-GFSK in slightly wider channels and trade a small sensitivity hit for ecosystem fit. **The bottleneck for matching pro quality is the codec, not the radio bandwidth.**

**Codecs the pro systems use (all proprietary, none publicly named):** Shure SLX-D / ULX-D / Axient and Sennheiser EW-D / EW-DX all use unnamed proprietary low-latency codecs in the ADPCM / sub-band ADPCM family. Spectera's named codec is SeDAC plus PCM modes. None use open codecs (Opus, LC3, AAC) because perceptual codecs have 5–20 ms+ encoder latency that breaks the sub-3 ms total system latency target. Sub-3 ms forces the codec into sample-by-sample or ~1 ms-block processing, which means ADPCM-family or uncompressed PCM.

**Audio profiles (initial set, all on v3 hardware with 4-GFSK + true 2-radio diversity):**

| Profile | Use case | Per-link bitrate | RF channel BW (4-GFSK) | Codec | Codec latency | Audio quality |
|---|---|---|---|---|---|---|
| `audio_pcm_48k_mono` | mics, instruments | 768 kbps | ~500 kHz | none (16-bit / 48 kHz) | ~0 ms | uncompressed pro audio, full 24 kHz response. **Exceeds what most pro mid-tier wireless mics deliver** (they all compress). |
| `audio_pcm_24k_stereo` | stereo IEM, full diversity | 768 kbps | ~500 kHz | none (16-bit / 24 kHz) | ~0 ms | broadcast-quality stereo, 12 kHz top end. No artifacts. Recommended default for stereo IEM if 12 kHz top is acceptable — true diversity comes free. |
| `audio_mulaw_48k_stereo` | stereo IEM, full-band | 768 kbps | ~500 kHz | µ-law 8-bit log / 48 kHz | ~0 ms | full 24 kHz response, ~78 dB dynamic range (~13-bit equivalent). **Quantization noise scales with signal — quiet passages stay clean**, unlike IMA ADPCM. Trivial implementation (256-entry table lookup). Strong default for stereo IEM where full bandwidth matters. |
| `audio_ima_adpcm_stereo` | stereo IEM, half the spectrum | 384 kbps | ~250 kHz | IMA ADPCM 4-bit / 48 kHz | <1 ms | full 20 kHz response, ~50–55 dB SNR. Audible quantization noise on quiet passages, decaying notes, reverb tails — signal-independent floor. Best spectrum density of any current profile but lowest quality. |
| `audio_pcm_dual_radio_stereo_48k` | stereo IEM, max quality (no diversity) | 768 kbps × 2 radios | ~500 kHz × 2 | none (16-bit / 48 kHz × 2) | ~0 ms | uncompressed full-band stereo via L on radio0, R on radio1. Shared timestamp for L/R sync. **Gives up diversity** in exchange for full quality — niche use (studio monitoring, short-range high-fidelity). |
| `audio_sbadpcm_stereo` *(roadmap, modest engineering)* | stereo IEM, ~70 dB at 400 kbps | ~400 kbps | ~250 kHz | sub-band ADPCM, small frames (1–2 ms) | 1–2 ms | full-band stereo at ~70 dB dynamic range. Beats SLX-D on latency, slightly behind on perceptual quality. **1–3 person-months of focused codec work.** |
| `audio_transparent_stereo` *(roadmap, real codec project)* | stereo IEM, true SLX-D parity | ~400 kbps | ~250 kHz | adaptive bit-allocation w/ noise shaping (CELT-class) | 2–3 ms | transparent stereo at sub-3 ms. **No good open implementation today — 6–12+ person-months by an audio DSP engineer.** Real gap in the open ecosystem, strong contribution target. |

**True diversity at full audio quality:** every profile in the table that uses a single radio (everything except `audio_pcm_dual_radio_stereo_48k`) runs on v3 with **both radios tuned to the same channel** and the existing diversity arbitration logic. Audio fits in a single radio's bandwidth, so the second radio is "free" for diversity. This is the same pattern pro IEM bodypacks use (mono RF link carrying stereo-encoded audio, diversity on that single channel).

**System architecture:** balanced mic preamp / instrument input / I²S codec (e.g. TLV320AIC3204) → MCU codec encode (or pass-through PCM) → AEAD encrypt → packetize → radio. Receiver inverts to I²S DAC → headphone amp. Small jitter buffer (~1–2 audio frames), no retry on audio packets, FEC optional.

**Scope in this project:** v3 board target + audio driver crate (`drivers/audio/i2s/`) + codec crates per-profile + `radio-si446x` driver. The link/diversity/key-store/crypto layers carry over from v2 unchanged. **This is the most underserved tier in pro wireless audio today** — there's no open-source SLX-D / EW-D equivalent, and an open uncompressed-PCM mono mic in 902 MHz with true diversity could measurably outperform mid-tier pro gear.

#### Tier 3 — Pro top-tier (Spectera class) — *sibling project, not buildable here*

**Target:** uncompressed PCM stereo, sub-ms latency, multi-channel multiplexing in 6–8 MHz UHF channels.
**Hardware:** wideband SDR transceiver (AD9361 / AD9364 / ADRV9002) + **FPGA** (Zynq-class). The FPGA is not optional — AD9361's parallel LVDS interface runs at hundreds of MHz and aggregates ~3 Gbps of digital data; no microcontroller has a peripheral that can drive it in real time at full sample rate. Pluto SDR (~$200) pairs AD9363 with Zynq-7010 for exactly this reason.
**Scope:** separate repository. Different silicon, different firmware (likely Rust-on-Linux orchestrating an FPGA bitstream), different compliance program. Protocol/key/diversity concepts from this project may inform it; code does not transfer.

### Potential future platforms

These were considered earlier and may return as community ports, but are not on the active roadmap:

- **STM32WBA5x** — capable hardware, but BLE-in-Rust support is currently rough (no mature stack equivalent to `nrf-softdevice`). Consider revisiting if TrouBLE or vendor Rust BLE matures.
- **TI CC1352R / CC1354R10** — strong hardware (CC1354R10 reaches 4 Mbps sub-GHz, more headroom than SLX-D-class targets need). No embassy HAL today. Either a community-built embassy HAL or a parallel Zephyr+C implementation could enable it.
- **TI CC1200** — narrowband sub-GHz transceiver supporting MSK (QPSK-adjacent) and 4-FSK, slightly higher spectral efficiency and adjacent-channel rejection than Si4463. Unlike TI's integrated wireless MCUs (CC1352R, CC1354R10), CC1200 is a standalone SPI-controlled radio in the same architectural class as SX1262 and Si4463 — **a `radio-cc1200` Rust crate built on `embedded-hal` traits is fully viable, no Zephyr+C required.** Driver effort comparable to writing the Si4463 crate (~3000–4000 lines, register map from datasheet, configuration sequences from SmartRF Studio). Tradeoff vs Si4463: marginally better RF performance, fewer pre-certified module options.
- **CML Microcircuits CMX940** — native π/4-DQPSK, exactly the modulation pro mid-tier wireless audio likely uses. Would close the spectral-efficiency gap to SLX-D/EW-D entirely. Commercial-radio silicon: no Rust ecosystem, no maker distribution, datasheet/sourcing friction. Not realistic for this project; mentioned for completeness because someone will eventually ask.

#### What "porting to C" means

If a contributor wants to support TI silicon (or anything else without an embassy HAL) via Zephyr+C, that's not a mechanical port of the Rust code — it's a **parallel C implementation that shares the on-air protocol spec and crypto test vectors but no source code**. Concretely, the C side would re-implement `core/link/`, `protocols/`, `crypto/`, and the radio driver from scratch in C against Zephyr's APIs. Devices running the Rust implementation and the Zephyr+C implementation interoperate over the air (same packet format, same AEAD, same key model); they do not share code. Analogous to BlueZ vs BlueDroid for Bluetooth — same protocol, separate codebases. Maintenance burden is real: every protocol change happens twice. Worth doing only if a contributor wants to own the C variant.

## Architecture

Layered so one app can build for many boards and radios:

```
app
  → profile           (which board + role + features + radio config)
    → core            (link, diversity arbitration, scheduler, config)
      → drivers       (radio, display, MIDI, input — embedded-hal trait bounds)
        → port        (embassy_stm32, embassy_nrf — platform-shared helpers)
          → board     (pin map + chip-specific HAL setup)
```

- `apps/` — high-level firmware roles (e.g. `midi_node` for TX or RX)
- `profiles/` — build configurations: board + role + features + radio params
- `boards/` — hardware pin maps and capabilities only, no behavior
- `ports/` — platform glue (`stm32_hal` first; `ti_simplelink` reserved for future)
- `drivers/` — `radio/sx126x`, `radio/cc13xx`, `display/ssd1306`, `midi/din_uart`, `input/buttons`
- `core/` — link layer, diversity, scheduler, config
- `protocols/` — frozen on-air packet formats (`midi_packet_v1`, …)
- `tools/` — host-side configurator, packet analyzer, latency tester
- `docs/` — hardware guides, regulatory notes, build/porting guides
- `examples/`, `tests/`

## Key Design Decisions

### 1. Profile-driven multi-target builds
A profile combines a board, a role (TX/RX), enabled features (display, diversity, …), and radio config. App and core code stay hardware-agnostic.

```
fw build dx_lr30_tx_basic
fw build dx_lr30_rx_basic
fw build stm_oled_rx_dual_spi   # v2 dual-radio diversity
```

### 2. Radio abstraction
App code calls `radio_send`, `radio_start_rx`, etc. Drivers implement those for SX126x today, with room for CC13xx, STM32WL integrated radio, etc. The driver is instance-based so two radios can coexist.

### 3. Packet-based MIDI, not byte-tunneling
MIDI events are parsed, packetized, and sent with sequence numbers and CRC. The receiver reconstructs DIN MIDI. This enables:
- duplicate suppression by sequence number
- prioritization (note on/off and sustain > CC > clock/sysex)
- duplicate transmission of critical events without ACK/retry latency
- a watchdog that fires all-notes-off on link loss

### 4. Diversity — two topologies, same arbitration
Two profiles, same `core/diversity` logic underneath:

- **UART slave** (Stage 2 prototype): two whole boards, slave forwards `RxReport` frames to master over UART. Used for the DX-LR30 prototype.
- **Dual-SPI** (Stage 4 custom board): two SX1262s on one MCU's SPI bus.
  - shared: SCK, MOSI, MISO
  - per-radio: CS (NSS), DIO1 (IRQ), BUSY, RESET

  Both radios stay in RX simultaneously. CS is the multiplexer — only the addressed radio drives MISO. DIO1 IRQs land on separate GPIOs; the ISR queues a per-radio "service me" flag and the handler reads each FIFO sequentially. SPI bus is not the bottleneck for MIDI or even pushed-hard audio.

Arbitration default: first valid packet (by sequence number) wins; later versions can add RSSI/quality-based selection.

### 5. Encryption — AEAD, cipher-agile, replay-protected
On-air packets carry a 1-byte cipher ID, sequence-number nonce, and an auth tag. Two AEAD ciphers are supported, plus debug/auth-only modes:

- `none` — no auth, no encryption (debug / lowest latency)
- `mac_only` — Poly1305 or HMAC, replay/tamper protection only
- `chacha20_poly1305` — universal software AEAD, 256-bit key
- `aes_128_ccm` — universal hardware AEAD on all wireless MCUs (BLE-aligned)

Defaults by chip (active platforms; future platforms in italics):

| Chip                       | AES-CCM hardware                                | Default cipher                |
| -------------------------- | ----------------------------------------------- | ----------------------------- |
| STM32F103 (DX-LR30, v1)    | none                                            | ChaCha20-Poly1305 (software)  |
| nRF52840 (T114, Stage 2.5) | yes (CCM peripheral, native to BLE Link Layer)  | AES-128-CCM (hardware)        |
| nRF5340 (v2 custom board)  | yes (CCM peripheral) + CryptoCell CC312         | AES-128-CCM (hardware)        |
| *STM32WBA5x* (future)      | *yes (CRYP peripheral)*                         | *AES-128-CCM (hardware)*      |
| *CC1352R* (future)         | *yes (AES accelerator)*                         | *AES-128-CCM (hardware)*      |

**Why CCM over GCM:** AES-CCM has hardware acceleration on every wireless MCU we target (GCM does not — nRF's CCM peripheral is the gap, and that's the chip family we most want hardware support on). CCM is also the same AES mode used by BLE Link Layer encryption, which aligns nicely with v2 BLE pairing. GCM's parallelization advantage doesn't matter at MIDI/audio packet sizes.

**Why not AES-256:** ChaCha20-Poly1305 already provides 256-bit symmetric security as the universal software cipher — there's no ChaCha-128. AES-256-CCM specifically *breaks* the nRF52840 hardware path (its CCM peripheral is AES-128 only), defeating the reason CCM was chosen. AES-128 is sufficient against any realistic adversary for stage RF gear.

ChaCha20-Poly1305 is the universal fallback for any link where one peer can't do AES-CCM in hardware (notably any link involving the DX-LR30). Software ChaCha is faster than software AES on Cortex-M3, so it's the right cross-chip default. Cost on Cortex-M4: tens of µs per packet, well under any latency budget — including future stereo IEMs at 1.5 Mbps.

### 6. Key distribution — provider abstraction

**Keys are typed by cipher and addressed on-air by a 1-byte user-assigned (or pairing-assigned) `key_id`.** Each key entry in the store is:

```c
struct key_entry {
    uint8_t  key_id;            // 1 byte — sent on-air; assigned by user (hardcoded mode) or by pairing (BLE/USB modes)
    uint8_t  cipher_id;         // NONE, MAC_ONLY, CHACHA20_POLY1305, AES_128_CCM
    uint8_t  key_bytes[32];     // 16 used for AES-128, all 32 for ChaCha20
    uint64_t tx_nonce_counter;  // monotonic, persisted to flash
    char     name[16];          // shown in on-device menu
};
```

Why typed by cipher:
- AES-128 keys are 16 bytes; ChaCha20 keys are 32 bytes — the entry has to know which cipher it's for
- Reusing the same bytes under two different AEAD constructions is unsafe; binding `(cipher, key)` together prevents it
- Nonce reuse rules differ per cipher; the `tx_nonce_counter` lives with the key

Why a 1-byte user-assigned `key_id` (instead of a content-derived fingerprint):
- Smallest on-air footprint — 1 byte of crypto routing per packet (~17% shrink on a 24 B packet, directly translating to less air time and lower latency)
- In **hardcoded-list mode**, the user is already maintaining a single `key_list.h` source file and flashing it to every device — assigning consistent `key_id`s in that file is a one-time discipline cost, not an ongoing burden
- In **paired modes (BLE / USB / peer-distribute)**, the pairing protocol negotiates the `key_id` automatically; user never sees it. This is how BLE Link Layer LTK indexing, IPsec SPIs, and SSH session-key IDs all work
- ID mismatch across devices fails loud via auth-tag failure — there's no silent corruption mode; receiver just rejects the packet

**No on-air `cipher_id`.** The cipher is recovered from the local key entry once the receiver matches the `key_id`. There's no downgrade-attack vector to worry about because there's nothing to downgrade — `cipher_id` isn't on the wire.

**"Encryption off" is just an entry** with `cipher_id = NONE`, not a separate concept. It gets its own `key_id` like any other key and shows up in the on-device menu.

**Provider abstraction.** Link layer calls `key_provider_lookup(key_id) → key_entry*`. Providers are pluggable so new modes drop in without touching the link layer:

- `hardcoded_list` (v1) — keys baked at flash time with user-assigned IDs; on-device menu selects active key; flashed onto every device the user owns
- `ble_config` (v2+) — keys received from a paired desktop/mobile app, IDs negotiated at pairing
- `usb_config` (v2+) — same idea over USB CDC
- `peer_distribute` (v2+) — one device generates a key, hands it to peers over BLE/USB
- future: NFC, QR-via-camera, etc.

Each device stores 1+ keys plus an active-key ID in flash.

### 7. Multi-band — firmware-flexible, sale-restricted
SX126x silicon is wideband. The firmware exposes the band as a profile parameter (`band: us_915`, `band: eu_868`, `band: tvband_470_510`, etc.) and lets users build whatever their radio module is matched for. Profiles ship for legal-by-default ISM bands. Profiles for restricted bands (e.g. 470–510 MHz, which overlaps US TV / Part 74 wireless mic spectrum) are available as templates but with explicit regulatory warnings — the user flashing them is responsible for legality in their jurisdiction.

For-sale hardware is restricted to bands with a viable certification path. Initially that means **902–928 MHz only**. A 470–510 MHz SKU would require either Part 74 / 15.236 wireless-mic authorization (which assumes audio program transmission and likely doesn't fit a data device) or TVWS Part 15 white-space-data-device certification (~$20–50k+ compliance program with geolocation + WSDB integration).

### 8. UI on a separate bus from the radio
OLED is I²C, buttons are GPIO. Radio gets unblocked SPI and deterministic IRQ servicing.

### 9. Open source firmware, hardware comes later
Firmware ships as source. Hardware sold (if any) starts as non-RF dev boards and lets users add their own pre-certified radio modules. Productized RF hardware is deferred until demand is proven and FCC/modular-approval cost is justified.

### 10. Rust + embassy as the primary platform
The firmware is written in Rust on top of [embassy](https://embassy.dev) — async, `no_std`, multi-vendor via `embedded-hal`. embassy has first-class support for STM32 (every series including F103 and WBA) and Nordic (nRF52, nRF53), which covers v1 through v2 with the same codebase. The `embedded-hal` trait abstractions replace what would otherwise be a hand-rolled `port_api.h`-style HAL.

**Why embassy over Zephyr+C:**

- **Lower latency** — no kernel scheduler, no driver locking, no IPC. The executor is a single-threaded cooperative state machine. RF IRQ → `await` resume → packet service is essentially the same code path as a hand-written ISR-plus-flag pattern, with no overhead.
- **async/await fits RF protocols structurally** — "wait for DIO1, then read FIFO" is one linear async function, not a callback or thread-blocked-on-semaphore.
- **Memory safety** at compile time — no buffer overflows or use-after-free in packet decode or the key store, which matters for code that runs the radio path live on stage.
- **Multi-vendor without DTS/Kconfig overhead** — `embedded-hal` traits give vendor-agnostic SPI/GPIO/UART without the framework ceremony.

**Portability boundary rule.** Vendor HAL crates (`embassy-stm32`, `embassy-nrf`, etc.) are only allowed in `boards/<board>/` and `ports/<port>/`. Everywhere else (`core/`, `drivers/<X>/`, `protocols/`, `apps/`) depends only on `embedded-hal` traits and project-internal crates. If a driver ever has to import `embassy-stm32` directly, that's a portability bug.

**Build system.** Cargo workspace with one crate per module (driver, protocol, board, app). Profiles live as YAML files; an `xtask` helper translates `profile.yaml` into the right `cargo build --features ... --target ... -p <board_app>` invocation.

**Other ports stay welcome as community contributions.** A Zephyr+C port (TI CC13xx support, alternative for users who prefer Zephyr's middleware) and a bare-metal STM32 HAL port (minimum-overhead reference) are both reasonable additions if someone wants to maintain them. They aren't on the maintainer's roadmap.

## Directory Structure

```
wireless-performer-fw/
├── Cargo.toml                              # workspace root
├── apps/                                   # bin crates (executables)
│   └── midi_node/                          # TX/RX firmware role; behavior selected by profile
├── boards/                                 # board crates: pin maps + chip-specific HAL setup
│   ├── dx_lr30/                            # v1 — STM32F103 + SX1262 (embassy-stm32)
│   ├── t114/                               # Stage 2.5 — Heltec T114, nRF52840 + SX1262 (embassy-nrf)
│   ├── nrf5340_dual_sx1262/                # v2 custom MIDI — nRF5340 + 2× SX1262 (embassy-nrf)
│   ├── nrf5340_dual_si4463/                # v3 custom audio — nRF5340 + 2× Si4463 + audio front-end
│   └── _future/                            # deferred targets, not on active roadmap
│       ├── stm32_custom_oled_dual_radio/   # was v2 plan — STM32WBA + 2× SX1262
│       └── ti_lpstk_cc1352r/               # TI CC1352R, awaiting embassy HAL
├── ports/                                  # platform-shared code (not chip-specific)
│   ├── embassy_stm32/                      # STM32 family helpers (flash partitions, etc.)
│   └── embassy_nrf/                        # nRF family helpers (softdevice glue, etc.)
├── profiles/                               # build configs: board + role + features + radio
│   ├── dx_lr30_tx_basic/                   # v1 transmitter
│   ├── dx_lr30_rx_basic/                   # v1 receiver
│   ├── t114_rx_diversity/                  # Stage 2.5 receiver on nRF52840
│   ├── nrf5340_v2_midi/                    # v2 board, MIDI with diversity
│   ├── nrf5340_v3_audio_iem/               # v3 board, stereo IEM (default: pcm_24k_stereo + diversity)
│   └── nrf5340_v3_audio_mic/               # v3 board, uncompressed PCM mono mic + diversity
├── drivers/                                # vendor-neutral library crates
│   ├── radio/
│   │   ├── sx126x/                         # SX1262/SX1268 — v1/v2, depends on embedded-hal traits only
│   │   ├── si446x/                         # Si4463/Si4464 — v3 audio path, 2-GFSK / 4-GFSK
│   │   ├── cc1200/                         # future — TI standalone SPI radio, MSK + 4-FSK, embassy-friendly
│   │   └── cc13xx/                         # future — TI integrated MCU+radio, requires Zephyr+C or embassy HAL port
│   ├── display/
│   │   └── ssd1306/                        # I²C OLED
│   ├── input/
│   │   └── buttons/                        # GPIO buttons / joystick
│   ├── midi/
│   │   └── din_uart/                       # opto-isolated DIN MIDI
│   └── audio/                              # v3+
│       ├── i2s/                            # I²S to/from audio codec IC
│       ├── codec_pcm/                      # raw PCM passthrough (no compression)
│       ├── codec_mulaw/                    # 8-bit µ-law log compression (table lookup)
│       └── codec_ima_adpcm/                # IMA ADPCM 4-bit encoder/decoder
├── core/                                   # portable, hardware-agnostic library crates
│   ├── link/                               # packetization, seq numbers, CRC, dedupe, watchdog
│   ├── diversity/                          # arbitration (UART-slave or dual-SPI)
│   ├── scheduler/                          # event/packet timing
│   └── config/                             # persisted settings, key store, active profile
├── protocols/                              # frozen on-air formats
│   └── midi_packet_v1/
├── crypto/                                 # AEAD wrappers (chacha20poly1305, ccm) over RustCrypto
├── xtask/                                  # build helper: profile.yaml → cargo invocation
├── tools/                                  # host-side
│   ├── configurator/                       # desktop/CLI key + channel config
│   ├── packet_analyzer/                    # off-air capture/decode
│   └── latency_tester/                     # bench measurement harness
├── docs/
│   ├── hardware_guides/                    # per-board build / wiring instructions
│   ├── regulatory/                         # band notes, FCC / certification context
│   └── build_guides/                       # toolchain + flashing
├── examples/
├── tests/
└── README.md
```

Each `boards/<name>/board.yaml` documents that board's MCU, pin map, and radio wiring. Each `profiles/<name>/profile.yaml` documents the role, features, and radio config for that build target. The `xtask` helper reads the profile YAML and invokes Cargo with the right features, target triple, and board crate.

## License

OpenStageRF is **dual-licensed**:

- **AGPLv3** for individual, hobbyist, performer, and open-source use — see [LICENSE](LICENSE)
- **Commercial license** for shipping closed-source hardware or software products — see [LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md)

### Free path (AGPLv3)

If you build, modify, run, or perform with OpenStageRF — including paid gigs, touring, churches, theaters, and small businesses doing one-off rigs — you can use the firmware freely under AGPLv3. Hack on it, fork it, build your own boards around it, take it on tour. The only obligation is that if you *distribute* a modified version, you also publish your modifications under AGPLv3.

### Commercial path

If you want to ship a closed-source hardware product (or closed-source firmware) that includes OpenStageRF code, that requires a separate commercial license. Contact the maintainer to negotiate terms.

### Why the commercial split

Honest version: I want to build and sell certified hardware running this firmware, and **FCC certification is expensive** — roughly $2–5k for SDoC-only digital boards, $5–10k for products built on pre-certified RF modules, and $10–20k+ for fully custom RF designs. That money has to come from somewhere. The commercial license track funds certification, hardware development, and ongoing maintenance.

The intent is **not** to extract money from end users or working musicians. It's specifically to capture revenue from companies that would otherwise embed open-source firmware into closed-source commercial products without contributing back. AGPL is the mechanism that creates the friction; the commercial license is the off-ramp.

### Contributions

See [CONTRIBUTING.md](CONTRIBUTING.md). All commits must be DCO-signed (`git commit -s`). A formal CLA process will be put in place before external contributions are merged, to keep the dual-licensing path open.
