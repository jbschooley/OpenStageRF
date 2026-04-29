// SPDX-License-Identifier: AGPL-3.0-or-later

//! Build helper invoked via `cargo xtask <subcommand> <profile>`.
//!
//! Reads profiles/<profile>/profile.yaml and shells out to the appropriate
//! cargo command with the correct target, package, and feature flags.
//!
//! Usage:
//!   cargo xtask build dx_lr30_tx_basic
//!   cargo xtask run   dx_lr30_tx_basic    # flash + attach RTT

use std::env;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: cargo xtask <build|run|check> <profile-name>");
        return ExitCode::FAILURE;
    }
    let subcommand = &args[1];
    let profile_name = &args[2];

    let workspace_root = workspace_root();
    let profile_path = workspace_root
        .join("profiles")
        .join(profile_name)
        .join("profile.yaml");

    let yaml = match std::fs::read_to_string(&profile_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read {}: {}", profile_path.display(), e);
            return ExitCode::FAILURE;
        }
    };

    let board = match yaml_field(&yaml, "board") {
        Some(v) => v,
        None => {
            eprintln!("error: profile missing 'board:' field");
            return ExitCode::FAILURE;
        }
    };
    let app = yaml_field(&yaml, "app").unwrap_or_else(|| "midi_node".into());

    let cargo_cmd = match subcommand.as_str() {
        "build" | "check" => subcommand.as_str(),
        "run" => "run",
        other => {
            eprintln!("error: unknown subcommand '{other}'; expected build, run, or check");
            return ExitCode::FAILURE;
        }
    };

    // Cargo package names use hyphens; profile YAML may use underscores.
    let package = format!("osrf-app-{}", app.replace('_', "-"));
    let features = board.clone();

    let mut cmd = Command::new("cargo");
    cmd.current_dir(&workspace_root)
        .arg(cargo_cmd)
        .arg("--target")
        .arg("thumbv7m-none-eabi")
        .arg("-p")
        .arg(&package)
        .arg("--features")
        .arg(&features);

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

/// Locate workspace root: walk ancestors of the running binary until we find
/// a Cargo.toml containing [workspace].
fn workspace_root() -> PathBuf {
    env::current_exe()
        .unwrap()
        .ancestors()
        .skip(1) // skip the binary file itself
        .find(|p| {
            let cargo_toml = p.join("Cargo.toml");
            cargo_toml.exists() && {
                std::fs::read_to_string(&cargo_toml)
                    .unwrap_or_default()
                    .contains("[workspace]")
            }
        })
        .expect("could not find workspace root")
        .to_path_buf()
}

/// Extract a scalar value from a simple YAML document by key name.
fn yaml_field(yaml: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    for line in yaml.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(&prefix) {
            let value = rest.trim().trim_matches('"').trim_matches('\'');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}
