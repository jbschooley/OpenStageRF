// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bare-metal blink — bypasses embassy executor, async, time driver, USB,
//! and the GPIO HAL.  Useful as a "did anything run at all?" probe when the
//! production blink misbehaves; it isolates VTOR / linker / flash-offset
//! issues from anything in the embassy stack.
//!
//! Toggles P1_03 (the green status LED, active-low) at 2 Hz via raw GPIO
//! register pokes and `cortex_m::asm::delay`.  Bootloader-leftover state
//! (GPIOTE / PPI / TIMERs / RTC / VTOR) is cleared by
//! `osrf_board_t114::bootloader_handoff()` in `#[pre_init]`.
//!
//! Run:
//!   cargo objcopy --example bare_blink -p osrf-board-t114 \
//!     --target thumbv7em-none-eabihf --release -- -O binary bare.bin
//!   python uf2conv.py bare.bin -c -b 0x1000 -f 0xADA52840 -o bare.uf2
#![no_std]
#![no_main]

use cortex_m_rt::{entry, pre_init};
use defmt_rtt as _;
// Force-link embassy-nrf so cortex-m-rt's `device` feature finds the
// nrf52840 `__INTERRUPTS` vector table.  We don't call any embassy APIs.
use embassy_nrf as _;
use panic_probe as _;

// nRF52840 GPIO P1 register block (base 0x5000_0300, registers at +0x500/+0x700).
const P1_OUTSET: *mut u32 = 0x5000_0808 as *mut u32;
const P1_OUTCLR: *mut u32 = 0x5000_080C as *mut u32;
const P1_DIRSET: *mut u32 = 0x5000_0818 as *mut u32;
const P1_PIN_CNF_3: *mut u32 = 0x5000_0A0C as *mut u32;

// nRF52840 GPIO P0 — used only to park P0_14 (NeoPixel data line).
const P0_OUTCLR: *mut u32 = 0x5000_050C as *mut u32;
const P0_DIRSET: *mut u32 = 0x5000_0518 as *mut u32;

#[pre_init]
unsafe fn pre_init() {
    // VTOR + NVIC + LFCLK + RTC/TIMER/GPIOTE/PPI teardown all live in
    // the board crate so every binary on this board pays the same cost.
    osrf_board_t114::bootloader_handoff();

    // Park P0_14 (NeoPixel) low — its WS2812 latches whatever noise it
    // sees on a floating data line, producing flicker that masks the
    // green status LED's actual state.
    core::ptr::write_volatile(P0_OUTCLR, 1 << 14);
    core::ptr::write_volatile(P0_DIRSET, 1 << 14);

    // Configure P1_03 as push-pull output, INPUT=disconnect.
    core::ptr::write_volatile(P1_PIN_CNF_3, 0x0000_0003);
    core::ptr::write_volatile(P1_DIRSET, 1 << 3);
}

#[entry]
fn main() -> ! {
    // 2 Hz blink (250 ms on / 250 ms off).  `cortex_m::asm::delay` is a
    // hand-written subs/bne loop the compiler cannot eliminate.  At 64 MHz
    // HFINT, ~2 cycles per iteration → 16M iter ≈ 250 ms.
    loop {
        unsafe { core::ptr::write_volatile(P1_OUTSET, 1 << 3) };
        cortex_m::asm::delay(16_000_000);
        unsafe { core::ptr::write_volatile(P1_OUTCLR, 1 << 3) };
        cortex_m::asm::delay(16_000_000);
    }
}
