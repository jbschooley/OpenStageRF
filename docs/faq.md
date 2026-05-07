# OpenStageRF FAQ

Reference for design decisions and recurring questions about the link layer,
MIDI traffic, failsafes, and hardware. Curated from in-the-trenches debugging
of milestone 4 link-layer hardware testing (rx5–rx12).

---

## Table of contents

- [Link layer & retransmits](#link-layer--retransmits)
- [Failsafes & stuck-note recovery](#failsafes--stuck-note-recovery)
- [Diversity RX](#diversity-rx)
- [Capacity, latency, and saturation math](#capacity-latency-and-saturation-math)
- [Synthetic scenarios](#synthetic-scenarios)
- [MIDI hardware (FeatherWing & DIN)](#midi-hardware-featherwing--din)
- [T114 board](#t114-board)

---

## Link layer & retransmits

### Why K=3 immediate retransmits?

Each channel-voice push enters the queue with `credits=3`, meaning the same
event's bytes ride three consecutive packets out the radio. At observed
per-second loss rates of 5–20%, the probability that all three copies are
lost is `loss^3` — drops a 20% per-event loss to a 0.8% per-event miss
rate. K=3 is the smallest count that keeps misses comfortably below 1%
under realistic RF conditions.

### Why also +30 ms and +60 ms delayed copies?

K=3 retransmits ship within ~3 ms of the original push. A burst of
interference longer than ~10 ms (e.g. WiFi traffic colliding on the
adjacent channel, microwave oven, fluorescent ballast) can take out all
three copies. The +30 ms / +60 ms time-spread copies survive bursts up
to that long. With all five copies, the all-lost probability for a 20%
loss window is `0.2^5 ≈ 0.03%`.

The +30 ms / +60 ms numbers were chosen as: clear of typical "long"
bursts (WiFi airtime fairness gaps are ~20 ms, microwave oven duty
cycles are ~10 ms), but not so long that they become audible if the
note is short.

### Why do delayed copies share the original's `event_seq`?

So the receiver's `EventReplayWindow16` deduplicates them. Each logical
event fires at the sink exactly once: whichever copy arrives first wins,
the rest are dropped before reaching the sink.

### Do delayed copies of CCs/PC/PB really need this?

Yes — and applying the delayed-copy tier to **all channel-voice messages**
(not just NoteOn/Off) is essentially free thanks to dedup cancellation:

- For **continuous controllers** (mod wheel, pitch bend, expression,
  channel pressure): `dedup_for_incoming` cancels stale delayed copies
  when a newer same-(ch, ctrl) value pushes. So a saturated sweep at
  200 ev/s never actually emits the +30/+60 ms copies — they're wiped
  by the next sweep value. Wire cost stays at K=3 only.
- For **one-off latched-state messages** (Program Change, Bank Select
  MSB/LSB, sustain pedal toggles, volume/pan changes): the delayed
  copies fire on schedule. These are exactly the messages where a
  single drop would put the synth in the wrong state indefinitely
  (wrong patch, wrong bank, sustain stuck on). 30× reliability boost
  is essential.

The change was: remove the `is_note_event` gate in
`MidiTxQueue::push_channel_voice`. Wire-bandwidth penalty for realistic
mixed traffic is ~15%; for sweep-heavy traffic it's ~0%.

### Are real-time messages (`0xF8`–`0xFF`) covered too?

No. Timing Clock fires 24× per quarter note and Active Sensing every
~300 ms — they're frequent, miss-tolerant, and adding K=3+2 retransmits
would block channel-voice traffic for several extra packets every
real-time message. They get K=1 with `REALTIME_PRIORITY` so they
preempt other traffic but don't bloat the wire.

### How does dedup cancel "stale" pending copies?

`dedup_for_incoming` (in `midi_tx.rs`) walks the entire queue when a new
event is pushed and removes superseded entries. The match rules:

- NoteOn cancels pending NoteOff for same (ch, note); NoteOff cancels
  pending NoteOn — eliminates ghost re-trigger
- New CC#X on ch Y cancels pending older CC#X on ch Y — high-rate
  sweeps wipe stale intermediate values
- New PC / Channel Pressure / Pitch Bend cancels pending same-status on
  same channel
- Poly Aftertouch cancels by (ch, note)

It scans both eligible and not-yet-eligible entries (i.e. the +30 / +60 ms
copies still in the queue waiting their turn), so a NoteOff cleanly wipes
the pending delayed NoteOn copies before they ever hit the wire.

### Why "wire-order" matters at the receiver

`LinkReceiver::process` assumes the caller delivers wire packets in send
order (`packet_seq` strictly ascending). On a single-antenna RX this is
trivial: serial wire + instantaneous air propagation = packets land in
the order they were sent.

If the wire-order invariant is violated (e.g. via independent-demod
diversity merging two streams without sequencing), the worst-case
failure mode is a **truncated note**, not a stuck one. The +30 ms
delayed NoteOn copy could in principle be reordered after the NoteOff
on a badly-engineered diversity setup with >20 ms of latency mismatch,
but the heartbeat-state failsafe converts that into a brief click
(10–20 ms blip) rather than a stuck note. See the
[Diversity RX](#diversity-rx) section.

---

## Failsafes & stuck-note recovery

### Three layers of stuck-note protection

1. **In-band reliability**: K=3 + 2 delayed copies = 5-copy delivery.
   Drops the per-event miss rate to ~0.03% at 20% loss windows.
2. **Heartbeat-state failsafe** (in-link): 2-byte big-endian
   active-channel mask in every heartbeat body lets RX detect "TX
   thinks ch X is silent but I have notes pressed there" and emit
   selective NoteOffs to recover. Time-thresholded; see below.
3. **Watchdog all-notes-off** (link-down): if no packet for 200 ms, RX
   emits CC#123 on all 16 channels and resets local state.

### Why selective NoteOff and not CC#123 for the heartbeat-state failsafe?

CC#123 ("All Notes Off") is a blunt instrument that some synths
implement as immediate gate-off — cutting any release tail still
sounding from a properly-delivered NoteOff a moment earlier. For a
single missed NoteOff on, say, note 60, releasing only that one note
preserves the release tails of unrelated chord notes that already had
their NoteOffs honored.

The receiver tracks `PressedNotes: [u128; 16]` (a per-channel bitmap of
notes currently believed pressed). When recovery fires for ch X, it
iterates the bitmap and emits one `[0x80|X, note, 0]` per pressed bit.

### Why is recovery **time-based** (100 ms) rather than count-based?

Original implementation required `STABLE_HEARTBEATS=2` consecutive
heartbeats showing the divergence, which at 12.5 ms heartbeat cadence
is only ~25 ms. That's shorter than the +60 ms delayed NoteOff arrival
ceiling, so the failsafe could "race" a legitimate-but-delayed NoteOff
and fire ~2 ms before the real NoteOff arrived (observed in rx11).

Fix: per-channel `divergence_since: [Option<Instant>; 16]`. Recovery
fires only after the divergence has persisted **continuously for ≥
100 ms** — comfortably past the +60 ms delayed-copy ceiling, with
40 ms of slack for heartbeat jitter and queue/SPI latency.

The fix is in `apps/link_bench/src/lib.rs` (`STUCK_NOTE_MIN_DIVERGENCE_MS`).
Validated on rx12: 0 false positives across 110 s of walking with the
antenna in a 3D printer.

### Why is divergence tracked **per channel** instead of globally?

A brief divergence on ch 0 shouldn't reset a longer-running real
divergence on ch 5. Per-channel timestamps let each channel's
recovery clock run independently — the failsafe is correctly scoped to
the channel that's actually wrong.

### What about the residual TX-side race?

TX updates `ChannelNoteCounts` (the source of the heartbeat mask) at
the moment a NoteOff is *pushed* to the queue, not when it goes on the
wire. So a heartbeat carrying mask=0 for ch X can leave TX before the
NoteOff packet does. The 100 ms threshold makes this race invisible to
the RX failsafe.

A future-cleaner fix would be to update `ChannelNoteCounts` only when
the NoteOff is actually *popped from the queue* (i.e. on the wire).
Filed as a future improvement; current behaviour is correct because
the time threshold absorbs the race.

### Will the failsafe fire on every link drop?

No. The watchdog (200 ms with no packet) runs separately from
heartbeat-state recovery. When the watchdog fires it emits CC#123 on
all 16 channels (blunt, but appropriate for total link loss) and
resets `PressedNotes`. The per-channel divergence timers also reset on
LINK LOST, so a stale mask from before the drop doesn't pollute
recovery once the link comes back.

### What does the listener actually hear in worst cases?

| Failure mode | What's audible |
|---|---|
| Single packet of K=3 + 2 delayed lost (one in-the-middle copy survives) | Latency excursion of +30 or +60 ms — single-frame perceptual blip |
| All 5 copies lost on a NoteOn (silent) | Note simply doesn't play; silent absence |
| All 5 copies lost on a NoteOff, failsafe catches it after 100 ms | Note plays an extra ~100 ms longer than intended (stuck-note avoided) |
| Diversity reorder violating wire-order invariant on a delayed NoteOn | Note plays for ~10–20 ms then cuts (truncated note, not stuck) |
| Total link loss > 200 ms (watchdog) | Held notes cut via CC#123 on all channels |

No path produces a "ghost re-trigger" or a stuck note that requires
manual intervention.

---

## Diversity RX

### How can diversity even reorder packets?

Wire transmission is serial — pkt N at t1, pkt N+1 at t2>t1, no
overlap. Air propagation is irrelevant (~3.3 µs/m). So both antennas
physically *receive* in send order.

Reorder happens **after** the antennas, in the receive chain:

```
antenna A → demod → SPI read → push to merge queue → app
antenna B → demod → SPI read → push to merge queue → app
```

If A's pipeline has latency T_A and B's has T_B, packets effectively
arrive at the app at `wire_time + T_path`. Reorder occurs when the
**latency mismatch** between paths exceeds the **wire gap** between
two packets:

```
T_A − T_B > wire_gap
```

### What's the "wire gap" for our packets?

| Packet pair | Wire gap |
|---|---|
| K=3 retransmits of same event (back-to-back) | ~1 ms |
| Last K=3 retransmit → first NoteOff retransmit | ~47 ms |
| **+30 ms delayed NoteOn copy → matching NoteOff** | **20 ms** |

So the latency-mismatch threshold for a problematic reorder is the
shortest wire gap between two events whose order matters: **20 ms**.

### Which diversity setups exceed 20 ms mismatch?

| Setup | Typical mismatch | Reorder hazard? |
|---|---|---|
| Two SPI radios on same MCU, single executor | <100 µs | No |
| Slave MCU with SPI relay (~4 MHz) | <1 ms | No |
| Slave MCU with UART relay (115200 baud) | ~2 ms | No |
| Slave MCU with slow UART (9600 baud) or queued/buffered relay | 5–50 ms | **Possible** |

Conclusion: any sensible diversity implementation (SPI relay, single-MCU
dual-radio) is safe. Slow-UART slave-MCU diversity is the only realistic
setup that creates the hazard, and the heartbeat-state failsafe converts
its worst-case behaviour from "stuck note" to "truncated note" anyway.

### Without delayed NoteOn copies, would this matter?

Essentially no. Without the +30 ms / +60 ms NoteOn copies, the wire gap
between K=3 NoteOns and the NoteOff is ~47 ms — no realistic diversity
setup exceeds that. The delayed-copy feature trades a 30 ms diversity
margin for a 17× improvement in NoteOn miss rate (1.4% → 0.08% at
24% loss). Worthwhile.

---

## Capacity, latency, and saturation math

### What's the theoretical wire capacity?

| | Value |
|---|---|
| Radio rate | 300 kbps GFSK = 37 500 bytes/sec |
| Packet header (proto v1) | 11 bytes |
| SX1262 GFSK overhead per packet (preamble + sync + len + CRC) | ~7 bytes |
| Channel-voice event in body | 5 bytes (`event_seq:2 + status:1 + data:0–2`) |
| Average events per packet (busy queue, batched) | ~3 |
| Wire bytes per event with K=3 + 2 delayed | ~31 amortized |

### Can the link sustain a fully-saturated DIN MIDI cable?

DIN MIDI at 31250 baud caps at ~1041 events/sec (worst case: every
event is 3 bytes, no running status — beyond what a human keyboard can
generate).

| Workload | Wire utilization |
|---|---|
| Realistic heavy playing, ~300 ev/s | ~25% |
| Synthetic scenarios (peaked ~500 ev/s) | ~40% |
| Saturated DIN cable (1041 ev/s, all NoteOn/Off) | ~86% |
| + heartbeats (~5%) | **~91%** total |

Yes, with margin for any realistic playing. Literal max-rate DIN
saturation works at 91% utilization (no headroom) — but that workload
is a contrived data-dump test, not music.

### What's the worst-case latency for a delivered event?

Bounded by the +60 ms delayed copy schedule. An event reaches RX as
soon as ANY of {original, K=3 retransmits, +30 ms copy, +60 ms copy}
arrives.

At observed loss rates:

| All-lost probability | Expected events | Excursion |
|---|---|---|
| Original delivered | typical case | <1 ms (queue + air) |
| Main lost, K=3 catches | ~6% per event | ~3 ms above baseline |
| All immediate lost, +30 ms copy | ~0.2% per event | +30 ms |
| Down to +60 ms copy | ~0.03% per event | +60 ms |
| All 5 lost (silent miss) | ~0.001% per event | n/a |

Worst-case latency ceiling for a delivered event: **~60 ms above
baseline**. Well below the 80–100 ms range a percussionist starts to
feel.

The watchdog (200 ms) is a hard upper bound on packet inter-arrival
during normal operation — if it expires, it's link loss, not latency.

### Why no bigger queue / higher K?

Diminishing returns and wire cost. K=3 + 2 delayed already gets us to
0.001% silent-miss rate. Going to K=4 saves only 1.4× on miss rate
but costs 33% more wire. Adding a +90 ms or +120 ms delayed copy
similarly: marginal reliability gain, and the listener's perceptual
threshold for note-onset latency means a copy past 80 ms is too late
to be useful for note-state events anyway.

---

## Synthetic scenarios

### Why so many scenarios? Why a 200 ms gap between them?

The original 1500 ms inter-scenario gap (rx10 and earlier) was so log
output was easy to read. For walk-around RF testing we wanted the link
to run essentially continuously, so the gap was cut to 200 ms for
rx12+. This keeps each scenario boundary visible in the log but
removes the dead-air windows that previously made up ~12% of run
duration.

### What's covered today (post-rx12)?

Rotation: `Scale → ChordProgression → Glissando → KeySmash → QuickStabs
→ PitchWheel → ModWheel → MixerAutomation → PatchWalk → AftertouchSweep`,
200 ms gaps between each.

| Scenario | Coverage focus |
|---|---|
| Scale | Single-channel NoteOn/Off, melodic |
| ChordProgression | Triads at varying densities |
| Glissando | Fast NoteOn/Off (30 ms/note) |
| KeySmash | 8-note cluster pressed within 8 ms |
| QuickStabs | Staccato chords, tests NoteOff cancelling pending NoteOn |
| PitchWheel | Pitch Bend (0xE0) sweep, tests dedup at ~200 ev/s |
| ModWheel | CC#1 sweep, dedup path for CC |
| MixerAutomation | Concurrent CC#7/CC#10/CC#64 on ch 0–2, multi-channel CC dedup |
| PatchWalk | Bank MSB + LSB + PC sequence on ch 0 and ch 9 — exercises latched-state reliability |
| AftertouchSweep | Channel Pressure (0xD0) + Poly Aftertouch (0xA0) sweeps on a held chord |

This collectively exercises every channel-voice message type, plus
multi-channel traffic.

---

## MIDI hardware (FeatherWing & DIN)

### How is the Adafruit MIDI FeatherWing wired?

```
keyboard MIDI OUT cable ──→ DIN-5 IN  ── opto-isolator ── UART RX (D0)
synth MIDI IN cable      ←── DIN-5 OUT ── buffer/resistors ── UART TX (D1)
```

- **Single UART**, full-duplex. Same RX and TX pins for both directions.
- **Opto-isolator is on the IN side only.** MIDI 1.0 spec mandates
  galvanic isolation on every receiver to break ground loops.
- **OUT side has no opto** — just a 3.3 V buffer + resistors driving
  the DIN-5. The receiving synth's MIDI IN provides its own isolation.
- **Two DIN-5 jacks: IN and OUT. No THRU.** If you need MIDI THRU
  you'd build it yourself.

### Will 3.3 V drive a synth's MIDI IN cleanly?

Yes. The MIDI 1.0 spec was officially updated in 2014 to recognize
"Type-A" 3.3 V transmitters, and almost every modern opto (6N138,
H11L1, PC900) triggers cleanly with the FeatherWing's resistor network.
Edge cases are 1980s gear with very tight LED-current thresholds.

### Can I wire just one direction (RX or TX, not both)?

Yes. The IN path and OUT path are independent. On each board you only
need 3 wires: `3V3`, `GND`, and whichever data line you're using.

```
MIDI-IN-only board (radio TX side):
  T114 3V3   →  FeatherWing 3V
  T114 GND   →  FeatherWing GND
  T114 P0_09 ←  FeatherWing RX  (TX pin and DIN OUT jack stay disconnected)

MIDI-OUT-only board (radio RX side):
  T114 3V3   →  FeatherWing 3V
  T114 GND   →  FeatherWing GND
  T114 P0_10 →  FeatherWing TX  (RX pin and DIN IN jack stay disconnected)
```

### Why doesn't the FeatherWing stack on the T114?

The T114 v2.0 has two 13-pin single-row headers (P1 and P2) — Heltec's
own 2×13 layout, not Adafruit Feather form factor. The FeatherWing is
designed for the standard Feather pinout (16 + 12 pins). 4 jumper
wires per board work fine.

---

## T114 board

### Where are the MIDI UART pins on the T114?

Bottom of the **P1 header** (right-side header looking at the board
from the top):

| Silkscreen | nRF52840 GPIO | Function |
|---|---|---|
| `0.09 / UART1_RX` | P0_09 | midi_uart RX |
| `0.10 / UART1_TX` | P0_10 | midi_uart TX |

Plus `3V3` and `GND` at the top of P1.

### What peripheral does midi_uart use?

UARTE1, the second nRF52840 UARTE block, configured at 31250 baud
8N1 with `BufferedUarte` so it exposes both
`embedded_io_async::Read` and `Write`. Plain `Uarte` only implements
`Write`.

UARTE0 is reserved for the bootloader / USB-CDC log path on the T114
(the chip's primary UART), so we use UARTE1 to avoid conflicts.

### What does `t114_midi_rx` do today?

Pure read-only loop: reads bytes from UARTE1, feeds them through
`MidiParser`, logs every parsed event via `defmt::info!`. No TX, no
link layer. Smoke test for "the FeatherWing IN path works and the
parser handles real MIDI cleanly".

### What does `t114_midi_tx` do today?

Mirror: emits a C-major arpeggio (NoteOn/NoteOff at ~1 Hz) using
running status, with `TimingClock` (0xF8) pulses interleaved to stress
the receiver's parser. No RX, no link layer.

### Why isn't there an integrated `midi_node` app yet?

It's the milestone-4 integration target. `apps/midi_node/` exists as a
skeleton; the actual TX (UART → parser → MidiTxQueue → LinkSender →
radio) and RX (radio → LinkReceiver → UART write, with `LinkLost` →
all-notes-off on the wire) loops still need to be wired together.
Once that lands, the same scenarios used in `link_bench` can run with
real MIDI in/out instead of the synthetic source.
