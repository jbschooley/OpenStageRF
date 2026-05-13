# OpenStageRF — Battery & Power Options for the T114

This doc covers every battery + power-source configuration the firmware currently supports
on the Heltec Mesh Node T114, the trade-offs between them, and the firmware constants +
hardware mods (if any) each one requires.

Two **orthogonal** profile-level constants control behaviour:

- **`BatteryChemistry`** — what cells are wired (LiPo, NiMH-direct, NiMH-via-boost,
  external regulator).  Controls the OCV table and the soft-off voltage threshold.
- **`PowerPolicy`** — how the device treats USB presence (handheld battery use vs.
  permanent install on a host's USB rail).  Controls boot dispatch + auto-shutdown.

If you want to skim, jump to the [decision tables](#which-option).

The T114 ships with an 800 mAh single-cell LiPo pouch wired to a JST-PH on the underside.
The on-board charger is a TP4054 (LiPo-only, 4.2 V CV) driven from VBUS through a TVS;
the LDO that sets chip Vdd from VBat is an HT7333-class part with ~150 mV dropout.  That
constrains every option below — VBat must sit somewhere between ~3.45 V (LDO minimum) and
~4.3 V (HT7333 absolute max input) for the regulator to do its job, *unless* you upstream a
boost/buck converter or skip the LDO with a hardware mod.

---

## Default: stock single-cell LiPo

What ships:

- **Cell**: 800 mAh LiPo pouch, JST-PH wired.
- **Charging**: TP4054, 4.2 V CV via USB-C.
- **Firmware**: `BatteryChemistry::LiPoSingle`.  This is the default in every t114
  profile; the OCV table is Meshtastic's reference (3.10–4.19 V mapped 0–100 %).
- **Hardware mods**: none.

This works out of the box and is what we ship enabled. Drawbacks:

- 800 mAh is not a lot for a continuous-TX rig.  Soak-tested runtime in M5 was [TODO:
  actual hours].
- The pouch isn't user-swappable mid-show without unscrewing the case and unplugging the
  JST-PH.

Both of those are addressed by the swap configurations below.

---

## Drop-in swap: 14500 / 18350 / 18650 LiPo cells

Same chemistry as the stock pouch, just in a cylindrical user-swappable cell.  Zero
firmware changes — the LiPo OCV table works identically.  Charge characteristic, voltage
range, BMS expectations are all the same.

| Cell    | Form factor (mm) | Capacity (mAh) | Holder candidate            | Notes                                              |
|---------|------------------|----------------|------------------------------|----------------------------------------------------|
| 14500   | 14 × 50 (AA)     | 800            | Keystone 79                  | Same case envelope as stock pouch; pocket-portable. |
| 18350   | 18 × 35          | 1000           | Keystone 1043 (1× 18350)     | Slight capacity bump, slightly fatter case.        |
| 18650   | 18 × 65          | 3000           | Keystone 1042 (1× 18650)     | 4× runtime; case has to accommodate the length.    |

**Charging**: external single-bay LiPo charger.  Nitecore F1 / XTAR MC1 / similar — $10–15.
You don't *need* to swap to external charging — the on-board TP4054 still works through the
JST-PH — but in practice "carry two cells, swap mid-show" needs the external charger
because the unit's own USB charge cycle is way slower than song-cycle.

**Hardware mods**: none on the T114 itself.  The holder + cell wires to the existing
JST-PH; that's it.

**Firmware**: `BatteryChemistry::LiPoSingle` (no change).

### ⚠️ Safety note on 14500

A 14500 LiPo is exactly the same physical size as a standard AA primary or NiMH cell, but
it sits at 3.0–4.2 V instead of 1.0–1.5 V.  Drop it into a device expecting AA (a TV remote,
a wireless mouse, a kid's toy) and you can vent / fire the cell or destroy the receiving
device's electronics.  Mitigations:

- **Don't store loose 14500s in a bag of mixed AAs.**  Plastic single-cell carriers
  (Pelican 1010, Nitecore NBM40) keep them isolated and labelled.
- **Mark the holder and cells** with bright tape if there's any chance someone else might
  pick one up.
- **Or — just use NiMH (next sections).**  The whole AA-LiPo class is a known foot-gun.

---

## NiMH Option A: 3× AA/AAA NiMH, direct connection

The simplest no-boost NiMH path.  3 cells in series gives 3.0–4.2 V loaded — comfortably
inside the T114's LDO input window, no conversion electronics required.

| Cell type      | Pack capacity | Holder candidate                    | Notes                                                  |
|----------------|---------------|--------------------------------------|--------------------------------------------------------|
| 3× AA NiMH     | ~2000 mAh     | Keystone 2462 (3× AA flat)           | Eneloops recommended; standard Panasonic / IKEA.       |
| 3× AAA NiMH    | ~700–900 mAh  | Keystone 2461 (3× AAA inline)        | Smaller form factor, ~1/3 the capacity of 3× AA.       |

**Hardware mods on the T114**:

- **De-pop or cut the TP4054 output trace.**  The TP4054 is hardwired to charge LiPo at
  4.2 V CV; plug USB on a NiMH pack and it will overcharge / damage / potentially vent the
  cells.  Two options:
  1. Desolder the TP4054 entirely (4-pin SOT-23-5, surface-mount, hot-air recommended).
  2. Cut the trace from TP4054's `BAT` pin to the JST-PH `+` net.
  The second is reversible; the first is cleaner if you're committed.
- **Re-wire JST-PH to the 3-cell holder.**  Strip the leads on the holder; solder + heat-
  shrink to the existing JST-PH pigtail (or swap the connector if you prefer the holder's
  native pigtail).

**Charging**:

- **External smart charger only.**  An off-board charger like the Powerex Pro / La Crosse
  BC700 / Panasonic BQ-CC55 handles NiMH delta-V termination correctly; the on-board TP4054
  is disabled (above).
- USB plugged-in to the T114 still works for *powering* the device (chip runs off VBUS-
  through-TP4054-leakage-path or via VBat if cells are installed), but no charging happens.
- The user-facing "lightning bolt" indicator on the title bar still tracks `vbus_present()`
  unchanged; just understand it means "powered from USB," not "charging the cells."

**Firmware**:

```rust
const CHEMISTRY: BatteryChemistry = BatteryChemistry::NimhPack { cells: 3 };
```

The 3-cell OCV table is the per-cell Eneloop curve × 3 (1.00–1.35 V/cell anchors).  The
M7/M8 soft-off threshold automatically scales — `shutdown_mv` returns 3000 (1.0 V/cell) for
this variant, the safe NiMH cutoff.

**Trade-offs**:

- ✓ No external regulator IC; simplest hardware path of any non-stock option.
- ✓ Full SoC gauge works — SAADC reads cell voltage directly.
- ✓ Largest capacity per swap of any AA-form-factor option (~2000 mAh).
- ✗ 3-cell holder is bulkier than 2× AA, much bulkier than the stock pouch.
- ✗ Have to commit to disabling the on-board charger.

---

## NiMH Option B: 1× or 2× AA/AAA NiMH via boost converter (no fuel gauge)

For users who specifically want **the most standard cell type** (AA or AAA NiMH, every gas
station and grocery store sells them) and want a simple firmware-only cutoff without
adding a third SAADC channel.

| Cells          | Pack voltage   | Notes                                                      |
|----------------|----------------|------------------------------------------------------------|
| 1× AA NiMH     | 1.0–1.4 V      | Marginal — boost ICs cold-start around 0.7–0.9 V; pick one rated for 1-cell explicitly (MAX17222, TPS61291). |
| 2× AA NiMH     | 2.0–2.8 V      | Sweet spot.  Every modern boost IC works here. ~2× the runtime of 1× AA. |
| 1× AAA NiMH    | 1.0–1.4 V      | Same as 1× AA but ~1/3 the capacity.                       |
| 2× AAA NiMH    | 2.0–2.8 V      | Smaller form factor than 2× AA, ~1/3 the capacity.         |

**Hardware**:

- 1× or 2× AA holder of choice.
- Boost converter module → JST-PH input.  Recommended:
  - **Pololu U1V11A33** (3.3 V output, ~$4): basic boost, no UVLO, relies on firmware
    cutoff.  Smallest + cheapest.
  - **Adafruit MAX17222 nanoBoost breakout** (~$5): nanoPower IC, TrueShutdown via SHDN
    pin, 0.3 µA quiescent when off — overkill unless you care about decade-long
    drawer-storage scenarios.
  - **Adafruit PowerBoost 500 basic** (~$10–15): TPS61090 with built-in UVLO at 2.5 V.
    Boost auto-cuts off at 1.25 V/cell — conservative, you "waste" some capacity, but no
    firmware-cutoff dependency.

  None of these need anything special wired beyond `VIN ← cells +`, `GND ← cells −`,
  `VOUT → JST-PH +`, `GND → JST-PH −`.

- **De-pop the TP4054 (same as Option A).**  USB plug-in with NiMH cells installed will
  destroy the cells via the LiPo charger otherwise.

**Firmware**:

```rust
const CHEMISTRY: BatteryChemistry = BatteryChemistry::Regulated {
    shutdown_mv: 3000,
    low_mv: 3100,
};
```

The SAADC reads the boost output (regulated 3.3 V).  As long as the boost can maintain
regulation, VBat sits near 3.3 V and the gauge reads 100 %.  When the NiMH cells weaken to
the point where the boost runs out of input headroom, VBat starts drooping; firmware
flags "low" below 3100 mV and triggers M8 soft-off at 3000 mV.

The 100 mV `low_mv − shutdown_mv` window typically lasts 30–60 seconds of operation as
cells finish dying — enough to flush a sustain pedal, save state, and shut the unit down
cleanly before the boost completely collapses.

**Trade-offs**:

- ✓ Truly common cell type — Eneloops everywhere.
- ✓ Smallest form factor (2× AA holder is barely bigger than the stock pouch envelope).
- ✓ Cleaner safety story than 14500 (NiMH primaries in a wrong device are inert at 1.5 V).
- ✗ No SoC percentage — the gauge shows "OK / Low / Critical," not "47 %."  Users only
  get warning when cells are nearly dead.
- ✗ Boost converter adds ~5–30 µA quiescent draw even when chip is in System OFF.  Doesn't
  matter for any realistic gig schedule (years of cell life), but worth knowing for
  drawer-storage.

---

## NiMH Option C: 1× or 2× NiMH via boost + fuel-gauge mod (accurate SoC)

Same hardware as Option B, **plus** a pre-boost voltage divider tapped into the chip's
unused AIN3 (P0_05).  Firmware reads cell voltage directly, so the SoC gauge works exactly
the way it does for LiPo or 3-cell direct NiMH — real percent, real low-battery warning,
real soft-off at the chemistry-correct threshold.

**Additional hardware on top of Option B**:

- **Pre-boost divider**: 1 MΩ + 1 MΩ resistors from cell `+` to GND, tap to P0_05.
  - For 2× AA NiMH (2.0–2.8 V): the 1:1 divider lands the input on the SAADC at 1.0–1.4 V,
    well inside the gain-1/4 0–2.4 V range.
  - For 1× AA NiMH (1.0–1.4 V): skip the divider, just put a 100 kΩ series resistor for ESD
    protection — the cell range is already inside the SAADC's gain-1/6 0–3.6 V window.

- **Divider current draw**: 2 MΩ total → ~1.5 µA continuous on 2× AA.  On 2000 mAh
  Eneloops that's 1.25 million hours.  Don't bother gating the divider.

**Firmware**:

```rust
const CHEMISTRY: BatteryChemistry = BatteryChemistry::NimhPack { cells: 2 };
// or { cells: 1 } for the single-cell case
```

⚠️ **This option also requires a small firmware change in `boards/t114/src/battery.rs`**
to add the second SAADC channel.  The `BatteryMonitor` currently only owns AIN2; for the
fuel-gauge mod it needs to grow to `Saadc<'static, 2>` with AIN3 added, and `sample()`
needs to return `BatterySample { bus_mv, cell_mv: Some(u16) }`.  The exact API is
documented inline in `battery.rs`'s module docstring — about 30–50 lines of changes when
you wire up the divider.

The chemistry enum and OCV tables are already in place (`NIMH_1CELL_OCV`, `NIMH_2CELL_OCV`
in `core/ui/src/battery.rs`); flipping the const and adding the second channel is all
that's needed.

**Trade-offs**:

- ✓ Real SoC percentage on the gauge.  M7 low-battery flow triggers at the actual NiMH
  safe-cutoff (1.0 V/cell × N), not at "boost is about to give up."
- ✓ Recommended path if you want NiMH + accurate runtime feedback.
- ✗ Smallest hardware mod story so far (2 resistors + 1 wire) but it's still a mod.
- ✗ The firmware-side second-SAADC-channel addition is not in the tree today.

---

## Which option

A rough decision tree.  Defaults assume gig use, swappable cells, no special
size/weight constraint.

```
Are you fine with the stock 800 mAh pouch?
├── Yes → keep stock.
└── No: do you want LiPo or NiMH?
    ├── LiPo (best Wh/$, smallest, but 14500-in-AA-form-factor risk)
    │   ├── 14500 (AA-sized) → option "drop-in swap"
    │   ├── 18350 (slightly fatter) → option "drop-in swap"
    │   └── 18650 (4× capacity, much fatter) → option "drop-in swap"
    └── NiMH (safer chemistry, primary-compatible form factor)
        ├── 3× AA/AAA (direct, no boost) → Option A
        └── 1× or 2× AA/AAA (smaller, needs boost)
            ├── Don't need accurate %SoC → Option B
            └── Want real %SoC → Option C
```

| Option           | Cells               | Hardware mods                                    | Firmware const                                              | %SoC gauge |
|------------------|---------------------|--------------------------------------------------|-------------------------------------------------------------|------------|
| Stock            | LiPo pouch, 800 mAh | none                                             | `LiPoSingle`                                                | yes        |
| Drop-in 14500    | LiPo, AA form       | swap to AA holder, JST-PH retained               | `LiPoSingle`                                                | yes        |
| Drop-in 18650    | LiPo, 18650         | swap to 18650 holder                             | `LiPoSingle`                                                | yes        |
| NiMH Option A    | 3× AA NiMH          | de-pop TP4054, swap to 3× holder                 | `NimhPack { cells: 3 }`                                     | yes        |
| NiMH Option B    | 1×/2× AA NiMH       | de-pop TP4054, add boost                         | `Regulated { shutdown_mv: 3000, low_mv: 3100 }`             | OK / Low only |
| NiMH Option C    | 1×/2× AA NiMH       | de-pop TP4054, add boost, add divider on P0_05   | `NimhPack { cells: 1 \| 2 }` + 2-channel SAADC firmware     | yes        |

---

---

## Power policy: handheld vs. permanent install

Independent of which cells you wire, the firmware also picks a `PowerPolicy` controlling
how the device behaves around USB power.

### `PowerPolicy::Battery` (default)

Handheld / pocketable use.

- User controls on/off explicitly: long-press Left → Right → Center to soft-off; Center
  press to wake.
- USB plug-in while soft-off shows a brief 2 s charging frame, then re-sleeps.
- Stays in real System OFF indefinitely until Center wakes it.
- This is what the stock t114 profiles ship with.  Use this when the device runs on its
  own pack and the user actively decides when it's on.

### `PowerPolicy::Wired`

Permanent-install on a host instrument (keytar, keyboard, synth, pedalboard).  The
device tracks the host's USB power: on when the host is on, off when the host is off.

- **USB plug-in → instant Idle.**  No charging frame, no Center-press required.  Wake
  source is the existing `USBDETECTED` event from the POWER peripheral; whenever the
  chip is in real System OFF, plugging USB cold-boots it straight into the UI.
- **USB unplug → 10-second grace timer.**  The device stays on for 10 s after USB power
  is lost, in case it's a brief host re-enumeration / loose cable seat.  If USB returns
  inside the grace window, the timer resets and the device stays up.  If 10 s elapses
  without recovery, the device soft-offs cleanly through the M8 deep-soft-off pipeline
  (display off, radio asleep, chip into System OFF).
- **Optional backup battery.**  A battery is *not* required in Wired mode — the device
  cold-boots on USB plug-in and dies immediately on unplug if no battery is wired.  When
  a battery is present, it bridges the 10 s grace window so transient host glitches
  don't show up as device flicker.
- **Operator soft-off gesture still works** — but if USB is still present when the SVC
  fires, the chip immediately wakes via USB detect and reboots back to Idle.  Net effect
  is a 2–3 second restart.

Tunable in `core/ui/src/lib.rs`:

- `WIRED_USB_LOSS_GRACE_SECS` — 10 seconds.  Make it shorter if you want snappier
  shutoffs; longer if your host has noisier USB power.

**Use this policy when**: the device lives on the host instrument and shouldn't require
the operator to press buttons to power it on.  Mounting it inside a keytar or under a
keyboard control panel, wired to one of the host's USB ports — the firmware just tracks
"is the host on?"

**Firmware**:

```rust
const POWER_POLICY: PowerPolicy = PowerPolicy::Wired;
```

**Decision table**:

| Scenario                                         | Battery policy                | Wired policy                       |
|--------------------------------------------------|--------------------------------|------------------------------------|
| Power on the device for a gig                    | Center press                   | Plug in (or turn on the host)      |
| Power off the device after a gig                 | Long-press Left → Right → Center | Unplug (or turn off the host)    |
| USB plug-in while soft-off                       | brief charging frame, re-sleep | instant Idle                       |
| USB unplug while running                         | nothing (battery only)         | 10 s grace, then soft-off          |
| USB temporarily disconnected (<10 s)             | n/a                            | device stays on through the dip    |
| Battery dies mid-use (no USB)                    | low-battery shutdown           | low-battery shutdown               |
| Battery dies mid-use (USB present)               | charging continues             | charging continues (Wired ignores) |
| Fresh power-on, no USB, no battery in            | impossible (no power)          | impossible (no power)              |
| Fresh power-on, no USB, battery in               | Idle, awaits Center to stay    | Idle for 10 s, then soft-off       |
| Fresh power-on, USB plugged                      | Idle                           | Idle                               |

---

## Universal safety notes

- **Never plug USB on a NiMH pack** unless you've de-popped the TP4054.  This is the
  single most likely way to damage cells in this whole guide.

- **Don't carry loose 14500 LiPos with primaries** of the same form factor.

- **Don't store charged Li-ion cells (any form factor) in metal containers** without
  individual insulation.

- **The M7 low-battery soft-off threshold is chemistry-aware** — it triggers at the
  chemistry's correct floor automatically once you set `const CHEMISTRY`.  Don't try to
  override it with a hand-picked mV value unless you've read the per-chemistry
  `BatteryChemistry::shutdown_mv()` in `core/ui/src/battery.rs` and have a specific reason.

- **The deep soft-off path (M8) measures real System OFF current at <1 µA** on any of the
  options that connect cells directly (stock, drop-in LiPo, NiMH Option A, NiMH Option C
  once the second SAADC channel lands).  On NiMH Option B (boost without fuel-gauge mod),
  the boost converter itself contributes ~5–30 µA in System OFF — still fine for years of
  shelf life, but worth knowing if you're trying to hit a sub-µA spec sheet number.

## See also

- `core/ui/src/battery.rs` — `BatteryChemistry` enum, OCV tables, threshold values.
- `boards/t114/src/battery.rs` — `BatteryMonitor` SAADC config and the inline TODO for
  the second-channel addition (Option C).
- `PLAN.md` § Milestone 8 — full M8 status including the low-battery soft-off pipeline.
- `core/ui/src/render.rs` — battery indicator widget logic; renders `voltage_mv == 0` as
  the "no cell" placeholder (`--%`).
