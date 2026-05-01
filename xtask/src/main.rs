// SPDX-License-Identifier: AGPL-3.0-or-later

//! Build helper: `cargo xtask <build|run|check> <profile-name>`
//!
//! Reads `[package.metadata.osrf]` from the profile crate's Cargo.toml to find
//! the board, reads the same block from the board crate's Cargo.toml to find
//! the rustc target triple, then shells out to cargo with the right flags.
//!
//! Diversity validation and pin-type checking are entirely compile-time via
//! the osrf-config trait system — no codegen, no pre-flight TOML validation.
//!
//! Usage:
//!   cargo xtask build dx_lr30_tx_basic
//!   cargo xtask run   dx_lr30_tx_basic    # flash + attach RTT
//!   cargo xtask check dx_lr30_rx_diversity

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde::Deserialize;

// ── Cargo.toml schema (only the fields we read) ─────────────────────────────

#[derive(Deserialize)]
struct CargoToml {
    #[serde(default)]
    package: Package,
}

#[derive(Deserialize, Default)]
struct Package {
    #[serde(default)]
    metadata: Metadata,
}

#[derive(Deserialize, Default)]
struct Metadata {
    #[serde(default)]
    osrf: Osrf,
}

#[derive(Deserialize, Default)]
struct Osrf {
    /// Set on board crates: rustc target triple.
    target: Option<String>,
    /// Set on profile crates: which board this profile targets.
    board: Option<String>,
    /// Set on profile crates: which app crate to build (defaults to "midi_node").
    app: Option<String>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: cargo xtask <build|run|check> <profile-name>");
        return ExitCode::FAILURE;
    }
    let subcommand = &args[1];
    let profile_name = &args[2];

    let workspace = workspace_root();

    // ── Profile metadata ─────────────────────────────────────────────────────
    let profile_cargo = workspace
        .join("profiles")
        .join(profile_name)
        .join("Cargo.toml");
    let profile_meta = match read_osrf_metadata(&profile_cargo) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let board = match profile_meta.board {
        Some(b) => b,
        None => {
            eprintln!(
                "error: profile `{profile_name}` is missing `[package.metadata.osrf].board` \
                 in {}",
                profile_cargo.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let app = profile_meta.app.unwrap_or_else(|| "midi_node".into());

    // ── Board metadata ───────────────────────────────────────────────────────
    let board_cargo = workspace.join("boards").join(&board).join("Cargo.toml");
    let board_meta = match read_osrf_metadata(&board_cargo) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let target = match board_meta.target {
        Some(t) => t,
        None => {
            eprintln!(
                "error: board `{board}` is missing `[package.metadata.osrf].target` \
                 in {}",
                board_cargo.display()
            );
            return ExitCode::FAILURE;
        }
    };

    // ── Cargo invocation ─────────────────────────────────────────────────────
    let cargo_cmd = match subcommand.as_str() {
        "build" | "check" | "run" => subcommand.as_str(),
        other => {
            eprintln!("error: unknown subcommand `{other}`; expected build, run, or check");
            return ExitCode::FAILURE;
        }
    };

    let package  = format!("osrf-app-{}", app.replace('_', "-"));
    let features = profile_name.as_str(); // profile name IS the cargo feature

    let mut cmd = Command::new("cargo");
    cmd.current_dir(&workspace)
        .arg(cargo_cmd)
        .arg("--target")
        .arg(&target)
        .arg("-p")
        .arg(&package)
        .arg("--features")
        .arg(features);

    if cargo_cmd == "run" {
        cmd.arg("--bin").arg(format!("embassy_{board}"));
    }

    println!("+ {cmd:?}");
    let status = cmd.status().expect("failed to run cargo");
    if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn read_osrf_metadata(path: &Path) -> Result<Osrf, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let parsed: CargoToml = toml::from_str(&text)
        .map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
    Ok(parsed.package.metadata.osrf)
}

fn workspace_root() -> PathBuf {
    env::current_exe()
        .unwrap()
        .ancestors()
        .skip(1)
        .find(|p| {
            let cargo_toml = p.join("Cargo.toml");
            cargo_toml.exists()
                && std::fs::read_to_string(&cargo_toml)
                    .unwrap_or_default()
                    .contains("[workspace]")
        })
        .expect("could not find workspace root")
        .to_path_buf()
}
