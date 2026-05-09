/* T114 v2.1 with SoftDevice S140 v7.3.0 active.
 *
 * Flash layout matches `memory.x` (MBR 0x0-0x1000, SD 0x1000-0x27000,
 * app at 0x27000).  RAM is shifted up to give the SoftDevice its
 * protocol-stack work area at the bottom.  The exact starting offset
 * is determined by the runtime config passed to `Softdevice::enable()`
 * — SD computes the required `ram_start` and panics with the exact
 * value to use if our linker setting is too low.
 *
 * Current value `0x20003338` is what SD reports for our config (LF
 * clock only, no BLE roles).  If we expand BLE config later (more
 * connections, advertising sets, larger GATT table), SD will demand a
 * higher `ram_start`; bump RAM ORIGIN to whatever the panic message
 * names and rebuild.  RAM LENGTH is `0x40000 - (ORIGIN - 0x20000000)`.
 *
 * This file is selected over `memory.x` by `build.rs` when the
 * `softdevice` feature is on.
 */
MEMORY
{
  FLASH : ORIGIN = 0x00027000, LENGTH = 0x000C6000
  RAM   : ORIGIN = 0x20003338, LENGTH = 0x0003CCC8
}
