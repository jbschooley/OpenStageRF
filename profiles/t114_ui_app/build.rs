// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bakes a deployment profile into the firmware at build time.
//!
//! Reads the TOML file named by the `OSRF_PROFILE` env var (set by the
//! xtask from `configs/<name>.toml`) and emits `$OUT_DIR/config.rs` with the
//! `ROLE` / `TX_SOURCE` / `DIVERSITY` / `BAND_PLANS` / `POWER_POLICY` /
//! `CHEMISTRY` / `NAME` constants that `src/main.rs` `include!`s and passes
//! to `t114_ui::run`.  `name` is the operator-set unit label shown in the
//! Idle top bar (empty = generic banner); will become BLE-settable.
//!
//! `band_plans = [...]` lists the band plans this build offers in the Band
//! Plan menu, by id (the `band_plans/<id>.toml` filename stem); a trailing
//! `*` is a prefix glob (e.g. `"shure_g58_*"`).  Indices are resolved against
//! the same lexicographically-sorted `band_plans/` directory that `core/ui`'s
//! build.rs uses, so they agree.
//!
//! With no `OSRF_PROFILE` set it builds a default (rx / `["ism915"]`) and
//! warns — use `cargo xtask run <profile>`.
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=OSRF_PROFILE");

    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    let plans_dir = Path::new(&manifest).join("../../band_plans");
    println!("cargo:rerun-if-changed={}", plans_dir.display());

    // Registry order = lexicographically-sorted band_plans/*.toml stems.
    // Must match core/ui/build.rs so BandPlan indices line up.
    let mut registry: Vec<String> = fs::read_dir(&plans_dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", plans_dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("toml"))
        .map(|p| p.file_stem().unwrap().to_str().unwrap().to_string())
        .collect();
    registry.sort();

    let mut role = String::from("rx");
    let mut tx_source = String::from("uart");
    let mut diversity = false;
    let mut band_plans: Vec<String> = vec![String::from("ism915")];
    let mut power_policy = String::from("battery");
    let mut name = String::new();
    let mut battery = String::from("lipo");
    let mut battery_cells: i64 = 3;
    let mut battery_shutdown_mv: i64 = 3000;
    let mut battery_low_mv: i64 = 3100;

    match env::var_os("OSRF_PROFILE") {
        Some(p) => {
            let path = PathBuf::from(p);
            println!("cargo:rerun-if-changed={}", path.display());
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("OSRF_PROFILE {}: {e}", path.display()));
            let t: toml::Table = text
                .parse()
                .unwrap_or_else(|e| panic!("{}: invalid TOML: {e}", path.display()));
            if let Some(v) = t.get("role").and_then(|v| v.as_str()) {
                role = v.to_string();
            }
            if let Some(v) = t.get("tx_source").and_then(|v| v.as_str()) {
                tx_source = v.to_string();
            }
            if let Some(v) = t.get("diversity").and_then(|v| v.as_bool()) {
                diversity = v;
            }
            if let Some(v) = t.get("power_policy").and_then(|v| v.as_str()) {
                power_policy = v.to_string();
            }
            if let Some(v) = t.get("name").and_then(|v| v.as_str()) {
                name = v.to_string();
            }
            if let Some(v) = t.get("battery").and_then(|v| v.as_str()) {
                battery = v.to_string();
            }
            if let Some(v) = t.get("battery_cells").and_then(|v| v.as_integer()) {
                battery_cells = v;
            }
            if let Some(v) = t.get("battery_shutdown_mv").and_then(|v| v.as_integer()) {
                battery_shutdown_mv = v;
            }
            if let Some(v) = t.get("battery_low_mv").and_then(|v| v.as_integer()) {
                battery_low_mv = v;
            }
            if let Some(arr) = t.get("band_plans").and_then(|v| v.as_array()) {
                band_plans = arr
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .unwrap_or_else(|| panic!("{}: band_plans entries must be strings", path.display()))
                            .to_string()
                    })
                    .collect();
                assert!(!band_plans.is_empty(), "{}: band_plans is empty", path.display());
            }
        }
        None => {
            println!(
                "cargo:warning=OSRF: OSRF_PROFILE not set — building default (rx, [\"ism915\"]). \
                 Flash a real profile with `cargo xtask run <profile>`."
            );
        }
    }

    // Resolve the band_plans list (ids + trailing-`*` globs) to registry
    // indices, preserving list order; globs expand in sorted registry order.
    let mut indices: Vec<usize> = Vec::new();
    for entry in &band_plans {
        let matched: Vec<usize> = if let Some(prefix) = entry.strip_suffix('*') {
            registry
                .iter()
                .enumerate()
                .filter(|(_, id)| id.starts_with(prefix))
                .map(|(i, _)| i)
                .collect()
        } else {
            match registry.iter().position(|id| id == entry) {
                Some(i) => vec![i],
                None => panic!(
                    "band_plans entry {entry:?} matches no band_plans/<id>.toml (have: {})",
                    registry.join(", ")
                ),
            }
        };
        if matched.is_empty() {
            panic!("band_plans glob {entry:?} matched nothing");
        }
        for i in matched {
            if !indices.contains(&i) {
                indices.push(i);
            }
        }
    }

    let role_v = match role.as_str() {
        "tx" => "Tx",
        "rx" => "Rx",
        o => panic!("profile `role` must be \"tx\" or \"rx\", got {o:?}"),
    };
    let tx_v = match tx_source.as_str() {
        "uart" => "Uart",
        "scenario" => "Scenario",
        o => panic!("profile `tx_source` must be \"uart\" or \"scenario\", got {o:?}"),
    };
    let plans_lit = indices
        .iter()
        .map(|i| format!("osrf_ui::BandPlan({i})"))
        .collect::<Vec<_>>()
        .join(", ");

    let power_v = match power_policy.as_str() {
        "battery" => "osrf_ui::PowerPolicy::Battery".to_string(),
        "wired" => "osrf_ui::PowerPolicy::Wired".to_string(),
        o => panic!("profile `power_policy` must be \"battery\" or \"wired\", got {o:?}"),
    };
    let chem_v = match battery.as_str() {
        "lipo" => "osrf_ui::BatteryChemistry::LiPoSingle".to_string(),
        "nimh" => format!("osrf_ui::BatteryChemistry::NimhPack {{ cells: {battery_cells} }}"),
        "regulated" => format!(
            "osrf_ui::BatteryChemistry::Regulated {{ shutdown_mv: {battery_shutdown_mv}, low_mv: {battery_low_mv} }}"
        ),
        o => panic!("profile `battery` must be \"lipo\", \"nimh\", or \"regulated\", got {o:?}"),
    };

    // Idle top bar is 24 cells wide; longer names truncate at runtime.
    if name.chars().count() > 24 {
        println!(
            "cargo:warning=OSRF: name {name:?} exceeds the 24-char Idle title width and will be truncated."
        );
    }
    // `{:?}` emits a properly-escaped Rust string literal.
    let name_lit = format!("{name:?}");

    let generated = format!(
        "// @generated by build.rs from OSRF_PROFILE\n\
         const ROLE: osrf_ui::Role = osrf_ui::Role::{role_v};\n\
         const TX_SOURCE: osrf_profile_t114_ui::TxSource = osrf_profile_t114_ui::TxSource::{tx_v};\n\
         const DIVERSITY: bool = {diversity};\n\
         const BAND_PLANS: &[osrf_ui::BandPlan] = &[{plans_lit}];\n\
         const POWER_POLICY: osrf_ui::PowerPolicy = {power_v};\n\
         const CHEMISTRY: osrf_ui::BatteryChemistry = {chem_v};\n\
         const NAME: &str = {name_lit};\n",
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::write(out.join("config.rs"), generated).unwrap();
}
