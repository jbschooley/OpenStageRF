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
    pub fn sorted_into<'a>(
        &self,
        buf: &'a mut [KeyEntry; MAX_KEYS],
    ) -> &'a [KeyEntry] {
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
}
