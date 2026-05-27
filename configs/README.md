# Profile configs

Each `*.toml` here is a **flashable deployment profile**, resolved at build
time (no_std has no runtime filesystem — `build.rs` reads the TOML and bakes
the values into the firmware as constants). The filename (without `.toml`) is
the profile name you pass to the xtask:

```bash
cargo xtask run ui_tx_470      # flashes configs/ui_tx_470.toml
cargo xtask build ui_rx
```

The xtask resolves `app` → the generic binary crate (`osrf-profile-<app>-app`),
`board` → the rustc target, sets `OSRF_PROFILE` (this file) and `OSRF_KEYS_FILE`
(`keys`), and builds it. The crate's `build.rs` reads `OSRF_PROFILE` and
generates `ROLE` / `TX_SOURCE` / `DIVERSITY` / `BAND_PLANS` / `POWER_POLICY` /
`CHEMISTRY` / `NAME`.

## Schema

| Key | Values | Notes |
|-----|--------|-------|
| `app` | `t114_ui` | Generic crate to build (`osrf-profile-<app>-app`). Required. |
| `board` | `t114` | Selects the rustc target via the board crate. Required. |
| `role` | `tx` \| `rx` | Required. |
| `name` | string | Operator-set unit label shown in the Idle top bar in place of the generic `OpenStageRF TX/RX` banner — handy for telling units apart at a venue. Truncated to 24 chars. Empty = generic banner. Will become BLE-settable. Default empty. |
| `tx_source` | `uart` \| `scenario` | TX only; `uart` = DIN MIDI, `scenario` = synthetic bench source. Default `uart`. |
| `diversity` | `true` \| `false` | RX only; dual-SPI receive diversity (radio0 + radio1 on SPI3). Default `false`. |
| `band_plans` | list of plan ids | Band plans this build offers in the Band Plan menu, by `band_plans/<id>.toml` stem. A trailing `*` is a prefix glob. Order is preserved; first entry is the boot default. Default `["ism915"]`. |
| `power_policy` | `battery` \| `wired` | `battery` = handheld, explicit user on/off; `wired` = permanent-install, auto-soft-off ~10 s after USB power is lost. Default `battery`. |
| `battery` | `lipo` \| `nimh` \| `regulated` | Battery chemistry / gauge model. `nimh` reads `battery_cells` (default 3); `regulated` reads `battery_shutdown_mv` / `battery_low_mv` (defaults 3000 / 3100). Default `lipo`. |
| `keys` | path | AEAD key file, relative to repo root (gitignored). Default `osrf-keys.toml`. |

`band_plans` examples:
- 902–928 MHz: `["ism915", "sennheiser", "shure", "dense_lo", "dense_mid", "dense_hi", "wide"]`
- 470–514 MHz: `["band470", "shure_g58_*", "senn_a1_*"]`

Band plan definitions live in [`band_plans/`](../band_plans/) (one `.toml` per plan: `label` + `channels_khz`). Add a plan = drop a file there; reference it by filename here. Adding a profile = drop a `.toml` in `configs/`. No new crate, no workspace edits.
