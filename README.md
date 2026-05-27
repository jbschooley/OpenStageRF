# OpenStageRF

Open-source firmware platform for low-latency wireless MIDI (and experimental audio later) over sub-GHz radio. Designed for live performance reliability — built around a packet-radio link with sequence numbers, duplicate suppression for critical events, and a watchdog all-notes-off on link loss.

## Flashing (T114)

`cargo xtask run <profile>` builds for the board's target, flashes via probe-rs, and attaches RTT logs (Ctrl-C to detach). A **profile** is a TOML file in [`configs/`](configs/) — role / tx_source / diversity / band are baked in at build time (the generic `t114_ui` UI binary is configured per build, not a crate per deployment). Current UI profiles:

```bash
# 902–928 MHz (SX1262) hardware:
cargo xtask run ui_tx            # UI transmitter — DIN MIDI in from the FeatherWing UART
cargo xtask run ui_rx_diversity  # UI receiver with dual-SPI receive diversity (on-board SX1262 + DX-LR30 on SPI3)
cargo xtask run ui_rx            # UI receiver — single radio
cargo xtask run ui_bench_tx      # UI transmitter driven by the built-in synthetic MIDI scenario source (no DIN needed)

# 470–514 MHz (CN470/SX1268) hardware — same UI, 470 band plans:
cargo xtask run ui_tx_470
cargo xtask run ui_rx_470
cargo xtask run ui_rx_diversity_470  # 470 receiver with dual-SPI diversity (DX-LR30-470 on SPI3)
cargo xtask run ui_bench_tx_470
```

The `_470` profiles are identical to their 900 MHz counterparts but ship the 470–514 MHz band plans (the Band Plan menu shows only the band the radio can tune). Flash the variant matching your hardware. **Adding a profile = drop a `.toml` in `configs/`** — see [`configs/README.md`](configs/README.md) for the schema; no new crate needed.

To capture logs to a file: `cargo xtask run <profile> 2>&1 | tee run.log`. If two probes are connected, flash one at a time (the xtask doesn't pass a `--probe` selector).

**AEAD keys** are baked into UI builds at build time from `osrf-keys.toml` at the repo root (gitignored; copy `osrf-keys.toml.example`), or the `keys = ` path in the profile TOML. Each top-level table is one key (table name = display name) and all of them appear in the device's Key menu; `active = "<name>"` picks the boot default. Paired TX/RX units must build with the same key and have it selected. With no key file the build falls back to insecure test keys and prints a `NOT FOR PRODUCTION` warning.

## First Edition Target

- **Board:** Heltec T114 v2.1 (nRF52840 + SX1262, built-in 240×135 ST7789 TFT)
- **Band:** US 902–928 MHz ISM (unlicensed)
- **Modulation:** GFSK — chasing low latency, not LoRa range
- **Scope:** one-way MIDI link, single radio per node, no diversity, no BLE
- **UI (RX side):** built-in TFT + external 5-way joystick on header pins
- **MIDI front end:** external DIN opto-isolator (e.g. Adafruit MIDI FeatherWing for prototype)
- **Language / framework:** Rust + [embassy](https://embassy.dev) (async, no_std, multi-vendor via `embedded-hal`)

The firmware is structured to grow into more boards, radios, and feature profiles, but the initial focus is making one rock-solid configuration before broadening.

> **Platform note (2026-05):** development began on the DX-LR30 (STM32F103 + SX1262). It's still
> a supported board crate and the schematic / pinmap is committed, but two facts moved the
> first-edition target to T114: SWD pins (PA13/PA14) aren't broken out on the LR30-SP carrier
> (no probe-rs flow without tack-soldering to the QFP48), and the T114 has a built-in TFT that
> made M6 UI work practical in a way the DX-LR30 alone wouldn't have.  DX-LR30 will return as a
> minimal-cost TX-only profile once the link, UI, and persistence stack stabilises on T114;
> until then it's exercised through the host-side test suites.  **Other boards (TI CC1352R,
> STM32WBA, the v2/v3 custom designs sketched in [ROADMAP.md](ROADMAP.md)) are community ports
> unless and until the maintainer needs them — they aren't on the active path.**

## Roadmap

Future stages, audio capability tiers, codec engineering plans, and deferred platforms are tracked separately in [ROADMAP.md](ROADMAP.md). The README covers v1 (current focus) and the architectural commitments that apply across all stages.

## Reliability and mid-show failure modes

Anything that takes either side of the link offline mid-show is a P0 failure.  The threat
list (heapless capacity exhaustion, executor stall, panic, MCU lockup, …), the runtime
mitigations (hardware watchdog, panic-to-flash + auto-reset, periodic invariant
assertions), and the offline checks (4-hour soak, stack-watermark probe, no-alloc CI audit)
all live in [docs/reliability.md](docs/reliability.md).  Implementation lands in M5 and M7.

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
