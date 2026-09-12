<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
# The MIDI link under 47 CFR 15.247 (902–928 MHz)

Status 2026-09-11: the `fcc247` waveform exists in firmware
(`LinkConfig::fcc247_915`, profiles `ui_tx_915_fcc` / `ui_rx_915_fcc` /
`ui_rx_diversity_915_fcc`); its bandwidth numbers are **simulation only**
until measured on the bench with a spectrum view.

## Why the bench waveform is not legal to sell

The MIDI link as deployed is 2-GFSK at 300 kb/s with 50 kHz deviation (index
0.33) at up to +22 dBm, on a 1 MHz grid in 903–926 MHz or 471–509 MHz, no
hopping.

| Band | Rule | What it allows | The bench waveform |
|---|---|---|---|
| 902–928 MHz | 15.247 | up to 1 W conducted for frequency hoppers, or digital modulation with a **6 dB bandwidth ≥ 500 kHz** (PSD ≤ 8 dBm per 3 kHz); antennas ≤ 6 dBi | ~175 kHz at 6 dB, no hopping: does not qualify |
| 902–928 MHz | 15.249 | 50 mV/m at 3 m ≈ 0.75 mW EIRP (−1 dBm) | what the bench waveform actually falls under: +22 dBm is ~23 dB over |
| 470–608 MHz | 15.236 | wireless microphones (devices that turn sound into a transmitted audio signal), 50 mW EIRP, ≤ 200 kHz; cue and control only as a function of such a device | a MIDI-only device is not a wireless microphone; no unlicensed rule covers it |
| 470–608 MHz | Part 74 | licensed low power auxiliary stations, cue and control included | licensees are broadcasters, productions and venues, not a product's customers |
| any | 2.803 | evaluation kits to developers only, labelled as not approved | where the T114 units are today |

Sources: 15.236, 15.247, 15.249 as published on law.cornell.edu (read
2026-09-11); the definitions in 15.236 and 74.801.

## The 15.247 shape

Digital modulation needs a 6 dB bandwidth of at least 500 kHz (ANSI C63.10:
100 kHz RBW, peak detector).  The SX1262's widest GFSK receive filter is
467 kHz, so the transmitted signal is made just wide enough to qualify and
the receiver accepts some clipping at the edges.  Simulated with the ideal
BT 0.5 Gaussian pulse (`tools/wmas_mask/mask.py`'s baseband, 100 kHz RBW,
peak detector):

| bit rate | deviation | index | 6 dB BW | 20 dB BW | power inside ±233 kHz |
|---:|---:|---:|---:|---:|---:|
| 300 kb/s | 50 kHz | 0.33 | 175 kHz | 325 kHz | −0.0 dB (the bench waveform) |
| 300 kb/s | 150 kHz | 1.0 | 375 kHz | 625 kHz | −0.1 dB |
| 200 kb/s | 200 kHz | 2.0 | 475 kHz | 625 kHz | −0.4 dB |
| 150 kb/s | 225 kHz | 3.0 | 525 kHz | 625 kHz | −0.9 dB |
| **150 kb/s** | **250 kHz** | **3.3** | **575 kHz** | **675 kHz** | **−1.7 dB** (`fcc247`) |
| 100 kb/s | 250 kHz | 5.0 | 575 kHz | 675 kHz | −1.5 dB |

`fcc247` is 150 kb/s at 250 kHz deviation: 75 kHz over the rule so a real PA
and filter have room, at ~1.7 dB of edge loss in the receiver.  225 kHz is
the fallback if the receive loss measures worse than the simulation.  The
PSD limit is not a concern: +22 dBm spread over ~600 kHz is about −1 dBm
per 3 kHz.  The bit rate halves the link's air throughput; MIDI needs a
fraction of it and the scheduler derives its timing from the configured
rate.

Range does not suffer: 15.247 allows up to 1 W where the bench waveform
was limited to 0.75 mW, and the receive noise bandwidth is unchanged
(the 467 kHz filter was already in use).  Index 3.3 is a better operating
point for the SX1262's demodulator than 0.33.  Path loss at 915 MHz is
~5 dB worse than at 490 MHz.

**Bench checks before anything is sent to a lab:** 6 dB and 20 dB
bandwidth on the TinySA (100 kHz RBW, max hold); receive sensitivity
`fcc247` vs `bench` with the attenuator; the SX1262's automatic frequency
correction range at this deviation.

## What certification takes

1. **Hardware that is a product.** The Heltec T114 has no FCC ID I could
   find; it stays an evaluation kit.  Either our own board with an SX1262
   and a full 15.247 test suite (conducted power, 6 dB bandwidth, PSD,
   band edges, radiated spurious to 10 GHz, Part 15B; ~$8–15k at a US lab)
   or a pre-certified module (Ebyte E22-900M22S, Seeed Wio-SX1262 claim
   FCC grants) used unmodified with an approved antenna **in the modes the
   grant covers** — those are usually LoRa settings, so wide GFSK likely
   needs its own test; the grant conditions on the FCC ID database decide.
2. **RF exposure.** KDB 447498: a body-worn transmitter is excluded from
   SAR testing when (power mW ÷ separation mm) × √f(GHz) ≤ 3.0.  At
   158 mW and 5 mm that is 30: SAR testing (~$3–6k) or a body-worn power
   cap near 12 dBm (16 mW).  A receive-only unit needs neither.
3. **Antenna** permanently attached or on a non-standard connector
   (15.203); RP-SMA is the usual answer.
4. **Firmware bounds.** Every reachable setting is what gets tested: a US
   build offers only 902–928 plans (`build.rs` enforces this for
   `fcc247`) and caps power at the certified level.  Later RF-parameter
   changes are a permissive-change filing.
5. **Paperwork.** FCC grantee code, TCB review and grant, FCC ID on the
   label, the 15.19 / 15.21 / 15.105 statements in the manual.

Canada follows on the same test data (RSS-247).  Europe is 863–870 MHz
under EN 300 220 with its own power and duty-cycle limits, a separate
exercise.

## 470 MHz for a standalone MIDI device

Three routes exist; none is "certify the bench waveform".

- **Be a wireless microphone system.**  15.236 covers devices that turn
  sound into a transmitted audio signal, and such a device may carry cue
  and control.  A MIDI pack that also carries a mono audio channel (a
  talkback or click input; `sb144m`-class codecs fit 200 kHz with room
  for MIDI) is such a device in the rules' own terms, at 50 mW EIRP in a
  200 kHz channel on the wireless-microphone coordination grid — exactly
  the coordination story 470 is wanted for.  Whether instrument control
  data counts as "cue and control" is a KDB inquiry question; file it
  with the R14 questions.  This makes the MIDI product a small audio
  product, on the audio codec and link work.
- **TV white space (Part 15 Subpart H).**  Unlicensed data devices in
  the TV bands with geolocation and a certified database that says which
  channels are vacant — the "no TV channel here" argument made
  official.  A rack with GPS and Ethernet would be the Mode II master,
  packs Mode I clients.  Costs: the database protocol and its security
  requirements, a portable device's power is 100 mW EIRP but the spectral
  density limit (2.2 dBm per 100 kHz conducted) leaves a ~600 kHz signal
  around 8 mW, and the set of active database administrators needs
  verifying before designing around it.
- **Part 74 licence** for eligible customers (broadcast, film and TV
  production, large venues): a certified LPAS device sold to licensees.
  Real for a niche, not a consumer path.

The plan of record for MIDI at 470 is the first route; 902–928 under
15.247 is the standalone MIDI product.
