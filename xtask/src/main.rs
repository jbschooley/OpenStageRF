// SPDX-License-Identifier: AGPL-3.0-or-later

//! Build helper: `cargo xtask <build|run|check> <profile-name>` plus
//! `cargo xtask audit` for portability + no-alloc CI checks.
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
//!   cargo xtask audit                     # portability + no-alloc CI gate

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
    if args.len() < 2 {
        eprintln!("usage: cargo xtask <build|run|check> <profile-name>");
        eprintln!("       cargo xtask audit");
        return ExitCode::FAILURE;
    }
    let subcommand = &args[1];

    let workspace = workspace_root();

    // ── Audit subcommand has no profile arg ──────────────────────────────────
    if subcommand == "audit" {
        return audit(&workspace);
    }

    if args.len() < 3 {
        eprintln!("usage: cargo xtask <build|run|check> <profile-name>");
        return ExitCode::FAILURE;
    }
    let profile_name = &args[2];

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
    // Two profile styles are supported:
    //   - Binary-style:  profile crate IS the deployment binary (has src/main.rs).
    //                    Metadata omits `app`.  Build directly.
    //   - Library-style: profile crate is a config library; an app crate hosts
    //                    the binary and gates this profile via a Cargo feature.
    //                    Metadata sets `app = "..."`.
    let app_field = profile_meta.app;

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

    let mut cmd = Command::new("cargo");
    cmd.current_dir(&workspace)
        .arg(cargo_cmd)
        .arg("--target")
        .arg(&target);

    match app_field {
        // Binary-style: build the profile crate itself.
        None => {
            let package = format!("osrf-profile-{}", profile_name.replace('_', "-"));
            cmd.arg("-p").arg(&package);
        }
        // Library-style: build the named app crate with this profile as a feature.
        Some(app) => {
            let package = format!("osrf-app-{}", app.replace('_', "-"));
            cmd.arg("-p")
                .arg(&package)
                .arg("--features")
                .arg(profile_name);
            if cargo_cmd == "run" {
                cmd.arg("--bin").arg(format!("embassy_{board}"));
            }
        }
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
    let parsed: CargoToml =
        toml::from_str(&text).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
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

// ── `audit` subcommand ────────────────────────────────────────────────────────

/// Top-level directories whose crates must stay HAL-agnostic and no-alloc.
/// `boards/` and `profiles/` are the place where embassy-* HAL deps and
/// `alloc` (if it were ever needed) belong; `ports/` is a HAL adapter
/// crate by definition and exempt for the same reason; `xtask/` is host-
/// side build tooling.
const AUDIT_DIRS: &[&str] = &["core", "drivers", "protocols", "crypto", "apps"];

/// Embassy crates that are *framework* (executor / sync / time / USB
/// stack), not HAL — these are board-agnostic and safe for shared
/// crates to depend on.  Any other `embassy-*` package found in
/// `[dependencies]` of an audited crate is treated as a HAL leak.
const EMBASSY_FRAMEWORK_WHITELIST: &[&str] = &[
    "embassy-time",
    "embassy-sync",
    "embassy-futures",
    "embassy-executor",
    "embassy-usb",
    "embassy-usb-driver",
    "embassy-usb-logger",
];

/// Audit shared crates for portability + no-alloc invariants.  Returns
/// `FAILURE` and prints each violation if anything is wrong; otherwise
/// `SUCCESS` with a one-line summary.  Intended for CI; the exit code
/// is the contract.
fn audit(workspace: &Path) -> ExitCode {
    let mut violations: Vec<String> = Vec::new();

    for dir in AUDIT_DIRS {
        let root = workspace.join(dir);
        if !root.exists() {
            continue;
        }
        walk_crates(&root, &mut |crate_dir| {
            audit_cargo_toml(crate_dir, &mut violations);
        });
        walk_rs_files(&root, &mut |rs_path, contents| {
            audit_rs_file(rs_path, contents, &mut violations);
        });
    }

    if violations.is_empty() {
        let n_crates: usize = AUDIT_DIRS
            .iter()
            .map(|d| {
                let mut c = 0;
                let root = workspace.join(d);
                if root.exists() {
                    walk_crates(&root, &mut |_| c += 1);
                }
                c
            })
            .sum();
        println!("audit: clean ({} crates checked)", n_crates);
        ExitCode::SUCCESS
    } else {
        for v in &violations {
            eprintln!("{v}");
        }
        eprintln!("audit: {} violation(s)", violations.len());
        ExitCode::FAILURE
    }
}

/// Read a crate's `Cargo.toml` and flag any disallowed deps.  Only
/// `[dependencies]` is checked — `[dev-dependencies]` and
/// `[build-dependencies]` are typically host-side and exempt.
fn audit_cargo_toml(crate_dir: &Path, violations: &mut Vec<String>) {
    let cargo_toml = crate_dir.join("Cargo.toml");
    let text = match std::fs::read_to_string(&cargo_toml) {
        Ok(t) => t,
        Err(_) => return,
    };

    let Ok(parsed) = toml::from_str::<toml::Value>(&text) else {
        violations.push(format!("{}: failed to parse", cargo_toml.display()));
        return;
    };

    let Some(deps) = parsed.get("dependencies").and_then(|v| v.as_table()) else {
        return;
    };

    for (dep_name, _value) in deps {
        if dep_name.starts_with("embassy-")
            && !EMBASSY_FRAMEWORK_WHITELIST.contains(&dep_name.as_str())
        {
            violations.push(format!(
                "{}: shared crate depends on HAL crate `{}` — move to boards/ or profiles/",
                cargo_toml.display(),
                dep_name
            ));
        }
    }
}

/// Scan a single `.rs` file for `extern crate alloc` or `use alloc::`
/// imports.  These mean the file pulls in heap-allocation primitives,
/// which is not allowed in shared crates (we run on no-allocator
/// targets and want allocation to stay an explicit per-board choice).
///
/// We don't try to be clever about comments / strings — a false
/// positive triggered by a literal "use alloc::" inside a doc-comment
/// is so unlikely it's not worth a real parser.
fn audit_rs_file(path: &Path, contents: &str, violations: &mut Vec<String>) {
    for (line_no, line) in contents.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        let is_extern_alloc = trimmed
            .strip_prefix("extern crate alloc")
            .map(|rest| rest.starts_with(';') || rest.starts_with(char::is_whitespace))
            .unwrap_or(false);
        let is_use_alloc = trimmed
            .strip_prefix("use alloc")
            .map(|rest| rest.starts_with("::") || rest.starts_with(';'))
            .unwrap_or(false);
        if is_extern_alloc || is_use_alloc {
            violations.push(format!(
                "{}:{}: imports `alloc` (heap not allowed in shared crates)",
                path.display(),
                line_no + 1
            ));
        }
    }
}

/// Walk every crate directory under `root` (a crate dir is any
/// directory containing a `Cargo.toml`).  Visits each found crate
/// dir once.  Skips `target/`.
fn walk_crates(root: &Path, visit: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("target") {
            continue;
        }
        if path.join("Cargo.toml").is_file() {
            visit(&path);
        }
        // Recurse — apps/ + drivers/ have nested subdirs (drivers/midi/din,
        // drivers/input/joystick5way, etc.).
        walk_crates(&path, visit);
    }
}

/// Walk every `.rs` file under `root` (recursively).  Skips
/// `target/`, generated `OUT_DIR` artefacts, and hidden directories.
fn walk_rs_files(root: &Path, visit: &mut dyn FnMut(&Path, &str)) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') || name == "target" {
            continue;
        }
        if path.is_dir() {
            walk_rs_files(&path, visit);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            if let Ok(contents) = std::fs::read_to_string(&path) {
                visit(&path, &contents);
            }
        }
    }
}
