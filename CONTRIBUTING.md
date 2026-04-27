# Contributing to OpenStageRF

Contributions are welcome. Please read this document before submitting a pull request.

## Status

OpenStageRF is in early development — pre-v1, mostly single-author. External contributions are welcomed but reviews may be slow until the v1 hardware target (DX-LR30 / SX1262) is functional.

For non-trivial changes, **open an issue first** to discuss. This avoids you doing significant work that ends up rejected on direction grounds.

## Licensing of contributions

This project is dual-licensed under AGPLv3 (default) and a separate commercial license (see [LICENSE-COMMERCIAL.md](LICENSE-COMMERCIAL.md)). To preserve the dual-licensing path, **all contributions must grant the maintainer the right to redistribute them under either license**.

For now this is handled informally:

1. **Sign off your commits using the [Developer Certificate of Origin](https://developercertificate.org/)** by adding `Signed-off-by: Your Name <your.email@example.com>` to commit messages. The easy way: `git commit -s`.
2. **By submitting a pull request, you agree that your contribution may be redistributed under both AGPLv3 and the project's commercial license**, and that you have the right to make that grant.

A formal click-through CLA (via CLA Assistant or similar) will be required before external contributions are merged into the main branch. Until that is set up, the maintainer is the only contributor and external PRs may be held until the CLA process is in place.

## Code style

To be defined as the project matures. For now:

- Match the style of surrounding code
- C: 4-space indent, snake_case for functions and variables, `UPPER_SNAKE_CASE` for macros
- No tabs in C source
- SPDX header at the top of every source file: `// SPDX-License-Identifier: AGPL-3.0-or-later`

## What to work on

Good first contributions (once v1 is up):

- New board targets (`boards/<your_board>/board.yaml` + port glue)
- New radio drivers (`drivers/radio/<chip>/`)
- Profile entries for new use cases or regions
- Documentation in `docs/hardware_guides/`

Larger features that are on the roadmap and would benefit from contributors:

- Frequency hopping (FHSS) support — see the README's frequency-hopping notes
- Multi-device TDMA superframe (Stage 5+)
- Mobile/desktop configurator app
- Audio experiments (later — not until MIDI link is solid)

## Reporting issues

Open a GitHub issue with:

- What you tried to do
- What happened
- What you expected to happen
- Hardware: board, radio module, MCU
- Build profile (e.g. `dx_lr30_tx_basic`)
- Firmware version / commit hash
- Logs or packet captures if relevant
