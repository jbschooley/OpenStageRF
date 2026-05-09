// SPDX-License-Identifier: AGPL-3.0-or-later
use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    // Pick memory layout based on whether the `softdevice` feature is on.
    // Cargo sets `CARGO_FEATURE_SOFTDEVICE=1` for downstream crates that
    // enable the feature on this board.  When the SoftDevice is active,
    // RAM origin shifts up to 0x20002000 to reserve the bottom 8 KB for
    // SD's protocol stack work area.
    let src = if env::var_os("CARGO_FEATURE_SOFTDEVICE").is_some() {
        "memory_softdevice.x"
    } else {
        "memory.x"
    };
    fs::copy(src, out.join("memory.x")).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=memory_softdevice.x");
    println!("cargo:rerun-if-changed=build.rs");
}
