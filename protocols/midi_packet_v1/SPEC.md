# OpenStageRF Transport Envelope v1 — Specification

This is the on-air packet format for OpenStageRF v1.  It defines a transport envelope shared across MIDI and (future) audio bodies, with cipher-agile authenticated encryption, dual-layer dedup (per-packet for diversity reception, per-event for retransmit dedup), and forward-compatible event-type discrimination.

## Design

The packet header carries two independent sequence-number fields:

| Counter | Width | Scope | Used for |
|---|---|---|---|
| `packet_seq` | 4 B | per wire transmission | AEAD nonce uniqueness, multi-antenna diversity dedup |
| `event_seq`  | 2 B (in body) | per logical MIDI event | end-to-end retransmit dedup |

This lets the link layer freely repack and round-robin events across packets — each retransmit gets a fresh `packet_seq` (so the AEAD nonce stays unique) while events keep their `event_seq` (so the receiver fires each event exactly once regardless of how many packets carried it).

It also enables clean cancellation: a queued NoteOn with `event_seq=N` can be removed before its remaining retransmits go out, and the receiver never sees a stale NoteOn after a NoteOff because the wire never carries `event_seq=N` again.

## Wire format

```
┌──────┬────────┬──────────────┬────────────┬────────────┬──────────────┬─────┐
│ ver  │ key_fp │ boot_counter │ packet_seq │ event_type │ body         │ tag │
│ 1 B  │ 3 B    │ 2 B          │ 4 B        │ 1 B        │ 0..N B       │ 0/8/16 B
└──────┴────────┴──────────────┴────────────┴────────────┴──────────────┴─────┘
   │       │           │             │            │              │            │
   │       │           │             │            │              └ encrypted in AEAD modes
   │       │           │             │            └─ first byte after header, identifies content
   │       │           │             └─ unique per wire transmission
   │       │           └────────────── randomized per power-on; identifies the session
   │       └─────────────── 3-byte key fingerprint (SHA-256 of key material)
   └──────────────────── transport envelope version (v1 = 0x01)

AAD = ver || key_fp || boot_counter || packet_seq || event_type    (11 bytes, authenticated)
```

Total fixed header overhead: 11 bytes.  Tag overhead: 0/8/16 bytes depending on cipher (see *Encryption modes*).

## Field definitions

### `ver` (1 byte)

Transport envelope version.  v1 is `0x01`.  Receivers MUST drop packets with unknown versions.  Bumped only on backward-incompatible wire-format changes.

### `key_fp` (3 bytes)

Same semantics as v1: a 3-byte fingerprint identifying which key was used.  `key_fp = SHA-256(cipher_id || key_bytes)[0..3]`.

| Value | Meaning |
|---|---|
| `0x000000` | No encryption, no authentication.  `tag` is omitted (0 bytes). |
| `0x000001`–`0xFFFFFE` | Key fingerprint.  Receiver scans local key store for a matching entry. |
| `0xFFFFFF` | Reserved. |

Collision handling and store lookup are unchanged from v1.

### `boot_counter` (2 bytes, big-endian)

Random 16-bit value generated at TX power-on.  Identifies the *session* — every reboot starts a new session with a fresh `boot_counter`.

The receiver uses `boot_counter` to detect TX restarts:

- First packet seen: record `boot_counter`, initialise replay windows.
- Subsequent packet with the same `boot_counter`: process normally.
- Subsequent packet with a *different* `boot_counter`: TX has restarted.  Reset both replay windows and any in-flight SysEx reassembly buffers, then accept the new packet as the first of the new session.

Collision rate (two consecutive sessions happening to draw the same `boot_counter`): 1 in 65 536.  See *Packet replay window* below for the fallback session-reset rule that catches collisions automatically.

`boot_counter` is also part of AEAD nonce derivation (see *Nonce construction*).

### `packet_seq` (4 bytes, big-endian)

32-bit counter incremented on every wire transmission, including retransmits.  Resets to 0 (or a small fixed offset) at the start of each session.  Rolls over after ~4.3 billion transmissions (~50 days continuous at 1000 packets/sec); on rollover the device MUST refuse further transmissions with the current `(boot_counter, key)` pair until rebooted (forcing a `boot_counter` change) or until a new key is provisioned.

Used at RX for two purposes:

1. **Multi-antenna diversity dedup.**  When the receiver processes the same packet twice (two antennas, two different demodulators), `packet_seq` matches identify the duplicate.
2. **Crypto nonce uniqueness.**  Combined with `boot_counter`, `device_id`, and `direction`, the resulting nonce is unique for the lifetime of any given key.

`packet_seq` does **not** identify MIDI events.  Two retransmits of the same logical event have *different* `packet_seq` values but identical body content.

### `event_type` (1 byte)

Discriminates the body content.  Authenticated (in AAD) but not encrypted, so receivers can dispatch before decryption.

| Value | Body type | Use |
|---|---|---|
| `0x00` | RESERVED | Do not transmit. Receivers MUST drop. |
| `0x01` | `HEARTBEAT` | Keepalive.  Body is empty. |
| `0x02` | `CHANNEL_VOICE` | One or more `(event_seq, midi_message)` tuples. |
| `0x03` | `SYSEX_FRAGMENT` | One fragment of a larger SysEx message. |
| `0x04`–`0x0F` | reserved for future MIDI extensions | |
| `0x10`–`0x1F` | reserved for AUDIO_FRAME body types | |
| `0x20`–`0x7F` | reserved for future expansion | |
| `0x80`–`0xFF` | reserved for vendor / experimental extensions | |

#### Body: `CHANNEL_VOICE` (event_type 0x02)

```
[event_seq:2][midi:1..3][event_seq:2][midi:1..3]...
```

A sequence of one or more `(event_seq, midi_message)` tuples.  No inter-tuple framing — the receiver delimits MIDI messages by the standard MIDI status-byte rule (status bytes have the high bit set; data bytes don't).

Per-message lengths follow the MIDI spec:

- 1 byte: System Real-Time messages (`0xF8`–`0xFF`)
- 2 bytes: Program Change (`0xCn`), Channel Pressure (`0xDn`), Song Select (`0xF3`), MIDI Time Code Quarter Frame (`0xF1`)
- 3 bytes: Note On (`0x9n`), Note Off (`0x8n`), Polyphonic Pressure (`0xAn`), Control Change (`0xBn`), Pitch Bend (`0xEn`), Song Position (`0xF2`)

**Running status is not used on the wire** — each tuple carries its full status byte.

`F0` (SysEx start) and `F7` (SysEx end) MUST NOT appear in `CHANNEL_VOICE` bodies; SysEx uses `SYSEX_FRAGMENT` (0x03).

`event_seq` values within a packet need not be contiguous or monotonic.  The receiver dedups each event against its replay window independently, then fires the surviving events in body order.

`event_seq` is a 16-bit big-endian value assigned by TX.  It increments per *logical* MIDI event (not per copy), and rolls over after 65 536 events.  See *Replay protection* for the modular comparison rule that handles rollover.

##### Batching policy

Events MAY be batched into one `CHANNEL_VOICE` packet to amortise the per-packet overhead.  The link layer batches by *priority and event type* — channel-voice events at the same priority pack together; system real-time events go in their own packets; SysEx fragments go in their own packets.

Mixing system-real-time bytes into a packet alongside regular channel-voice events is permitted by the wire format but discouraged at the link layer (real-time messages are jitter-sensitive and shouldn't share a packet with non-urgent traffic).

#### Body: `SYSEX_FRAGMENT` (event_type 0x03)

```
[sysex_id:2][frag_idx:1][frag_total:1][frag_data:1..N]
```

| Field | Width | Meaning |
|---|---|---|
| `sysex_id` | 2 B | Identifies the SysEx message.  Assigned by TX, monotonically incremented per SysEx (modulo 2¹⁶). |
| `frag_idx` | 1 B | Index of this fragment within the SysEx.  Range `0..frag_total`. |
| `frag_total` | 1 B | Total fragment count for this SysEx.  Range `1..=255`. |
| `frag_data` | variable | Raw SysEx body bytes between `F0` and `F7`, exclusive of both markers. |

A complete SysEx is reassembled at the receiver as `F0 || frag_data[0] || frag_data[1] || ... || frag_data[frag_total-1] || F7`.

Per-fragment dedup at RX uses the tuple `(sysex_id, frag_idx)` directly — there is no separate `event_seq` for fragments.  Retransmits of the same fragment carry identical `(sysex_id, frag_idx)` and are dedupped on receipt.

`frag_total = 1`, `frag_idx = 0` is the single-fragment case (entire SysEx fits in one packet).

##### Reassembly behavior

The receiver maintains up to `MAX_CONCURRENT_SYSEX` (typically 2–4) reassembly buffers, keyed by `sysex_id`.  Behaviour:

1. On first fragment seen for a `sysex_id`: allocate a buffer if available; otherwise drop the fragment (and silently abandon the SysEx).
2. On subsequent fragments: store at `frag_idx`, mark as received in the per-buffer `received_mask`.
3. On all `frag_total` fragments received: concatenate, prepend `F0`, append `F7`, deliver the complete SysEx to the application sink.
4. If a buffer goes more than `SYSEX_REASSEMBLY_TIMEOUT_MS` (default 5000 ms) without a new fragment, discard it (partial SysEx is lost).

A SysEx whose first fragment is lost is unrecoverable — the receiver has no way to know how big to size the buffer or what `frag_total` is.  This is acceptable; SysEx is rarely time-critical and applications needing guaranteed delivery should send during setup over USB or at the application layer.

#### Body: `HEARTBEAT` (event_type 0x01)

Empty (0 bytes).  Sent by TX whenever the queue has been silent for `HEARTBEAT_PERIOD_MS` (default 10 ms).  Receiver uses heartbeat arrivals to feed the link watchdog.

### `body` (variable length)

Body content.  Length is determined by the radio packet length minus the fixed header and tag.  In AEAD modes, `body` is the encrypted plaintext; in `none`/`mac_only` modes it's plaintext.

### `tag` (0, 8, or 16 bytes)

AEAD authentication tag.  Same options and defaults as v1.

| `cipher_id` | `tag` size |
|---|---|
| `NONE` (when `key_fp == 0x000000`) | 0 bytes |
| `MAC_ONLY` | 8 bytes |
| `CHACHA20_POLY1305` | 8 bytes (default) or 16 bytes |
| `AES_128_CCM` | 8 bytes (CCM-8) or 16 bytes (CCM-16) |

## Replay protection

Two replay windows operate at RX, with disjoint scopes.

### Packet replay window (32-bit)

Tracks `packet_seq` within the current `boot_counter`.  Standard sliding-window bitmap, 64 bits deep:

```rust
struct PacketReplayWindow {
    high: u32,       // highest packet_seq seen this session
    bitmap: u64,     // bit i = whether (high - i) was seen, i in 0..64
}
```

On packet with `packet_seq = s`:

- If `s > high`: shift `bitmap` left by `(s - high)`, set bit 0, update `high = s`.  Accept.
- If `s == high`: bit 0 already set; reject as replay.
- If `high - s >= SESSION_RESET_GAP`: assume TX has restarted with a colliding `boot_counter` (1 in 65 536).  Reset `high`, `bitmap`, the event replay window, and any in-flight SysEx reassembly buffers; accept the packet as the first of the new session.  See *Session-reset fallback* below.
- If `s + 64 < high`: too old; reject.
- Otherwise: check `bitmap` bit `(high - s)`.  If set, reject as replay; else set the bit, accept.

On `boot_counter` change (detected before this window check): clear `high` and `bitmap`, treat the incoming packet as the first of a new session.

#### Session-reset fallback

`SESSION_RESET_GAP = 100_000` (configurable; default chosen to comfortably exceed peak `packet_seq` advance over one minute of sustained max-rate transmission).

A backward jump of more than `SESSION_RESET_GAP` is impossible from legitimate radio behaviour — packets aren't held in transit for minutes, and `packet_seq` advances monotonically within a session.  The only real-world cause is a TX reboot whose new `boot_counter` happened to draw the same value as the previous session's (1/65 536 collision rate).  When the receiver detects this, it treats the packet as the first of a fresh session: clear both replay windows, abort any in-flight SysEx reassembly, and accept the packet.

Estimating the threshold: at the maximum sustainable transmit rate (~1500 packets/sec including retransmits, given ~700 µs per single-event packet plus radio framing), `packet_seq` advances by ~90 000 per minute.  A threshold of 100 000 covers any plausible legitimate gap and any out-of-order delay; thresholds larger than that (e.g., 1 000 000) are also safe and trade off a longer recovery delay on collision against a tighter false-positive bound.

### Event replay window (16-bit, modular)

Tracks `event_seq` for `CHANNEL_VOICE` packets within the current `boot_counter`.  64-bit bitmap, modular comparison:

```rust
struct EventReplayWindow {
    high: u16,
    bitmap: u64,
}
```

On event with `event_seq = s`:

```rust
let d = s.wrapping_sub(self.high);  // u16 modular subtraction
match d {
    0          => Replay,                                  // s == high
    1..=32_767 => Forward(d),                              // ahead → advance window
    32_768..=65_471 => TooOld,                             // backward by > 64 → drop
    32_768.. /* i.e. 65_472..=65_535 */ => Backward,       // backward by ≤ 64 → check bitmap
}
```

Concretely:

- **Forward** (`d` in `[1, 32_767]`): shift bitmap left by `d`, set bit 0, update `high = s`.  Accept.
- **Replay** (`d == 0`): reject.
- **Backward** (`d` in `[65_472, 65_535]`, i.e. backward by 1..64): check `bitmap` bit `(65_536 - d)`.  If set, reject as replay; else set, accept.
- **TooOld** (`d` in `[32_768, 65_471]`): reject (outside window).

The split point at half the seq space (32 768) means:

- Wraparound (e.g., `high=65_535`, new `s=0`) is naturally treated as forward by 1.  No epoch counter needed.
- Out-of-order packets within the 64-deep window are accepted via the `Backward` branch.
- Long gaps that exceed 32 768 events would be ambiguous, but the watchdog (200 ms link-down detection) makes that impossible in practice — total RF blackout > 30 seconds at peak rates would be required.

On `boot_counter` change: reset both `high` and `bitmap` to 0.

## Sequence number allocation at TX

### `packet_seq`

- Initialised at session start (e.g., to 0 or a small constant).
- Incremented by 1 on every wire transmission, including retransmits.
- A given logical event going out 3 times (retransmit redundancy) consumes 3 `packet_seq` values.

### `event_seq`

- Initialised at session start to 0.
- Incremented by 1 per *logical* event (each MIDI message from the local source consumes one `event_seq`).
- A given logical event keeps the same `event_seq` across all its retransmits.
- Cancellation (e.g., NoteOff cancelling a queued NoteOn) does **not** reuse the cancelled event's `event_seq` — the new NoteOff gets the next `event_seq` value.  The cancelled NoteOn's seq is simply never resent; the receiver's `event_seq` window has a 1-bit gap that's harmless.
- `sysex_id` for SYSEX_FRAGMENT bodies is allocated from the same name-space rationale (monotonic 16-bit counter at TX), but it lives in its own counter independent of `event_seq` — they don't collide because they're scoped to different `event_type` values at RX dedup time.

## Encryption modes

Unchanged from v1.  See v1 spec for full details.

- `none` (`key_fp == 0x000000`): plaintext, no tag.
- `mac_only`: plaintext body, 8-byte MAC tag over AAD.
- `chacha20_poly1305`: full AEAD, 8- or 16-byte tag.
- `aes_128_ccm`: full AEAD, 8- or 16-byte tag.

## Nonce construction

Same approach as v1 with the seq fields renamed.

**ChaCha20-Poly1305 (12-byte nonce):**

```
[device_id:4][direction:1][packet_seq:4][boot_counter:2][reserved:1=0x00]
```

**AES-128-CCM (13-byte nonce):**

```
[device_id:4][direction:1][packet_seq:4][boot_counter:2][reserved:2=0x0000]
```

`device_id` and `direction` semantics are unchanged from v1.  Nonce uniqueness across the lifetime of a `(key, device_id)` pair is guaranteed as long as `(boot_counter, packet_seq)` never repeats — `boot_counter` differs per session, and `packet_seq` is monotonic within a session.

## Link layer behavior

### Sender (TX)

1. On power-on:
   - Generate a fresh random `boot_counter`.
   - Initialise `packet_seq = 0`, `event_seq = 0`, `sysex_id = 0`.
2. Capture an event from the local source (MIDI parser, app).
3. Apply queue dedup rules (see *Queue dedup rules* below).
4. If the event survives dedup, assign the next `event_seq` and enqueue with K=3 retransmit credits.
5. The transmit loop pops a packet's worth of same-type, same-priority events from the front of the queue, encodes them with a fresh `packet_seq`, transmits, decrements credits on consumed entries, and requeues survivors at the back of their priority class.
6. If the queue has been empty for `HEARTBEAT_PERIOD_MS`, transmit an empty `HEARTBEAT` packet.
7. If `packet_seq` would overflow, refuse further transmissions.

### Receiver (RX)

1. Receive packet from radio (CRC validated by hardware).
2. Decode header.  Drop if `ver != 0x01` or `key_fp` doesn't match the local store.
3. Compare `boot_counter` to the recorded session value.  If it differs, reset both replay windows and abort any in-flight SysEx reassembly buffers, then proceed with the new session.  (The packet replay window's session-reset-fallback rule catches the rare 1/65 536 case where `boot_counter` happens to repeat across reboots.)
4. Apply *packet replay window* check.  If duplicate or too-old, drop.
5. Verify AEAD tag (or skip in `none` mode).  If invalid, drop silently.
6. Decrypt body if AEAD mode.
7. Dispatch by `event_type`:
   - `HEARTBEAT`: kick watchdog timer, emit nothing to consumer.
   - `CHANNEL_VOICE`: parse body into `(event_seq, midi)` tuples; for each, apply *event replay window* check and emit surviving MIDI to consumer in body order.
   - `SYSEX_FRAGMENT`: dedup `(sysex_id, frag_idx)`, accumulate into reassembly buffer, emit complete SysEx to consumer when reassembled.
   - Unknown event_type within reserved ranges: drop silently.
8. **Watchdog:** if no packet (including heartbeats) received for `WATCHDOG_MS` (default 200 ms), fire `LinkLost` → emit all-notes-off + sustain-off + pitch-bend-center, AND mark the link as down internally.  The *next* packet received after the link is marked down triggers a full session reset (clear both replay windows and SysEx reassembly state) regardless of `boot_counter` value.  Combined with the packet-replay-window fallback above, this guarantees the receiver recovers within one packet of any TX restart, whether or not `boot_counter` happens to collide.

   Why this matters: a `boot_counter` collision (1/65 536) in a *low-traffic* session might leave only a small `packet_seq` gap (e.g., a 1000-packet window from a 20-second idle session) — too small for the `SESSION_RESET_GAP` fallback to trigger but too large for the 64-deep replay window to absorb.  The watchdog-driven reset closes that gap because TX boot times always exceed `WATCHDOG_MS`.

## Queue dedup rules (TX side)

These rules run when an event is pushed into the TX queue (before `event_seq` assignment).  They prevent the queue from holding self-contradicting state.

| Incoming event | Removes from queue |
|---|---|
| NoteOff (0x8n) note N | NoteOns (0x9n) on same channel + same note |
| NoteOn (0x9n) note N | NoteOffs (0x8n) on same channel + same note |
| PolyAftertouch (0xAn) note N | PolyAT on same channel + same note |
| Control Change (0xBn) ctrl C | CC on same channel + same controller |
| Program Change (0xCn) | PC on same channel |
| Channel Pressure (0xDn) | CP on same channel |
| Pitch Bend (0xEn) | PB on same channel |

Two consecutive NoteOns for the same note (with no intervening NoteOff) are *not* deduplicated — those represent legitimate rapid re-strikes (legato attacks with potentially different velocity).  Same for two consecutive NoteOffs.

System-real-time messages (0xF8–0xFF) and SysEx fragments are never deduplicated — each carries unique semantics or is identified by its own ID.

## Sizes and timing

For a typical Note On (3-byte MIDI):

| Mode | Header | event_type | event_seq + midi | tag | Total |
|---|---|---|---|---|---|
| `none` | 10 | 1 | 5 | 0 | **16 bytes** |
| `mac_only` (8-byte) | 10 | 1 | 5 | 8 | **24 bytes** |
| `aead` (8-byte tag) | 10 | 1 | 5 | 8 | **24 bytes** |
| `aead` (16-byte tag) | 10 | 1 | 5 | 16 | **32 bytes** |

For a 3-note chord batched into one packet:

| Mode | Header | event_type | 3 × (event_seq + midi) | tag | Total |
|---|---|---|---|---|---|
| `none` | 10 | 1 | 15 | 0 | **26 bytes** |
| `aead` (8-byte tag) | 10 | 1 | 15 | 8 | **34 bytes** |

For a single-fragment small SysEx (e.g., 6-byte body):

| Mode | Header | event_type | sysex_id + idx + total + data | tag | Total |
|---|---|---|---|---|---|
| `aead` (8-byte tag) | 10 | 1 | 4 + 6 = 10 | 8 | **29 bytes** |

Plus radio framing (preamble + sync + length + CRC ≈ 12 bytes) on the air.  At 300 kbps GFSK:

- 16 + 12 = 28 B → ~747 µs air time
- 26 + 12 = 38 B → ~1013 µs air time
- 34 + 12 = 46 B → ~1227 µs air time

Maximum batch size (assuming `RF_PAYLOAD_MAX = 64`, 8-byte tag, 11-byte header):

- ChannelVoice: `(64 - 11 - 8) / 5 = 9` 3-byte channel-voice events per packet.
- SysExFragment: 1 fragment of up to `64 - 11 - 8 - 4 = 41` body bytes per packet.

## Forward compatibility

**Adding new MIDI event types:** assign a new value in `0x04`–`0x0F`.  Receivers running older firmware drop unknown values silently.

**Adding audio (v3-or-later body types):** uses event_type values `0x10`–`0x1F`.  Each value defines a specific codec + sample rate + channel count + frame duration.  The transport envelope, encryption, and replay protection are unchanged.

**Adding new ciphers:** register a new `cipher_id` value in the local key store.  Wire format unchanged.

**Breaking changes:** bump the `ver` byte.  Receivers running old firmware drop unknown versions.

## Test vectors

(To be added during M5 implementation in `protocols/midi_packet_v1/test_vectors.json`.)

## Open issues / future work

- **Bidirectional links** (`direction = 0x01`): for a future RX→TX channel.  Reserved in nonce.
- **Key rotation in flight:** v1 has no in-band rotation.  Future versions could add a `KEY_ROTATE` event_type.
- **Persistent `packet_seq`:** for very-high-security deployments, `packet_seq` could be persisted to flash across reboots, eliminating the 1/65 536 `boot_counter` collision risk.  Not currently planned.
- **`event_seq` rollover under sustained load:** at peak wire-MIDI rates (~1041 events/sec), `event_seq` rolls over every 63 seconds.  The modular replay window handles this transparently, but instrumented testing should verify behaviour across a rollover boundary.
