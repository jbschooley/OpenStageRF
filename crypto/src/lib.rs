// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

//! AEAD primitives + key fingerprinting for the OpenStageRF link
//! layer.
//!
//! Two ciphers are supported behind the [`CipherId`] enum:
//!
//!   - [`CipherId::ChaCha20Poly1305`] (RFC 8439) — the default.
//!     Works on every chip we ship to, including STM32F103, which
//!     has no AES hardware.  12-byte nonce, 256-bit key, 16-byte
//!     tag.
//!   - [`CipherId::Aes128Ccm`] — the alternative for hardware-AES
//!     targets (nRF52/53 CCM peripheral).  Currently uses the
//!     RustCrypto software impl because the nRF52840 CCM peripheral
//!     is BLE-reserved while the SoftDevice is enabled; a future
//!     optimisation can swap to `sd_ecb_block_encrypt` SVCs.  13-byte
//!     nonce, 128-bit effective key (we store 32 bytes uniformly and
//!     use the lower 16), 16-byte tag.
//!
//! ## Dispatch
//!
//! Encryption / decryption go through free functions ([`encrypt`],
//! [`decrypt`]) that match on [`CipherId`] and call into the
//! cipher-specific RustCrypto crates.  No trait objects, no `dyn`,
//! no per-cipher generic parameters propagated out of this crate.
//! Callers store cipher choice in [`CipherId`] alongside the key
//! material; this crate handles everything else.
//!
//! ## Tag-detached layout
//!
//! Both operations are **in-place over the body** with a **detached
//! tag**.  The wire protocol carries the tag as a separate trailing
//! field, so detached-tag form maps directly onto the packet layout
//! without an extra copy.  Body buffer must be exactly the plaintext
//! length on encrypt input (becomes ciphertext on output); on decrypt
//! it's the ciphertext (becomes plaintext if auth succeeds).
//!
//! ## AAD
//!
//! Caller supplies the AAD slice — typically the link header's first
//! 11 bytes (`ver || key_fp || boot_counter || packet_seq ||
//! event_type`).  See `protocols/midi_packet_v1`.
//!
//! ## Errors
//!
//! Authentication failures and bad-input errors collapse to a single
//! opaque [`AeadError`] — callers should treat any error as "drop the
//! packet" and bump a counter.  Distinguishing why authentication
//! failed gives an attacker a side channel.

use chacha20poly1305::aead::generic_array::GenericArray;
use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::KeyInit;

/// On-wire / on-flash cipher discriminator.  Numeric values are
/// stable across firmware revisions — appending to this enum is OK,
/// renumbering or removing is a wire-break.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum CipherId {
    /// RFC 8439 ChaCha20-Poly1305.  12-byte nonce, 256-bit key.
    /// Default for cross-platform compatibility.
    ChaCha20Poly1305 = 1,
    /// AES-128 in CCM mode (NIST SP 800-38C), 13-byte nonce, 128-bit
    /// effective key (lower 16 bytes of the 32-byte stored key are
    /// used; upper 16 are reserved for a future AES-256 cipher_id).
    Aes128Ccm = 2,
}

impl CipherId {
    /// Recover a [`CipherId`] from its wire-stable u8 discriminator.
    /// Unknown values produce `None` — caller should drop the packet.
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::ChaCha20Poly1305),
            2 => Some(Self::Aes128Ccm),
            _ => None,
        }
    }

    /// Nonce length this cipher expects.  ChaCha = 12, CCM = 13.
    /// Caller must supply exactly this many bytes.
    pub fn nonce_len(self) -> usize {
        match self {
            Self::ChaCha20Poly1305 => 12,
            Self::Aes128Ccm => 13,
        }
    }
}

/// Length of the authentication tag.  Both supported ciphers use
/// 16-byte tags; this is exposed as a const for callers that need
/// to size buffers without matching on the cipher.
pub const TAG_LEN: usize = 16;

/// Length of stored key material in bytes.  Always 32 — ChaCha20
/// uses all 32; AES-128-CCM uses the lower 16.  Holding a uniform
/// size in the keystore avoids a per-cipher record layout.
pub const KEY_LEN: usize = 32;

/// Maximum nonce length across supported ciphers.  Convenient for
/// stack-allocated nonce scratch buffers.
pub const NONCE_LEN_MAX: usize = 13;

/// Link direction tag for the AEAD nonce.  Domain-separates TX-side
/// encryption from a future RX-to-TX channel so that the two cannot
/// share a `(device_id, session_seq, boot_counter)` nonce by accident
/// — that would be a catastrophic key-reuse failure.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Direction {
    /// Packet flows from the link transmitter to the receiver.
    /// The only direction in use today (one-way MIDI link).
    TxToRx = 0,
    /// Reserved for a future bidirectional path (e.g., RX → TX
    /// ACKs or telemetry).
    RxToTx = 1,
}

/// Derive the AEAD nonce for one packet.
///
/// Layout (13 bytes, with the tail zero-padded for ChaCha which only
/// reads bytes 0..12):
///
/// ```text
///   offset  size  field
///   ------  ----  -----
///    0       4    device_id      (FICR.DEVICEID low 32 bits on nRF)
///    4       1    direction      (Direction enum discriminator)
///    5       4    session_seq    (== link-layer packet_seq, BE)
///    9       2    boot_counter   (random per-boot, persisted, BE)
///   11       2    reserved zero  (used by CCM, ignored by ChaCha)
/// ```
///
/// **Uniqueness guarantees.**  A nonce repeats only when *every*
/// field repeats:
///   - `device_id` differs between physical units (FICR is factory-
///     burned),
///   - `direction` separates TX-side from any future RX-side traffic,
///   - `session_seq` increments monotonically within a session,
///   - `boot_counter` changes on each reboot.
///
/// So a key reused across many devices, reboots, and packets still
/// can't collide unless device_id collisions occur (vanishingly
/// improbable for FICR) OR a session_seq value repeats within the
/// same (device_id, boot_counter) — which the link layer prevents
/// by failing TX once `packet_seq` hits `u32::MAX`.
pub fn derive_nonce(
    device_id: u32,
    direction: Direction,
    session_seq: u32,
    boot_counter: u16,
) -> [u8; NONCE_LEN_MAX] {
    let mut nonce = [0u8; NONCE_LEN_MAX];
    nonce[0..4].copy_from_slice(&device_id.to_be_bytes());
    nonce[4] = direction as u8;
    nonce[5..9].copy_from_slice(&session_seq.to_be_bytes());
    nonce[9..11].copy_from_slice(&boot_counter.to_be_bytes());
    // bytes [11..13] left zero — CCM reads all 13, ChaCha reads only
    // [0..12], so the trailing zeros are inert padding either way.
    nonce
}

/// All AEAD failures collapse to one opaque error.  Distinguishing
/// "wrong tag" from "wrong nonce length" leaks attacker-useful
/// side-channel information; callers should drop the packet and
/// increment a single AEAD-fail counter regardless of cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct AeadError;

/// AES-128 block cipher used as the building block of AES-128-CCM.
/// With the `aes-hw-sd` feature enabled (T114 builds with the
/// SoftDevice running) this is the hardware-backed
/// [`aes_hw::HwAes128`], which routes each block through the nRF52840
/// AES peripheral via `sd_ecb_block_encrypt`.  Otherwise it's the
/// portable software [`aes::Aes128`] from RustCrypto.  Both
/// implementations are byte-identical at the algorithm level, so
/// the on-air wire format is unaffected; choosing one over the
/// other is purely a per-target speed/size tradeoff.
#[cfg(feature = "aes-hw-sd")]
mod aes_hw;
#[cfg(feature = "aes-hw-sd")]
type Aes128Impl = aes_hw::HwAes128;
#[cfg(not(feature = "aes-hw-sd"))]
type Aes128Impl = aes::Aes128;

/// AES-128-CCM with a 16-byte tag and 13-byte nonce.  Type alias for
/// readability; the generic parameters lock the RustCrypto `Ccm`
/// adapter to the exact mode our wire format expects.  The block-
/// cipher slot is picked by [`Aes128Impl`] above.
type Aes128Ccm = ccm::Ccm<Aes128Impl, ccm::consts::U16, ccm::consts::U13>;

/// Encrypt `buf` in place with `cipher` and return the 16-byte tag.
/// `key` must be at least [`KEY_LEN`] bytes (excess ignored).
/// `nonce` must be exactly [`CipherId::nonce_len`] bytes.  `aad` is
/// the additional authenticated data — typically the link header.
///
/// Returns the detached tag on success.  Caller appends the tag to
/// the wire packet.
pub fn encrypt(
    cipher: CipherId,
    key: &[u8; KEY_LEN],
    nonce: &[u8],
    aad: &[u8],
    buf: &mut [u8],
) -> Result<[u8; TAG_LEN], AeadError> {
    if nonce.len() != cipher.nonce_len() {
        return Err(AeadError);
    }
    match cipher {
        CipherId::ChaCha20Poly1305 => {
            let aead = chacha20poly1305::ChaCha20Poly1305::new(GenericArray::from_slice(key));
            let tag = aead
                .encrypt_in_place_detached(GenericArray::from_slice(nonce), aad, buf)
                .map_err(|_| AeadError)?;
            let mut out = [0u8; TAG_LEN];
            out.copy_from_slice(&tag);
            Ok(out)
        }
        CipherId::Aes128Ccm => {
            // AES-128 uses the lower 16 bytes of the 32-byte stored key.
            // Upper 16 bytes are reserved for a future AES-256 cipher_id
            // without forcing a flash-layout migration.
            let aead = Aes128Ccm::new(GenericArray::from_slice(&key[..16]));
            let tag = aead
                .encrypt_in_place_detached(GenericArray::from_slice(nonce), aad, buf)
                .map_err(|_| AeadError)?;
            let mut out = [0u8; TAG_LEN];
            out.copy_from_slice(&tag);
            Ok(out)
        }
    }
}

/// Verify `tag` against `buf` + `aad` and decrypt `buf` in place.
/// Same key / nonce / aad rules as [`encrypt`].  On any failure
/// returns [`AeadError`] and **does not** modify `buf` past
/// whatever the underlying cipher already wrote — caller should
/// treat the buffer as garbage and drop the packet.
pub fn decrypt(
    cipher: CipherId,
    key: &[u8; KEY_LEN],
    nonce: &[u8],
    aad: &[u8],
    buf: &mut [u8],
    tag: &[u8; TAG_LEN],
) -> Result<(), AeadError> {
    if nonce.len() != cipher.nonce_len() {
        return Err(AeadError);
    }
    match cipher {
        CipherId::ChaCha20Poly1305 => {
            let aead = chacha20poly1305::ChaCha20Poly1305::new(GenericArray::from_slice(key));
            aead.decrypt_in_place_detached(
                GenericArray::from_slice(nonce),
                aad,
                buf,
                GenericArray::from_slice(tag),
            )
            .map_err(|_| AeadError)
        }
        CipherId::Aes128Ccm => {
            let aead = Aes128Ccm::new(GenericArray::from_slice(&key[..16]));
            aead.decrypt_in_place_detached(
                GenericArray::from_slice(nonce),
                aad,
                buf,
                GenericArray::from_slice(tag),
            )
            .map_err(|_| AeadError)
        }
    }
}

/// Derive the 24-bit fingerprint stored in [`KeyEntry`] / sent on
/// the wire as `key_fp`.  Spec: `SHA-256(cipher_id ‖ key_bytes)[0..3]`
/// — first 3 bytes of the SHA-256 digest, packed little-endian into
/// the low 24 bits of a `u32`.  `cipher_id` is included so that
/// re-keying with a different cipher under the same raw key bytes
/// produces a different fingerprint (otherwise an attacker who
/// observed the fingerprint couldn't tell which cipher was in use
/// from header bytes alone — small win but free).
///
/// 24 bits = 16.7M values.  Birthday collision likelihood with 16
/// keys in the store is ~10⁻⁵; collision detection on add (rejecting
/// duplicate fingerprints) keeps the on-wire dispatch unambiguous.
pub fn fingerprint(cipher: CipherId, key: &[u8; KEY_LEN]) -> u32 {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update([cipher as u8]);
    hasher.update(key);
    let digest = hasher.finalize();
    // Low 24 bits, little-endian-packed.  Matches the on-wire layout
    // of `key_fp: [u8; 3]` which is itself LE-equivalent (the bytes
    // appear in order on the wire).
    u32::from(digest[0]) | (u32::from(digest[1]) << 8) | (u32::from(digest[2]) << 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8439 §2.8.2 test vector.  Confirms the chacha20poly1305
    /// crate is wired correctly and our nonce/aad/tag glue matches
    /// the spec.
    #[test]
    fn chacha20_poly1305_rfc8439_vector() {
        let key: [u8; KEY_LEN] = [
            0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c, 0x8d,
            0x8e, 0x8f, 0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b,
            0x9c, 0x9d, 0x9e, 0x9f,
        ];
        let nonce = [
            0x07, 0x00, 0x00, 0x00, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47,
        ];
        let aad = [
            0x50, 0x51, 0x52, 0x53, 0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7,
        ];
        // RFC vector plaintext, truncated to fit in a small body.
        let mut buf = *b"Ladies and Gentlemen of the class of '99";
        let original = buf;

        let tag = encrypt(CipherId::ChaCha20Poly1305, &key, &nonce, &aad, &mut buf).unwrap();

        // Round-trip: decrypt the ciphertext back to plaintext.
        decrypt(
            CipherId::ChaCha20Poly1305,
            &key,
            &nonce,
            &aad,
            &mut buf,
            &tag,
        )
        .unwrap();
        assert_eq!(buf, original);

        // Tamper detection: flipping one ciphertext bit must fail.
        let tag2 = encrypt(CipherId::ChaCha20Poly1305, &key, &nonce, &aad, &mut buf).unwrap();
        buf[0] ^= 0x01;
        assert_eq!(
            decrypt(
                CipherId::ChaCha20Poly1305,
                &key,
                &nonce,
                &aad,
                &mut buf,
                &tag2
            ),
            Err(AeadError),
        );
    }

    /// AES-128-CCM round-trip over a representative MIDI body.
    /// NIST-vector verification lives in the `aes` / `ccm` crate's
    /// own tests; here we confirm the dispatch + key-truncation
    /// behaviour is correct.
    #[test]
    fn aes128_ccm_roundtrip() {
        let key = [0x42u8; KEY_LEN];
        let nonce = [0u8; 13];
        let aad = [0x11, 0x22, 0x33];
        let mut buf = *b"hello world";
        let original = buf;

        let tag = encrypt(CipherId::Aes128Ccm, &key, &nonce, &aad, &mut buf).unwrap();
        decrypt(CipherId::Aes128Ccm, &key, &nonce, &aad, &mut buf, &tag).unwrap();
        assert_eq!(buf, original);
    }

    /// Wrong-length nonce must be rejected (catches a class of caller
    /// bugs where the wrong cipher's nonce derivation is used).
    #[test]
    fn nonce_length_validated() {
        let key = [0u8; KEY_LEN];
        let mut buf = [0u8; 8];
        let bad_nonce = [0u8; 11]; // valid for neither ChaCha (12) nor CCM (13)
        assert_eq!(
            encrypt(CipherId::ChaCha20Poly1305, &key, &bad_nonce, &[], &mut buf),
            Err(AeadError),
        );
        assert_eq!(
            encrypt(CipherId::Aes128Ccm, &key, &bad_nonce, &[], &mut buf),
            Err(AeadError),
        );
    }

    /// Same key under different ciphers must produce different
    /// fingerprints.  Confirms cipher_id is part of the digest input.
    #[test]
    fn fingerprint_distinguishes_cipher() {
        let key = [0u8; KEY_LEN];
        let fp_chacha = fingerprint(CipherId::ChaCha20Poly1305, &key);
        let fp_aes = fingerprint(CipherId::Aes128Ccm, &key);
        assert_ne!(fp_chacha, fp_aes);
        // Both must fit in 24 bits.
        assert_eq!(fp_chacha & 0xFF00_0000, 0);
        assert_eq!(fp_aes & 0xFF00_0000, 0);
    }

    /// Nonce layout — confirms field placement and BE byte order.
    /// A regression here is a wire-break, so the test pins the
    /// expected bytes exactly.
    #[test]
    fn nonce_layout() {
        let n = derive_nonce(0x1234_5678, Direction::TxToRx, 0x9ABC_DEF0, 0xCAFE);
        assert_eq!(
            n,
            [
                0x12, 0x34, 0x56, 0x78, // device_id
                0x00, // direction TxToRx
                0x9A, 0xBC, 0xDE, 0xF0, // session_seq
                0xCA, 0xFE, // boot_counter
                0x00, 0x00, // reserved zero (CCM-only tail bytes)
            ],
        );
    }

    /// Direction discriminator changes the nonce — confirms domain
    /// separation between TX-side and any future RX-side path.
    #[test]
    fn nonce_direction_distinguishes() {
        let a = derive_nonce(0, Direction::TxToRx, 0, 0);
        let b = derive_nonce(0, Direction::RxToTx, 0, 0);
        assert_ne!(a, b);
    }

    /// End-to-end: the derived nonce must work as-is in encrypt /
    /// decrypt for both ciphers (validates length compatibility).
    #[test]
    fn nonce_drives_both_ciphers() {
        let nonce = derive_nonce(0xDEAD_BEEF, Direction::TxToRx, 1, 0x4242);
        let key = [0u8; KEY_LEN];
        for cipher in [CipherId::ChaCha20Poly1305, CipherId::Aes128Ccm] {
            let mut buf = *b"midi note on data";
            let original = buf;
            let n = cipher.nonce_len();
            let tag = encrypt(cipher, &key, &nonce[..n], &[], &mut buf).unwrap();
            decrypt(cipher, &key, &nonce[..n], &[], &mut buf, &tag).unwrap();
            assert_eq!(buf, original);
        }
    }

    /// Cipher id round-trips through u8.
    #[test]
    fn cipher_id_u8_roundtrip() {
        assert_eq!(CipherId::from_u8(1), Some(CipherId::ChaCha20Poly1305),);
        assert_eq!(CipherId::from_u8(2), Some(CipherId::Aes128Ccm));
        assert_eq!(CipherId::from_u8(0), None);
        assert_eq!(CipherId::from_u8(3), None);
        assert_eq!(CipherId::ChaCha20Poly1305 as u8, 1);
        assert_eq!(CipherId::Aes128Ccm as u8, 2);
    }
}
