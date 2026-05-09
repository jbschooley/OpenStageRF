/* T114 layout — SoftDevice S140 v6.1.1 active (Heltec stock
 * bootloader's bundled SD).
 *
 *   MBR  0x0000 - 0x1000
 *   SD   0x1000 - 0x26000   (SD_FLASH_SIZE = 0x25000 on v6.1.1)
 *   App  0x26000 - 0xED000
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
  FLASH : ORIGIN = 0x00026000, LENGTH = 0x000C7000
  RAM   : ORIGIN = 0x200032D8, LENGTH = 0x0003CD28
}
