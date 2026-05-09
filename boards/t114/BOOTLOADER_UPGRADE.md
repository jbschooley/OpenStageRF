# T114 Bootloader Upgrade — S140 v6.1.1 → v7.3.0

The Heltec stock bootloader for the T114 (`HT-n5262`, dated 2024-07-09) bundles
SoftDevice S140 **v6.1.1**. The maintained `nrf-softdevice` Rust crate targets
S140 **v7.x.x** — calls to its `Softdevice::enable()` and even bare
`sd_softdevice_enable` SVC hang on v6 hardware due to ABI divergence in the
softdevice's protocol-stack init path. Empirically (long debug session ending
2026-05-08) we also found that *running our app without enabling SD* on these
particular T114 v2.1 boards produces flaky display behaviour — the LCM only
renders reliably with an SWD probe attached, which we hypothesise is because
the probe's wires absorb power-rail transients that SD's chip-management code
would otherwise prevent. Both problems are solved by upgrading the bootloader's
SD to v7.3.0.

This file walks through building a v7.3.0 bootloader for T114 and flashing it.
After that's done, see `## Post-upgrade code changes` for the one-line edits to
this repo.

## Source — `oltaco/Adafruit_nRF52_Bootloader_OTAFIX`

There is no pre-built T114 + S140 v7.3.0 hex anywhere I could find as of
2026-05; everything published bundles v6.1.1. We have to build from source.

The source is the **oltaco fork of Adafruit's nRF52 bootloader**:
`https://github.com/oltaco/Adafruit_nRF52_Bootloader_OTAFIX`

This is the same fork your factory bootloader was built from. Adafruit
upstream doesn't have a `heltec_t114` board target — only Adafruit-branded
hardware — so we'd have to add T114 board support ourselves to use upstream.
Oltaco already did that work plus contributed BLE-OTA reliability patches
(the `OTAFIX` in the name). Behaviour-wise this fork is identical to your
factory bootloader except for the SoftDevice version.

## Build (Docker — recommended)

The bootloader source assumes a specific ARM GCC version (12.3.Rel1, what
Adafruit CI uses).  Building with whatever your host has installed is fragile —
GCC 14+ trips a series of `-Wfatal-errors` on intentional memory-mapped
pointer dereferences in `bootloader_settings.c` and other files.  Skip the
host-toolchain headache by building inside a container that mirrors CI:

```bash
git clone --recursive https://github.com/oltaco/Adafruit_nRF52_Bootloader_OTAFIX.git

# Build the image once (downloads ARM GCC 12.3.Rel1, ~5 min):
docker build -t osrf/bootloader-builder \
    -f /Users/jacob/Projects/wireless-performer-fw/boards/t114/bootloader.Dockerfile \
    /Users/jacob/Projects/wireless-performer-fw/boards/t114/

# Build the T114 + S140 v7.3.0 bootloader (default), output goes back
# into the host repo dir via the bind-mount:
docker run --rm \
    -v "$(pwd)/Adafruit_nRF52_Bootloader_OTAFIX:/build" \
    osrf/bootloader-builder
```

Output lands in `Adafruit_nRF52_Bootloader_OTAFIX/_build/build-heltec_t114/`.
Three artifacts of interest:

| File | Contents | Flash via |
|---|---|---|
| `heltec_t114_bootloader-<ver>_s140_7.3.0.hex` | MBR + SD v7.3.0 + bootloader (combo) | openocd via SWD |
| `heltec_t114_bootloader-<ver>_s140_7.3.0.zip` | Same payload, signed Nordic-DFU package | `adafruit-nrfutil dfu serial` over USB-CDC |
| `update-heltec_t114_bootloader-<ver>_nosd.uf2` | **Bootloader-only** update (does NOT update SD) | drag onto T114BOOT |

**SD upgrades cannot be done via UF2 drag-and-drop.**  The Adafruit bootloader's
UF2 self-update path is intentionally bootloader-only — it will refuse to
overwrite the SD region (the staging logic isn't built for the address ranges
involved, and a half-written SD has no recovery short of openocd).  The "full
UF2" approach the older draft of this doc described doesn't work; an SD-bearing
UF2 tagged with the user-app family ID gets rejected by the running bootloader
(visible as a fast blink and an entry in `T114BOOT/INFO_UF2.TXT`'s error log).

The probe-free SD upgrade path is **`adafruit-nrfutil dfu serial`** against the
DFU zip — see the next section.

The Dockerfile pins `ARG GCC_VERSION=12.3.rel1` and `ARG UF2CONV_REV=…` to
keep the build deterministic even if the upstream Makefile or uf2conv repo
bumps.  See the inline comments in `bootloader.Dockerfile` and
`build-bootloader.sh` for override hints (different board, different SD
version, etc).

## Build (host toolchain — fallback)

If you don't have Docker available, install ARM GCC 12.3.Rel1 directly:

```bash
# ARM GCC 12.3.Rel1 from official source — older than what `brew install --cask
# gcc-arm-embedded` gives you (which is currently 14+ and trips array-bounds
# false positives on this codebase).
curl -fsSL https://developer.arm.com/-/media/Files/downloads/gnu/12.3.rel1/binrel/arm-gnu-toolchain-12.3.rel1-darwin-x86_64-arm-none-eabi.tar.xz \
    -o /tmp/arm-gcc.tar.xz
tar -xJf /tmp/arm-gcc.tar.xz -C ~/opt/
export PATH="$HOME/opt/arm-gnu-toolchain-12.3.rel1-darwin-x86_64-arm-none-eabi/bin:$PATH"
arm-none-eabi-gcc --version  # should report 12.3.x

# Python helpers used by the Makefile to merge hexes and produce the .zip.
pip3 install --user adafruit-nrfutil intelhex requests setuptools uritemplate
```

Then:

```bash
git clone --recursive https://github.com/oltaco/Adafruit_nRF52_Bootloader_OTAFIX.git
cd Adafruit_nRF52_Bootloader_OTAFIX
make BOARD=heltec_t114 SD_VERSION=7.3.0 all
```

If you forget `--recursive` on the clone, run
`git submodule update --init --recursive` to pull the nRF SDK + tinyusb
submodules.

Output lives in `_build/build-heltec_t114/`.  The relevant artifact:

```
_build/build-heltec_t114/heltec_t114_bootloader-<ver>_s140_7.3.0.hex
```

This is the *combined* MBR + SoftDevice v7.3.0 + bootloader hex — flash this
to fully replace the factory layout in one shot.

## Flash — option A: openocd via SWD (recommended)

You already have openocd wired and used it once to restore the v6.1.1 stock
hex. Same flow:

```bash
openocd -f interface/<your_probe>.cfg -f target/nrf52.cfg \
  -c "program _build/build-heltec_t114/heltec_t114_bootloader-<ver>_s140_7.3.0.hex verify reset; exit"
```

Replace `<your_probe>.cfg` with whatever interface file you used before
(`stlink.cfg`, `cmsis-dap.cfg`, etc.). This flashes MBR + SD + bootloader
atomically from a halted CPU and is the lowest-risk path. ~5 seconds.

Verify post-flash by triggering DFU mode (double-tap reset) and checking
`T114BOOT/INFO_UF2.TXT` — the `SoftDevice:` line should now read `S140 7.3.0`.

## Flash — option B: `adafruit-nrfutil` over USB-CDC (no probe needed)

This is the only supported probe-free path for SoftDevice upgrades on the
Adafruit-derived bootloader.  When the T114 is in DFU mode (double-tap reset),
the bootloader exposes both `T114BOOT` (mass-storage for UF2 drag-drop, app
updates only) *and* a USB-CDC serial port that speaks Nordic's DFU protocol.
`adafruit-nrfutil` is the host-side client.  It pushes the signed DFU package
(MBR + SD + bootloader) over the serial link and the bootloader stages and
swaps the regions atomically using Nordic's official update flow.

```bash
# One-time host setup:
pip3 install --user adafruit-nrfutil

# Put the T114 in DFU mode: double-tap reset.  T114BOOT mounts AND a USB-CDC
# serial port appears.  Find the port:
ls /dev/cu.usbmodem*    # macOS
ls /dev/ttyACM*         # Linux
# (Windows: open Device Manager and note the COMnn the T114 enumerates as.)

adafruit-nrfutil dfu serial \
  -pkg _build/build-heltec_t114/heltec_t114_bootloader-<ver>_s140_7.3.0.zip \
  -p /dev/cu.usbmodemXXXX \
  -b 115200
```

The transfer takes ~30-60 seconds (the protocol is slower than UF2 mass-
storage but uses Nordic-blessed staging that survives interrupts — half-
finished updates roll back rather than brick).  After it completes the
T114 will reset automatically into the new bootloader.

**Verify the upgrade**: double-tap reset, open `T114BOOT/INFO_UF2.TXT` —
`SoftDevice:` should now read `S140 7.3.0`.

If something does go wrong (lost USB connection, host crash mid-transfer),
openocd via SWD recovers — flash any known-good combo hex over the top.

## End-user distribution

For a future where users buy a T114 and want to flash OpenStageRF without an
SWD probe, ship the DFU zip alongside instructions to run
`adafruit-nrfutil dfu serial`.  This is the same UX the Bluefruit, Heltec,
and Meshtastic ecosystems already use, so users coming from those
communities will be familiar.  A future polish pass could wrap this in a
GUI tool (Adafruit's "Bluefruit LE Connect" app does similar over BLE; web-
serial-based flashers exist for Meshtastic) but the CLI works fine for
v1.

## Post-upgrade code changes

S140 v7.3.0 is 4 KB larger than v6.1.1 (`SD_FLASH_SIZE = 0x26000` vs
`0x25000`), which shifts the user-app start from `MBR_SIZE + SD_SIZE = 0x26000`
to `0x27000`. After flashing the new bootloader, three lines in this repo
need updating:

1. `boards/t114/memory.x`
   ```diff
   - FLASH : ORIGIN = 0x00026000, LENGTH = 0x000C7000
   + FLASH : ORIGIN = 0x00027000, LENGTH = 0x000C6000
   ```

2. `boards/t114/memory_softdevice.x` (same change as above).

3. `boards/t114/src/lib.rs` `FLASH_ORIGIN` const:
   ```diff
   - pub const FLASH_ORIGIN: u32 = 0x0002_6000;
   + pub const FLASH_ORIGIN: u32 = 0x0002_7000;
   ```

4. UF2 conversion command (in scripts / commit messages):
   ```diff
   - python3 tools/uf2conv.py app.bin -c -b 0x26000 -f 0xADA52840 -o app.uf2
   + python3 tools/uf2conv.py app.bin -c -b 0x27000 -f 0xADA52840 -o app.uf2
   ```

5. Re-enable the `softdevice` feature on the board crate dep in
   `profiles/t114_ui/Cargo.toml` and switch the call site in
   `profiles/t114_ui/src/lib.rs` from `enable_chip_only()` back to the full
   `Softdevice::enable()` + `softdevice::run` task. The bare-SVC stub stays
   in `boards/t114/src/softdevice.rs` as fallback for any v6.1.1 stragglers
   in the wild.

## What about app UF2s on devices we already shipped?

A device with the v7.3.0 bootloader will reject app UF2s built for `0x26000`
(it expects them at `0x27000`). After this upgrade:

- All in-development T114 units need the bootloader update flashed once.
- Subsequent app builds use `-b 0x27000`.
- The two layouts aren't directly compatible — there's no "build once, works
  on either bootloader" path. If we ever need to support both fleets, we'd
  add a build matrix and ship two UF2s per release.

For the small number of T114s that exist (your bench units and any later
production hardware), the cleanest answer is to flash the new bootloader
to all of them and standardise on the v7.3.0 layout going forward.
