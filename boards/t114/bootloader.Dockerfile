# Build the Heltec T114 bootloader (S140 v7.3.0) in a reproducible
# container.  Mirrors what Adafruit's GitHub CI does — same Ubuntu,
# same ARM GCC version, same Python deps — so we sidestep all the
# host-toolchain mess (the GCC 14 false-positives the user hit on
# `brew install --cask gcc-arm-embedded` go away because CI uses
# 12.3.Rel1, which builds the bootloader cleanly).
#
# Source of truth:
#   https://github.com/adafruit/Adafruit_nRF52_Bootloader/blob/master/.github/workflows/githubci.yml
#
# Usage (from the bootloader-source repo, e.g. /Users/jacob/Projects/
# Adafruit_nRF52_Bootloader_OTAFIX):
#
#   # 1. Build the image once (a few minutes — downloads ARM GCC):
#   docker build -t osrf/bootloader-builder \
#       -f /Users/jacob/Projects/wireless-performer-fw/boards/t114/bootloader.Dockerfile \
#       /Users/jacob/Projects/wireless-performer-fw/boards/t114/
#
#   # 2. Run the build, mounting the bootloader source:
#   docker run --rm \
#       -v /Users/jacob/Projects/Adafruit_nRF52_Bootloader_OTAFIX:/build \
#       osrf/bootloader-builder
#
# Default build target is `heltec_t114` + S140 v7.3.0.  Override at
# run-time:
#   docker run --rm -v $(pwd):/build osrf/bootloader-builder \
#       make BOARD=heltec_t114 SD_VERSION=7.3.0 all
#
# Build artifacts land in `_build/build-heltec_t114/` inside the
# mounted source directory — accessible from the host afterwards.
# Notably:
#   _build/build-heltec_t114/heltec_t114_bootloader-<ver>_s140_7.3.0.hex
# is the merged MBR + SD + bootloader hex you flash via openocd.

FROM ubuntu:24.04

# ARM GCC version matching Adafruit CI's
# `carlosperate/arm-none-eabi-gcc-action@v1` with release `12.3.Rel1`.
# Bumping this means we may pick up new -Werror cascades the upstream
# Makefile doesn't suppress; pin until CI bumps.
ARG GCC_VERSION=12.3.rel1

# OS deps.  `ca-certificates` is needed for the curl over HTTPS;
# `xz-utils` for the GCC tarball; `python3-venv` because Ubuntu 24.04's
# system Python is externally-managed (PEP 668) and we'll install pip
# packages with `--break-system-packages` rather than spinning up a
# venv inside a one-shot container.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
        make \
        python3 \
        python3-pip \
        python3-venv \
        xz-utils && \
    rm -rf /var/lib/apt/lists/*

# Install ARM GCC at the CI-pinned version.  The Adafruit GitHub
# action `carlosperate/arm-none-eabi-gcc-action` resolves `12.3.Rel1`
# to the official ARM-published tarball, which we fetch directly here
# so the container is fully reproducible.  Pick x86_64 vs aarch64 at
# build time so the same Dockerfile works on Apple Silicon and Intel
# alike (Mac users: Docker Desktop emulates linux/amd64 by default
# but linux/arm64 is faster — `docker build --platform linux/arm64`).
RUN set -eux; \
    case "$(uname -m)" in \
        x86_64)  ARM_ARCH=x86_64 ;; \
        aarch64) ARM_ARCH=aarch64 ;; \
        *) echo "unsupported host arch: $(uname -m)" && exit 1 ;; \
    esac; \
    curl -fsSL "https://developer.arm.com/-/media/Files/downloads/gnu/${GCC_VERSION}/binrel/arm-gnu-toolchain-${GCC_VERSION}-${ARM_ARCH}-arm-none-eabi.tar.xz" \
        -o /tmp/arm-gcc.tar.xz; \
    mkdir -p /opt/arm-gcc; \
    tar -xJf /tmp/arm-gcc.tar.xz -C /opt/arm-gcc --strip-components=1; \
    rm /tmp/arm-gcc.tar.xz; \
    /opt/arm-gcc/bin/arm-none-eabi-gcc --version

ENV PATH="/opt/arm-gcc/bin:${PATH}"

# Python deps from CI's verbatim pip command.  `--break-system-packages`
# bypasses Ubuntu 24.04's PEP 668 marker; safe inside an isolated
# container.  `setuptools` is included even though many of these
# wheels supply it, because that's what CI does — keep parity.
RUN pip3 install --break-system-packages --no-cache-dir \
        adafruit-nrfutil \
        intelhex \
        requests \
        setuptools \
        uritemplate

# (Earlier drafts of this Dockerfile installed Microsoft's `uf2conv.py`
# to synthesise a "full SD+BL UF2" alongside the Makefile's outputs.
# Removed: the Adafruit-derived bootloader's UF2 self-update path is
# intentionally bootloader-only and rejects an SD-bearing UF2 with a
# fast-blink error.  Probe-free SD upgrades go through
# `adafruit-nrfutil dfu serial` against the DFU zip over USB-CDC —
# see BOOTLOADER_UPGRADE.md.  No uf2conv needed in the build image.)

# Source lives outside the image — caller mounts via -v.
WORKDIR /build

# Defaults match what we want for OpenStageRF.  Override at run with:
#   docker run ... osrf/bootloader-builder make BOARD=foo SD_VERSION=7.3.0 all
ENV BOARD=heltec_t114 \
    SD_VERSION=7.3.0

# Wrapper that runs `make … all` and then prints a summary of which
# artifact to flash how.  Ensures the user sees the openocd vs
# adafruit-nrfutil paths laid out clearly at build time.
COPY build-bootloader.sh /usr/local/bin/build-bootloader
RUN chmod +x /usr/local/bin/build-bootloader

CMD ["build-bootloader"]
