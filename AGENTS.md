# AI Agent Context for SRTLA Sender

This document provides context for AI coding assistants working on the SRTLA Sender project.

## Credits & Acknowledgments

This Rust implementation builds upon several open source projects and ideas:

- **[Moblin](https://github.com/eerimoq/moblin)** and **[Moblink](https://github.com/eerimoq/moblink)** - Inspired by ideas and algorithms
- **[Bond Bunny](https://github.com/dimadesu/bond-bunny)** - Android SRTLA bonding app that inspired many of the enhanced connection selection algorithms
- **[Original SRTLA](https://github.com/BELABOX/srtla)** - The foundational SRTLA protocol and reference implementation by Belabox

The burst NAK penalty logic, quality scoring, and connection exploration features were directly inspired by the Moblin and Bond Bunny implementations.

## Project Overview

### Purpose

SRTLA Sender is a Rust implementation of the SRTLA bonding sender. SRTLA is a SRT transport proxy with link aggregation for connection bonding that can transport SRT traffic over multiple network links for capacity aggregation and redundancy. The intended application is bonding mobile modems for live streaming.

### Key Features

- Multi-uplink bonding using local source IPs
- Registration flow (REG1/REG2/REG3) with ID propagation
- SRT ACK and NAK handling with correct NAK attribution to sending uplink
- Burst NAK penalty for connections experiencing packet loss bursts
- Keepalives with RTT measurement and time-based window recovery
- Dynamic path selection: score = window / (in_flight + 1)
- Live IP list reload via SIGHUP (Unix)
- Runtime toggles via stdin (no restart required)

### Tech Stack

- **Language**: Rust (Edition 2024, requires nightly toolchain)
- **Minimum Rust Version**: 1.87
- **Async Runtime**: Tokio (multi-threaded runtime with macros, net, time, io-util, signal)
- **CLI**: clap with derive features
- **Logging**: tracing + tracing-subscriber with env-filter
- **Networking**: socket2 for low-level socket operations, tokio::net for async UDP
- **Other dependencies**: anyhow (error handling), rand, bytes, chrono, smallvec

### Build Profiles

- `dev`: opt-level = 1
- `release-debug`: release with debug symbols, thin LTO
- `release-lto`: full fat LTO, stripped symbols

### Version

Current version: 2.3.0

## Codebase Structure

### Directory Layout

```
src/
  tests/
    connection_tests.rs
    end_to_end_tests.rs
    integration_tests.rs
    mod.rs
    protocol_tests.rs
    registration_tests.rs
    sender_tests.rs
    toggles_tests.rs
  connection.rs       - Connection management and quality scoring
  lib.rs             - Library exports
  main.rs            - CLI entry point
  protocol.rs        - SRTLA protocol definitions
  registration.rs    - Registration flow (REG1/REG2/REG3)
  sender.rs          - Main sender logic and packet forwarding
  test_helpers.rs    - Test utilities
  toggles.rs         - Runtime toggle system
  utils.rs           - Utility functions
.cargo/
  config.toml
.github/
  workflows/
    build-debian.yml
    ci.yml
Cargo.toml          - Project manifest
build.rs            - Build script
rustfmt.toml        - Formatting configuration
```

### Module Organization

- `connection`: SrtlaConnection struct, bind/resolve utilities, incoming packet handling
- `protocol`: SRTLA protocol constants and structures
- `registration`: Registration manager for SRTLA connection setup
- `sender`: Main packet forwarding logic, connection selection algorithm
- `toggles`: Runtime toggle system for classic mode, quality, exploration, RTT-threshold
- `utils`: Common utilities (now_ms, etc.)

### Test Organization

Tests are located in `src/tests/`:

- Unit tests: In-module tests for individual components
- Integration tests: Cross-module tests
- End-to-end tests: Full system tests
- Protocol tests: SRTLA protocol implementation tests
- Feature flag: `test-internals` exposes internal fields for testing

## Code Style and Conventions

### Formatting Configuration (rustfmt.toml)

The project uses **Rust nightly** with unstable rustfmt features:

- Edition: 2024
- `unstable_features = true`
- `wrap_comments = false`
- `imports_granularity = "Module"`
- `group_imports = "StdExternalCrate"` (std, external, crate order)
- `format_code_in_doc_comments = true`
- `format_macro_matchers = true`
- `hex_literal_case = "Lower"`
- `format_strings = true`
- `use_field_init_shorthand = true`
- `use_try_shorthand = true`

### Naming Conventions

- Constants: `SCREAMING_SNAKE_CASE` (e.g., `NAK_SEARCH_LIMIT`, `MIN_SWITCH_INTERVAL_MS`)
- Structs: `PascalCase` (e.g., `SrtlaConnection`, `SequenceTrackingEntry`)
- Functions: `snake_case` (e.g., `handle_srt_packet`, `select_connection_idx`)
- Modules: `snake_case`

### Code Patterns

- **Error Handling**: Uses `anyhow::Result` for most error propagation
- **Visibility**: Uses conditional compilation with `#[cfg(feature = "test-internals")]` to expose internal fields for testing
- **Imports**: Grouped as std → external → crate, with module-level granularity
- **Logging**: Uses `tracing` macros (`debug!`, `info!`, `warn!`, etc.)
- **Async**: Heavy use of Tokio for async networking and timers

### Documentation

- Keep code self-documenting through clear naming
- Use doc comments for public API when necessary

### Important Constraints

- **Requires Rust nightly** due to unstable rustfmt features
- All formatting must pass `cargo fmt --all -- --check`
- All code must pass clippy with `-D warnings` (warnings as errors)

## Development Commands

### Building

```bash
# Standard build
cargo build

# Release build
cargo build --release

# Release with debug symbols
cargo build --profile release-debug

# Release with fat LTO (optimized)
cargo build --profile release-lto
```

### Testing

```bash
# Run all tests (requires nightly, with test-internals)
cargo test --features test-internals

# Run with verbose output
cargo test --features test-internals --verbose

# Run all tests with all features
cargo test --all-features --verbose

# Run only unit tests (library tests)
cargo test --lib --verbose

# Run specific test
cargo test test_connection_score

# Run unit tests without test features (verify encapsulation)
cargo test --lib
```

### Formatting

```bash
# Format code (requires nightly)
cargo fmt --all

# Check formatting without modifying
cargo fmt --all -- --check
```

### Linting

```bash
# Run clippy (warnings as errors)
cargo clippy -- -D warnings

# Check compilation
cargo check

# Check release compilation
cargo check --release
```

### Security

```bash
# Install cargo-audit (first time only)
cargo install cargo-audit

# Run security audit
cargo audit
```

### Running

```bash
# Run with logging
RUST_LOG=info cargo run -- 6000 rec.example.com 5000 ./uplinks.txt

# Run with debug logging
RUST_LOG=debug cargo run -- 6000 rec.example.com 5000 ./uplinks.txt

# Run binary directly (after build)
./target/release/srtla_send 6000 rec.example.com 5000 ./uplinks.txt
```

## Task Completion Checklist

When a coding task is completed, the following steps MUST be performed:

### 1. Format Code

```bash
cargo fmt --all
```

Verify it passes with:

```bash
cargo fmt --all -- --check
```

### 2. Run Clippy

```bash
cargo clippy -- -D warnings
```

All clippy warnings must be resolved (treated as errors).

### 3. Check Compilation

```bash
cargo check
cargo check --release
```

### 4. Run Tests

```bash
# Run all tests with test-internals feature
cargo test --features test-internals --verbose

# Optionally run without test features to verify encapsulation
cargo test --lib --verbose
```

### 5. Build Verification

```bash
cargo build --release
```

### Important Notes

- **NEVER commit changes unless explicitly asked by the user**
- All steps must pass before considering the task complete
- Tests require the `test-internals` feature for full coverage
- The project requires Rust nightly toolchain
- Use `RUST_LOG=info` or `RUST_LOG=debug` for runtime debugging
