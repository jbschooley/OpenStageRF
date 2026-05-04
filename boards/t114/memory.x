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
/* On this T114 unit the bootloader's `is_sd_existed()` check at 0x2004 is
 * returning FALSE — either Heltec ships some units without S140, or this
 * particular SoftDevice region got wiped.  The bootloader's runtime
 * `CODE_REGION_1_START` therefore evaluates to MBR_SIZE (0x1000) instead
 * of SD_SIZE (0x26000), and any app flashed to 0x26000 is silently
 * ignored.  Flashing at 0x1000 with the corresponding `uf2conv -b 0x1000`
 * lands the app where the bootloader actually looks for it.
 *
 * Side-effect: if S140 was still partially present, our app overwrites
 * its leading words and `is_sd_existed()` will return FALSE on every
 * subsequent boot — fine, we don't use BLE.
 */
MEMORY
{
  FLASH : ORIGIN = 0x00001000, LENGTH = 0x000EC000
  RAM   : ORIGIN = 0x20000008, LENGTH = 0x0003FFF8
}
