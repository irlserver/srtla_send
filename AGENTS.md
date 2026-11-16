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
- SRT ACK and NAK handling with correct NAK attribution to sending uplink. In enhanced mode a NAK for a resend (a retransmit copy or a same-link repeat) only clears the entry and does not shrink the window, while classic mode charges it like `srtla_send.c`. In enhanced mode a NAK for a first send is charged only once the link's own SRTLA ACKs show the packet was lost, or after a 1 s fallback
- Burst NAK penalty for connections experiencing packet loss bursts
- Keepalives with RTT measurement and time-based window recovery
- Dynamic path selection: score = window / (in_flight + 1)
- Live IP list reload via SIGHUP (Unix)
- Runtime configuration via stdin (no restart required)

### Tech Stack

- **Language**: Rust (Edition 2024). `rust-toolchain.toml` pins the stable toolchain; nightly is used only for rustfmt
- **Minimum Rust Version**: the `rust-version` in `Cargo.toml`, checked by the CI MSRV job
- **Async Runtime**: Tokio (multi-threaded runtime with macros, net, time, io-util, signal, sync)
- **CLI**: clap with derive features
- **Logging**: tracing + tracing-subscriber with env-filter
- **Networking**: socket2 for low-level socket operations, tokio::net for async UDP
- **Other dependencies**: anyhow (errors), rand, serde and serde_json (control protocol, stats), toml (config file), smallvec, rustc-hash (srtla-core maps), mimalloc (default allocator)

### Build Profiles

- `dev`: opt-level = 1
- `release-debug`: release with debug symbols, thin LTO
- `release-lto`: full fat LTO, stripped symbols

### Version

The root package version is the `version` field in the root `Cargo.toml`, and the crates under `crates/` carry their own versions.

## Codebase Structure

### Directory Layout

```
src/                     root crate srtla_send: the I/O shell and the binary
  main.rs                CLI (clap), config file merge, startup
  lib.rs                 library exports
  config.rs              DynamicConfig (runtime atomics), stdin command reader
  control.rs             JSON-RPC 2.0 dispatcher (stdin and Unix socket)
  control_socket.rs      async Unix control socket
  subscriptions.rs       server-push subscriptions for the control socket
  priority_listener.rs   UDP priority sidecar listener
  stats.rs               StatsSnapshot for get_stats
  metrics.rs             Prometheus /metrics endpoint
  toml_config.rs         --config TOML file
  version.rs             -v/--version line
  test_helpers.rs        test helpers (test or test-internals)
  net/                   uplink socket I/O
    batch_recv.rs        recvmmsg/sendmmsg batch socket (the only unsafe FFI besides apple.rs)
    socket.rs            uplink socket creation, egress binders, receiver DNS resolution
    apple.rs             egress binding for Apple platforms
  sender/                the event loop and everything it drives
    mod.rs               run_sender_with_config, the main select! loop
    packet_handler.rs    local SRT ingress, ACK/NAK handling, batch flushing
    uplink.rs            per-uplink reader tasks, ConnIo
    uplink_recv.rs       inbound uplink packet processing
    housekeeping.rs      1 s tick: keepalives, timeouts, reconnects
    connections.rs       uplink creation and SIGHUP reload apply
    reload.rs            SIGHUP IP-list reload guard
    rehome.rs            whole-bond re-home
    sequence.rs          sequence-to-link ring buffer
    client_dedup.rs      one forward per distinct SRT ACK/NAK
    client_pin.rs        which source the local SRT listener treats as the encoder
    no_link_log.rs       outage logging for packets no link can carry
    status.rs            periodic status log
  tests/                 in-crate tests (#[cfg(test)])
tests/                   integration tests: netns_*.rs (need root), parser_proptest.rs,
                         srtla_wire_conformance.rs, signal_shutdown.rs, startup_bind_ordering.rs
  common/                shared helpers for the integration tests
crates/
  srtla-core/            sans-IO core: connection state, selection, registration, congestion
    src/connection/      SrtlaConnection, ack_nak, rtt, bitrate, incoming, congestion/,
                         reconnection, batch_send
    src/selection/       classic, enhanced, classifier, link_cc, quality
    src/registration/    REG1/REG2/REG3 state machine, probing
    src/config_snapshot.rs, mode.rs, priority.rs, seq.rs, kalman.rs, ewma.rs, windowed_min.rs,
    utils.rs, test_helpers.rs (test or test-internals)
  srtla-protocol/        stateless wire format: parsers, builders, constants, types
  network-sim/           dev-only netns + tc netem harness for tests/netns_*.rs
.github/workflows/       ci.yml (lint, test matrix, MSRV, beta, miri, supply chain),
                         build.yml (release .debs), release.yml (draft release on tag)
.cargo/config.toml       cargo aliases (xlint, xtest) and the workspace rustflags
Cargo.toml               workspace and root package manifest
build.rs                 build script (git metadata for the version line)
rustfmt.toml             formatting configuration (nightly rustfmt)
```

### Module Organization

The workspace splits policy from I/O. `srtla-core` holds all scheduling and connection state with no sockets and no tokio in production code. Its methods take the current time as a `now_ms` argument instead of reading the clock. `srtla-protocol` is the stateless wire format. The root crate `srtla_send` owns sockets, tasks, signals, DNS, the control plane and telemetry, and drives the core types. New I/O goes in the root crate, new policy goes in `srtla-core`.

`srtla_send` (root crate):

- `config`: `DynamicConfig` (runtime atomics read through `ConfigSnapshot`) and the stdin command reader
- `control`: JSON-RPC 2.0 dispatcher shared by stdin and the Unix socket
- `control_socket`: async Unix control socket
- `subscriptions`: server-push subscriptions on the control socket
- `priority_listener`: UDP listener for the encoder priority sidecar
- `stats`: `StatsSnapshot` served by `get_stats`
- `metrics`: Prometheus `/metrics` endpoint
- `toml_config`: `--config` TOML file loading
- `net`: uplink sockets (`batch_recv`, `socket`, `apple`)
- `sender`: the event loop and everything it drives (packet handling, uplink readers, housekeeping, reload, re-home)
- `test_helpers`: test helpers, built under `test` or the `test-internals` feature
- `version`: `-v/--version` line composition. The build metadata is OPTIONAL: `build.rs` emits an
  empty string for anything it could not resolve (no git checkout, detached HEAD), and
  `compose_version_line()` drops the parenthetical entirely rather than printing a placeholder
  word. Do not reintroduce `"unknown"`, and do not infer "dirty" from a non-zero `git diff` exit
  code (it exits 128/129 when there is no repository at all).

`srtla-core`:

- `connection`: `SrtlaConnection` and its per-link state (ACK/NAK, RTT, bitrate, congestion, reconnection, batch send)
- `selection`: link selection (`classic`, `enhanced`, `classifier`, `link_cc`, `quality`)
- `registration`: REG1/REG2/REG3 state machine, plus the REG2 probes that pick the lowest-RTT link for the first REG1
- `config_snapshot`: `ConfigSnapshot` and the scheduler tunables
- `mode`: `SchedulingMode` (`Classic`, `Enhanced`)
- `priority`: `CriticalWindow`, the state behind the priority sidecar
- `seq`: wrap-aware arithmetic for 31-bit SRT sequence numbers
- `kalman`, `ewma`, `windowed_min`: numeric filters
- `utils`: `now_ms()`, the monotonic clock the shell reads and passes in
- `test_helpers`: socket-free connection builders, built under `test` or the `test-internals` feature

`srtla-protocol`: `builders`, `parsers`, `constants` and `types`, all re-exported from the crate root.

### Test Organization

- `src/tests/*.rs`: in-crate tests, compiled only under `#[cfg(test)]`.
- In-module `#[cfg(test)] mod tests` blocks in `crates/srtla-core` and `crates/srtla-protocol`.
- `tests/*.rs`: integration tests. `signal_shutdown.rs`, `startup_bind_ordering.rs` and `netns_*.rs` spawn the built binary. `parser_proptest.rs` and `srtla_wire_conformance.rs` test `srtla-protocol` directly. `tests/netns_*.rs` need root, `ip`, `tc`, `ss`, `srtla_rec` and `srt-live-transmit` (the impairment tests also need `sch_netem`), and skip at runtime when any is missing.
- The `test-internals` feature makes core fields `pub` for cross-crate tests through `#[cfg(feature = "test-internals")]` field pairs, for example `packet_log` on `SrtlaConnection`:

  ```rust
  #[cfg(feature = "test-internals")]
  pub packet_log: FxHashMap<i32, Option<u64>>,
  #[cfg(not(feature = "test-internals"))]
  pub(crate) packet_log: FxHashMap<i32, Option<u64>>,
  ```

  The root `Cargo.toml` enables it on `srtla-core` through `[dev-dependencies]`.

## Code Style and Conventions

### Formatting Configuration (rustfmt.toml)

rustfmt.toml uses unstable options, so formatting runs on nightly (`cargo +nightly fmt`):

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

- Formatting needs nightly: all code must pass `cargo +nightly fmt --all -- --check`. The pinned stable toolchain installs without rustfmt, so a plain `cargo fmt` fails instead of formatting with stable rules
- All code must pass `cargo xlint` (clippy over the whole workspace with `-D warnings`)

## Robustness Behaviors

Recovery behaviors whose *trigger policy* is the load-bearing part. Change the
mechanism freely; change a trigger only with the reasoning below in hand.

### All-links-failed timeout

`sender::housekeeping` arms `all_failed_at` the first tick on which every uplink
is timed out, and measures elapsed-time-*since*-failure against
`GLOBAL_TIMEOUT_MS`. It must never be re-derived from process uptime: that made a
transient all-down blip trip the timeout the instant uptime exceeded the window.

### Reconnect pacing for an established link

`ReconnectionState` in `srtla-core` paces the socket rebuild and REG2 that
housekeeping sends for a timed-out link that has registered before.

- **4 attempts at 1 s, then 5 s per attempt.** SRT drops the session after 5 s
  of silence, so a sub-second blip must be retried on the next housekeeping
  tick. Once the fast attempts are spent, the 5 s cadence gives a recovering
  modem with a multi-second RTT time to answer: each retry rebuilds the socket,
  which drops any reply still in flight. There is no exponential backoff,
  because a link that comes back after minutes of outage must not wait minutes
  more.
- **Only REG3 resets the count.** A successful socket rebuild only proves the
  local bind worked. If the rebuild reset the count, a link whose path stays
  dead would stay in the fast attempts forever.
- **Half a tick of slack.** Attempts are stamped with the time their tick was
  serviced, so a strict `>= 1000` check misses by a few ms whenever one tick
  runs late. The retry then slips a whole tick.

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
- **Per-family pick.** Every uplink dials the first answer in its own address
  family (`net::select_remote_for_source`). Startup resolves the receiver once
  and every uplink picks from that answer. A reload reuses the address the bond
  is pinned to in each family and looks up only a family the bond lacks (once
  per reload, under the re-home timeout). Re-home picks from its fresh answer.
  A dual-stack receiver's A and AAAA records must reach the same receiver
  group; reachability is not tested. An uplink whose family a re-home
  answer no longer offers is dropped from the bond until an IP-list reload
  names it again. An answer that no uplink's family can reach is not a move.
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

The root is also a package, so a bare `cargo test` covers srtla_send alone and skips the crates under `crates/`. Use `--workspace`, or the `xtest` alias that CI runs.

The netns tests skip when their dependencies are missing, and `NETNS_REQUIRED=1` makes a missing dependency fail the test instead (the CI netns job sets it).

```bash
# Run all tests in the workspace with all features (what CI runs)
cargo xtest

# Compile every crate without test-internals (the pub(crate) visibility side)
cargo check --workspace --lib --bins
# Compile the integration tests without the root test-internals feature
cargo check --workspace --all-targets

# Run a specific test
cargo test test_connection_score
```

### Formatting

```bash
# Format code (nightly)
cargo +nightly fmt --all

# Check formatting without modifying
cargo +nightly fmt --all -- --check
```

### Linting

```bash
# Run clippy over the workspace, warnings as errors (what CI runs)
cargo xlint

# Spell check (config in .codespellrc)
codespell
```

### Security

```bash
# Advisories and crate sources (config in deny.toml)
cargo deny check advisories sources
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
cargo +nightly fmt --all
```

### 2. Run Clippy

```bash
cargo xlint
```

All clippy warnings must be resolved (treated as errors).

### 3. Run Tests

```bash
cargo xtest

# Compile every crate without test-internals (the pub(crate) visibility side)
cargo check --workspace --lib --bins
# Compile the integration tests without the root test-internals feature
cargo check --workspace --all-targets
```

### 4. Build Verification

```bash
cargo build --release
```

## Style Guide

- Write commit messages using [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/).
- Never bump the package version in `Cargo.toml`. The maintainer bumps it as part of a release (see RELEASING.md).
- Rust files use LF line endings.

### Important Notes

- **NEVER commit changes unless explicitly asked by the user**
- All steps must pass before considering the task complete
- Tests require the `test-internals` feature for full coverage
- Formatting requires the nightly toolchain; everything else uses the stable toolchain pinned in `rust-toolchain.toml`
- Use `RUST_LOG=info` or `RUST_LOG=debug` for runtime debugging
