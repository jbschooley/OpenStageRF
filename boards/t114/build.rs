// SPDX-License-Identifier: AGPL-3.0-or-later
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::copy("memory.x", out.join("memory.x")).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=build.rs");

    // Capture short git hash + dirty marker for the About screen and
    // any future diagnostic surface.  Falls back to "unknown" when
    // git isn't available (e.g. building from a release tarball) so
    // builds don't break.  Re-runs on every build — cheap, and we
    // want the hash to reflect the actual build, not a stale value.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    let hash = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    let suffix = if dirty { "*" } else { "" };
    println!("cargo:rustc-env=OSRF_GIT_HASH={}{}", hash, suffix);
}
