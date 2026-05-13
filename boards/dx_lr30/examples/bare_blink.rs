// SPDX-License-Identifier: AGPL-3.0-or-later
//! Truly bare-metal "did our code run at all?" test for DX-LR30.
//!
//! Bypasses embassy-stm32, embassy-time, and even the GPIO HAL — we use
//! only `cortex_m_rt::entry` for the reset vector and raw RCC/GPIO register
//! pokes for everything else.  If THIS doesn't make PC13 produce a distinct
//! ON/OFF pattern, the chip is not actually running our flash.
//!
//! Pattern: PC13 *and* PB0 toggle in phase at ~1 Hz forever.
//!
//! - PC13 drives the on-board status LED (LED2) through R2 = 4.7K → ~64 µA,
//!   essentially invisible under normal lighting and useless if LED2 is
//!   damaged anyway.  Useful for multimeter probing on H3 pin 19.
//! - PB0 is a free GPIO broken out on **H3 pin 8** with full ~25 mA drive.
//!   Wire a 5 mm LED + 1 KΩ resistor between H3 pin 8 and any GND pin
//!   (H3 pin 3 or H3 pin 4 is `NRST`, so prefer H3 pin 3) for an
//!   eye-blastingly bright 1 Hz blink — anode to PB0, cathode to GND.
//!
//! Visible blink on the external LED = chip is running our flash, end of
//! discussion.
//!
//! Build + flash via the CH340C USB-C bridge:
//!   cargo objcopy --example bare_blink -p osrf-board-dx-lr30 \
//!     --target thumbv7m-none-eabi --release -- -O binary bare-stm.bin
//!   stm32flash -w bare-stm.bin -v -g 0x08000000 \
//!     -i '-rts,dtr,-dtr:rts,-dtr' /dev/tty.usbserial-2110
//!
//! After flashing, power-cycle the board (or release SW2 + tap SW1 reset)
//! so the chip boots from flash, not the System Memory bootloader.
#![no_std]
#![no_main]

use cortex_m_rt::entry;
use defmt_rtt as _;
// Force-link embassy-stm32 so cortex-m-rt's `device` feature finds the
// stm32f103 `__INTERRUPTS` vector table.  We don't call any embassy-stm32
// APIs in this example — peripherals are driven via raw register pokes.
use embassy_stm32 as _;
use panic_probe as _;

// RCC_APB2ENR: bits 3 (IOPBEN) and 4 (IOPCEN) gate the GPIOB and GPIOC clocks.
const RCC_APB2ENR: *mut u32 = 0x4002_1018 as *mut u32;
const IOPBEN: u32 = 1 << 3;
const IOPCEN: u32 = 1 << 4;

// GPIOB port: PB0 broken out on H3 pin 8, drives the external LED.
// CRL bits [3:0] = CNF0[1:0] | MODE0[1:0].  0b0010 = push-pull output, 2 MHz.
const GPIOB_CRL: *mut u32 = 0x4001_0C00 as *mut u32;
const GPIOB_BSRR: *mut u32 = 0x4001_0C10 as *mut u32;
const BSRR_BS0: u32 = 1 << 0; // set PB0 high
const BSRR_BR0: u32 = 1 << 16; // reset PB0 low

// GPIOC port: PC13 = on-board LED2 through R2 = 4.7 KΩ (likely dead/dim).
// CRH bits [23:20] = CNF13[1:0] | MODE13[1:0].  0b0010 = push-pull, 2 MHz.
const GPIOC_CRH: *mut u32 = 0x4001_1004 as *mut u32;
const GPIOC_BSRR: *mut u32 = 0x4001_1010 as *mut u32;
const BSRR_BS13: u32 = 1 << 13; // set PC13 high
const BSRR_BR13: u32 = 1 << 29; // reset PC13 low

#[entry]
fn main() -> ! {
    unsafe {
        // Enable GPIOB and GPIOC clocks.
        let v = core::ptr::read_volatile(RCC_APB2ENR);
        core::ptr::write_volatile(RCC_APB2ENR, v | IOPBEN | IOPCEN);

        // PB0 → 2 MHz push-pull output.  Clear bits [3:0] of CRL; leave
        // PB1..PB7 fields untouched.
        let v = core::ptr::read_volatile(GPIOB_CRL);
        core::ptr::write_volatile(GPIOB_CRL, (v & !0xF) | 0x2);

        // PC13 → 2 MHz push-pull output.  Clear bits [23:20] of CRH; leave
        // PC8..PC12, PC14..PC15 fields untouched.
        let v = core::ptr::read_volatile(GPIOC_CRH);
        core::ptr::write_volatile(GPIOC_CRH, (v & !(0xF << 20)) | (0x2 << 20));

        // Toggle PB0 + PC13 in phase at ~1 Hz.  Reset clock is HSI = 8 MHz;
        // release-mode loop body is ~4-5 cycles per iter, so 800K iters ≈
        // half a second per phase.
        loop {
            core::ptr::write_volatile(GPIOB_BSRR, BSRR_BR0);
            core::ptr::write_volatile(GPIOC_BSRR, BSRR_BR13);
            for _ in 0..800_000u32 {
                cortex_m::asm::nop();
            }
            core::ptr::write_volatile(GPIOB_BSRR, BSRR_BS0);
            core::ptr::write_volatile(GPIOC_BSRR, BSRR_BS13);
            for _ in 0..800_000u32 {
                cortex_m::asm::nop();
            }
        }
    }
}
