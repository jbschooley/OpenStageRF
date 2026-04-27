# OpenStageRF

Open-source firmware platform for low-latency wireless MIDI (and experimental audio later) over sub-GHz radio. Designed for live performance reliability — built around a packet-radio link with sequence numbers, duplicate suppression for critical events, and a watchdog all-notes-off on link loss.

## First Edition Target

- **Board:** DX-LR30 (STM32F103C8T6 + SX1262, detachable radio module)
- **Band:** US 902–928 MHz ISM (unlicensed)
- **Modulation:** GFSK — chasing low latency, not LoRa range
- **Scope:** one-way MIDI link, single radio per node, no diversity, no BLE
- **UI (RX side, optional):** external I²C OLED + GPIO buttons
- **MIDI front end:** external DIN opto-isolator (e.g. Adafruit MIDI FeatherWing for prototype)
- **Build system:** STM32 HAL + CMake

The firmware is structured to grow into more boards, radios, and feature profiles, but the initial focus is making one rock-solid configuration before broadening.

## Prototype Stages

### Stage 1 — basic link
1× DX-LR30 TX, 1× DX-LR30 RX. One-way packetized MIDI over GFSK at ~915 MHz, no diversity, no encryption. Goal: prove latency, range, and packet reliability end-to-end with real instruments. (See *First Edition Target* above.)

### Stage 2 — diversity (UART slave first)
Add a second DX-LR30 to the receive end as a **UART slave**: it runs nearly the same firmware as the master, receives RF independently, and forwards `RxReport` frames (seq, RSSI, payload) to the master over UART. Master runs the dedupe/arbitration logic.

Why UART-slave before dual-SPI on one MCU:
- both boards run nearly identical code; no radio-driver hacks
- diversity arbitration is developed in isolation on the master
- the slave can sit physically apart for real spatial diversity
- the dual-SPI implementation gets done once, on the v2 custom board

Dual-SPI (both SX1262s on one MCU's SPI bus) is also a supported profile (`stm_oled_rx_dual_spi`) and becomes the default on the v2 custom board.

### Stage 3 — encryption + authentication
Add AEAD with a sequence-number nonce. Default cipher: ChaCha20-Poly1305 (works on every chip; F103 has no crypto hardware). On targets with AES acceleration (STM32WBA, CC1352R, nRF52/53), AES-128-CCM is selectable and faster. The on-air header carries a 1-byte cipher ID so receivers can verify regardless. Replay protection comes from the AEAD nonce; tamper detection from the auth tag. See *Encryption* and *Key distribution* below.

### Stage 4 — custom board
Spin a custom **STM32WBA + 2× SX1262** board: native BLE for config/pairing, both radios on shared SPI for true diversity, smaller form factor for keytar-mount. Stage 4 also decides the band question: stay on 902–928 if it holds up live, or add a 470–510 MHz SKU/profile (SX1268) for users in noisier ISM environments.

### Beyond v2

- channel scan, frequency diversity, mobile configurator app
- experimental mono compressed audio (stretches the link, instructive)
- stereo IEM-class audio is a separate hardware tier (SDR / wideband transceiver + FPGA/SoC) — not buildable on SX126x

Audio is deferred until the MIDI link is solid live. SX1262-class radios are bandwidth-starved for stereo audio.

## Architecture

Layered so one app can build for many boards and radios:

```
app
  → profile           (which board + role + features + radio config)
    → core            (link, diversity arbitration, scheduler, config)
      → drivers       (radio, display, MIDI, input — vendor-neutral interfaces)
        → port        (STM32 HAL, TI SimpleLink later)
          → board     (pin map only)
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

Defaults by chip:

| Chip                   | AES-CCM hardware        | Default cipher                |
| ---------------------- | ----------------------- | ----------------------------- |
| STM32F103 (DX-LR30)    | none                    | ChaCha20-Poly1305 (software)  |
| STM32WBA5x             | yes (CRYP peripheral)   | AES-128-CCM (hardware)        |
| CC1352R                | yes (AES accelerator)   | AES-128-CCM (hardware)        |
| nRF52840 / nRF5340     | yes (CCM peripheral, native to BLE Link Layer) | AES-128-CCM (hardware) |

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

### 10. STM32 HAL + CMake first; Zephyr later
Faster low-level control for proving SX1262 latency. A Zephyr port is plausible later for broader board support, but it is not the entry path.

## Directory Structure

```
wireless-performer-fw/
├── apps/
│   └── midi_node/                          # TX/RX firmware role; behavior selected by profile
├── boards/                                 # hardware pin maps and capabilities
│   ├── dx_lr30/                            # v1 — STM32F103 + SX1262
│   ├── stm32_custom_oled_dual_radio/       # v2 — STM32WBA + 2× SX1262
│   └── ti_lpstk_cc1352r/                   # future — TI integrated MCU+radio
├── ports/                                  # platform glue
│   ├── stm32_hal/                          # primary
│   └── ti_simplelink/                      # reserved
├── profiles/                               # build configs: board + role + features + radio
│   ├── dx_lr30_tx_basic/                   # v1 transmitter
│   ├── dx_lr30_rx_basic/                   # v1 receiver
│   └── stm_oled_rx_dual_spi/               # v2 dual-radio diversity receiver
├── drivers/                                # vendor-neutral hardware interfaces
│   ├── radio/
│   │   ├── sx126x/                         # SX1262/SX1268
│   │   └── cc13xx/                         # future
│   ├── display/
│   │   └── ssd1306/                        # I²C OLED
│   ├── input/
│   │   └── buttons/                        # GPIO buttons / joystick
│   └── midi/
│       └── din_uart/                       # opto-isolated DIN MIDI
├── core/                                   # portable, hardware-agnostic
│   ├── link/                               # packetization, seq numbers, CRC, dedupe, watchdog
│   ├── diversity/                          # arbitration (UART-slave or dual-SPI)
│   ├── scheduler/                          # event/packet timing
│   └── config/                             # persisted settings, key store, active profile
├── protocols/                              # frozen on-air formats
│   └── midi_packet_v1/
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

Each `boards/<name>/board.yaml` documents that board's MCU, pin map, and radio wiring. Each `profiles/<name>/profile.yaml` documents the role, features, and radio config for that build target.

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
