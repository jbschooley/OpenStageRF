// SPDX-License-Identifier: AGPL-3.0-or-later

//! nRF52840 hardware-AES backend, accessed via the SoftDevice's
//! `sd_ecb_block_encrypt` SVC.
//!
//! The nRF52840 has a dedicated AES-128 ECB peripheral, but while
//! the SoftDevice is enabled the peripheral is owned by the SD (BLE
//! link-layer encryption uses it).  Direct register pokes fault.
//! `sd_ecb_block_encrypt` is the SD-mediated entry point: pass a
//! `nrf_ecb_hal_data_t` struct (16-byte key + 16-byte cleartext +
//! 16-byte ciphertext-out slot), the SVC runs the AES core
//! synchronously and writes the ciphertext.  Cost is ~25 µs per
//! call (10 µs SVC overhead + ~7 µs AES + the SD's internal
//! serialisation).  Software AES-128 on Cortex-M4 at 64 MHz is
//! ~50 µs per block — savings are real but modest; the win is
//! biggest on multi-block bodies (CCM mode runs 2× block-encrypts
//! per body block).
//!
//! We implement the RustCrypto [`cipher`] traits so `ccm::Ccm<…>`
//! transparently uses this backend instead of `aes::Aes128`.  The
//! algorithm is byte-identical so wire-format compatibility is
//! preserved; a software-AES TX and a hardware-AES RX
//! interoperate.
//!
//! ## Threading — known constraint
//!
//! ARM Cortex-M's SVC instruction generates the SVCall exception.
//! If the calling context's priority is **higher** than the SVCall
//! handler's priority, the chip cannot escalate and HardFaults
//! instead.  Nordic's SoftDevice (S140) configures SVCall at
//! priority **4**.  Our T114 link-runtime task runs on
//! `EXECUTOR_LINK` (bound to SWI0_EGU0) at priority **2** — higher
//! than 4 → SVC instruction faults the chip on the first call.
//!
//! As a result this backend is **not currently wired into any
//! shipping profile** (the `aes-hw-sd` feature line is commented out
//! in `profiles/t114_*/Cargo.toml`).  The crate keeps the
//! implementation for two reasons: (a) it's correct and would work
//! from any thread-mode or P5+ context, (b) Stage 4 hardware
//! (nRF5340 + CryptoCell CC312) likely changes the SVC-priority
//! story.
//!
//! If you want to re-enable hardware AES on the T114 today, the
//! options are:
//!   1. Drop the link-runtime executor to priority P5 or P6 (loses
//!      the radio-IRQ → packet preemption that motivated P2);
//!   2. Defer AES calls to the thread-mode main task via a channel
//!      (latency hit per encrypted packet);
//!   3. Skip SVC entirely and access the AES peripheral directly
//!      when the SoftDevice is *disabled* — only viable on radio-
//!      only / non-BLE builds where the SD isn't owning the
//!      peripheral.
//!
//! Software AES on Cortex-M4 at 64 MHz costs ~50 µs per block.  For
//! our typical 37-byte AEAD body that's ~250 µs total, comfortably
//! invisible at MIDI cadences — the hardware path is a checkmark,
//! not a feature.

use cipher::consts::{U1, U16};
use cipher::generic_array::GenericArray;
use cipher::inout::InOut;
use cipher::{
    Block, BlockBackend, BlockCipher, BlockClosure, BlockEncrypt, BlockSizeUser, Key, KeyInit,
    KeySizeUser, ParBlocksSizeUser,
};
use nrf_softdevice_s140 as sd;

/// AES-128 block cipher backed by the SoftDevice's
/// `sd_ecb_block_encrypt` SVC.  Drop-in for `aes::Aes128` from the
/// perspective of [`ccm::Ccm`] — same trait surface, same block
/// size, same key size, same algorithm.
#[derive(Clone)]
pub struct HwAes128 {
    key: [u8; 16],
}

impl KeySizeUser for HwAes128 {
    type KeySize = U16;
}

impl BlockSizeUser for HwAes128 {
    type BlockSize = U16;
}

impl BlockCipher for HwAes128 {}

impl KeyInit for HwAes128 {
    fn new(key: &Key<Self>) -> Self {
        let mut k = [0u8; 16];
        k.copy_from_slice(key.as_slice());
        Self { key: k }
    }
}

impl BlockEncrypt for HwAes128 {
    fn encrypt_with_backend(&self, f: impl BlockClosure<BlockSize = U16>) {
        let mut backend = HwAes128Backend { key: &self.key };
        f.call(&mut backend);
    }
}

/// Per-call backend.  Holds a borrow on the key so we don't have to
/// re-copy 16 bytes into the `sd_ecb_block_encrypt` argument struct
/// for every block — but we DO have to re-copy into the SVC's input
/// slot each call because the SD layout pins the key alongside the
/// per-block plaintext.
struct HwAes128Backend<'a> {
    key: &'a [u8; 16],
}

impl BlockSizeUser for HwAes128Backend<'_> {
    type BlockSize = U16;
}

impl ParBlocksSizeUser for HwAes128Backend<'_> {
    // The SVC is one-block-at-a-time.  CCM doesn't benefit from
    // parallel-block hints on this hardware (each SVC call is
    // independent); declare ParBlocksSize = 1 so RustCrypto falls
    // back to the per-block path.
    type ParBlocksSize = U1;
}

/// 4-byte aligned wrapper around `nrf_ecb_hal_data_t`.  The SD's
/// `sd_ecb_block_encrypt` documentation states the pointer **must**
/// be 4-byte aligned; bindgen generates the underlying struct as
/// `align(1)` (all fields are `u8` arrays), so without this wrapper
/// a stack-allocated instance is alignment-1 and the SD's internal
/// 4-byte-wide access faults the chip.  Manifested as: first
/// encrypted packet after picking AES crashes the board, panic-stage
/// reboots, settings flash still has AES selected → boot loop.
#[repr(C, align(4))]
struct AlignedEcbData(sd::nrf_ecb_hal_data_t);

impl BlockBackend for HwAes128Backend<'_> {
    fn proc_block(&mut self, mut block: InOut<'_, '_, Block<Self>>) {
        // SAFETY: zero-init is a valid `nrf_ecb_hal_data_t` (all
        // fields are byte arrays).  The SVC reads `key` + `cleartext`
        // and writes `ciphertext` only; no aliasing or out-of-bounds
        // access is possible from app code.
        let mut aligned: AlignedEcbData = unsafe { core::mem::zeroed() };
        aligned.0.key.copy_from_slice(self.key);
        aligned
            .0
            .cleartext
            .copy_from_slice(block.get_in().as_slice());

        // SAFETY: `&mut aligned.0` is a unique mutable reference,
        // 4-byte aligned (per `AlignedEcbData`'s `repr(align(4))`),
        // and lives for the duration of the SVC call.
        let err = unsafe { sd::sd_ecb_block_encrypt(&mut aligned.0 as *mut _) };
        if err != 0 {
            // `sd_ecb_block_encrypt` only fails on null pointer
            // (impossible here) per the SD docs.  Treat as fatal:
            // a silent miscompute would break authentication on the
            // RX side and we'd rather panic-stage + reset than ship
            // junk.
            #[cfg(feature = "defmt")]
            defmt::error!("sd_ecb_block_encrypt failed: err={=u32}", err);
            panic!("sd_ecb_block_encrypt failed");
        }

        let out: &mut GenericArray<u8, U16> = block.get_out();
        out.copy_from_slice(&aligned.0.ciphertext);
    }
}
