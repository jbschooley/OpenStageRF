#!/usr/bin/env bash
# Build entrypoint baked into osrf/bootloader-builder.  Runs the
# upstream `make ... all` and prints a summary of the artifacts that
# matter for flashing the result.
#
# Outputs (in `_build/build-${BOARD}/`):
#   ${BOARD}_bootloader-<ver>.hex                          bootloader-only hex
#   ${BOARD}_bootloader-<ver>_s140_${SD_VERSION}.hex       MBR + SD + BL combo (flash via openocd)
#   ${BOARD}_bootloader-<ver>_s140_${SD_VERSION}.zip       Nordic DFU package (adafruit-nrfutil)
#   update-${BOARD}_bootloader-<ver>_nosd.uf2              BL-only UF2 self-update (existing SD stays)
#
# Note: there is intentionally NO "full SD+BL UF2" output.  The
# Adafruit-derived bootloader's UF2 self-update path is bootloader-
# only — SD upgrades have to go through `adafruit-nrfutil dfu serial`
# against the .zip over USB-CDC, which uses Nordic's atomic-staging
# DFU protocol.  See BOOTLOADER_UPGRADE.md for details.

set -euo pipefail

: "${BOARD:?BOARD env not set}"
: "${SD_VERSION:?SD_VERSION env not set}"

# Run the upstream build (makes hex, combo hex, BL-only UF2, DFU zip).
make "BOARD=${BOARD}" "SD_VERSION=${SD_VERSION}" all

BUILD_DIR="_build/build-${BOARD}"

COMBO_HEX=$(ls "${BUILD_DIR}/${BOARD}_bootloader-"*"_s140_${SD_VERSION}.hex" 2>/dev/null | head -1 || true)
DFU_ZIP=$(ls   "${BUILD_DIR}/${BOARD}_bootloader-"*"_s140_${SD_VERSION}.zip" 2>/dev/null | head -1 || true)
UPDATE_UF2=$(ls "${BUILD_DIR}/update-${BOARD}_bootloader-"*"_nosd.uf2"        2>/dev/null | head -1 || true)

echo
echo "── Build complete ────────────────────────────────────────────────"
echo "  Combo hex (openocd / SWD):     ${COMBO_HEX:-MISSING}"
echo "  DFU zip (adafruit-nrfutil):    ${DFU_ZIP:-MISSING}"
echo "  Bootloader-only update UF2:    ${UPDATE_UF2:-MISSING}"
echo
echo "Flash a fresh T114 (first time):"
echo "  • With SWD probe:    openocd … program ${COMBO_HEX:-…} verify reset"
echo "  • Without probe:     adafruit-nrfutil dfu serial -pkg ${DFU_ZIP:-…} -p /dev/cu.usbmodem* -b 115200"
echo
echo "Recovery if anything bricks: openocd via SWD always works."
