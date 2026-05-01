// SPDX-License-Identifier: AGPL-3.0-or-later
#![no_std]

// Shared MIDI-node application logic.
// Platform entry points live in src/bin/; shared code goes here.

// Active profile — pulled in by the profile feature passed to xtask.
// Application code refers to `crate::profile::radio0::Cs`, etc.,
// regardless of which profile (or board) is selected.
pub mod profile;
