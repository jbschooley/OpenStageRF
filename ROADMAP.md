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

## Audio capability tiers

Wireless audio breaks into two architecturally distinct tiers reachable from this project, plus one explicitly out of scope. **2.4 GHz / BLE Audio is not a goal of this project** — sub-GHz spectrum is a hard requirement for stage propagation.

### Tier 2 — Pro wireless audio (SLX-D / ULX-D / EW-D class) — *active roadmap, lands on v3 hardware*

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
| `audio_sbadpcm_stereo` *(roadmap, modest engineering)* | stereo IEM, ~70 dB at 400 kbps | ~400 kbps | ~250 kHz | sub-band ADPCM, small frames (1–2 ms) | 1–2 ms | full-band stereo at ~70 dB dynamic range. Beats SLX-D on latency, slightly behind on perceptual quality. **1–3 person-months of focused codec work.** See *Codec engineering* below. |
| `audio_transparent_stereo` *(roadmap, real codec project)* | stereo IEM, true SLX-D parity | ~400 kbps | ~250 kHz | adaptive bit-allocation w/ noise shaping (CELT-class) | 2–3 ms | transparent stereo at sub-3 ms. **No good open implementation today — 6–12+ person-months by an audio DSP engineer.** Real gap in the open ecosystem, strong contribution target. |

**True diversity at full audio quality:** every profile in the table that uses a single radio (everything except `audio_pcm_dual_radio_stereo_48k`) runs on v3 with **both radios tuned to the same channel** and the existing diversity arbitration logic. Audio fits in a single radio's bandwidth, so the second radio is "free" for diversity. This is the same pattern pro IEM bodypacks use (mono RF link carrying stereo-encoded audio, diversity on that single channel).

**System architecture:** balanced mic preamp / instrument input / I²S codec (e.g. TLV320AIC3204) → MCU codec encode (or pass-through PCM) → AEAD encrypt → packetize → radio. Receiver inverts to I²S DAC → headphone amp. Small jitter buffer (~1–2 audio frames), no retry on audio packets, FEC optional.

**Scope in this project:** v3 board target + audio driver crate (`drivers/audio/i2s/`) + codec crates per-profile + `radio-si446x` driver. The link/diversity/key-store/crypto layers carry over from v2 unchanged. **This is the most underserved tier in pro wireless audio today** — there's no open-source SLX-D / EW-D equivalent, and an open uncompressed-PCM mono mic in 902 MHz with true diversity could measurably outperform mid-tier pro gear.

### Tier 3 — Pro top-tier (Spectera class) — *sibling project, not buildable here*

**Target:** uncompressed PCM stereo, sub-ms latency, multi-channel multiplexing in 6–8 MHz UHF channels.
**Hardware:** wideband SDR transceiver (AD9361 / AD9364 / ADRV9002) + **FPGA** (Zynq-class). The FPGA is not optional — AD9361's parallel LVDS interface runs at hundreds of MHz and aggregates ~3 Gbps of digital data; no microcontroller has a peripheral that can drive it in real time at full sample rate. Pluto SDR (~$200) pairs AD9363 with Zynq-7010 for exactly this reason.
**Scope:** separate repository. Different silicon, different firmware (likely Rust-on-Linux orchestrating an FPGA bitstream), different compliance program. Protocol/key/diversity concepts from this project may inform it; code does not transfer.

## Codec engineering — sub-band ADPCM

Design notes for the `audio_sbadpcm_stereo` and `audio_transparent_stereo` profiles.

**Realistic target:** transparent or near-transparent stereo at ~400 kbps, sub-3 ms total codec latency, full 20 kHz response. Competitive with SLX-D / EW-D class systems. Beating Spectera (which uses uncompressed PCM modes in 6–8 MHz channels) is out of scope — different problem.

**Reference points:** Bluetooth's SBC codec is the closest open relative (sub-band ADPCM, 4 or 8 bands, ~328 kbps stereo) but it's tuned for ~10–20 ms latency. CELT (the predecessor to Opus) achieves transparent quality at low bitrates with 2.5 ms frames but uses MDCT, not ADPCM, and adds frame-buffering latency. The proprietary pro codecs (SLX-D, EW-D) are almost certainly sub-band ADPCM with adaptive bit allocation tuned for sub-3 ms — same general structure as SBC but with different latency/quality knobs.

### Architecture

1. **QMF (Quadrature Mirror Filter) analysis filterbank** splitting 48 kHz audio into 4 sub-bands of 6 kHz each (or 8 sub-bands of 3 kHz). 4-band is the safer starting point — lower latency, simpler. Polyphase implementation with a short prototype filter (~16–32 taps) keeps the analysis/synthesis delay under 0.5 ms each way.

2. **Per-band ADPCM quantizers** with adaptive step-size prediction. Each band gets its own ADPCM encoder; the step size adapts to recent signal magnitude in that band. Different bands need different bit depths — low bands (most musical energy) get more bits, high bands get fewer.

3. **Adaptive bit allocation** across bands. Compute a quick band-energy estimate per frame, allocate a bit budget proportional to perceptual importance (lower bands weighted higher). Reallocate every 1–2 ms. Total budget: ~400 kbps stereo / 48000 frames/s = ~8.3 bits per stereo sample, distributed across bands.

4. **Per-band predictive coding.** Within each band, use a short LPC predictor (order 2–4) to remove redundancy before quantization. This is what gets you closer to transparent — much of the bit savings vs raw ADPCM comes from prediction.

5. **Noise shaping.** Spectrally shape the quantization noise so it falls under the masking curve of the audio. Even a simple first-order shaper improves perceived quality significantly. This is where pro codecs do real work — psychoacoustic tuning that takes time to get right.

6. **Joint stereo coding.** Mid/side encoding for stereo (sum and difference channels rather than L/R) typically gives 10–20% bit savings on real music because the side signal has less energy than either L or R alone. Add a simple intensity coding fallback for high frequencies where stereo localization matters less.

7. **Very small frames.** 1–2 ms frames (48–96 samples per channel). Each frame self-contained — no inter-frame dependencies that would force the decoder to wait for the next packet. This is the key constraint that distinguishes a real-time codec from SBC/Opus.

8. **No bit-reservoir, no lookahead.** Both add latency. Every frame is a fixed-size packet that the radio can transmit independently. If a frame arrives corrupt, the decoder substitutes silence or repeats the last frame; no error propagation.

### Latency budget

| Stage | Time |
|---|---|
| QMF analysis (4-band, 16-tap) | ~0.3 ms |
| Frame buffering (96 samples) | ~2.0 ms |
| ADPCM encode + bit allocation | ~0.05 ms |
| RF transit + jitter buffer | ~0.5 ms |
| ADPCM decode | ~0.05 ms |
| QMF synthesis | ~0.3 ms |
| **Total** | **~3.2 ms** |

Just over 3 ms. To get under 3 ms cleanly, drop frame size to 48 samples (1 ms) at the cost of slightly worse compression efficiency, or use a faster filterbank (8-tap, less stopband rejection but lower latency).

### Where the pro codecs likely beat this

- Better psychoacoustic tuning — masking models honed over years on real audio material
- Smarter bit allocation — possibly using look-ahead in a different way (e.g. transient detection)
- Better stereo coding — possibly perceptual joint coding more sophisticated than mid/side
- Hand-optimized for specific source material types (vocals vs full mix vs dense electronic)

A first version gets to "close to transparent on most program material with audible artifacts on hard cases (transients, dense polyphonic mixes, high-frequency detail)." Iterating to "transparent on everything" is the codec-research grind.

### Implementation approach in Rust

```
crates/codec_sbadpcm/
├── src/
│   ├── lib.rs
│   ├── qmf.rs          # polyphase 4-band analysis/synthesis
│   ├── adpcm.rs        # per-band quantizer with adaptive step
│   ├── lpc.rs          # short-order linear predictor
│   ├── allocator.rs    # adaptive bit allocation
│   ├── shaper.rs       # noise shaping filter
│   ├── joint.rs        # mid/side stereo coding
│   └── frame.rs        # frame format + sync words + bit-packing
└── tests/
    ├── reference_vectors/   # known-good encode/decode round-trips
    └── perceptual/          # PEAQ-style automated quality measurement
```

`no_std` compatible, fixed-point arithmetic where possible (Cortex-M33 has a single-precision FPU but fixed-point is faster for filterbank stages), no heap allocation. Should fit comfortably in nRF5340's 512 KB RAM with room for jitter buffers.

### Effort tiers

- **Working baseline (better than IMA, worse than SLX-D):** 4–6 weeks. QMF + uniform ADPCM + simple bit allocation. Audible improvement over IMA, demonstrates the architecture.
- **Competitive with SLX-D on most material:** 3–6 months. Add LPC prediction, noise shaping, joint stereo, perceptual tuning iterations.
- **Indistinguishable from SLX-D / EW-D:** 6–12+ months of focused codec work. Where psychoacoustic refinement, edge-case handling, and listening tests dominate the timeline.

### Validation strategy

PEAQ (Perceptual Evaluation of Audio Quality, ITU-R BS.1387) gives an objective score, but ABX listening tests against reference-quality audio on real IEMs are the only way to know if it's transparent. Test material should include the hard cases: solo piano with reverb tails (decay artifacts), close-mic'd vocals (masking failures), dense rock mixes (allocation thrashing), classical strings (harmonic detail).

The architecture is well-understood — every piece above has decades of literature behind it. The hard part isn't the design; it's the tuning iterations and listening-test grind that pro codec teams spend most of their time on.

## Potential future platforms

These were considered earlier and may return as community ports, but are not on the active roadmap:

- **STM32WBA5x** — capable hardware, but BLE-in-Rust support is currently rough (no mature stack equivalent to `nrf-softdevice`). Consider revisiting if TrouBLE or vendor Rust BLE matures.
- **TI CC1352R / CC1354R10** — strong hardware (CC1354R10 reaches 4 Mbps sub-GHz, more headroom than SLX-D-class targets need). No embassy HAL today. Either a community-built embassy HAL or a parallel Zephyr+C implementation could enable it.
- **TI CC1200** — narrowband sub-GHz transceiver supporting MSK (QPSK-adjacent) and 4-FSK, slightly higher spectral efficiency and adjacent-channel rejection than Si4463. Unlike TI's integrated wireless MCUs (CC1352R, CC1354R10), CC1200 is a standalone SPI-controlled radio in the same architectural class as SX1262 and Si4463 — **a `radio-cc1200` Rust crate built on `embedded-hal` traits is fully viable, no Zephyr+C required.** Driver effort comparable to writing the Si4463 crate (~3000–4000 lines, register map from datasheet, configuration sequences from SmartRF Studio). Tradeoff vs Si4463: marginally better RF performance, fewer pre-certified module options.
- **CML Microcircuits CMX940** — native π/4-DQPSK, exactly the modulation pro mid-tier wireless audio likely uses. Would close the spectral-efficiency gap to SLX-D/EW-D entirely. Commercial-radio silicon: no Rust ecosystem, no maker distribution, datasheet/sourcing friction. Not realistic for this project; mentioned for completeness because someone will eventually ask.

### What "porting to C" means

If a contributor wants to support TI silicon (or anything else without an embassy HAL) via Zephyr+C, that's not a mechanical port of the Rust code — it's a **parallel C implementation that shares the on-air protocol spec and crypto test vectors but no source code**. Concretely, the C side would re-implement `core/link/`, `protocols/`, `crypto/`, and the radio driver from scratch in C against Zephyr's APIs. Devices running the Rust implementation and the Zephyr+C implementation interoperate over the air (same packet format, same AEAD, same key model); they do not share code. Analogous to BlueZ vs BlueDroid for Bluetooth — same protocol, separate codebases. Maintenance burden is real: every protocol change happens twice. Worth doing only if a contributor wants to own the C variant.
