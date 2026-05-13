// SPDX-License-Identifier: AGPL-3.0-or-later

//! Runtime-mutable key store.
//!
//! Variable-length list of named keys.  Empty entries are not
//! stored — the list is always tightly packed.  Each entry has a
//! 24-bit fingerprint (the low 3 bytes of `SHA-256(cipher_id ‖
//! key_material)`, matching the on-wire `key_fp` header field), a
//! [`CipherId`] selecting which AEAD this key drives, and a user-
//! readable name (≤16 chars).
//!
//! ## "Open" pseudo-entry
//!
//! The UI always shows an extra `"Open"` row at the top of the
//! key list, representing "no encryption" (`active_key_fp = None`).
//! It's not stored in the [`KeyStore`] — the renderer synthesises
//! it.
//!
//! ## Lookup on RX
//!
//! When a packet arrives, the link receiver reads the 24-bit
//! `key_fp` from the header and calls [`KeyStore::find`] to
//! resolve it to a key entry — from which it then loads the
//! cipher choice + material from the flash record indexed by
//! the same fingerprint.  If the fingerprint is `0x000000`, the
//! packet is in the open path.
//!
//! ## Stage 3 status
//!
//! Functional but not yet flash-persistent: `KeyStore::default()`
//! is empty; the boot path will populate it from
//! `sequential-storage::map` once the persistence wiring in
//! `apps/ui_runtime` lands (task #17).

use core::fmt::Write as _;
use heapless::{String, Vec};
use osrf_crypto::CipherId;

/// Maximum number of keys held in the runtime store.  Beyond this,
/// [`KeyStore::add`] returns an error.  Sized for plausible
/// venue / band setups (16 named keys easily covers a multi-act
/// festival).
pub const MAX_KEYS: usize = 16;

/// Maximum length of a user-assigned key name.
pub const MAX_KEY_NAME: usize = 16;

/// Symmetric AEAD key material length in bytes.  Mirrors
/// [`osrf_crypto::KEY_LEN`] — always 32, even for AES-128 (which
/// uses the lower 16) so the on-flash [`KeyRecord`] layout stays
/// uniform across ciphers.
pub const KEY_MATERIAL_LEN: usize = osrf_crypto::KEY_LEN;

/// On-flash representation of a single key store entry.  Fixed size
/// with `#[repr(C)]` so the byte layout is stable across firmware
/// revisions.  Read / written via `sequential-storage::map` keyed
/// by the 24-bit fingerprint.
///
/// Layout (64 bytes total):
///
/// ```text
///   offset  size  field
///   ------  ----  -----
///    0       4    fingerprint (u32 LE; only low 24 bits meaningful, top byte zero)
///    4       1    cipher_id (1 = ChaCha20-Poly1305, 2 = AES-128-CCM)
///    5       1    name_len (0..=MAX_KEY_NAME)
///    6      16    name_bytes (UTF-8, zero-padded, not null-terminated)
///   22      32    key_material (raw symmetric key)
///   54      10    reserved (zero-filled padding for forward compat)
/// ```
///
/// **Note:** the 64-byte total was locked in at the v1 commitment;
/// `cipher_id` was carved out of the original 11-byte reserved
/// block, so a v1 record read by Stage-3 firmware will deserialize
/// with `cipher_id = 0` and `to_entry` will reject it — meaning
/// any pre-Stage-3 keystore record (none in practice; the UI had
/// no add-key flow before) is treated as corrupt and skipped.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KeyRecord {
    /// 24-bit fingerprint in the low bits; top byte zero.  Used as
    /// the storage key (for `sequential-storage::map`) AND as a
    /// sanity check inside the value — if the two disagree on
    /// read, the entry is considered corrupt.
    pub fingerprint: u32,
    /// AEAD cipher discriminator.  Wire-stable u8 — see [`CipherId`].
    /// `0` means "unknown" / corrupt and triggers rejection in
    /// [`Self::to_entry`].
    pub cipher_id: u8,
    /// Valid byte count of [`Self::name_bytes`].
    pub name_len: u8,
    /// Name as UTF-8 bytes.  Not null-terminated; consume with
    /// `&name_bytes[..name_len as usize]`.
    pub name_bytes: [u8; MAX_KEY_NAME],
    /// Raw symmetric key material.  For AES-128-CCM only the lower
    /// 16 bytes are used by the cipher; upper 16 are reserved for
    /// a future AES-256 entry without forcing a layout migration.
    pub key_material: [u8; KEY_MATERIAL_LEN],
    /// Reserved for forward compatibility.  Must be zero on write
    /// today; future versions may use this region for KDF salt,
    /// expiry timestamp, etc.
    pub reserved: [u8; 10],
}

impl KeyRecord {
    /// Construct a record from a runtime [`KeyEntry`] and the
    /// given key material.  The cipher choice comes from the
    /// entry; the caller is responsible for ensuring the material
    /// was generated for that cipher.
    pub fn from_entry(entry: &KeyEntry, material: [u8; KEY_MATERIAL_LEN]) -> Self {
        let mut name_bytes = [0u8; MAX_KEY_NAME];
        let bytes = entry.name.as_bytes();
        let n = bytes.len().min(MAX_KEY_NAME);
        name_bytes[..n].copy_from_slice(&bytes[..n]);
        Self {
            fingerprint: entry.fingerprint & 0x00FF_FFFF,
            cipher_id: entry.cipher as u8,
            name_len: n as u8,
            name_bytes,
            key_material: material,
            reserved: [0; 10],
        }
    }

    /// Lift the name + cipher portion back to a [`KeyEntry`].  Drops
    /// `key_material` since [`KeyEntry`] doesn't carry it — the
    /// material lives in the on-flash record only and is looked
    /// up by fingerprint when the link layer needs it.  Returns
    /// `None` if the name bytes don't decode as UTF-8, `name_len`
    /// is out of bounds, or `cipher_id` doesn't match a known
    /// cipher (treats unknown cipher as corrupt; safer than
    /// guessing).
    pub fn to_entry(&self) -> Option<KeyEntry> {
        let n = self.name_len as usize;
        if n > MAX_KEY_NAME {
            return None;
        }
        let cipher = CipherId::from_u8(self.cipher_id)?;
        let s = core::str::from_utf8(&self.name_bytes[..n]).ok()?;
        let mut name: String<MAX_KEY_NAME> = String::new();
        for c in s.chars().take(MAX_KEY_NAME) {
            let _ = name.push(c);
        }
        Some(KeyEntry {
            fingerprint: self.fingerprint & 0x00FF_FFFF,
            cipher,
            name,
        })
    }
}

/// Compile-time check that the [`KeyRecord`] size matches the
/// documented 64-byte layout.  Bumping any field size / count
/// here without updating the doc comment is a build error.
const _: () = {
    assert!(core::mem::size_of::<KeyRecord>() == 64);
};

/// One key in the runtime store.  Cloned around the UI; the
/// canonical `key_material` lives in a separate secure store
/// (flash region with read-protect bits set), accessed by
/// fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEntry {
    /// 24-bit fingerprint matching the on-wire `key_fp` header
    /// field.  Stored as `u32` for alignment; only the low 24
    /// bits are meaningful.  `0x000000` is reserved for the
    /// "Open" pseudo-entry and must not be assigned to a real
    /// key (the link receiver uses 0 as a sentinel for "no
    /// crypto").
    pub fingerprint: u32,
    /// AEAD cipher this key drives.  Different ciphers under the
    /// same raw key bytes produce different fingerprints (the
    /// `cipher_id` is part of the fingerprint hash) — so the
    /// runtime never has to disambiguate which cipher to use
    /// from key_fp alone.
    pub cipher: CipherId,
    /// User-assigned name.
    pub name: String<MAX_KEY_NAME>,
}

impl KeyEntry {
    /// Format the fingerprint as 6 hex chars, e.g. `"a3f9c1"`.
    pub fn format_fingerprint(&self) -> String<8> {
        let mut out: String<8> = String::new();
        let _ = write!(&mut out, "{:06x}", self.fingerprint & 0x00FF_FFFF);
        out
    }
}

/// Runtime-mutable list of keys.  Held by the host alongside
/// [`crate::Settings`].  Starts empty; the profile boot path
/// populates it via flash-load (see task #17).
#[derive(Debug, Clone, Default)]
pub struct KeyStore {
    entries: Vec<KeyEntry, MAX_KEYS>,
}

impl KeyStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a key to the store.  Returns the fingerprint on
    /// success, or `Err` if the store is full or the fingerprint
    /// is `0x000000` (reserved) or already present.  Names need
    /// not be unique.
    #[allow(clippy::result_unit_err)] // Stage 3 will replace with a typed error.
    pub fn add(&mut self, name: &str, cipher: CipherId, fingerprint: u32) -> Result<u32, ()> {
        let fp = fingerprint & 0x00FF_FFFF;
        if fp == 0 {
            return Err(());
        }
        if self.entries.iter().any(|e| e.fingerprint == fp) {
            return Err(());
        }
        let mut n: String<MAX_KEY_NAME> = String::new();
        for c in name.chars().take(MAX_KEY_NAME) {
            let _ = n.push(c);
        }
        self.entries
            .push(KeyEntry {
                fingerprint: fp,
                cipher,
                name: n,
            })
            .map(|_| fp)
            .map_err(|_| ())
    }

    /// Remove the key with the given fingerprint.  No-op if
    /// nothing matches.
    pub fn remove(&mut self, fingerprint: u32) {
        let fp = fingerprint & 0x00FF_FFFF;
        if let Some(idx) = self.entries.iter().position(|e| e.fingerprint == fp) {
            self.entries.swap_remove(idx);
        }
    }

    /// Look up a key by fingerprint.  Used by the receiver to
    /// resolve incoming `key_fp` values to a cipher choice;
    /// the actual key material comes from the flash record by
    /// the same fingerprint.
    pub fn find(&self, fingerprint: u32) -> Option<&KeyEntry> {
        let fp = fingerprint & 0x00FF_FFFF;
        self.entries.iter().find(|e| e.fingerprint == fp)
    }

    /// Number of keys currently in the store (excludes the
    /// "Open" pseudo-entry that the UI synthesises).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return the entries sorted alphabetically by name into the
    /// caller-provided buffer.  Returns the populated slice.
    /// We use this signature (rather than returning an iterator
    /// over a sorted view) because heapless::Vec doesn't give
    /// us a place to materialise the sort cheaply, and we don't
    /// want to mutate the underlying store.
    pub fn sorted_into<'a>(&self, buf: &'a mut [KeyEntry; MAX_KEYS]) -> &'a [KeyEntry] {
        let n = self.entries.len();
        for (i, e) in self.entries.iter().enumerate() {
            buf[i] = e.clone();
        }
        // Insertion sort — small N, pre-allocated, no heap.
        for i in 1..n {
            let mut j = i;
            while j > 0 && buf[j - 1].name > buf[j].name {
                buf.swap(j - 1, j);
                j -= 1;
            }
        }
        &buf[..n]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_entry() -> KeyEntry {
        KeyEntry {
            fingerprint: 0,
            cipher: CipherId::ChaCha20Poly1305,
            name: String::new(),
        }
    }

    #[test]
    fn add_and_find() {
        let mut store = KeyStore::new();
        let fp = store
            .add("Alice", CipherId::ChaCha20Poly1305, 0x123456)
            .unwrap();
        assert_eq!(fp, 0x123456);
        let found = store.find(0x123456).unwrap();
        assert_eq!(found.name.as_str(), "Alice");
        assert_eq!(found.cipher, CipherId::ChaCha20Poly1305);
    }

    #[test]
    fn add_rejects_zero_fingerprint() {
        let mut store = KeyStore::new();
        assert!(store.add("Open", CipherId::ChaCha20Poly1305, 0).is_err());
    }

    #[test]
    fn add_rejects_duplicate() {
        let mut store = KeyStore::new();
        store.add("A", CipherId::ChaCha20Poly1305, 0xAAAA).unwrap();
        // Duplicate fingerprint even with a different cipher is rejected:
        // the on-wire dispatch is by fingerprint alone, so duplicates
        // would be ambiguous.
        assert!(store.add("B", CipherId::Aes128Ccm, 0xAAAA).is_err());
    }

    #[test]
    fn add_strips_high_byte_of_fingerprint() {
        let mut store = KeyStore::new();
        let fp = store
            .add("X", CipherId::ChaCha20Poly1305, 0xFF12_3456)
            .unwrap();
        assert_eq!(fp, 0x12_3456); // 24-bit only
        assert!(store.find(0xFF12_3456).is_some()); // lookup masks too
    }

    #[test]
    fn remove_works() {
        let mut store = KeyStore::new();
        store.add("A", CipherId::ChaCha20Poly1305, 0xAAAA).unwrap();
        store.remove(0xAAAA);
        assert!(store.find(0xAAAA).is_none());
    }

    #[test]
    fn sorted_by_name() {
        let mut store = KeyStore::new();
        store
            .add("Charlie", CipherId::ChaCha20Poly1305, 0x111)
            .unwrap();
        store
            .add("Alice", CipherId::ChaCha20Poly1305, 0x222)
            .unwrap();
        store.add("Bob", CipherId::ChaCha20Poly1305, 0x333).unwrap();

        let mut buf: [KeyEntry; MAX_KEYS] = core::array::from_fn(|_| empty_entry());
        let sorted = store.sorted_into(&mut buf);
        assert_eq!(sorted[0].name.as_str(), "Alice");
        assert_eq!(sorted[1].name.as_str(), "Bob");
        assert_eq!(sorted[2].name.as_str(), "Charlie");
    }

    #[test]
    fn format_fingerprint_renders_6_hex_chars() {
        let e = KeyEntry {
            fingerprint: 0xa3f9c1,
            cipher: CipherId::ChaCha20Poly1305,
            name: String::new(),
        };
        assert_eq!(e.format_fingerprint().as_str(), "a3f9c1");
    }

    #[test]
    fn key_record_roundtrip_chacha() {
        let mut name: String<MAX_KEY_NAME> = String::new();
        name.push_str("Stage Left").unwrap();
        let entry = KeyEntry {
            fingerprint: 0x12_3456,
            cipher: CipherId::ChaCha20Poly1305,
            name,
        };
        let material = [0xAB; KEY_MATERIAL_LEN];
        let record = KeyRecord::from_entry(&entry, material);
        assert_eq!(record.fingerprint, 0x12_3456);
        assert_eq!(record.cipher_id, CipherId::ChaCha20Poly1305 as u8);
        assert_eq!(record.name_len, "Stage Left".len() as u8);
        assert_eq!(record.key_material, material);
        assert_eq!(record.reserved, [0; 10]);

        let back = record.to_entry().expect("round trips");
        assert_eq!(back.fingerprint, 0x12_3456);
        assert_eq!(back.cipher, CipherId::ChaCha20Poly1305);
        assert_eq!(back.name.as_str(), "Stage Left");
    }

    #[test]
    fn key_record_roundtrip_aes() {
        let mut name: String<MAX_KEY_NAME> = String::new();
        name.push_str("FX rack").unwrap();
        let entry = KeyEntry {
            fingerprint: 0xABCDEF,
            cipher: CipherId::Aes128Ccm,
            name,
        };
        let record = KeyRecord::from_entry(&entry, [0; KEY_MATERIAL_LEN]);
        assert_eq!(record.cipher_id, CipherId::Aes128Ccm as u8);
        let back = record.to_entry().expect("round trips");
        assert_eq!(back.cipher, CipherId::Aes128Ccm);
    }

    #[test]
    fn key_record_rejects_bad_name_len() {
        let record = KeyRecord {
            fingerprint: 0xAABBCC,
            cipher_id: 1,
            name_len: 99, // > MAX_KEY_NAME
            name_bytes: [0; MAX_KEY_NAME],
            key_material: [0; KEY_MATERIAL_LEN],
            reserved: [0; 10],
        };
        assert!(record.to_entry().is_none());
    }

    #[test]
    fn key_record_rejects_unknown_cipher() {
        // cipher_id = 0 = the sentinel for "this record was written
        // by pre-Stage-3 firmware or is corrupt".  Reject.
        let record = KeyRecord {
            fingerprint: 0xAABBCC,
            cipher_id: 0,
            name_len: 1,
            name_bytes: [b'X'; MAX_KEY_NAME],
            key_material: [0; KEY_MATERIAL_LEN],
            reserved: [0; 10],
        };
        assert!(record.to_entry().is_none());
        // cipher_id = 99 = future cipher this firmware doesn't know
        // about.  Also rejected — keeps RX dispatch unambiguous.
        let record = KeyRecord {
            cipher_id: 99,
            ..record
        };
        assert!(record.to_entry().is_none());
    }

    #[test]
    fn key_record_strips_high_byte_of_fingerprint() {
        let mut name: String<MAX_KEY_NAME> = String::new();
        name.push_str("X").unwrap();
        let entry = KeyEntry {
            fingerprint: 0xFF_12_3456,
            cipher: CipherId::ChaCha20Poly1305,
            name,
        };
        let record = KeyRecord::from_entry(&entry, [0; KEY_MATERIAL_LEN]);
        assert_eq!(record.fingerprint, 0x12_3456);
    }
}
