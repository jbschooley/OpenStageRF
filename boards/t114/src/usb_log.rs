// SPDX-License-Identifier: AGPL-3.0-or-later

//! USB-CDC `log` transport, opt-in via the `usb-log` Cargo feature.
//!
//! Spawns a background task that exposes the nRF52840 USB device as a
//! CDC-ACM virtual serial port and pumps any `log::info!()` etc. calls out
//! to the host.  After [`spawn()`] returns, code anywhere in the firmware
//! can do `log::info!("hi")` and a host running e.g.
//!
//! ```sh
//! screen /dev/tty.usbmodem* 115200
//! ```
//!
//! will see a human-readable line.  No host-side `defmt-print` or anything
//! else is needed — that's the whole point of this path: zero tooling on
//! top of a generic serial terminal.
//!
//! # Honest tradeoff
//!
//! Only `log::*` calls are visible.  `defmt::*` calls (which the existing
//! `apps/blink` and `apps/radio_bench` use heavily) still go through the
//! `defmt-rtt` global logger and end up in an RTT buffer that nothing
//! reads when the user has only USB plugged in.  See the bin crates for
//! how they re-emit the most important state via `log::*` so users see
//! something useful over USB.
//!
//! # HFCLK requirement
//!
//! The nRF52840 USB peripheral derives its 48 MHz USB reference from
//! HFCLK and won't enumerate reliably on the internal RC.  The caller
//! **must** initialise the chip with [`crate::clocks::usb_config()`] (or
//! some other config that selects `HfclkSource::ExternalXtal`) before
//! invoking [`spawn()`].

use embassy_executor::Spawner;
use embassy_nrf::peripherals;
use embassy_nrf::usb::{vbus_detect::HardwareVbusDetect, Driver};
use embassy_nrf::{bind_interrupts, usb, Peri};

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    // CLOCK_POWER drives `HardwareVbusDetect` (USB-detected, USB-removed,
    // USB-power-ready events from the POWER peripheral).  This is the
    // non-softdevice path; if SoftDevice ever lands on T114, switch to
    // `SoftwareVbusDetect` and feed it events from SD callbacks instead.
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
});

/// Spawn the USB-CDC `log` consumer task.  After this returns, any
/// `log::info!()` etc. call from any task is visible on the USB-CDC
/// virtual serial port.
///
/// Caller passes ownership of the `USBD` peripheral token — typically
/// obtained from [`crate::resources_and_usbd_with`].
/// `HardwareVbusDetect` claims the `CLOCK_POWER` interrupt internally;
/// don't bind it elsewhere.
///
/// Pre-requisites:
///   * `embassy_nrf::init` was called with [`crate::clocks::usb_config()`]
///     (HFXO required for USB-spec timing).
pub fn spawn(spawner: &Spawner, usbd: Peri<'static, peripherals::USBD>) {
    // HardwareVbusDetect grabs the CLOCK_POWER IRQ via the binding above
    // and watches the POWER peripheral's USB events.
    let vbus = HardwareVbusDetect::new(Irqs);
    let driver = Driver::new(usbd, Irqs, vbus);
    spawner.spawn(usb_logger_task(driver).unwrap());
}

/// 1 KiB of pipe buffer for log lines.  ~10–15 typical info lines worth
/// of slack before the producer blocks; bump if app code starts logging
/// at high rates.  Polled at 115200-baud-equivalent throughput by the
/// embassy-usb-logger pump.
const LOG_BUFFER_BYTES: usize = 1024;

#[embassy_executor::task]
async fn usb_logger_task(driver: Driver<'static, HardwareVbusDetect>) {
    embassy_usb_logger::run!(LOG_BUFFER_BYTES, log::LevelFilter::Info, driver);
}
