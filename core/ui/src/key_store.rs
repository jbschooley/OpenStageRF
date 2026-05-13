// SPDX-License-Identifier: AGPL-3.0-or-later

//! Runtime-mutable key store.
//!
//! Variable-length list of named keys.  Empty entries are not
//! stored — the list is always tightly packed.  Each entry has a
//! 24-bit fingerprint (the low 3 bytes of `SHA-256(key_material)`,
//! matching the on-wire `key_fp` header field) and a user-readable
//! name (≤16 chars).
//!
//! ## "Open" pseudo-entry
//!
//! The UI always shows an extra `"Open"` row at the top of the
//! key list, representing "no encryption" (`active_key_fp = None`).
//! It's not stored in the [`KeyStore`] — the renderer synthesises
//! it.  Users can always pick it; v1 (no AEAD) only ever uses it.
//!
//! ## Lookup on RX
//!
//! When a packet arrives, the link receiver reads the 24-bit
//! `key_fp` from the header and calls [`KeyStore::find`] to
//! resolve it to key material.  If the fingerprint is `0x000000`,
//! the packet is in the open path; otherwise the receiver looks
//! up the key and decrypts.
//!
//! ## v1 status
//!
//! Stub.  `KeyStore::default()` is empty, and the UI offers no
//! way to add keys.  When AEAD lands (Stage 3 in ROADMAP.md),
//! a "+ Add Key" entry will appear on the KeySelect screen and
//! invoke a key-generation / -import flow.

use core::fmt::Write as _;
use heapless::{String, Vec};

/// Maximum number of keys held in the runtime store.  Beyond this,
/// [`KeyStore::add`] returns an error.  Sized for plausible
/// venue / band setups (16 named keys easily covers a multi-act
/// festival).
pub const MAX_KEYS: usize = 16;

/// Maximum length of a user-assigned key name.
pub const MAX_KEY_NAME: usize = 16;

/// Symmetric AEAD key material length in bytes.  256-bit because
/// that's what ChaCha20-Poly1305 (the Stage-3 candidate) needs;
/// AES-256-GCM uses the same.  Locked here so the on-flash
/// [`KeyRecord`] format doesn't shift when AEAD wiring lands.
pub const KEY_MATERIAL_LEN: usize = 32;

/// On-flash representation of a single key store entry.  Fixed size
/// with `#[repr(C)]` so the byte layout is stable across firmware
/// revisions — this is the v1 commitment that Stage 3 AEAD work
/// won't have to migrate around.  Read / written via
/// `sequential-storage::map` keyed by the 24-bit fingerprint.
///
/// Layout (64 bytes total):
///
/// ```text
///   offset  size  field
///   ------  ----  -----
///    0       4    fingerprint (u32 LE; only low 24 bits meaningful, top byte zero)
///    4       1    name_len (0..=MAX_KEY_NAME)
///    5      16    name_bytes (UTF-8, zero-padded, not null-terminated)
///   21      32    key_material (raw symmetric key; zeroed in v1)
///   53      11    reserved (zero-filled padding for forward compat)
/// ```
///
/// **v1 status:** [`KeyStore`] doesn't expose any way to populate
/// `key_material` — the UI offers no add-key flow and AEAD isn't
/// wired into the link layer.  Records are not pushed to flash by
/// any current code path.  When Stage 3 lands:
///
/// 1. UI gains an Add Key flow that generates / imports key
///    material and pushes a [`KeyRecord`] to flash.
/// 2. Boot path reads all records via [`KeyStore::load_from_flash`]
///    (wiring TBD by the profile) and populates a runtime
///    [`KeyStore`].
/// 3. The link layer's AEAD step looks up `key_material` by
///    fingerprint via [`KeyStore::find`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KeyRecord {
    /// 24-bit fingerprint in the low bits; top byte zero.  Used as
    /// the storage key (for `sequential-storage::map`) AND as a
    /// sanity check inside the value — if the two disagree on
    /// read, the entry is considered corrupt.
    pub fingerprint: u32,
    /// Valid byte count of [`Self::name_bytes`].
    pub name_len: u8,
    /// Name as UTF-8 bytes.  Not null-terminated; consume with
    /// `&name_bytes[..name_len as usize]`.
    pub name_bytes: [u8; MAX_KEY_NAME],
    /// Raw symmetric key material.  Zeroed until Stage 3 AEAD
    /// lands.  The on-wire `key_fp` header field already carries
    /// the fingerprint; the receiver looks up the material here.
    pub key_material: [u8; KEY_MATERIAL_LEN],
    /// Reserved for forward compatibility.  Must be zero on write
    /// today; future versions may use this region for a
    /// `key_type` discriminator, KDF salt, etc.
    pub reserved: [u8; 11],
}

impl KeyRecord {
    /// Construct a record from a runtime [`KeyEntry`] with the
    /// given key material.  v1 callers pass `[0; 32]` — the
    /// material slot is reserved but unused until AEAD lands.
    pub fn from_entry(entry: &KeyEntry, material: [u8; KEY_MATERIAL_LEN]) -> Self {
        let mut name_bytes = [0u8; MAX_KEY_NAME];
        let bytes = entry.name.as_bytes();
        let n = bytes.len().min(MAX_KEY_NAME);
        name_bytes[..n].copy_from_slice(&bytes[..n]);
        Self {
            fingerprint: entry.fingerprint & 0x00FF_FFFF,
            name_len: n as u8,
            name_bytes,
            key_material: material,
            reserved: [0; 11],
        }
    }

    /// Lift the name portion back to a [`KeyEntry`].  Drops
    /// `key_material` since [`KeyEntry`] doesn't carry it — the
    /// material lives in the on-flash record only and is looked
    /// up by fingerprint when the link layer needs it.  Returns
    /// `None` if the name bytes don't decode as UTF-8 or
    /// `name_len` is out of bounds.
    pub fn to_entry(&self) -> Option<KeyEntry> {
        let n = self.name_len as usize;
        if n > MAX_KEY_NAME {
            return None;
        }
        let s = core::str::from_utf8(&self.name_bytes[..n]).ok()?;
        let mut name: String<MAX_KEY_NAME> = String::new();
        for c in s.chars().take(MAX_KEY_NAME) {
            let _ = name.push(c);
        }
        Some(KeyEntry {
            fingerprint: self.fingerprint & 0x00FF_FFFF,
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
/// [`crate::Settings`].  v1 starts empty; future work populates
/// it via key generation / import / flash-load on boot.
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
    pub fn add(&mut self, name: &str, fingerprint: u32) -> Result<u32, ()> {
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
    /// resolve incoming `key_fp` values.
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

    #[test]
    fn add_and_find() {
        let mut store = KeyStore::new();
        let fp = store.add("Alice", 0x123456).unwrap();
        assert_eq!(fp, 0x123456);
        let found = store.find(0x123456).unwrap();
        assert_eq!(found.name.as_str(), "Alice");
    }

    #[test]
    fn add_rejects_zero_fingerprint() {
        let mut store = KeyStore::new();
        assert!(store.add("Open", 0).is_err());
    }

    #[test]
    fn add_rejects_duplicate() {
        let mut store = KeyStore::new();
        store.add("A", 0xAAAA).unwrap();
        assert!(store.add("B", 0xAAAA).is_err());
    }

    #[test]
    fn add_strips_high_byte_of_fingerprint() {
        let mut store = KeyStore::new();
        let fp = store.add("X", 0xFF12_3456).unwrap();
        assert_eq!(fp, 0x12_3456); // 24-bit only
        assert!(store.find(0xFF12_3456).is_some()); // lookup masks too
    }

    #[test]
    fn remove_works() {
        let mut store = KeyStore::new();
        store.add("A", 0xAAAA).unwrap();
        store.remove(0xAAAA);
        assert!(store.find(0xAAAA).is_none());
    }

    #[test]
    fn sorted_by_name() {
        let mut store = KeyStore::new();
        store.add("Charlie", 0x111).unwrap();
        store.add("Alice", 0x222).unwrap();
        store.add("Bob", 0x333).unwrap();

        let mut buf: [KeyEntry; MAX_KEYS] = core::array::from_fn(|_| KeyEntry {
            fingerprint: 0,
            name: String::new(),
        });
        let sorted = store.sorted_into(&mut buf);
        assert_eq!(sorted[0].name.as_str(), "Alice");
        assert_eq!(sorted[1].name.as_str(), "Bob");
        assert_eq!(sorted[2].name.as_str(), "Charlie");
    }

    #[test]
    fn format_fingerprint_renders_6_hex_chars() {
        let e = KeyEntry {
            fingerprint: 0xa3f9c1,
            name: String::new(),
        };
        assert_eq!(e.format_fingerprint().as_str(), "a3f9c1");
    }

    #[test]
    fn key_record_roundtrip() {
        let mut name: String<MAX_KEY_NAME> = String::new();
        name.push_str("Stage Left").unwrap();
        let entry = KeyEntry {
            fingerprint: 0x12_3456,
            name,
        };
        let material = [0xAB; KEY_MATERIAL_LEN];
        let record = KeyRecord::from_entry(&entry, material);
        assert_eq!(record.fingerprint, 0x12_3456);
        assert_eq!(record.name_len, "Stage Left".len() as u8);
        assert_eq!(record.key_material, material);
        assert_eq!(record.reserved, [0; 11]);

        let back = record.to_entry().expect("round trips");
        assert_eq!(back.fingerprint, 0x12_3456);
        assert_eq!(back.name.as_str(), "Stage Left");
    }

    #[test]
    fn key_record_rejects_bad_name_len() {
        let record = KeyRecord {
            fingerprint: 0xAABBCC,
            name_len: 99, // > MAX_KEY_NAME
            name_bytes: [0; MAX_KEY_NAME],
            key_material: [0; KEY_MATERIAL_LEN],
            reserved: [0; 11],
        };
        assert!(record.to_entry().is_none());
    }

    #[test]
    fn key_record_strips_high_byte_of_fingerprint() {
        let mut name: String<MAX_KEY_NAME> = String::new();
        name.push_str("X").unwrap();
        let entry = KeyEntry {
            fingerprint: 0xFF_12_3456,
            name,
        };
        let record = KeyRecord::from_entry(&entry, [0; KEY_MATERIAL_LEN]);
        assert_eq!(record.fingerprint, 0x12_3456);
    }
}
