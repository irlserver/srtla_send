# Changelog

Changes on the `exp` branch since the `v3.0.0` release (https://github.com/irlserver/srtla_send/releases).

This is an overview of the **current** state of `exp`, not a replay of every commit. Several ideas were prototyped and then removed before they reached this branch tip; those are listed under "Explored and removed" so the commit history makes sense. The version in `Cargo.toml` is still `3.0.0`, so this work is unreleased.

## Scheduling and link selection

The runtime still has exactly two scheduling modes: `classic` (capacity based, matches the original C behaviour) and `enhanced` (quality aware, the default). Enhanced selection gained an admission gate on top of its quality scoring:

* **Weak-link admission gate.** A classifier (`sender/selection/classifier.rs`) marks a link weak when its RTT busts the chosen delay tier or its throughput share falls below an entering threshold, with enter/leave hysteresis. Enhanced selection excludes weak links from ranking when at least one healthy link can carry the packet, and falls back to the full pool when every link is weak (better to send on a weak link than drop the packet).
* **Congestion-aware skip.** Each connection carries a `cc_backing_off` flag set by the per-link CC controller. Enhanced selection treats a backing-off link as an extra weak signal and skips it under the same fallback rule.
* **Critical-packet priority.** During a keyframe (IDR / SPS / PPS) the scheduler routes packets to the highest-quality link. Two triggers feed this: a heuristic burst detector (`sender/keyframe.rs`, which declares a burst after 5 consecutive max-MTU 1316-byte packets), and an explicit critical window opened by an upstream encoder (see the priority sidecar below).

## RTT estimation

* **Kalman filter as the primary RTT estimator.** `RttTracker` now uses a 2-state Kalman filter (value, velocity) as its smooth RTT source, replacing the old smooth/fast EWMA pair. The filter tracks trends naturally, so its velocity term doubles as spike detection. A new `kalman.rs` module holds the filter and its RTT preset.
* **Dual-window minimum tracking and a sample filter.** RTT keeps a fast and a slow minimum window plus a small min-sample filter for stability, exposed through `LinkStats`.
* EWMA is retained only for the symmetric `rtt_avg_delta`. The asymmetric (fast-down, slow-up) EWMA variant exists in the tree but is `#[cfg(test)]` only, so it does not compile into the production binary.

## Per-link congestion control

* **`link_cc` controller** (`sender/selection/link_cc.rs`). A per-connection state machine (`CcState`: Bootstrap, Climbing, Holding, BackingOff, Drain) with a `ClimbMode` sub-state (Hai, FastRecovery, Normal) that produces a `target_bps` soft cap from age-bucketed RTT EWMA, RTT variance, a sliding-window loss permille, and observed bitrate.
* **Current wiring.** Only the `BackingOff` state influences selection today (via the `cc_backing_off` flag described above). The `target_bps` soft cap is computed and exported for telemetry but is not yet applied as a rate cap in the data path.

## Transport and egress

* **Pluggable uplink binder.** A new `UplinkBinder` trait makes egress steering injectable. `SourceIpBinder` keeps the existing source-IP bind (Linux source routing); a callback binder lets a library consumer steer the raw fd (for example Android `Network.bindSocket`) while keeping `IpAddr` as the uplink identity. The binder is threaded through sender startup, connection creation, and reconnect so the same binding is re-applied on reconnect.
* **Adaptive batch-send regimes.** `batch_send.rs` picks a batch threshold per connection from observed bitrate: LowActivity (under 500 kbps, threshold 4), Normal (500 kbps to 5 Mbps, threshold 16), HighLoad (above 5 Mbps, threshold 32). The 15ms flush interval is unchanged. The regime is recomputed once per housekeeping tick.

## Configuration, control, and observability

* **TOML config file.** A `--config` flag loads tunable constants from TOML (`toml_config.rs`), falling back to defaults on error, reloaded on SIGHUP.
* **Dynamic runtime config.** `DynamicConfig` replaces the old `DynamicToggles`, holding the scheduling mode and toggles behind atomics for thread-safe runtime changes over stdin and a control socket.
* **JSON-RPC control socket.** A Unix-socket control plane (`control.rs`, `control_socket.rs`, `--control-socket`) supports `set_mode` (classic or enhanced), `set_quality`, `get_status`, `get_stats`, and `subscribe` / `unsubscribe` to the `stats` and `priority.window` topics. Documented in `docs/CONTROL_PROTOCOL.md`.
* **Critical-packet priority sidecar.** A dedicated UDP socket (`priority.rs`, `--priority-bind`) takes a 5-byte datagram from an encoder to open a critical window of N milliseconds. Loopback UDP shares the data path's network stack, so the hint stays tightly ordered against the packets it describes (tighter than the out-of-band JSON-RPC channel). Overlapping windows extend the deadline monotonically. Documented in `docs/KEYFRAME_PRIORITY.md`.
* **Prometheus metrics endpoint.** A hand-rolled `/metrics` HTTP server (`metrics.rs`, `--metrics-bind`, no axum/hyper dependency) exports per-link and aggregate gauges plus the current mode. A shared stats layer (`stats.rs`, `subscriptions.rs`) backs both the metrics endpoint and the control socket subscriptions.

## Testing and tooling

* **`network-sim` crate.** A new workspace crate under `crates/network-sim` providing an integration-test harness, impairment models, scenario definitions, and topology helpers.
* **Network-namespace integration tests.** `tests/netns_basic.rs` (registration and forwarding), `tests/netns_failure.rs` (link failure and recovery), `tests/netns_impairment.rs` (adaptation to impairments), and `tests/netns_scenario.rs` (stability under evolving conditions).
* **CodeRabbit** review config added (`.coderabbit.yaml`).
* Test-only items moved from `#[allow(dead_code)]` to `#[cfg(test)]` gating, with new unit coverage for the CC state machine, batch regimes, weak-link gating, and the TOML config.

## Housekeeping

* Removed the bundled `receiver` symlink and the `.serena` memory files.
* Selection-strategy modules (`classic`, `enhanced`) made private.
* Connection-rotation tuning: `MIN_SWITCH_INTERVAL_MS` to 15, `STARTUP_GRACE_MS` to 5000.
* Dependency refreshes in `Cargo.lock`, plus `cargo fmt` and clippy cleanups across all targets.

## Explored and removed (not in the current branch)

These appear in the commit history between `v3.0.0` and `exp` but are **not present in the current tree**. They were prototyped, then dropped or superseded:

* **EDPF scheduler (Earliest Delivery Path First), BLEST head-of-line guard, and IoDS reordering prevention.** The whole arrival-time-prediction scheduling pipeline was removed. No `edpf` / `blest` / `iods` modules exist; `congestion/` holds only `classic`, `enhanced`, and `mod`.
* **Shared bottleneck detection (RFC 8382).** Removed along with the EDPF pipeline it fed.
* **Connection exploration (`--exploration`, `set_exploration`).** Removed entirely: the flag, the JSON-RPC method, `sender/selection/exploration.rs`, and the `enable_explore` plumbing. The original version probed the *second-best* link (usually a healthy link already earning its own ACKs), fired every 30s whether or not anything was wrong, and had no budget. It diverted roughly half of all traffic for as long as its trigger held, surviving only because the switch cooldown happened to rate-limit it. It was rewritten as a bounded probe of *starved* links (one packet per link per 200ms) and then measured on the netem testbed against the scenario it exists for: a link gated to 0.00 Mbps, silently healed, then needed when the healthy link collapsed. It moved delivery 0.76 pts (Welch t=0.75, n=15), which is to say not at all. `GATED_LINK_PENALTY` already keeps a gated link *rankable* rather than excluded, so it retains a 0.2 to 0.5 Mbps trickle and re-adopts itself about 7s after healing, and the classifier's probation re-test covers the share-starvation latch on top of that. A mechanism that cannot be shown to help is debt, so it is gone rather than kept off by default.
* **RTT-threshold scheduling mode and the `edpf` mode.** `SchedulingMode` now has only `Classic` and `Enhanced`; the parser explicitly rejects `rtt-threshold` and `edpf`, and there is no `--rtt-delta-ms` flag in the CLI. `README.md` was updated to drop these modes (along with the stale `set_rtt_delta` and `mark_critical` control-socket examples), and `docs/RTT_THRESHOLD_SCHEDULING.md` was removed.
