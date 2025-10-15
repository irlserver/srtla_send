# Project Overview

## Purpose
SRTLA Sender is a Rust implementation of the SRTLA bonding sender. SRTLA is a SRT transport proxy with link aggregation for connection bonding that can transport SRT traffic over multiple network links for capacity aggregation and redundancy. The intended application is bonding mobile modems for live streaming.

## Key Features
- Multi-uplink bonding using local source IPs
- Registration flow (REG1/REG2/REG3) with ID propagation
- SRT ACK and NAK handling with correct NAK attribution to sending uplink
- Burst NAK penalty for connections experiencing packet loss bursts
- Keepalives with RTT measurement and time-based window recovery
- Stickiness-aware path selection: score = window / (in_flight + 1)
- Live IP list reload via SIGHUP (Unix)
- Runtime toggles via stdin (no restart required)

## Tech Stack
- **Language**: Rust (Edition 2024, requires nightly toolchain)
- **Minimum Rust Version**: 1.87
- **Async Runtime**: Tokio (multi-threaded runtime with macros, net, time, io-util, signal)
- **CLI**: clap with derive features
- **Logging**: tracing + tracing-subscriber with env-filter
- **Networking**: socket2 for low-level socket operations, tokio::net for async UDP
- **Other dependencies**: anyhow (error handling), rand, bytes, chrono, smallvec

## Build Profiles
- `dev`: opt-level = 1
- `release-debug`: release with debug symbols, thin LTO
- `release-lto`: full fat LTO, stripped symbols

## Version
Current version: 2.3.0
