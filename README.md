
# Wireless Performer Firmware

## Overview
Open-source firmware platform for low-latency wireless MIDI and experimental audio systems.

## Architecture
- Apps: high-level firmware roles (TX/RX)
- Profiles: define behavior (radio config, UI, topology)
- Boards: hardware pin mappings
- Ports: platform abstraction (STM32 HAL, TI SimpleLink)
- Drivers: radio, display, MIDI, input
- Core: link layer, diversity, scheduler
- Protocols: packet formats

## Key Design Decisions

### 1. Separation of Concerns
- Apps are hardware-agnostic
- Boards define pin mappings only
- Profiles define behavior and topology

### 2. Radio Abstraction
Supports:
- SX126x
- CC13xx

### 3. Diversity Support
- Dual SPI radios
- UART slave receiver option

### 4. SPI Sharing
Multiple radios share SPI bus:
- Shared: SCK, MOSI, MISO
- Separate: CS, IRQ, BUSY, RESET

### 5. Profiles vs Boards
- Board = hardware
- Profile = behavior

### 6. Build Strategy
Single repo, multiple targets:
cmake -DPROFILE=...

### 7. Future Goals
- MIDI first
- Audio experimental later
- Eventually SDR-based systems

## Directory Structure
See folders for details.

