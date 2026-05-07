// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! MIDI node TX, T114 deployment.
//!
//! Wires the T114 board's `radio0` + `status_led` + `midi_uart` into
//! [`osrf_app_midi_node::run_tx`] with a [`UartMidiSource`] reading
//! the FeatherWing's DIN MIDI IN.  Each parsed channel-voice event is
//! pushed through the link runtime (`MidiTxQueue` → `LinkSender` →
//! radio) with the same K=3 + delayed-copy reliability machinery
//! validated in the rx10–rx12 link bench runs.
//!
//! Hardware setup:
//!   - keyboard MIDI OUT → cable → FeatherWing **DIN IN** jack
//!   - FeatherWing `RX` (D0) → T114 P0_09
//!   - FeatherWing `3V` → T114 3V3
//!   - FeatherWing `GND` → T114 GND
//!   - DIN OUT jack and FeatherWing `TX` pin: leave disconnected.

use embassy_executor::Spawner;
use osrf_app_midi_node::{run_tx, LinkConfig, UartMidiSource};
use osrf_board_t114 as board;

use defmt_rtt as _;
use panic_probe as _;

#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    board::bootloader_handoff();
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let mut r = board::resources();
    defmt::info!("OpenStageRF MIDI node TX — T114 starting");

    // boot_counter is the high 16 bits of the link-layer `seq`.  It MUST
    // change across resets — otherwise after a TX reboot the receiver's
    // replay window sees the new low-seq packets as ancient duplicates
    // (since `latest` is still ~the last seq from the prior session).
    //
    // Production: persist a 16-bit counter in flash and bump on every
    // reset (with periodic flushes for wear).  Bench: pull from the
    // nRF52 hardware RNG so every reboot looks unique to the receiver.
    let boot_counter = read_random_u16();
    defmt::info!("boot_counter = {} (random per-boot)", boot_counter);

    let config = LinkConfig::default_915();
    let mut source = UartMidiSource::new(r.midi_uart);

    run_tx(
        &mut r.radio0,
        &mut r.status_led,
        &mut source,
        boot_counter,
        &config,
    )
    .await
}

/// Pull two random bytes from the nRF52840 RNG peripheral and pack them
/// into a `u16`.  Each byte takes ~120 µs to generate (with bias
/// correction); the whole call is well under a millisecond.  Done with
/// raw register pokes to avoid the embassy-nrf RNG driver's interrupt
/// binding (we only need this once at boot).
fn read_random_u16() -> u16 {
    // RNG peripheral, datasheet §6.27.
    const TASKS_START: *mut u32 = 0x4000_D000 as *mut u32;
    const TASKS_STOP: *mut u32 = 0x4000_D004 as *mut u32;
    const EVENTS_VALRDY: *mut u32 = 0x4000_D100 as *mut u32;
    const CONFIG: *mut u32 = 0x4000_D504 as *mut u32;
    const VALUE: *const u32 = 0x4000_D508 as *const u32;

    let mut bytes = [0u8; 2];
    unsafe {
        // Enable bias correction (DERCEN = bit 0).
        core::ptr::write_volatile(CONFIG, 0x01);
        for b in bytes.iter_mut() {
            core::ptr::write_volatile(EVENTS_VALRDY, 0);
            core::ptr::write_volatile(TASKS_START, 1);
            while core::ptr::read_volatile(EVENTS_VALRDY) == 0 {
                cortex_m::asm::nop();
            }
            *b = core::ptr::read_volatile(VALUE) as u8;
        }
        core::ptr::write_volatile(TASKS_STOP, 1);
    }
    u16::from_be_bytes(bytes)
}
