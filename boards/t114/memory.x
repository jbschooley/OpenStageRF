/* T114 layout — SoftDevice S140 v6.1.1 active (Heltec stock
 * bootloader's bundled SD).
 *
 *   MBR       0x00000 - 0x01000
 *   SD        0x01000 - 0x26000   (SD_FLASH_SIZE = 0x25000 on v6.1.1)
 *   App       0x26000 - 0xE7000   (772 KB)
 *   Persistence 0xE7000 - 0xED000 (24 KB — see below)
 *     - Settings    0xE7000 - 0xE9000  (8 KB / 2 pages)
 *     - Key store   0xE9000 - 0xEB000  (8 KB / 2 pages)
 *     - Panic ring  0xEB000 - 0xED000  (8 KB / 2 pages)
 *
 * Persistence regions sit at the top of the app FLASH window so that
 * bumping the app size limit (`FLASH` length) doesn't accidentally
 * overrun stored settings.  The linker only sees `FLASH` — the
 * persistence area is just "below the bootloader" from its
 * perspective.  Addresses are read by `boards/t114/src/storage.rs`
 * (must stay in sync if either changes).  Each region is exactly
 * 2 erase pages (4 KB each on nRF52840) — that's
 * `sequential-storage`'s minimum for wear-leveled key-value storage.
 *
 * RAM ORIGIN is `0x200032D8`, the value SD reports it needs for our
 * minimal config (LF clock only, no BLE roles).  Using less panics
 * at SD-enable time; using more wastes RAM that the app could
 * otherwise have.  Expand BLE config later (advertising sets,
 * connections, larger GATT table) and SD will demand a higher
 * `ram_start`; bump RAM ORIGIN to whatever its panic message names
 * and rebuild.  RAM LENGTH is `0x40000 - (ORIGIN - 0x20000000)`.
 *
 * UF2 conversion: `python3 tools/uf2conv.py app.bin -c -b 0x26000 -f 0xADA52840`
 */
MEMORY
{
  FLASH : ORIGIN = 0x00026000, LENGTH = 0x000C1000
  RAM   : ORIGIN = 0x200032D8, LENGTH = 0x0003CD28
}
