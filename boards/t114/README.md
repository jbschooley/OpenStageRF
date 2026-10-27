# T114 MIDI board: build guide

The OpenStageRF transmitter and receiver are a [Heltec T114](https://heltec.org/project/mesh-node-t114/) (nRF52840 + SX1262, built-in 240×135 TFT) plugged into a small carrier board with TRS MIDI, a 5-way joystick, an optional second radio for receive diversity, and a debug header. The same board is used for the TX and the RX unit; the firmware profile decides the role.

The board files are in [`pcb/`](pcb/) (exported from EasyEDA Pro on 2026-10-26).

## What's on the board

| Part | Designator | What it does |
|------|------------|--------------|
| Heltec T114 v2.1 | U1 | MCU, radio, display; plugs into two 13-pin sockets (HX1, HX2) |
| H11L1 optocoupler + 220 Ω + 1N4007 | U2, R3, D1 | MIDI in: isolated current loop from the TRS input, the diode protects against a reversed source |
| 470 Ω | R5 | pull-up on the optocoupler output to the T114's switched 3.3 V |
| 2 × 33 Ω | R4, R6 | MIDI out current-loop resistors (3.3 V MIDI) |
| 2 × PJ-3270-4A 3.5 mm TRS jacks | MIDI_I, MIDI_O | MIDI in and out |
| K1-5202UA-01 5-way switch | SW1 | the joystick for the on-device UI (up / down / left / right / centre) |
| 2×6 socket | R2 | optional second radio (a DX-LR30 module) for receive diversity |
| 2×3 header | DBG | SWD (SWDIO, SWCLK, SWO, 3.3 V, 5 V, GND) for flashing and RTT logs |

The BOM with LCSC part numbers is [`pcb/BOM_BoardV2_PCB1_2_2026-10-26.xlsx`](pcb/BOM_BoardV2_PCB1_2_2026-10-26.xlsx); the Gerbers and the pick-and-place file are alongside it.

## TRS MIDI wiring

The jacks are wired **TRS Type A** (the MIDI Association's standard, RP-054: tip = sink, ring = source, sleeve = shield). Use adapters and cables of the same type: with a Type A/B mismatch, MIDI won't get through.

## What the firmware expects

From [`src/lib.rs`](src/lib.rs):

- **MIDI:** UARTE1 at 31250 baud 8N1, P0.09 = RX (from the optocoupler), P0.10 = TX (to the MIDI out jack).
- **Joystick:** up P1.14, right P1.12, left P0.07, down P0.08, centre P0.13 (the centre also wakes the unit from soft-off).
- **Second radio (diversity RX):** SPI3 with SCK P0.28, MISO P0.29, MOSI P0.30, NSS P0.31, BUSY P1.13, DIO1 P1.15, NRST P0.05, TXEN P0.16, RXEN P1.01.

## Building one

1. **Order the PCB** from the Gerber zip (a 4-layer board; the zip includes EasyEDA's `How-to-order-PCB.txt`). The BOM lists LCSC part numbers for the parts that have one, and the pick-and-place file is there if you want the SMD parts assembled.
2. **Fit the through-hole parts:** the two 13-pin sockets for the T114, the TRS jacks, the 2×6 socket (only if you want diversity), and the debug header.
3. **Plug in the T114** (and the DX-LR30 module on the 2×6 socket for a diversity receiver). Fit the T114's antenna for your band.
4. **Flash it** over the debug header: see [Building and flashing](../../README.md#flashing-t114) in the main README. Build a TX profile for the transmitter and an RX profile (or `ui_rx_diversity`) for the receiver, both from the same key file.

### MIDI in and out on one board

The board has both jacks, but each unit only uses one: the transmitter uses MIDI in (`MIDI_I`) and the receiver uses MIDI out (`MIDI_O`). The case is made for one jack, so pick one of these:

- **Order one version with both jacks and remove the extra one** with a desoldering pump: take `MIDI_O` off the transmitter board and `MIDI_I` off the receiver board. This was my solution and it works well. The case I designed puts the antenna port in the location of the removed jack.
- **Keep both jacks.** This needs a larger case, which you'd have to design.
- **Order two versions of the board**, one without `MIDI_O` for the transmitter and one without `MIDI_I` for the receiver.

### About the DRC errors in the PCB file

EasyEDA's DRC reports errors where the carrier's header pads sit on top of the holes of the T114 footprint. They come from placing the sockets over the module footprint's own pin holes, not from a fault on the board: the board as ordered works. If you edit the design, the clean fix is to keep the T114 footprint as an outline only (no pads or holes of its own), so the sockets are the only copper there; otherwise those errors can be ignored. I simply am not experienced enough with PCB design to spend the time on this myself.

## 3D-printed case

The case is a work in progress: the button is still hard to reach.

The print files are in [`3d/`](3d/):

| File | What |
|------|------|
| `TXTop.3mf`, `TXBottom.3mf` | transmitter case |
| `RXTop.3mf`, `RXBottom.3mf` | receiver case |
| `Flap.3mf` | flap |
| `OpenStageRF_T114.f3z` | Fusion source for both: set the user parameter `IsRx` to `0` for the transmitter or `1` for the receiver |
| `OpenStageRF_T114_TX.step`, `OpenStageRF_T114_RX.step` | STEP exports of each variant, for other CAD tools (no parameters) |

`Top.3mf` / `Bottom.3mf` are an earlier version, and the `t114-featherwing-5din-*.3mf` files are for the first FeatherWing / 5-pin DIN build.
