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
generates `ROLE` / `TX_SOURCE` / `DIVERSITY` / `BAND_PLANS`.

## Schema

| Key | Values | Notes |
|-----|--------|-------|
| `app` | `t114_ui` | Generic crate to build (`osrf-profile-<app>-app`). Required. |
| `board` | `t114` | Selects the rustc target via the board crate. Required. |
| `role` | `tx` \| `rx` | Required. |
| `tx_source` | `uart` \| `scenario` | TX only; `uart` = DIN MIDI, `scenario` = synthetic bench source. Default `uart`. |
| `diversity` | `true` \| `false` | RX only; dual-SPI receive diversity (radio0 + radio1 on SPI3). Default `false`. |
| `band` | `915` \| `470` | `915` → 902–928 MHz (SX1262) band plans; `470` → 470–514 MHz (CN470/SX1268) band plans. Default `915`. |
| `keys` | path | AEAD key file, relative to repo root (gitignored). Default `osrf-keys.toml`. |

Adding a profile = drop a `.toml` here. No new crate, no workspace edits.
