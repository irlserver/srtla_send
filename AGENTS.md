# AI Agent Context for SRTLA Sender

This document provides context for AI coding assistants working on the SRTLA Sender project.

## Credits & Acknowledgments

This Rust implementation builds upon several open source projects and ideas:

- **[Moblin](https://github.com/eerimoq/moblin)**  - Inspired by ideas and algorithms
- **[Original SRTLA](https://github.com/BELABOX/srtla)** - The foundational SRTLA protocol and reference implementation by Belabox

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
- Runtime configuration via stdin (no restart required)

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

Current version: 3.0.0

## Codebase Structure

### Directory Layout

```
src/
  tests/
    config_tests.rs
    connection_tests.rs
    end_to_end_tests.rs
    integration_tests.rs
    mod.rs
    protocol_tests.rs
    registration_tests.rs
    rtt_threshold_tests.rs
    sender_tests.rs
  config.rs          - Runtime configuration (DynamicConfig, ConfigSnapshot)
  connection.rs      - Connection management and quality scoring
  lib.rs             - Library exports
  main.rs            - CLI entry point
  mode.rs            - Scheduling mode enum (Classic, Enhanced, RttThreshold)
  protocol.rs        - SRTLA protocol definitions
  registration.rs    - Registration flow (REG1/REG2/REG3)
  sender.rs          - Main sender logic and packet forwarding
  test_helpers.rs    - Test utilities
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

- `config`: Runtime configuration with atomic settings (DynamicConfig, ConfigSnapshot)
- `mode`: SchedulingMode enum (Classic, Enhanced, RttThreshold)
- `connection`: SrtlaConnection struct, bind/resolve utilities, incoming packet handling
- `protocol`: SRTLA protocol constants and structures
- `registration`: Registration manager for SRTLA connection setup
- `sender`: Main packet forwarding logic, connection selection algorithm
- `utils`: Common utilities (now_ms, etc.)
- `version`: `-v/--version` line composition. The build metadata is OPTIONAL: `build.rs` emits an
  empty string for anything it could not resolve (no git checkout, detached HEAD), and
  `compose_version_line()` drops the parenthetical entirely rather than printing a placeholder
  word. Do not reintroduce `"unknown"`, and do not infer "dirty" from a non-zero `git diff` exit
  code (it exits 128/129 when there is no repository at all).

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

## Robustness Behaviors

Recovery behaviors whose *trigger policy* is the load-bearing part. Change the
mechanism freely; change a trigger only with the reasoning below in hand.

### All-links-failed timeout

`sender::housekeeping` arms `all_failed_at` the first tick on which every uplink
is timed out, and measures elapsed-time-*since*-failure against
`GLOBAL_TIMEOUT_MS`. It must never be re-derived from process uptime: that made a
transient all-down blip trip the timeout the instant uptime exceeded the window.

### Whole-bond re-home (`sender::rehome`, `--no-rehome` to disable)

When the bond is dead and the receiver's hostname has moved, migrate **every**
uplink to the new address together and re-register from scratch.

- **All-or-nothing, always.** SRTLA registration binds the bond to a
  receiver-generated connection id, which only means something to the receiver
  *instance* that minted it. Repointing one uplink splits the bond across two
  receiver identities. `io.remote` therefore may only change here, and only
  because it changes for the whole bond in one housekeeping pass. The per-uplink
  reconnect path in `sender::connections` stays **detect-only** (it warns about
  DNS drift and never swaps the address).
- **Trigger — all three, conservatively.** (1) Every uplink timed out *and* the
  bond has stayed that way past the existing all-failed window above — the same
  timer, not a parallel one; a bond with any live uplink is never touched.
  (2) A fresh lookup of the receiver hostname succeeds *and* returns none of the
  addresses the bond is pinned to. A failed, timed-out, or empty lookup is **not**
  drift, and neither is a reordered multi-A answer that still lists one of ours —
  GeoDNS and round-robin must not cause thrash. (3) At most one attempt per
  `REHOME_MIN_INTERVAL_MS` (60s), process-wide; the limit covers the DNS probe
  too, since housekeeping ticks every second.
- **Client id survives.** `SrtlaRegistrationManager::reset_for_rehome` keeps the
  first half of `srtla_id` (the only part a receiver reads out of a REG1) so a
  receiver deriving its half deterministically reissues the identical `full_id`
  and a load balancer keeps tracking one continuous group. The discarded half is
  re-randomized, not zeroed, so the REG1 is byte-shaped exactly like a fresh
  sender's.
- **Wire compatibility.** Standard RTT probe → REG1/REG2/REG3 only. No new packet
  types and no registration-timing changes: a re-home must be indistinguishable
  from a fresh sender start to the C reference receiver, BELABOX, and irlserver
  `srtla_rec`.
- **Telemetry is log-only.** The decision is logged once per attempt at `warn`.
  `StatsSnapshot` is a published JSON/Prometheus shape and was deliberately not
  widened; `rehome_on_failure` is likewise absent from the hot-path
  `ConfigSnapshot`.
- **Test gap.** The trigger policy and mechanism are unit-tested through a
  `ReceiverResolver` seam (`sender::rehome::StubResolver`). There is no netns
  integration test: `tests/common` starts the sender against `topo.receiver_ip`,
  an IP literal, so "the receiver address moves" would need a hostname plus a
  per-namespace resolver whose answer changes mid-test — new harness
  infrastructure, not a small addition.

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

## Style Guide

- Write commit messages using [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/).
- Never bump the internal package version in `Cargo.toml`. This is handled automatically by the release process.
- Rust files use LF line endings.

### Important Notes

- **NEVER commit changes unless explicitly asked by the user**
- All steps must pass before considering the task complete
- Tests require the `test-internals` feature for full coverage
- The project requires Rust nightly toolchain
- Use `RUST_LOG=info` or `RUST_LOG=debug` for runtime debugging
