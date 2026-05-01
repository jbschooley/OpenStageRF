// SPDX-License-Identifier: AGPL-3.0-or-later

//! Build helper: `cargo xtask <build|run|check> <profile-name>`
//!
//! 1. Reads profiles/<profile>/profile.yaml
//! 2. Reads boards/<board>/board.yaml
//! 3. Validates the profile's diversity mode against the board's supported list
//! 4. Generates apps/<app>/src/generated/profile_config.rs
//! 5. Shells out to cargo with the correct target and feature flags
//!
//! Usage:
//!   cargo xtask build dx_lr30_tx_basic
//!   cargo xtask run   dx_lr30_tx_basic    # flash + attach RTT
//!   cargo xtask check dx_lr30_rx_diversity

use std::env;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use serde::Deserialize;

// ── YAML schema ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct BoardYaml {
    name: String,
    target: String,
    #[serde(default)]
    diversity_modes: Vec<String>,
}

#[derive(Deserialize)]
struct ProfileYaml {
    board: String,
    #[serde(default = "default_app")]
    app: String,
    diversity: Option<DiversityConfig>,
}

fn default_app() -> String {
    "midi_node".into()
}

#[derive(Deserialize)]
struct DiversityConfig {
    mode: String,
    radio1: Option<Radio1Pins>,
}

#[derive(Deserialize)]
struct Radio1Pins {
    spi: String,
    sck: String,
    miso: String,
    mosi: String,
    cs: String,
    busy: String,
    dio1: String,
    nrst: String,
    txen: Option<String>,
    rxen: Option<String>,
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

    // ── Load profile ─────────────────────────────────────────────────────────
    let profile_path = workspace
        .join("profiles")
        .join(profile_name)
        .join("profile.toml");
    let profile: ProfileYaml = match load_toml(&profile_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    // ── Load board ───────────────────────────────────────────────────────────
    let board_path = workspace
        .join("boards")
        .join(&profile.board)
        .join("board.toml");
    let board: BoardYaml = match load_toml(&board_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    // ── Resolve diversity mode ("none" is legacy alias for "single") ─────────
    let raw_mode = profile
        .diversity
        .as_ref()
        .map(|d| d.mode.as_str())
        .unwrap_or("single");
    let diversity_mode = if raw_mode == "none" { "single" } else { raw_mode };

    // ── Validate mode against board's supported list ─────────────────────────
    if !board.diversity_modes.is_empty()
        && !board.diversity_modes.iter().any(|m| m == diversity_mode)
    {
        eprintln!(
            "error: board `{}` does not support diversity mode `{}`.\n\
             \x20      Supported modes: {}",
            board.name,
            diversity_mode,
            board.diversity_modes.join(", ")
        );
        return ExitCode::FAILURE;
    }

    // ── Require radio1 pins for dual_spi modes ───────────────────────────────
    if diversity_mode.starts_with("dual_spi")
        && profile
            .diversity
            .as_ref()
            .and_then(|d| d.radio1.as_ref())
            .is_none()
    {
        eprintln!(
            "error: diversity mode `{diversity_mode}` requires a `radio1:` pin block in the profile"
        );
        return ExitCode::FAILURE;
    }

    // ── Generate profile_config.rs ───────────────────────────────────────────
    let app_dir = workspace.join("apps").join(&profile.app);
    if let Err(e) = generate_profile_config(&app_dir, diversity_mode, &profile.diversity) {
        eprintln!("error generating profile_config.rs: {e}");
        return ExitCode::FAILURE;
    }

    // ── Build cargo command ──────────────────────────────────────────────────
    let cargo_cmd = match subcommand.as_str() {
        "build" | "check" | "run" => subcommand.as_str(),
        other => {
            eprintln!("error: unknown subcommand `{other}`; expected build, run, or check");
            return ExitCode::FAILURE;
        }
    };

    let package = format!("osrf-app-{}", profile.app.replace('_', "-"));
    let features = profile.board.clone();

    let mut cmd = Command::new("cargo");
    cmd.current_dir(&workspace)
        .arg(cargo_cmd)
        .arg("--target")
        .arg(&board.target)
        .arg("-p")
        .arg(&package)
        .arg("--features")
        .arg(&features);

    if cargo_cmd == "run" {
        cmd.arg("--bin").arg(format!("embassy_{}", profile.board));
    }

    println!("+ {cmd:?}");
    let status = cmd.status().expect("failed to run cargo");
    if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

// ── Code generation ───────────────────────────────────────────────────────────

fn generate_profile_config(
    app_dir: &PathBuf,
    mode: &str,
    diversity: &Option<DiversityConfig>,
) -> Result<(), String> {
    let generated_dir = app_dir.join("src").join("generated");
    std::fs::create_dir_all(&generated_dir)
        .map_err(|e| format!("cannot create {}: {e}", generated_dir.display()))?;

    let out_path = generated_dir.join("profile_config.rs");
    let num_radios: usize = if mode.starts_with("dual_spi") { 2 } else { 1 };

    let mut code = format!(
        "// AUTO-GENERATED by `cargo xtask build` — do not edit by hand.\n\
         // Re-run `cargo xtask build <profile>` to regenerate.\n\
         \n\
         pub const DIVERSITY_MODE: &str = \"{mode}\";\n\
         pub const NUM_RADIOS: usize = {num_radios};\n"
    );

    if mode.starts_with("dual_spi") {
        if let Some(DiversityConfig {
            radio1: Some(pins), ..
        }) = diversity
        {
            code.push_str("\npub mod radio1 {\n");
            // The peripheral crate depends on the board's target. Currently only STM32
            // boards support dual_spi; add an embassy_nrf branch when T114 gains a board crate.
            code.push_str("    use embassy_stm32::peripherals;\n");
            push_type(&mut code, "Spi", &pins.spi);
            push_type(&mut code, "Sck", &pins.sck);
            push_type(&mut code, "Miso", &pins.miso);
            push_type(&mut code, "Mosi", &pins.mosi);
            push_type(&mut code, "Cs", &pins.cs);
            push_type(&mut code, "Busy", &pins.busy);
            push_type(&mut code, "Dio1", &pins.dio1);
            push_type(&mut code, "Nrst", &pins.nrst);
            if let Some(p) = &pins.txen {
                push_type(&mut code, "Txen", p);
            }
            if let Some(p) = &pins.rxen {
                push_type(&mut code, "Rxen", p);
            }
            code.push_str("}\n");
        }
    }

    std::fs::write(&out_path, &code)
        .map_err(|e| format!("cannot write {}: {e}", out_path.display()))?;

    println!("  generated {}", out_path.display());
    Ok(())
}

fn push_type(buf: &mut String, alias: &str, peripheral: &str) {
    // Peripheral names in embassy are all-caps (PA5, SPI2, USART3…).
    // Accept lowercase in YAML (spi2, pa5) and uppercase them here.
    let p = peripheral.to_uppercase();
    buf.push_str(&format!("    pub type {alias:<5} = peripherals::{p};\n"));
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn load_toml<T: for<'de> Deserialize<'de>>(path: &PathBuf) -> Result<T, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    toml::from_str(&text)
        .map_err(|e| format!("cannot parse {}: {e}", path.display()))
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
