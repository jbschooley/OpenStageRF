// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]
#![no_main]

//! T114 UI receiver binary.  Same UI stack as `t114_ui_tx` but
//! constructed with [`Role::Rx`].  The link receiver runs with the
//! hardcoded test key in its keyring + `allow_open = true`, so it
//! auto-decrypts whichever the TX is sending (encrypted or
//! plaintext) — no need to "set the key" on this side via the UI.

use defmt_rtt as _;
use embassy_executor::Spawner;
use osrf_board_t114 as board;
use osrf_profile_t114_ui::{run, TxSource};
use osrf_ui::Role;

#[cortex_m_rt::pre_init]
unsafe fn pre_init() {
    board::bootloader_handoff();
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    // `tx_source` is meaningless for Rx; pass any value.
    run(spawner, Role::Rx, TxSource::Uart, false).await;
}
