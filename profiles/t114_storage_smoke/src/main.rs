// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! Flash-persistence smoke test (M7 foundation work).
//!
//! Verifies the end-to-end stack from `boards/t114/src/storage.rs` →
//! `nrf-softdevice::Flash` → `sequential-storage::map`.  On boot:
//!
//!   1. Enable SoftDevice (RAM origin / IRQ priorities normal).
//!   2. Take the singleton `Flash`.
//!   3. Read a `u32` keyed `0` (a "boot-count") from the Settings
//!      region — `None` on the very first run, `Some(n)` thereafter.
//!   4. Log the previous value, increment, write back.
//!   5. Loop forever logging "boot N alive @ Ts" once a second so
//!      the operator can confirm the chip isn't crashed.
//!
//! Verification procedure:
//!
//!   - Flash this profile via `cargo run …`.  First boot logs
//!     `previous = None`, `new = 1`.
//!   - Hit the reset button (or `probe-rs reset`).  Next boot logs
//!     `previous = Some(1)`, `new = 2`.
//!   - Power-cycle the battery (probe disconnected).  Next boot
//!     logs `previous = Some(2)`, `new = 3`.
//!   - That third step proves write durability across true power
//!     loss — i.e., the data lived through the cell discharge, not
//!     just through the probe-managed reset.

use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_time::Timer;
use osrf_board_t114 as board;
use panic_probe as _;
use sequential_storage::cache::NoCache;
use sequential_storage::map;

#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    board::bootloader_handoff();
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    defmt::info!("storage_smoke: booting");

    let _r = board::resources();
    let sd = board::softdevice::enable();
    spawner
        .spawn(board::softdevice::run(sd).expect("alloc softdevice run task"));

    // Small settling delay so the SD's first event loop tick has
    // happened before we take Flash.  Empirically not strictly
    // needed on this SD version but it's a 50 µs cost for
    // robustness.
    Timer::after_millis(10).await;

    let mut flash = board::storage::flash(sd);

    // `sequential-storage::map` needs a small scratch buffer sized
    // to hold one record + alignment overhead.  64 bytes is plenty
    // for our u32 value + small overhead.
    let mut buf = [0u8; 64];
    let mut cache = NoCache::new();

    // Key `0u8` → boot count.  In real M7 work each field will have
    // its own key (an enum variant) but for the smoke test a single
    // counter is enough to prove persistence.
    let prev_count: Option<u32> = match map::fetch_item::<u8, u32, _>(
        &mut flash,
        board::storage::SETTINGS_RANGE,
        &mut cache,
        &mut buf,
        &0u8,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            defmt::error!("storage_smoke: fetch_item failed: {:?}", defmt::Debug2Format(&e));
            None
        }
    };

    let new_count: u32 = prev_count.unwrap_or(0).wrapping_add(1);
    defmt::info!(
        "storage_smoke: previous boot count = {:?}, writing new = {}",
        prev_count,
        new_count
    );

    match map::store_item::<u8, u32, _>(
        &mut flash,
        board::storage::SETTINGS_RANGE,
        &mut cache,
        &mut buf,
        &0u8,
        &new_count,
    )
    .await
    {
        Ok(()) => defmt::info!("storage_smoke: write OK"),
        Err(e) => defmt::error!(
            "storage_smoke: store_item failed: {:?}",
            defmt::Debug2Format(&e)
        ),
    }

    // Read back immediately to confirm the write took.
    match map::fetch_item::<u8, u32, _>(
        &mut flash,
        board::storage::SETTINGS_RANGE,
        &mut cache,
        &mut buf,
        &0u8,
    )
    .await
    {
        Ok(Some(v)) if v == new_count => {
            defmt::info!("storage_smoke: readback confirms new value = {}", v)
        }
        Ok(v) => defmt::warn!(
            "storage_smoke: readback mismatch — expected {}, got {:?}",
            new_count,
            v
        ),
        Err(e) => defmt::error!(
            "storage_smoke: readback failed: {:?}",
            defmt::Debug2Format(&e)
        ),
    }

    // Idle forever, logging every 5 s so the operator can confirm
    // the firmware is still alive after the flash work.
    let mut tick: u32 = 0;
    loop {
        Timer::after_secs(5).await;
        tick = tick.wrapping_add(1);
        defmt::info!(
            "storage_smoke: alive (boot {} tick {})",
            new_count,
            tick
        );
    }
}
