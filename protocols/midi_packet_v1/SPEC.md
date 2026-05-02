# OpenStageRF Transport Envelope v1 — Specification

This is the on-air packet format for OpenStageRF v1. It defines a transport envelope shared across MIDI and (future) audio bodies, with cipher-agile authenticated encryption, replay protection via sequence-number nonces, and forward-compatible event-type discrimination.

The directory is named `midi_packet_v1` because v1 ships only MIDI body types. Audio body types (added when v3 ships) reuse the same envelope without schema changes — they just occupy reserved `event_type` values.

## Wire format

```
┌──────┬────────┬─────┬────────────┬──────────────┬─────┐
│ ver  │ key_fp │ seq │ event_type │ event_data   │ tag │
│ 1 B  │ 3 B    │ 6 B │ 1 B        │ 0..N B       │ 0/8/16 B
└──────┴────────┴─────┴────────────┴──────────────┴─────┘
   │       │      │       │              │            │
   │       │      │       │              └ encrypted in AEAD modes (none/mac_only: plaintext)
   │       │      │       └─ first byte of body, identifies content
   │       │      └────────── 6-byte sequence number = [boot_counter:2][session_seq:4]
   │       └─────────────── 3-byte key fingerprint (SHA-256 of key material, device-independent)
   └──────────────────── transport envelope version (v1 = 0x01)

AAD = ver || key_fp || seq || event_type     (11 bytes, authenticated, never encrypted)
```

Total fixed overhead: 11 bytes (header + event_type) + 0/8/16 bytes (tag).

## Field definitions

### `ver` (1 byte)

Transport envelope version. v1 is `0x01`. Receivers MUST drop packets with unknown versions. Bumped only on backward-incompatible wire-format changes.

### `key_fp` (3 bytes)

A 3-byte fingerprint that identifies which key was used to encrypt the packet. The fingerprint is derived from the key material itself, making it **device-independent**: two devices that hold the same key bytes will compute the same fingerprint regardless of what local key slot they assigned it to. This eliminates the pairing ceremony of synchronizing local key IDs across devices.

`key_fp = SHA-256(cipher_id || key_bytes)[0..3]` (big-endian, first three bytes of the SHA-256 digest)

The cipher_id is implicit in the looked-up key entry and is **not** sent on the wire (see README Decision #6) — it is recovered from local key store lookup by fingerprint.

| Value | Meaning |
|---|---|
| `0x000000` | No encryption, no authentication. Reserved for debug / lowest-latency builds. `tag` field is omitted. |
| `0x000001–0xFFFFFE` | Key fingerprint. Receiver scans its local key store for an entry whose `key_fp` matches, then uses the associated cipher and key material. |
| `0xFFFFFF` | Reserved. |

**Collision probability:** with a 3-byte (16.7 M value) fingerprint, the chance that any two keys in a 56-key store share a fingerprint is ~0.009% (birthday problem: `1 - e^(-n(n-1) / 2*16777215)`). Effectively never in practice.

**Collision handling:** in the vanishingly rare event of a collision, the receiver SHOULD try each matching key in order and accept the first whose AEAD tag verifies. If no key verifies, drop the packet silently. A collision causes at most one extra failed AEAD check — never a security breach.

A receiver that finds no `key_fp` match in its store MUST drop the packet silently.

### `seq` (6 bytes, big-endian)

Sequence number, structured as `[boot_counter:2][session_seq:4]`:

- **`boot_counter`** (2 bytes, big-endian, MSB first): increments by 1 on every device boot, persisted to flash. Rolls over after 65,535 boots.
- **`session_seq`** (4 bytes, big-endian, MSB first): starts at 0 each boot, increments by 1 for every transmitted packet. Rolls over after ~4.3 billion packets per session (~50 days at 1000 packets/sec).

`seq` is a key contributor to AEAD nonce uniqueness (see *Nonce construction* below). Combined with `device_id` and `direction`, the resulting nonce is unique for the lifetime of any given key.

### `event_type` (1 byte)

Discriminates the body content. The value is **part of the AAD** — it is authenticated by the AEAD tag but not encrypted. This lets receivers route packets to the correct handler before decrypting; an attacker cannot tamper with `event_type` to redirect a payload undetected.

| Value | Body type | Use |
|---|---|---|
| `0x00` | RESERVED | Do not transmit. Receivers MUST drop. |
| `0x01` | `HEARTBEAT` | Keepalive. `event_data` is empty (0 bytes). |
| `0x02` | `MIDI_MESSAGE` | Single MIDI message. `event_data` = 1 to 3 raw MIDI bytes (status + 0–2 data). |
| `0x03` | `MIDI_SYSEX_FRAGMENT` | Fragment of a SysEx message. `event_data` = `[frag_state:1] + sysex_bytes`. |
| `0x04`–`0x0F` | reserved for future MIDI extensions | |
| `0x10`–`0x1F` | reserved for AUDIO_FRAME body types (defined when v3 audio ships) | |
| `0x20`–`0x7F` | reserved for future expansion | |
| `0x80`–`0xFF` | reserved for vendor / experimental extensions | |

#### Body: `MIDI_MESSAGE` (event_type 0x02)

`event_data` = 1 to 3 bytes of raw MIDI:

- 1 byte: System Real-Time messages (`0xF8`–`0xFF`) — Timing Clock, Start, Continue, Stop, Active Sensing, System Reset
- 2 bytes: Program Change (`0xCn`), Channel Pressure (`0xDn`), Song Select (`0xF3`), MIDI Time Code Quarter Frame (`0xF1`)
- 3 bytes: Note On (`0x9n`), Note Off (`0x8n`), Polyphonic Pressure (`0xAn`), Control Change (`0xBn`), Pitch Bend (`0xEn`), Song Position (`0xF2`)

**Running status is not used on the wire.** Each packet carries the full status byte. The TX-side parser may have used running status to consume bytes from a DIN MIDI stream, but the protocol always sends explicit status. Senders MUST NOT compress consecutive same-status messages by omitting the status byte.

`MIDI_MESSAGE` packets MUST contain exactly one MIDI message. Bundling multiple messages into one packet is not supported in v1.

#### Body: `MIDI_SYSEX_FRAGMENT` (event_type 0x03)

`event_data` = `[frag_state:1] + sysex_bytes:1..N`

`frag_state` byte values:

| Value | Meaning |
|---|---|
| `0x01` | First fragment. `sysex_bytes` begins with `0xF0` (SysEx start). |
| `0x02` | Middle fragment. `sysex_bytes` is a continuation; no `0xF0` or `0xF7` markers within. |
| `0x03` | Last fragment. `sysex_bytes` ends with `0xF7` (SysEx end). |
| `0x04` | Single-fragment SysEx. `sysex_bytes` begins with `0xF0` and ends with `0xF7` (i.e., the entire SysEx fits in one packet). |
| other | reserved; receivers MUST drop |

The receiver maintains one in-progress SysEx buffer per `device_id` (sender). Receiving a fragment with state `0x01` resets the buffer; subsequent `0x02` fragments append; `0x03` flushes the complete SysEx to the MIDI output. `0x04` is a one-shot.

If the receiver sees an out-of-order fragment (e.g., `0x02` without prior `0x01`), it SHOULD drop the buffer and ignore the fragment. The original SysEx is lost; this is an acceptable failure mode given that SysEx is rarely time-critical.

System Real-Time messages MAY be sent in `MIDI_MESSAGE` packets while a SysEx is in flight; they don't disturb the SysEx state machine on either side because they're routed independently.

#### Body: `HEARTBEAT` (event_type 0x01)

`event_data` is empty (0 bytes). Sent by TX every 20 ms (50 Hz) when no other event has been transmitted in that window. Receiver uses heartbeat arrivals to feed the link watchdog (see *Link layer behavior*).

### `event_data` (variable length)

Body content. Length is determined by the radio packet length minus the fixed header and tag. In the AEAD modes, `event_data` is the encrypted plaintext; in `none`/`mac_only` modes it's plaintext.

### `tag` (0, 8, or 16 bytes)

AEAD authentication tag (or auth-only MAC for `mac_only` mode).

| `cipher_id` (in local key_entry) | `tag` size |
|---|---|
| `NONE` (when `key_fp == 0x0000`) | 0 bytes |
| `MAC_ONLY` (Poly1305 or HMAC-SHA-256-truncated) | 8 bytes |
| `CHACHA20_POLY1305` | **8 bytes** (default) or 16 bytes (full tag, per-key option) |
| `AES_128_CCM` | **8 bytes** (default, CCM-8) or 16 bytes (CCM-16, per-key option) |

**Default tag size: 8 bytes.** 64-bit forgery resistance is well-precedented in embedded wireless (BLE uses 4-byte MIC; WPA2 uses 8-byte TKIP MIC) and is more than adequate for stage RF threat models. An attacker who knows the ciphertext, AAD, and tag would need ~2⁶⁴ attempts to forge a valid packet — computationally infeasible.

**16-byte tag option:** available as a per-key configuration flag (`tag_size: 16`) for deployments where stronger guarantees are required (e.g., broadcast infrastructure, high-value control signals, security-conscious installations). Most stage rigs have no use for 16-byte tags — the real-world threat model doesn't justify the +8 bytes per packet of wire cost. The option exists for completeness and for users who want it.

## Encryption modes

### `none` (key_id = 0x00)

No encryption, no authentication. `tag` is omitted (0 bytes). All fields plaintext. Radio CRC is the only integrity check.

Use cases: bench testing, lowest-latency debug builds, hostile-RF-but-no-malicious-actors environments.

### `mac_only`

Plaintext `event_data`, but `tag` is computed over `AAD || event_data` using a keyed MAC (Poly1305 with a derived key, or HMAC-SHA-256-128). Provides:

- **Tamper detection:** receiver rejects packets with invalid tags.
- **Replay protection:** combined with the seq window check, replays are rejected.
- **No confidentiality:** anyone listening can read MIDI content.

Use cases: stage rigs where MIDI content isn't sensitive but tampering/replay must be prevented.

### `chacha20_poly1305`

Full AEAD. `event_data` is encrypted with ChaCha20; `tag` is Poly1305 over `AAD || ciphertext`. Universal software cipher (works on all chips including F103). 256-bit key.

### `aes_128_ccm`

Full AEAD. `event_data` is encrypted with AES-128-CTR; `tag` is computed via CBC-MAC (CCM construction). 128-bit key. Hardware-accelerated on every wireless MCU we target.

### Nonce construction

The AEAD nonce is constructed at TX and reconstructed at RX from fields the receiver already knows about the sender (after pairing) plus the seq from the packet:

**ChaCha20-Poly1305 (12-byte nonce):**

```
[device_id:4][direction:1][session_seq:4][boot_counter:2][reserved:1=0x00]
```

**AES-128-CCM (13-byte nonce):**

```
[device_id:4][direction:1][session_seq:4][boot_counter:2][reserved:2=0x0000]
```

Field meanings:

- `device_id` (4 bytes): unique per-device identifier. Source on STM32F103: lower 4 bytes of the chip's 96-bit factory unique ID (`0x1FFFF7E8` register). On nRF chips: lower 4 bytes of `FICR.DEVICEID`. May be overridden per-device at pairing time.
- `direction` (1 byte): `0x00` = TX→RX, `0x01` = RX→TX. Reserved for future bidirectional protocols. v1 is one-way (TX→RX only); always `0x00` in v1.
- `session_seq` (4 bytes): from `seq` low half.
- `boot_counter` (2 bytes): from `seq` high half.
- `reserved`: always zero.

Nonce uniqueness guarantee: across the lifetime of any given `(key, device_id)` pair, every packet has a unique nonce as long as `(boot_counter, session_seq)` never repeats. `boot_counter` increments on every boot (persisted to flash) and `session_seq` increments per-packet within a session. If `session_seq` overflows (4.3 billion packets ≈ 50 days at 1000 packets/sec), the device MUST refuse to transmit further packets with the current key until rebooted (forcing a `boot_counter` increment) or until a new key is provisioned.

## Link layer behavior

The protocol envelope is consumed by `osrf-link`. Required link-layer behaviors:

### Sender (TX)

1. Capture event from local source (MIDI parser, etc.).
2. Look up active key entry by local slot; resolve `key_entry` (cipher + key_bytes + key_fp + tx_nonce_counter).
3. Build packet:
   - `ver = 0x01`
   - `key_fp` from active key entry (precomputed SHA-256(cipher_id || key_bytes)[0..2])
   - `seq` = `[boot_counter, session_seq]`; increment `session_seq`
   - `event_type` from event
   - `event_data` from event
   - Compute AEAD over (AAD, plaintext) → ciphertext + tag (or skip for `none`)
4. Push to radio TX FIFO + start TX.
5. If no event has been transmitted in the last 20 ms, transmit a `HEARTBEAT` packet.

### Receiver (RX)

1. Receive packet from radio (radio CRC already validated by hardware; drop if bad).
2. Decode header. If `ver != 0x01`, drop.
3. Look up `key_fp` in local key store by matching the 2-byte fingerprint against each stored entry's precomputed `key_fp`. If no match, drop silently. If multiple entries match (fingerprint collision), try each in order — accept the first whose AEAD tag verifies.
4. **Replay window check:** within a 64-packet sliding window keyed by `device_id`:
   - If `seq < last_seq − 64`, drop (too old).
   - If `seq` already in seen-bitmap, drop (replay).
   - Otherwise mark seq as seen, advance `last_seq` if `seq > last_seq`.
5. **AEAD verification:** reconstruct nonce from `(device_id, direction, seq)`, verify tag over `(AAD, ciphertext)`. If fails, drop silently.
6. **Decrypt** if AEAD mode; otherwise plaintext.
7. **Dispatch by `event_type`:**
   - `HEARTBEAT` → feed watchdog timer; emit nothing to consumer.
   - `MIDI_MESSAGE` → emit `MidiEvent` to consumer.
   - `MIDI_SYSEX_FRAGMENT` → buffer / append / flush per `frag_state`.
   - Unknown event_type within reserved ranges → drop silently (forward compatibility).
8. **Watchdog:** if no packet (including heartbeats) received from a paired sender for 200 ms, fire `LinkLost` → consumer emits all-notes-off + sustains-off + pitch-bend-center to MIDI out.

### Replay window detail

State per peer (`device_id`):

```rust
struct ReplayWindow {
    last_seq: u64,             // highest seq seen (interpret 6-byte seq as u64)
    seen_bitmap: u64,          // bit i = whether (last_seq - i) was seen, i in 0..64
}
```

On packet with sequence `s`:

- If `s > last_seq`: shift `seen_bitmap` left by `(s - last_seq)`, set bit 0, update `last_seq = s`. Accept.
- If `s == last_seq`: bit 0 already set; reject as replay.
- If `s + 64 < last_seq`: too old; reject.
- Otherwise: check `seen_bitmap` bit `(last_seq - s)`. If set, reject as replay; else set the bit, accept.

## Sizes and timing

For a typical Note On (3-byte MIDI):

| Mode | Header (ver+key_fp+seq) | event_type | event_data | tag | Total |
|---|---|---|---|---|---|
| `none` (key_fp=0x000000) | 10 | 1 | 3 | 0 | **14 bytes** |
| `mac_only` (8-byte) | 10 | 1 | 3 | 8 | **22 bytes** |
| `aead` (8-byte tag, default) | 10 | 1 | 3 | 8 | **22 bytes** |
| `aead` (16-byte tag, option) | 10 | 1 | 3 | 16 | **30 bytes** |

Plus radio framing (preamble + sync + length + CRC ≈ 12 bytes) on the air.

At 300 kbps GFSK (v1 default):
- 14 + 12 = 26 B → ~693 µs air time
- 22 + 12 = 34 B → ~907 µs air time
- 30 + 12 = 42 B → ~1120 µs air time

All comfortably within MIDI latency budget.

## Forward compatibility

**Adding new MIDI event types:** assign a new value in the `0x04`–`0x0F` range. Receivers running older firmware that don't recognize the value MUST drop silently (no error to user). Sender uses the new event_type only when it knows the receiver supports it (negotiated at pairing time, or always-on for `MIDI_MESSAGE` / `MIDI_SYSEX_FRAGMENT` since those are v1-mandatory).

**Adding audio (v3):** uses event_type values `0x10`–`0x1F`. Each value defines a specific codec + sample rate + channel count + frame duration. The transport envelope, encryption, and replay protection are unchanged. Audio-specific concerns (jitter buffer, packet loss concealment, frame timing) are handled in a separate audio link layer above this protocol — they're not envelope-format concerns.

**Adding new ciphers:** register a new `cipher_id` value in the local key store. Wire format unchanged (cipher_id is not on the wire). Receivers that don't support the new cipher will fail key lookup and drop the packet.

**Breaking changes:** bump the `ver` byte. Receivers running old firmware drop unknown versions. Major upgrade path requires firmware updates on both ends.

## Test vectors

(To be added during Milestone 4 implementation. Reference encode/decode round-trip vectors will live in `protocols/midi_packet_v1/test_vectors.json` for cross-implementation interop testing — including a future Zephyr+C variant if one is ever written.)

## Open issues / future work

- **Bidirectional links** (`direction = 0x01`): for a future RX→TX channel (telemetry, battery status, link quality reports). Reserved in nonce but not exercised in v1.
- **Key rotation in flight:** v1 has no in-band key rotation. To rotate keys, both sides must be reflashed (hardcoded mode) or re-paired (BLE/USB modes). Future versions could add an in-band `KEY_ROTATE` event_type.
- **Negotiated tag size:** currently a per-key static config. Could be made a per-packet flag in a future version if mixed-tag-size traffic becomes useful.
- **Compression:** none. MIDI compresses poorly at packet sizes this small; the protocol overhead would exceed the savings.
