/* nRF52840 with Heltec ht-n5262 0.9.0 bootloader (T114 v2.0).
 *
 * Heltec's bootloader is the `oltaco/Adafruit_nRF52_Bootloader_OTAFIX` fork
 * (board name `heltec_t114`, BLEDIS_MODEL "HT-n5262", DEVICE_NAME "T114_DFU").
 * The factory T114 ships with Nordic SoftDevice S140 v6.1.1 already flashed
 * at 0x1000, even though the stock Heltec firmware doesn't use BLE.  The
 * bootloader's `is_sd_existed()` check at runtime decides where the user
 * app starts:
 *   - SD magic present at 0x2000  → app at 0x26000   (factory state)
 *   - no SD                       → app at 0x01000
 * UF2 flashing never erases the SD, so we stay on the 0x26000 path.  A full
 * `nrfjprog --eraseall` would move the goalpost to 0x01000.
 *
 * Flash layout (from the bootloader's own `linker/nrf52840.ld` and
 * `src/usb/uf2/uf2cfg.h`, verified against Meshtastic's working board.json):
 *   0x00000000 - 0x00000FFF  MBR                  (4 KB)
 *   0x00001000 - 0x00025FFF  SoftDevice S140 6.1.1 (148 KB)
 *   0x00026000 - 0x000ECFFF  USER APPLICATION    (796 KB)  ← this firmware
 *   0x000ED000 - 0x000F3FFF  App-data reserved    (28 KB, DFU bank/state)
 *   0x000F4000 - 0x000FDFFF  Bootloader           (40 KB)
 *   0x000FE000 - 0x000FEFFF  MBR params page
 *   0x000FF000 - 0x000FFFFF  Bootloader settings
 *
 * When converting to UF2, `uf2conv -b <offset>` must match FLASH ORIGIN.
 *
 * RAM origin is offset by 8 bytes — cortex-m-rt reserves the bottom 8 bytes
 * for its initial-stack guard.  Full LENGTH is 0x40000 - 8 = 0x3FFF8.  This
 * uses the entire 256 KB RAM because the bare-metal app does not run the
 * SoftDevice protocol stack (which would otherwise claim 0x20000000-0x20006000).
 *
 * IMPORTANT: cortex-m-rt does not relocate VTOR.  The bootloader hands off
 * with VTOR still pointing at the SoftDevice's vector table at 0x1000, so
 * the app must write SCB->VTOR = 0x26000 very early in main() before any
 * interrupt can fire.  See `bare_blink.rs` for the exact incantation.
 */
/* Standard layout (T114 + S140 v7.3.0 bootloader, upgraded 2026-05-09):
 * MBR 0x0000-0x1000, SD v7.3.0 0x1000-0x27000, user app at 0x27000.
 *
 * v7.3.0 is 4 KB larger than v6.1.1 — that's the only reason the app
 * shifted from 0x26000 to 0x27000.  The MBR at 0x0000 forwards interrupts
 * through the SoftDevice when SD is enabled; our app calls
 * `Softdevice::enable()` early in run() so SD takes over POWER/CLOCK
 * management and chip-level transitions.  See BOOTLOADER_UPGRADE.md for
 * the bootloader build + flash workflow.
 *
 * UF2 conversion: `python3 tools/uf2conv.py app.bin -c -b 0x27000 -f 0xADA52840`
 *
 * (Boards still on v6.1.1 SD use FLASH ORIGIN 0x26000.  Don't mix
 * artifacts — a v6.1.1 UF2 flashed onto a v7.3.0 bootloader fails to
 * boot because the vector table lands at the wrong address relative to
 * SD_FLASH_END.)
 */
MEMORY
{
  FLASH : ORIGIN = 0x00027000, LENGTH = 0x000C6000
  RAM   : ORIGIN = 0x20000008, LENGTH = 0x0003FFF8
}
