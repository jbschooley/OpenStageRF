# Audio over OpenStageRF

Design notes and target architecture for adding wireless audio to
the link layer (Stage 4 in [ROADMAP.md](../ROADMAP.md)). Captures the
protocol reuse, what changes for audio, the hardware path, and how the
pro-grade IEM/wireless-mic systems achieve their latency numbers.

---

## Target hardware and capability — the goal

**The audio platform is nRF5340 (MCU) + a >1 Mbps sub-GHz radio
(CC1200 or Si4464, leaning Si4464) operating in the 470–608 MHz TV
White Space band under FCC Part 15 Subpart H, sold as a development
kit (no certification).** This is the specific combination this
document is written around. Everything below is reasoning about
why and how, but the architecture decision is settled.

What this combo can deliver:

| Spec | Target |
|---|---|
| Audio config | **24-bit / 48 kHz stereo** (transparent or near-transparent quality) |
| Codec | aptX HD (~576 kbps) or custom sub-band ADPCM (~700 kbps) |
| Wire bandwidth | **~750–900 kbps** of the radio's 1.0–1.25 Mbps ceiling (~60–80% utilization, depending on chip) |
| End-to-end latency (engineering target) | **~3–4 ms** |
| End-to-end latency (theoretical floor) | ~1.5–2 ms with hand-tuned everything |
| Encryption | AES-CTR + periodic Poly1305/GCM tag, hardware-accelerated on CC312 |
| Modulation | 4-FSK (highest practical for our chip class) |
| Transmission mode | Continuous streaming (eliminates per-frame preamble overhead) |
| Frequency band | 470–608 MHz (TV White Space) for 6 MHz channel width and lower noise floor |
| Regulatory path | **Sold as a dev kit / evaluation hardware, no FCC certification.** Not pursuing Part 15 TVWS certification for v1. |
| Spectrum per stereo IEM | ~700 kHz of TV channel space → 6–8 channels per 6 MHz TV slot |

Why this hardware specifically:

- **nRF5340** keeps the dual-core M33 + CC312 hardware crypto we already
  use for MIDI. No reason to switch — the MCU is not the bottleneck.
- **CC1200 or Si4464** — both support 4-FSK at ≥1 Mbps and continuous
  streaming, the two radio properties needed to hit ~3 ms total latency
  at 24/48 stereo. Trade-offs are summarized in the [v1 architecture
  section](#radio-selection-cc1200-vs-si4464) — Si4464 is the leading
  candidate for the dev-kit (built-in +20 dBm Tx, wider frequency range,
  cheaper); CC1200 is the alternative with ~9 dB better sensitivity at
  high bitrate.
- **4-FSK + continuous streaming** are *both* required, and they're
  independent properties of the radio config. 4-FSK gives the bits/Hz
  efficiency to fit 24/48 stereo in a ~700 kHz TV-channel slice;
  continuous streaming eliminates per-frame preamble/sync overhead
  that would otherwise dominate latency at 16-sample micro-frames.
- **470–608 MHz TVWS** gives 6 MHz channel widths with low ambient
  noise — five+ times the spectral budget of the 915 MHz ISM band.
- **Dev-kit / experimenter sale** sidesteps the $10–30k FCC TVWS
  certification cost. The product is sold as evaluation hardware
  (not as a finished consumer product), end users assume Part 15
  compliance responsibility, and the device does not need to
  implement the TVWS database client. **This is the v1 plan — we
  are not pursuing TVWS certification.** Door is left open for a
  certified product later if the project scales.

The rest of this document explains why each of those decisions was
made and how the pieces fit together.

---

## What carries over from the MIDI link layer

The wire framing and link-control machinery transfer almost completely.
A new packet body variant (`Body::AudioFrame(...)`) slots into the
existing protocol without disturbing MIDI traffic.

| Component | Reusable? |
|---|---|
| 11-byte header (`ver`, `key_fp`, `packet_seq`, `event_type`) | Yes — add `event_type = AudioFrame` |
| CRC + length framing | Yes |
| `boot_counter` ⊕ `session_seq` packing | Yes |
| AAD-then-AEAD layout (`encode` writes plaintext, caller encrypts) | Yes |
| AES-128-CCM (hardware ECB on nRF52840 / CC312 on nRF5340) | Yes |
| `PacketReplayWindow32` anti-replay | Yes |
| `Sx1262Radio` driver | Yes (probably retuned for higher bitrate) |
| `WatchdogTimer` (link loss detection) | Yes (with a tighter timeout) |
| `HeartbeatTimer` (silence frames) | Yes |
| `LinkSender` / `LinkReceiver` plumbing | Yes |

Roughly **70%** of `core/link` and `protocols/midi_packet_v1` is shared.

## What changes for audio — the retransmit philosophy flips

Audio is **stream-of-now**. A late packet is worthless: the listener
already heard whatever filler was substituted, and inserting the late
audio at its real timestamp would create a discontinuity (one-sample
waveform jump = audible click). So everything in the current link
that buys reliability via *time* (retransmits + delayed copies) is the
wrong tool.

### Removed for audio

| Feature | Why it goes |
|---|---|
| K=3 immediate retransmits | Each retransmit pushes packet 3 ahead by 3× wire time |
| +30 / +60 ms delayed copies | Past audio's relevance window (frames are ~1–3 ms apart) |
| `event_seq` body-level dedup | Audio frames identified by `packet_seq` alone |
| `MidiTxQueue` priority round-robin | Audio needs fixed-cadence delivery, not priority bursts |
| Heartbeat-state failsafe | No "stuck" state to recover, just dropouts to conceal |

### Added for audio

| Feature | Purpose |
|---|---|
| **Forward error correction (FEC)** | Recover bit errors *in place* with no round-trip cost |
| **Packet loss concealment (PLC)** | Smooth dropouts when FEC can't recover |
| **Jitter buffer** | Absorb inter-arrival variance (trade latency for smoothness) |
| **Fixed-cadence frame pump** | Synchronous TX timer, not opportunistic queue |
| **Tighter watchdog** (~30 ms) | Audio dropout > 50 ms is jarring |

## Should any redundancy stay?

**Only FEC. Retransmits go.** The reason: audio is sensitive to *late*
arrivals as well as *missing* ones. With MIDI events, "30 ms late" is
fine — the synth still plays the right note at the right pitch. With
audio, 30 ms late is *worse* than missing — concealment already filled
that frame, and inserting the late one creates a click.

- ✅ FEC (Reed-Solomon at 15–25% overhead corrects most short fades)
- ✅ Jitter buffer (handles timing variance without retransmit)
- ✅ PLC (graceful loss handling — repeat last frame, fade, LP from history)
- ❌ Retransmit (correlated late-arrival glitches)
- ❌ Delayed copies (same issue, longer tail)

## Audio quality vs. bandwidth tradeoff (on CC1200)

CC1200 at 4-FSK gives **1.25 Mbps gross**. The whole audio capability
analysis is sized to that ceiling.

### Codec options at 24/48 stereo

| Codec | Compressed rate | Algorithmic delay | Quality |
|---|---|---|---|
| Per-sample ADPCM (custom sub-band) | ~580–800 kbps (3:1–4:1) | <0.05 ms | Good — slight artifacts on harsh transients |
| **aptX HD** | **576 kbps (fixed)** | **~0.08 ms** | **Near-transparent. Used in flagship audiophile gear.** |
| LC3plus (low-delay mode) | ~700–900 kbps | ~5 ms (codec floor) | Excellent, but algorithmic delay eats latency budget |
| Opus CELT (low-delay) | 500–700 kbps | ~2.5 ms (codec floor) | Excellent, same delay caveat |
| Lossless (FLAC etc.) | ~1.3–1.5 Mbps | ~1 ms | Bit-perfect, but at the edge of CC1200's wire capacity |

For sub-3 ms total system latency, **only ADPCM and aptX HD are
viable** — LC3plus and Opus low-delay have hard algorithmic floors
above 2.5 ms that you can't engineer around. **aptX HD is the
preferred default** for the v1 audio target.

### Wire-utilization breakdown

CC1200's hard ceiling at 4-FSK is **1.25 Mbps gross**. Subtracting
overheads:

| Layer | Overhead |
|---|---|
| Compressed audio payload | (varies — see codec table) |
| AES-CTR encryption | 0% (stream cipher, no expansion) |
| Periodic GCM/Poly1305 MAC tag | ~1% (a tag every ~16 ms) |
| Forward error correction (Reed-Solomon ~25%) | 25% |
| Sync words / framing for stream resync | ~3% |

| Codec choice | Wire utilization |
|---|---|
| **aptX HD (576 kbps payload)** | **~750 kbps** — 60% of ceiling |
| Custom ADPCM at 700 kbps | ~910 kbps — 73% of ceiling |
| LC3plus at 800 kbps | ~1.04 Mbps — 83% of ceiling |
| Lossless ~1.4 Mbps | ~1.82 Mbps — **exceeds ceiling** ❌ |

Headline: **750–900 kbps wire** for high-quality 24/48 stereo with
40–25% headroom for FEC, resync, and the rare retransmit margin (e.g.
duplicating a sync header). Lossless 24/48 stereo is borderline —
would need either a weaker FEC, a >1.7:1 lossless codec, or a faster
radio class (SDR).

### Lower-end option: 24/48 mono fallback

If for any reason you're stuck with the existing SX126x-class radio
(300 kbps GFSK ceiling), the math collapses to mono:

| Sample rate / bits | Raw rate | ADPCM 4:1 | Use case |
|---|---|---|---|
| 16 kHz × 16-bit mono | 256 kbps | 64 kbps | telephony+, voice monitor only |
| 24 kHz × 16-bit mono | 384 kbps | 96 kbps | broadcast vocals, clean instruments |
| 32 kHz × 16-bit mono | 512 kbps | 128 kbps | musical monitoring, no harsh transients |
| 48 kHz × 16-bit mono | 768 kbps | 192 kbps | full-band mono, fits with FEC overhead |

This is a Phase A bootstrap target only — get audio working on the
existing radio while waiting for CC1200 hardware. It is **not** the
v1 audio product.

## End-to-end latency on nRF5340 + CC1200 — three engineering tiers

The same hardware can be tuned across three latency tiers depending
on how aggressive the engineering effort is. All three deliver 24/48
stereo with aptX HD or comparable codec quality.

### Tier 1 (theoretical floor, hand-tuned everything): ~1.5–2 ms

| Stage | Time |
|---|---|
| ADC capture (8 samples @ 48 kHz) | 0.17 ms |
| Per-sample ADPCM or aptX HD encode | <0.05 ms |
| Packet assembly + AES-CTR + FEC encode | 0.03 ms |
| SPI to CC1200 (DMA, 8 MHz) | 0.03 ms |
| CC1200 continuous TX of compressed bits | 0.05 ms (8 samples × 8 bits × 2 ch ÷ 1.25 Mbps) |
| RX SPI + AES verify + FEC decode | 0.08 ms |
| Codec decode | <0.05 ms |
| 1-frame jitter buffer (8 samples) | 0.17 ms |
| DAC output (8 samples) | 0.17 ms |
| Embassy task scheduling + DMA setup overhead | 0.3–0.5 ms total |
| **Total** | **~1.5–2 ms** |

Requires:
- Custom CC1200 driver tuned for continuous streaming (no per-packet
  preamble between frames)
- Audio pipeline pinned to highest executor priority
- DMA throughout — no CPU copies in the audio path
- No `await` points except at sample-clock boundaries
- Hardware AES via CC312 (sub-µs per block)
- Net core (M33 #2) handling SPI to radio in parallel with app core
  handling I²S/codec

This is what you hit if you treat the audio path like a hard real-time
system. Roughly matches Sennheiser EW-DX (1.9 ms) and Shure Axient
Digital (2.0 ms).

### Tier 2 (realistic engineering target): ~3–4 ms

Same hardware, same continuous streaming, slightly more conservative:
- 16-sample frames (0.33 ms each) instead of 8 — larger SPI batches,
  less overhead per byte
- 1- or 2-frame jitter buffer
- Standard FEC (Reed-Solomon 255/223 or similar)
- Standard Embassy task setup without obsessive priority pinning
- Per-sample ADPCM or aptX HD with normal block boundaries

This is the v1 target. Pro digital wireless mics live in the
1.9–3.0 ms range; we'd be roughly matching their numbers with
off-the-shelf parts and Rust + Embassy. Plenty of margin for
implementation realities.

### Tier 3 (comfortable, less engineering effort): ~5 ms

- 32-sample frames (0.67 ms each)
- 2-frame jitter buffer
- Packet mode with sync resync every few frames (not pure continuous)
- Standard Embassy task scheduling
- aptX HD or simpler ADPCM

Ship-this-and-call-it-good version. Still beats most consumer wireless
audio products, plenty of margin for RF anomalies, simpler to debug.

### Caveats that bite if you push hard

- **CC1200 internal pipeline latency**: datasheet specifies the demod
  chain delay; verify it matches the ~50–100 µs assumed above. Some
  sub-GHz radios have hidden ~500 µs of pipeline that you can't
  shorten. Worth a careful read before committing to Tier 1.
- **Sample-clock drift**: TX ADC and RX DAC clocks aren't locked.
  Even with 20 ppm crystals, ~1 sample of drift accumulates over a
  few minutes. Either implement async sample-rate conversion (adds
  ~0.5 ms) or recover sample clock from packet timing (more
  engineering, no added latency).
- **FEC decode time on M33**: software Reed-Solomon decode for one
  block at 1+ Mbps is ~0.3–0.6 ms on Cortex-M33 at 128 MHz. Already
  in the budget above. If faster needed, use systematic codes that
  skip full decode unless errors are detected.
- **RF fading without diversity**: audio has no retransmit, so a
  single deep fade = audible glitch. Stage 2's dual-radio diversity
  is **more important for audio than for MIDI**. Plan to add a
  second CC1200 for diversity from the start of Stage 4 (selection
  or maximal-ratio combining at the bit level).
- **Battery life**: continuous TX at 1.25 Mbps with CC1200 is
  ~30 mA at 13 dBm output. Plus nRF5340 active core ~5–10 mA.
  ~40 mA total ≈ ~10 hours from a 400 mAh LiPo. Probably fine for a
  stage performance, factor into power budget.
- **CC1200 max output power**: 13 dBm without external PA. For Part
  15 TVWS Mode II portable (20 dBm cap) you'd want a small external
  PA to reach the limit, which adds BOM, current draw, and noise
  floor concerns.

### Latency tier sanity check vs. the pro tier

| Reference system | Latency | What we'd match it with |
|---|---|---|
| Sennheiser EW-DX, Shure Axient Digital | 1.9–2.0 ms | Tier 1 (theoretical floor) |
| Shure ULX-D, Sennheiser Digital 6000 | 2.9–3.0 ms | Tier 2 (v1 engineering target) |
| Most consumer 2.4 GHz wireless audio | 5–15 ms | Tier 3 |

Sub-1 ms latency is **only achievable in analog FM** systems
(Sennheiser PSM 1000, EW IEM G4) — those don't have the codec or
modem pipeline at all. No digital system at any price hits sub-1 ms.

---

## How do Sennheiser, Shure, Lectrosonics get sub-3 ms?

Pro digital wireless systems target ≤ 2 ms for high-quality stereo
audio. Their numbers, for reference:

| System | Claimed latency | Audio quality |
|---|---|---|
| Shure Axient Digital (mic) | 2.0 ms | 24-bit 48 kHz |
| Shure ULX-D (mic) | 2.9 ms | 24-bit 48 kHz |
| Sennheiser EW-DX (mic) | 1.9 ms | 24-bit 48 kHz |
| Sennheiser Digital 6000 (mic) | 3.0 ms | 24-bit 48 kHz |
| Lectrosonics Digital Hybrid | 1.5 ms | 24-bit 48 kHz |
| Sennheiser PSM 1000 (IEM) | analog FM, ≪1 ms | analog companded |
| Sennheiser EW IEM G4 | analog FM, ≪1 ms | analog companded |

(The IEM systems above are still mostly **analog FM** — companded but
not digitized. Pure digital IEMs from any major brand land in the
2–5 ms range.)

These numbers are not magic — they come from a stack of advantages we
mostly can't replicate cheaply:

### 1. Dedicated silicon, not a general-purpose MCU

Pro systems use custom ASICs or large FPGAs implementing the
encoder/decoder/modem as a single deterministic pipeline. A sample
hits the input and exits the output on a known cycle count. There's
no executor, no task switching, no SPI handshake to a separate radio
chip — just a hard pipeline running at the audio sample rate.

We're on a Cortex-M4F at 64 MHz running Embassy + Rust + a separate
SX126x chip over SPI. Each of those layers adds tens of µs to hundreds
of µs of variable latency. Embassy's task-switch overhead alone is
~10–20 µs; SPI transactions to the radio are ~50 µs each.

**Their advantage: ~1–2 ms saved purely from hardware integration.**
We can't beat this without spinning custom silicon or moving to an
FPGA-based design. The roadmap's Stage 4 nRF5340 + dedicated codec
helps but doesn't close the gap.

### 2. Massive RF channel bandwidth

Pro UHF systems run on **dedicated 200 kHz – 1 MHz wide channels** in
the 470–960 MHz TV bands under FCC Part 74 (or Part 15 in some
configurations), often at higher transmit power. With more spectral
bandwidth, you push the same audio bitrate through with shorter
on-air time.

Examples:
- Shure Axient: ~200 kHz channel, but uses high SNR + custom modulation
- Sennheiser Digital 6000: 600 kHz / 1.2 MHz wide
- Lectrosonics: 200 kHz with hybrid FM-digital

We have ~500 kHz max GFSK channel at 915 MHz under Part 15 (US ISM),
limited to 27 dBm with hopping or 21 dBm without. Our wire bitrate
ceiling is ~1 Mbps with LR1262, vs. their effective 2–4 Mbps with
proprietary modulations on wider channels.

**Their advantage: ~0.5–1 ms saved by shorter on-air time.**

### 3. Sample-by-sample streaming, not frame-based packetization

This is the biggest *algorithmic* trick and the one we could partially
adopt.

We're planning to capture 64 samples (1.3 ms @ 48 kHz), encode the
block, packetize, transmit. That's **2.6 ms of unavoidable latency
just from frame buffering** (one frame to fill on TX side, one to
play on RX side).

Pro systems use **continuous streaming**:
- The ADC samples are fed into the encoder one at a time
- The encoder emits compressed bits continuously, not block-by-block
- Bits flow into the modulator as they're produced
- The decoder runs in lockstep on the receive side
- Audio comes out the DAC ~1 sample after corresponding TX sample plus on-air time

This works because the codecs are **predictive sample-by-sample**
(adaptive ADPCM with sample-rate state, not block-mode), and the
modem is a **continuous bitstream** (not packetized framing with
preamble/sync overhead per block).

Adapting this would mean:
- Use ADPCM with per-sample state (4-bit nibbles output per audio sample)
- Group samples into very small "micro-frames" (8–16 samples = 0.17–0.33 ms)
- Skip the per-packet preamble/sync overhead by running continuous TX bursts
- Synchronize sample clocks across the link via packet_seq + bitstream timing recovery

**Their advantage from streaming: ~2–3 ms saved over our 64-sample
block design.** Cutting our block size from 64 → 16 would close a
fraction of this (~0.6 ms saved on each side) but the per-packet radio
overhead becomes proportionally worse.

### 4. Sample-clock locking across the link

Pro systems lock the RX DAC clock to the TX ADC clock via the
recovered bit clock. No async sample-rate conversion, no clock-drift
buffer. Their jitter buffer is microseconds, not milliseconds.

Without clock locking we need a jitter buffer that absorbs both wire
jitter *and* slow clock drift between TX and RX MCUs. Even with
crystal oscillators on both ends, ~20 ppm drift means a buffer that
underruns or overruns over minutes if not actively managed. Most
implementations use a small jitter buffer (1–3 frames) plus an
asynchronous SRC (sample rate converter) to gradually realign.

**Their advantage: ~2–3 ms saved on jitter buffer.** Adding bit-clock
recovery to our SX126x-based design is non-trivial — the radio's
demodulator does some of this internally, but exposing the recovered
clock to the MCU at sub-µs precision is not how the SX126x is wired.

### 5. RF-level antenna diversity

Pro receivers use **two complete RF chains** (two antennas, two LNAs,
two demods) and combine the analog signals **before** digitization
— either selection diversity (pick the stronger) or maximal ratio
combining (sum weighted by SNR). This happens at IF or baseband, not
at the packet layer, so there's zero added latency and no packet
reordering.

Our planned diversity (Stage 2 in ROADMAP) is at the packet layer:
two radios, two SPI streams, app-level merge. That introduces tens
of microseconds of latency mismatch between paths and the reorder
hazard we wrote up in the FAQ. Useful for redundancy, but doesn't
buy latency.

### 6. Proprietary codecs designed for the link

Sennheiser's Digital Audio Codec, Shure's Acoustic Engineering codec,
aptX Live, etc. — these are tuned for:
- Sample-by-sample latency (no block buffering)
- Built-in error resilience (single-bit errors don't cascade)
- Specific-rate operation (e.g. exactly 24-bit @ 48 kHz mono fitting
  exactly into a fixed bits-per-sample encoding)

Open codecs in this regime: **LC3** (used in LE Audio, ~3–7 ms total
codec latency), **Opus** at 2.5 ms low-delay mode (still ~5 ms
algorithmic), **CELT** (predecessor to Opus, ~2.5 ms). For our
purposes, **per-sample ADPCM** is the simplest match — slightly worse
quality than Opus/LC3 at same bitrate, but ~1 sample of algorithmic
delay vs. their multi-millisecond block buffering.

### 7. They're FCC Part 74 licensed (in the US)

Pro wireless mics on UHF TV bands operate under Part 74 with higher
transmit power (up to 250 mW for handhelds) and exclusive channels.
This translates to better link margin → fewer retransmits / FEC
needed → simpler protocol → lower latency.

Our v1 path is **Part 15 TVWS in the same UHF band** — license-free
operation in unused TV channels at 100 mW EIRP with the database
client requirement. Same 6 MHz channel widths as Part 74 mics, lower
power, slightly more procedural complexity (database query) but no
licensing fees. Good RF environment compared to the 915 MHz ISM band's
shared spectrum.

## How that maps to our hardware target

The gap between pro systems and what nRF5340 + CC1200 can do:

| Their advantage | Approximate cost | Closeable on our hardware? |
|---|---|---|
| Dedicated silicon vs. MCU + separate radio | ~1–2 ms | Partly — net core handles SPI in parallel |
| Sample-by-sample streaming vs. block packetization | ~2–3 ms | **Yes** — CC1200 supports continuous streaming natively |
| Sample-clock locking vs. jitter buffer | ~2–3 ms | Partly — recover sample clock from packet timing |
| RF-level antenna diversity | latency-neutral, reliability win | Yes — Stage 2 dual-CC1200 |
| Proprietary low-delay codecs | ~1 ms | Yes — aptX HD or per-sample ADPCM gets us most of the way |
| Higher TX power on exclusive channels | reliability margin | Partial — Part 15 TVWS allows 100 mW with database |

**Conclusion: ~3–4 ms is engineering-realistic; ~1.5–2 ms is
theoretical floor.** Detailed budget per tier is in the [End-to-end
latency](#end-to-end-latency-on-nrf5340--cc1200--three-engineering-tiers)
section above.

## Audio hardware roadmap

### Bitrate math for the audio targets

| Audio config | Raw rate | ADPCM 4:1 | aptX / LC3 (~2.5:1) | Wire need (with 25% FEC) |
|---|---|---|---|---|
| 24-bit / 48 kHz mono | 1.15 Mbps | 290 kbps | 460 kbps | 360 / 575 kbps |
| 24-bit / 48 kHz stereo | 2.30 Mbps | 580 kbps | 920 kbps | **720 / 1.15 Mbps** |
| Lossless 24/48 stereo | 2.30 Mbps | (~1.5 Mbps with FLAC) | n/a | 1.9 Mbps |

So **24/48 stereo with ADPCM compression needs ~720 kbps wire**.
Higher-quality codec (aptX HD / LC3) is ~1.15 Mbps. Lossless pushes
past 2 Mbps.

### LR1262 vs SX1262 — same chip

`LR1262` is Heltec's module branding for an SX1262 die with their
reference RF front-end. Per the Semtech SX126x datasheet, GFSK
datarate is specified as **0.6 to 300 kbps** — there is no secret
"LR1262 unlocks 1 Mbps" mode. Same silicon, same ceiling.

Where confusion arises: Semtech does have higher-bitrate parts, but
they are **different chips in different bands**:

| Part | Band | Max GFSK |
|---|---|---|
| SX1262 / LR1262 | 150–960 MHz (incl. 470) | 300 kbps |
| SX1268 | 410–525 MHz (China/India variant) | 300 kbps |
| **SX1280 / SX1281** | **2.4 GHz only** | **2 Mbps** |
| LR1110 / LR1120 / LR1121 | sub-GHz multiband | 300 kbps GFSK + LR-FHSS extras |

**Conclusion: for sub-GHz GFSK above 300 kbps, the SX126x family is
not an option.**

### Sub-GHz radio options for >300 kbps

| Chip | Freq range | Max bitrate sub-GHz | Continuous streaming? | Rust/Embassy support |
|---|---|---|---|---|
| SX1262 / LR1262 | 150–960 MHz | 300 kbps | bursty (not real continuous) | mature |
| Si4463 (Silabs) | 142–1050 MHz | ~1 Mbps | partial | weak |
| **Si4464 (Silabs)** | **119–1050 MHz** | **1 Mbps GFSK** | **direct mode** | **weak — driver to write** |
| Si4468 (Silabs) | 119–1050 MHz | 1 Mbps (extended-range tuning) | direct mode | weak |
| **CC1200 (TI)** | **164–960 MHz** | **1.25 Mbps (4-FSK)** | **yes, native** | **weak — driver to write** |
| CC1101 (TI, older) | 300–928 MHz | ~500 kbps | partial | weak |
| AX5043 (Onsemi) | 27–1050 MHz | 350 kbps | yes | weak |
| SDR (AD9361, LimeSDR LMS7002M) | 70 MHz–6 GHz | arbitrary | yes (everything is) | none — build it |

For 24/48 stereo at ~720 kbps wire, the realistic options are
**CC1200**, **Si4464**, or an **SDR**. SX126x can't reach the
required bitrate at all.

### Si4464 vs Si4468 vs CC1200 — picking among the >1 Mbps candidates

**Si4464 vs Si4468** are both Silabs EZRadioPRO sub-GHz transceivers
covering the same 119–1050 MHz range with the same modulations
(2-FSK / 4-FSK / GFSK / GMSK / OOK) and same +20 dBm Tx. They differ
in operating-point optimization:

| | Si4464 | Si4468 |
|---|---|---|
| Marketing positioning | High-performance | Extended range |
| Sensitivity at 100 bps | -124 dBm | **-126 dBm** (better) |
| Sensitivity at 1 Mbps | **~-86 dBm** (better) | ~-83 dBm |
| Best at | High-bitrate, balanced | Long-range slow telemetry |

**For our high-bitrate audio use case, Si4464 is the right one in
that family.** Si4468 is for sensor-network-style low-rate operation.

**Si4464 vs CC1200** at the bitrate we care about:

| Spec | Si4464 | CC1200 |
|---|---|---|
| Frequency coverage | **119–1050 MHz** (covers VHF too) | 164–960 MHz |
| Max bitrate | 1 Mbps GFSK | **1.25 Mbps 4-FSK** |
| **Sensitivity at ~1 Mbps** | -86 dBm | **-95 dBm** (~9 dB better) |
| Continuous streaming | Direct mode (legacy, fiddly) | **Native, designed for it** |
| Spectral efficiency at 1 Mbps | 1 bit/symbol GFSK → ~1.2 MHz channel | **2 bits/symbol 4-FSK → ~700 kHz channel** |
| Output power | **+20 dBm built-in** | +13 dBm (external PA for higher) |
| Cost | ~$2–3 in volume | ~$4 in volume |

**CC1200 wins on the technical axes that matter for our audio use:**
better sensitivity at high bitrate (more fade margin in shared TVWS
spectrum), more spectrally efficient (more channels per TV slot),
cleaner continuous-streaming support.

**Si4464 wins on:** wider frequency range (could reach VHF Band III
if you ever wanted), built-in +20 dBm Tx (no external PA), slightly
cheaper.

For the v1 audio platform, **CC1200 is still the chosen radio**. The
~9 dB sensitivity advantage at 1 Mbps roughly translates to 3× more
range or much better fade margin against TVWS-adjacent broadcast
interference. Si4464 is a defensible alternative if CC1200 sourcing
ever becomes an issue or if VHF flexibility becomes desirable later.

### Spectrum per stereo IEM channel

How much TVWS spectrum a single stereo IEM stream consumes,
depending on radio and modulation:

| Radio + modulation | Wire bitrate | Channel allocation needed | Stereo IEMs per 6 MHz TV slot |
|---|---|---|---|
| **CC1200 4-FSK at 1.25 Mbps** | up to 1.25 Mbps | **~700 kHz** | **~6–8 channels** |
| Si4464 GFSK at 1 Mbps | up to 1 Mbps | ~1.2 MHz | ~4 channels |
| Si4464 4-FSK at 1 Mbps | up to 1 Mbps | ~600 kHz | ~9 channels (at lower bitrate) |
| Pro reference: Sennheiser Digital 6000 | 24/48 | 300 kHz | 18 (custom modem) |
| Pro reference: Shure Axient Digital | 24/48 | ~500 kHz | 12 |

**A single stereo IEM channel uses ~700 kHz of spectrum on CC1200.**
In a 6 MHz TV slot that's 6–8 simultaneous stereo IEM users — more
than enough for any band. A 5-piece group all on stereo IEMs uses
~3.5 MHz of one TV channel. For a typical small stage, **one TVWS
channel is sufficient**.

We're competitive with pro gear at the per-channel level (200 kHz to
1 MHz is typical pro-tier), just not as spectrally tight as the
proprietary modems used in flagship Sennheiser/Shure systems.

### How CC1200 hits 1.25 Mbps in sub-GHz

A few generations newer than SX126x:

- **4-FSK modulation** — 2 bits/symbol vs GFSK's 1 bit/symbol, doubles
  raw bitrate at the same RF symbol rate
- **Wider configurable RX/TX bandwidth filters** (up to ~1.6 MHz) so
  the receiver can see the wider modulation
- **Better synthesizer phase noise** at high symbol rates
- **Per-bit FIFO streaming mode** with no per-packet preamble required

SX126x targets long-range/low-power LoRa; CC1200 targets industrial
telemetry and digital wireless audio. Different design centers.

### MCU question — nRF5340 still adequate?

Yes. The MCU is not the bottleneck for 24/48 stereo audio:

- ADPCM encode at 48 kHz × 2 ch ≈ ~96k samples/sec × 0.2 µs/sample ≈
  2% CPU
- AES-CCM via CC312 is ~5 µs per packet
- I²S peripheral does audio I/O via DMA (no CPU intervention)
- Embassy task-switch is ~10 µs, easily managed with proper priority

What the nRF5340 has that matters:

- ✅ I²S master/slave with DMA
- ✅ CC312 hardware AES-CCM (sub-µs encrypt/decrypt for our packet sizes)
- ✅ Dual-core M33 (network core handles radio, app core handles audio)
- ✅ TIMER + PPI for sample-clock generation
- ❌ No hardware audio codec engine (software ADPCM is fine)
- ❌ No sub-GHz radio integrated (need external chip — CC1200 or LR1262)

Alternative MCUs worth considering only for specific needs:

| MCU | Why pick it | Why not |
|---|---|---|
| **STM32H753** (480 MHz M7) | Lots of audio peripherals (SAI, DFSDM); mature Rust ecosystem; FPU + DSP instructions | Bigger, hotter; still external radio |
| **STM32WL55** (sub-GHz integrated) | Cortex-M4 + SX126x-equivalent radio in one chip | Same 300 kbps GFSK ceiling; AES-128 only (no AES-256 hardware) |
| **nRF54H20** | Newest Nordic; dual M33 + audio codec engine + DSP | Embassy support just emerging; 2.4 GHz radio only — still need external sub-GHz |
| **i.MX RT1170** (1 GHz M7) | Massive headroom for any codec | Overkill; Embassy support emerging; bigger BOM |

I would not move off nRF5340 unless a specific peripheral need
appears. CC312 + I²S DMA + dual M33 at 128 MHz is plenty.

### Integrated chip (radio + MCU on one die) for sub-GHz

| Chip | Verdict |
|---|---|
| **STM32WL55** | Best Rust/Embassy story for integrated sub-GHz. Cortex-M4 + SX126x-equivalent radio. Same 300 kbps ceiling — does not solve the audio bandwidth problem. AES-128 hardware only (Stage 4 wants AES-256, which would be software). |
| **CC1352R7** | TI integrated. Better radio (1.25 Mbps 4-FSK). Worse Rust ecosystem. |
| **Silicon Labs EFR32FG** | Sub-GHz + M33. Very limited Rust support. |

For audio specifically, integration loses to discrete: you want a
radio that can do 1+ Mbps with 4-FSK and continuous streaming
(CC1200), and there's no integrated M33-class MCU with that radio
that has Rust/Embassy support. So discrete is the practical answer.

### Modulation vs transmission mode — orthogonal axes

These are two **independent** properties of the radio config. A
common confusion is to lump them together; they're not the same
thing and they affect different parts of the system.

| Axis | What it is | Affects… | Examples |
|---|---|---|---|
| **Modulation** | How bits map onto the RF signal | Spectral efficiency (bits/Hz) and SNR requirement | OOK, 2-FSK, GFSK, **4-FSK**, MSK, OQPSK, QAM-16, OFDM |
| **Transmission mode** | How bytes are framed and fed to the radio | Per-frame overhead (preamble + sync + dead air) → framing latency | Packet mode (with preamble/sync), continuous streaming (always-TX) |

You can combine them freely: packet GFSK, continuous GFSK, packet
4-FSK, continuous 4-FSK, packet OFDM (WiFi), continuous OFDM
(DVB-T), etc. **Continuous streaming is not "faster than 4-FSK" —
they're orthogonal.**

### What we actually need for audio

For the audio target (24/48 stereo, ~900 kbps wire, ~3 ms latency),
we need both:

- **4-FSK** for spectral efficiency: 2 bits/symbol vs GFSK's 1
  bit/symbol. Lets us fit the wire bitrate in a ~700 kHz channel
  instead of ~1.2 MHz, so more simultaneous channels per TV slot.
- **Continuous streaming** for framing latency: eliminates per-frame
  preamble + sync + dead-air overhead. At 16-sample micro-frames
  (~0.33 ms each), per-frame overhead would otherwise dominate.

Modulation choice does **not** directly affect latency — it affects
bitrate-per-Hz and SNR requirement. The latency advantage pro digital
wireless systems have over us is driven by their custom silicon,
sample-by-sample codecs, and sample-clock locking — not by their
choice of QPSK over 4-FSK.

### What's beyond 4-FSK in our chip class

For a Rust + Embassy project on off-the-shelf sub-GHz silicon:

| Modulation | Bits/symbol | Available on |
|---|---|---|
| OOK | 1 | most chips |
| 2-FSK / GFSK / GMSK / MSK | 1 | SX126x, CC1200, Si4464, Si4468, CC1101, AX5043 |
| **4-FSK** | **2** | **CC1200, Si4464, Si4468 — practical ceiling for our class** |
| OQPSK / QPSK | 2 | TI CC1352 (802.15.4g sub-GHz mode), AT86RF215; weak Rust support |
| 8-PSK / 16-QAM / OFDM | 3+ | SDR only (AD9361, LimeSDR) or proprietary pro silicon |

**4-FSK is the highest practical modulation order for our project.**
Pro digital wireless mics use proprietary QPSK-derivative
modulations for *spectral efficiency* (more channels per TV slot),
not for lower latency. Stepping beyond 4-FSK means SDR or custom
silicon, both of which are out of scope for v1.

### Why packet mode hurts at small frames

Even with 4-FSK, packet-mode framing kills your latency budget at
small frame sizes.

8-sample audio frames at 48 kHz = 0.17 ms of audio per frame. Add 50
µs of preamble + sync + length per frame and you've got 50/170 ≈ 29%
overhead just for framing. Plus inter-packet TX→RX switch dead time
(~100 µs on SX126x-class radios) — at 6000 frames/sec that's 600 ms/sec
of dead air, more than the wire can carry.

Solutions:
- **Larger frames** (32 or 64 samples) — accept ~1.3 ms of buffering
- **Continuous TX with embedded sync words** — radio stays TX-on, you
  embed alignment markers in the data stream, RX uses them to recover
  frame boundaries. Eliminates per-packet preamble. This is what pro
  digital wireless actually does.

#### Which radios do real continuous streaming?

- **SX126x family**: has a "continuous mode" but intended for short
  bursts, not multi-second streams. Not designed for this case.
- **CC1200**: proper "sync detect on rising edge" framing that works
  without per-packet preamble. Datasheet documents this case.
- **AX5043**: explicit continuous-mode support, well documented.
- **SDR (AD9361, etc.)**: trivial — *everything* is continuous.

CC1200 is the most realistic in-class option. AD9361 is the unbounded
option.

### Encryption with continuous streaming

Fully compatible. The trick is to use **stream-cipher modes**:

- **AES-CTR** (Counter Mode): encrypts byte-by-byte using a keystream
  derived from `AES_K(counter || nonce)`. No packet boundary needed.
  Same throughput as ECB, sub-µs on CC312.
- **AES-GCM** (CTR + Galois MAC): encrypt in CTR, MAC over the stream.
  Periodic MAC verification (e.g. every 16 ms = 768 samples = 96 stream
  blocks).
- **ChaCha20-Poly1305**: alternative if AES-GCM isn't supported in
  hardware. Similar properties, software-friendly on M4F.

For our case:
- **Encryption side**: CTR mode runs in lockstep with audio
  production, ~0 latency.
- **Authentication side**: MAC computed over each "audio block" (e.g.
  each 1 ms of audio). MAC verification on RX side is one block of
  latency — so 1 ms latency added.

You don't lose authentication just because you went to continuous
streaming. You just check the tag at logical block boundaries instead
of packet boundaries.

**Caveat:** if a stream cipher's keystream gets out of sync (because
some bytes were dropped), you can't recover without a resync point.
So continuous-streaming protocols typically embed periodic resync
markers (e.g. every 16 ms, send a sync word + counter snapshot). On
RX, if the MAC fails over a block, you wait for next sync, then
resume. Audio is silent or concealed during that window.

### Phased path to the 24/48 stereo @ 3 ms target on nRF5340 + CC1200

The end goal is fixed (nRF5340 + CC1200, 24/48 stereo, ~3 ms latency,
TVWS). The phasing is about ordering the engineering work so each
phase produces a working, testable system.

#### Phase A — bootstrap on existing radio class (mono, ~7 ms)

Cheapest path that validates the whole audio pipeline before the
CC1200 driver lands:

- nRF5340 + existing SX1262/LR1262 (already on hand from MIDI work)
- 300 kbps GFSK, packet mode (existing radio driver)
- 64-sample frames at 48 kHz (1.3 ms each)
- ADPCM 4:1 of **24/48 mono** → ~290 kbps wire (fits 300 kbps
  ceiling)
- AES-CCM per packet (CC312)
- 2-frame jitter buffer
- Target: ~7–8 ms end-to-end, **mono only**

The point of Phase A is **not the eventual product** — it's to
validate the audio path, codec, AES integration, jitter buffer, and
DAC end-to-end with hardware we already have. Discovers integration
bugs while the CC1200 driver work is happening in parallel.

#### Phase B — write the CC1200 driver and port to it

- Implement Rust + Embassy driver for CC1200 (~weeks of work; the
  register interface is well documented but no existing crate)
- Bring up CC1200 in continuous-streaming mode at 4-FSK 1.25 Mbps
- Port the Phase A audio pipeline to it
- Mono still — focus on validating CC1200 modem behavior, FEC, and
  continuous-streaming framing

#### Phase C — go to stereo + tighten latency to ~3 ms

The actual target architecture:

- 24/48 **stereo** with aptX HD or sub-band ADPCM (~580–700 kbps
  payload)
- 16-sample micro-frames in continuous streaming
- 1-frame jitter buffer with sample-clock recovery from packet timing
- Aggressive Embassy task scheduling (highest priority audio task,
  net core handling SPI to CC1200 in parallel)
- Reed-Solomon 25% FEC
- Target: **~3–4 ms end-to-end**

#### Phase D (optional, future) — diversity and Tier 1 latency

- Add a second CC1200 for RF-level antenna diversity (selection or
  combining)
- Push frame size to 8 samples + heroic optimization for Tier 1
  (~1.5–2 ms) latency
- Optional sample-clock locking via packet-timing PLL (replaces
  jitter buffer)

#### Why not consider an SDR alternative?

The AD9361 / LimeSDR path is mentioned for completeness but is
**not** the v1 plan. It's ~6 months of additional engineering for a
custom FPGA baseband, custom modulation stack, and no off-the-shelf
Rust drivers. Pays off only if you eventually commercialize and want
to match pro-tier specs (sub-2 ms with lossless audio). Not the
right place to start.

### Frequency band — 470–608 MHz (TV White Space) vs 915 MHz ISM

A 6 MHz TV channel comfortably hosts a 1+ Mbps modem with margin.
That is the **biggest practical win** over Part 15 sub-GHz at 915
MHz, where you're limited to ~500 kHz channels in most configurations.
For high-bitrate audio, TVWS is the right band.

#### How TVWS Part 15 actually works (Subpart H: 47 CFR §15.701-15.717)

1. **Geolocation required.** Device must know its position to ±50 m
   — typically GPS, or for fixed installations, manually-entered
   coordinates verified at install time.
2. **Database query mandatory.** Before transmitting, device queries
   an FCC-approved TVWS database (Spectrum Bridge, iconectiv, etc.)
   over the internet. Database returns the list of TV channels unused
   (or available at restricted power) at the device's location.
3. **Transmit only on cleared channels.** Device picks a channel from
   the available list. Periodically re-queries (FCC requires at least
   every 24 hours, plus on relocation).
4. **Power limits:**
   - **Mode II portable** (handheld, battery): up to **100 mW EIRP**
     (20 dBm), within a 6 MHz channel
   - **Fixed**: up to **4 W EIRP** (36 dBm)
   - **Adjacent channel** (1st-adjacent to active TV): reduced to
     40 mW (16 dBm) for portables
5. **Channel bandwidth:** must fit within a single 6 MHz TV channel.
6. **RF sensing optional** for modern geolocation-only devices.

#### Engineering work added by TVWS

- HTTPS client to talk to the TVWS database (REST API, several free
  options including Spectrum Bridge and Microsoft's free dev API)
- Geolocation source — GPS hardware on device, or manual entry at
  setup
- Periodic re-query logic (≥ every 24 hours; on power-on; if location
  changes by >100 m)
- Channel-changing logic in the radio driver — hop to a new channel
  from the available list when the current one becomes unavailable
- A "fail-safe silent mode" if database is unreachable and last
  query is older than 24 hours
- Roughly **1–2 weeks of engineering** for a clean implementation
  plus testing

#### What the FCC actually charges

**No recurring or per-unit fee from the FCC for Part 15 TVWS
operation.** No spectrum lease, no royalties, no annual filing. Costs
land entirely in the *certification* you do once before selling
devices:

| Cost item | Approximate $ | Who you pay |
|---|---|---|
| FCC Part 15 Subpart H equipment authorization (TCB filing) | $1,200 – $5,000 | TCB (UL, Bureau Veritas, etc.) |
| Compliance testing (lab) | $8,000 – $25,000 | TCB lab |
| TVWS database registration (one-time per device class) | typically free or low | FCC-approved DB operators |
| FCC certification application fee | ~$565 (current) | FCC directly |
| **Total for a small-volume product** | **~$10,000 – $30,000** | |

For comparison: Part 74 modular licensing (the wireless-mic
professional path) is more expensive ($30k+) and requires
coordinating with broadcasters. Part 15 TVWS is cheaper but more
procedurally complex (the database client requirement adds
engineering work).

#### The dev-kit path — skip certification entirely

Two FCC-acceptable shortcuts that avoid the $10–30k:

- **Personal use under §15.23**: building 1–5 units for personal use
  doesn't require certification or database compliance, only general
  Part 15 emissions limits (which any reasonable design meets).
- **Dev kit / experimenter sale**: ship as "evaluation hardware, end
  user responsible for compliance." Most sub-GHz dev boards are sold
  this way (Arduino LoRa boards, Heltec boards, RAK Wireless boards).
  No certification, no database client required on the device — the
  user is responsible for operating it within Part 15 limits.
  Enforcement against personal experimenter use is essentially
  nonexistent.

| Path | Up-front cost | Per-unit cost | Selling? |
|---|---|---|---|
| Personal use (under §15.23) | ~$0 | ~$0 | No — personal use only |
| **Dev kit, no cert** | **~$0** | **~$0** | **Sell as bare hardware to experimenters / makers** |
| Part 15 TVWS certification | $10–30k | none | Sell as a finished product to end users |

### v1 chosen path: dev kit, no TVWS certification

OpenStageRF v1 is **sold as a development kit / evaluation hardware,
not pursuing Part 15 TVWS certification.** Concrete implications:

- The product ships as a programmable dev board with documentation,
  not as a sealed consumer electronics product.
- Marketed and labeled as "evaluation hardware — end user is
  responsible for ensuring Part 15 compliance in their jurisdiction."
- Standard practice for sub-GHz dev boards (Heltec, RAK Wireless,
  Adafruit Feather radio boards, Pycom, etc. all ship this way).
- No TVWS database client required on the device — the user
  consults the database manually if they're operating in TVWS.
- No $10–30k certification cost.
- No spectrum lease or FCC fee.

If the project later scales to a certified consumer product, the
upgrade path is clean: integrate a TVWS database client, do the
certification work once, and sell the certified version alongside
the dev kit. The hardware design and firmware architecture don't
change — only the regulatory wrapper.

### v1 audio architecture — settled

The v1 audio configuration:

| Decision | Choice |
|---|---|
| **MCU** | nRF5340 (kept from MIDI work) |
| **Radio** | **CC1200 or Si4464** (both viable; trade-off below) |
| **Band** | 470–608 MHz TVWS (Part 15 Subpart H) |
| **Modulation** | 4-FSK |
| **Transmission mode** | Continuous streaming with embedded resync words |
| **Codec** | aptX HD (576 kbps) — primary; per-sample sub-band ADPCM as fallback |
| **Channels / sample rate / bit depth** | Stereo / 48 kHz / 24-bit |
| **Crypto** | AES-CTR + periodic Poly1305/GCM tag, CC312 hardware accelerated |
| **Frame size** | 16 samples (~0.33 ms) at the v1 latency target |
| **Jitter buffer** | 1 frame |
| **FEC** | Reed-Solomon ~25% overhead |
| **Engineering latency target** | **3–4 ms** end-to-end |
| **Theoretical floor** | ~1.5–2 ms with Tier 1 optimization |
| **Regulatory path** | **Dev-kit / evaluation hardware. Not pursuing FCC TVWS certification for v1.** Hardware design and firmware are forward-compatible with a future certified version. |

These are not open questions — the architecture decision is made.
Open *implementation* decisions remain (specific aptX licensing /
custom ADPCM, exact FEC code parameters, sample-clock recovery
strategy, and the final radio pick) but the hardware and protocol
topology is settled.

#### Radio selection: CC1200 vs Si4464

Both reach the latency target at 24/48 stereo. The trade-off:

| Factor | CC1200 favors | Si4464 favors |
|---|---|---|
| Sensitivity at 1+ Mbps | ✅ ~9 dB better fade margin | |
| Spectral efficiency | ✅ 4-FSK at 1.25 Mbps in ~700 kHz | |
| Continuous streaming polish | ✅ native, well-documented | |
| Output power (built-in) | | ✅ +20 dBm (no external PA) |
| Frequency range | | ✅ 119–1050 MHz (vs 164–960 MHz) |
| BOM cost | | ✅ ~$2–3 vs ~$4 |
| Driver-writing burden | tie (both need fresh Rust + Embassy driver) | tie |
| Frequency coverage **for TVWS specifically** | tie (both cover 470–698 MHz UHF and 174–216 MHz VHF Band III) | tie |

**The leading candidate is Si4464** for the v1 dev-kit because:
- Built-in +20 dBm Tx removes the external PA, simplifying the BOM
  and the dev-board layout
- Wider frequency range gives users more experimentation
  flexibility without changing hardware
- ~9 dB sensitivity disadvantage is real but partially offsetable
  by Si4464's 7 dB higher Tx, leaving a ~2 dB net link-budget gap
  — manageable for typical performer-to-receiver distances

CC1200 remains a defensible alternative if the sensitivity gap
proves problematic in real RF environments or if pro-tier
spectral efficiency becomes a priority later. The protocol stack
and driver structure are nearly identical, so swapping radios in
firmware is a contained change.

## Implementation decisions still to make

When Stage 4 begins, the remaining tactical questions:

1. **aptX HD license vs. custom sub-band ADPCM** — aptX HD requires
   a Qualcomm license fee. Custom ADPCM avoids licensing but quality
   is slightly lower on transients. Most likely: prototype with
   custom ADPCM, evaluate quality, decide whether to license aptX
   HD before commercial shipping.
2. **Exact FEC parameters** — Reed-Solomon (255, 223) is the obvious
   default at 25% overhead; we could go (255, 239) at 12.5% overhead
   for less wire cost if RF margin proves generous.
3. **Sample-clock approach** — free-run with async SRC at RX (~0.5
   ms latency hit, simpler), or PLL recovery from packet timing
   (zero latency hit, more engineering)
4. **Diversity from day one or as Phase D?** — second CC1200 doubles
   BOM and current draw but is the main reliability lever for audio.
   Likely yes from day one for any product that ships.
5. **Frame size for v1: 8 vs 16 vs 32 samples** — depends on whether
   we hit Tier 1 (~2 ms) or Tier 2 (~3–4 ms) targets in development.
   Start at 16, downsize if optimization permits.

## Things to keep in mind today (Stage 1–3)

A few decisions in the current code that make audio integration easier
later:

1. **Keep `event_type` extensible.** Already done — `Body<'a>` has
   `Unknown(u8)` for forward-compat. Audio adds `Body::AudioFrame(...)`
   without breaking older receivers.
2. **Keep the AAD-then-cipher boundary clean.** Already done —
   `proto::encode` writes plaintext, caller does AEAD. Audio frames
   with FEC will encrypt the same way (FEC bytes appended after the
   AEAD tag).
3. **Don't tangle `MidiTxQueue` priority logic with the radio loop.**
   Mostly clean today. Audio TX will be a parallel task that emits
   frames on a fixed timer, independent of the queue's priority/credit
   machinery.
4. **Reserve a body-format / event_type range for "no-retransmit
   payloads".** An audio frame doesn't need `event_seq` — it
   identifies itself by `packet_seq` alone. Worth knowing this is
   coming when we add `event_type = AudioFrame`.

Nothing in the current direction has painted us into a corner — the
retransmit logic is contained to `MidiTxQueue` and the receiver's
`EventReplayWindow16`, both bypassable per packet variant. The wire
format itself is general enough.
