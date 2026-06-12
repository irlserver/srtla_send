# srtla-send-rs

Parent: [`../AGENTS.md`](../AGENTS.md)

## ROLE IN THE GROUP

CERALIVE's **fork** of [`irlserver/srtla_send`](https://github.com/irlserver/srtla_send) —
the Rust SRTLA bonding **sender**. It reads local SRT (UDP) on a listen port and forwards
it over multiple bonded uplinks (one bound UDP socket per source IP) to an SRTLA receiver,
balancing traffic by link capacity/quality (EDPF/BLEST/IoDS scheduling, Kalman-smoothed
RTT). On the device it is driven by CeraUI and feeds the bonded path into
`irl-srt-server`. Canonical branch `main`; sibling checkout under the workspace root
(see CRITICAL CONSTRAINTS below).

> **Status:** bootstrap (Task 7) + telemetry sink (Task 10). Fork created from
> upstream HEAD; nightly pinned; baseline gate green on the pinned toolchain. The
> opt-in `--stats-file` ADR-001 telemetry sink is implemented (`src/telemetry_file.rs`);
> packaging/parity wiring and CeraUI integration land in follow-up tasks.

**Relationship to `srtla/`:** this is the **sender** engine (Rust). The existing
`srtla/` repo holds the C `srtla_send`/`srtla_rec` pair plus the bonding receiver and
the **TypeScript bindings** (`@ceralive/srtla`, consumed by CeraUI via the sibling
`link:`). **The TS bindings stay in `srtla/`.** This repo ships a binary only — it has
**no `bindings/` directory** and must never grow one.

## UPSTREAM RELATIONSHIP

Two remotes — keep both:

```
origin    https://github.com/CERALIVE/srtla-send-rs.git   (our fork; push here)
upstream  https://github.com/irlserver/srtla_send.git     (read-only; merge source)
```

- **License:** upstream is **MIT**; CERALIVE ships under **AGPLv3**. MIT → AGPLv3
  incorporation is compatible. Keep upstream's `LICENSE` and credits intact; add CERALIVE
  licensing at the workspace/distribution layer, not by stripping upstream notices.
- **Fork start point:** upstream `80cd0c4` ("feat: use Kalman-smoothed RTT in EDPF
  arrival time prediction").

### Upstream-merge policy — MANUAL & COMPAT-GATED

- **Upstream merges are MANUAL and COMPAT-GATED. Never set up auto-sync / scheduled
  upstream merges / bots that open update PRs.** Pull upstream deliberately, in a
  dedicated PR, only when there is a reason to.
- Each merge runs the **full gate green** (below) on the **pinned** toolchain before
  it can land, and must not regress the CLI parity contract or the telemetry contract.
- Upstream HEAD is sometimes red on its own CI (e.g. an unformatted commit failing
  "Check formatting"). Do **not** import red upstream state — green it with a
  **mechanical-only** `cargo fmt` + `clippy --fix` pass (zero behavior change) as part
  of the merge PR, exactly as the bootstrap did.

## PINNED TOOLCHAIN

`rust-toolchain.toml` pins an **exact** nightly: `channel = "nightly-2026-06-12"`
(rustc 1.98.0-nightly, `b30f3df3b`), components `rustfmt clippy rust-src`, target
`aarch64-unknown-linux-gnu`.

- **Nightly is mandatory** — `rustfmt.toml` enables unstable formatting features
  (edition 2024, `group_imports`, `format_strings`, …). Upstream CI floats on
  `nightly`; the fork pins a date so the **device image build is reproducible**.
- **Bump only deliberately** (a toolchain-bump or upstream-merge PR), and re-run the
  full gate after any bump. Never let the pin drift silently.
- **aarch64 cross-build** (device target) needs the GNU cross linker; mirror upstream's
  `build-debian.yml`:
  - linker: `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc`
  - apt: `gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libc6-dev-arm64-cross binutils-aarch64-linux-gnu pkg-config`
  - `PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig`

## PARITY CONTRACT (do not break without a versioned change)

CeraUI and the device integration depend on these staying stable:

- **Binary name:** exactly `srtla_send`.
- **CLI positional order:**
  `srtla_send <SRT_LISTEN_PORT> <SRTLA_HOST> <SRTLA_PORT> <BIND_IPS_FILE> [OPTIONS]` —
  the four positionals are load-bearing and resolved in exactly the order CeraUI's
  `buildSrtlaSendArgs` emits them.
- **CeraLive control-plane flags:** `--verbose` (debug logging), `--dry-run` (parse the
  IP list and resolve the receiver, print them, then exit `0` without binding any socket;
  an unusable IP list — missing/unreadable, empty, or zero valid IPs — exits non-zero with
  a specific error), `--stats-file <path>` and `--stats-file-interval <ms>` (default
  `1000`). The `--stats-file` telemetry sink is **implemented** (`src/telemetry_file.rs`)
  and opt-in — absent means no file is ever written.
- **Upstream scheduler/control-socket flags** (`--mode`, `--no-quality`, `--exploration`,
  `--rtt-delta-ms`, `--control-socket`, `-v/--version`) stay present and functional but
  are **not** surfaced in CeraUI.
- **Telemetry contract (`--stats-file <path>`, ADR-001):** opt-in (absent ⇒ no file is
  ever written). Newline-free JSON document, atomically published (temp sibling →
  `fsync` → `rename(2)`), shape
  `{"schema_version":1,"last_updated_ms":<ms>,"connections":[{"conn_id","rtt_ms","nak_count","weight_percent","window","in_flight","bitrate_bps"}]}`.
  `bitrate_bps` is wire-bytes/s × 8 (the ×8 bits-per-second conversion is mandatory);
  `conn_id` is the string IP-list index (stable until a SIGHUP reorder); `window` and
  `in_flight` are **required** by the frozen `@ceralive/srtla` Zod reader. The cadence is
  `--stats-file-interval` ms (default 1000). The live file is unlinked on clean shutdown
  (SIGTERM/SIGINT). `schema_version` is additive over the C producer — the Zod reader
  strips it. Implemented in `src/telemetry_file.rs`; CeraUI parses this verbatim.
- **IP-list reload:** `SIGHUP` (Unix) reloads `BIND_IPS_FILE` without restart.

## BUILD / GATE

Run the **full gate green on the pinned nightly** before every PR (it auto-selects via
`rust-toolchain.toml`):

```bash
cargo build --release
cargo fmt --all -- --check
cargo clippy -- -D warnings          # lib + bin (matches upstream ci.yml)
cargo test --lib
cargo test --all-features
cargo test --features test-internals # upstream's full-coverage suite
```

`test-internals` exposes internal fields for assertions; `--all-features` enables it.
The whole sender test corpus runs in-process over loopback UDP — **no root / netns /
CAP_NET_ADMIN required** (0 tests are gated/ignored).

## CODEBASE (inherited from upstream)

```
src/
  main.rs            CLI entry point (clap)
  lib.rs             library exports
  config.rs / config/    runtime config (DynamicConfig, ConfigSnapshot); stdin + Unix-socket control
  mode.rs            SchedulingMode (Classic | Enhanced | RttThreshold)
  connection/        SrtlaConnection, bind/resolve, incoming packet handling, RTT (Kalman)
  protocol.rs        SRTLA protocol constants/structures
  registration.rs    REG1/REG2/REG3 flow + ID propagation
  sender/            packet forwarding + selection/ (BLEST → IoDS → EDPF), status logging
  tests/             unit / integration / e2e / protocol / registration suites
crates/network-sim/  dev-only network simulation harness (workspace member)
rust-toolchain.toml  pinned nightly (CERALIVE)
rustfmt.toml         unstable nightly fmt config (edition 2024)
.github/workflows/   upstream CI (ci.yml) + Debian packaging (build-debian.yml)
```

Conventions (enforced by the gate): edition 2024, `anyhow::Result`, `tracing` macros,
Tokio async, imports grouped std → external → crate (module granularity), constants
`SCREAMING_SNAKE_CASE`. Three scheduling modes; enhanced (default) adds NAK-decay
quality scoring + optional exploration. See `README.md` for the full operator/runtime
reference (modes, runtime commands, tuning constants).

## ANTI-PATTERNS

- **No `bindings/` directory.** TS bindings live in `srtla/` (`@ceralive/srtla`). This
  repo is a binary; do not add a bindings tree or a sibling `link:` for it.
- **No auto-sync with upstream.** Merges are manual + compat-gated (see policy above).
- **Don't unpin / silently bump the toolchain.** The exact nightly is load-bearing for a
  reproducible device build.
- **Don't break the parity contract** (binary name, CLI positional order, telemetry
  JSON shape, `bitrate_bps` ×8, SIGHUP reload) without a deliberate versioned change —
  CeraUI depends on it.
- **Don't strip upstream MIT/credits.** Layer AGPLv3 at distribution, keep notices.
- **No path above the repo root in any tracked file.** This repo builds, tests, and
  releases **standalone** in CI; the workspace parent does not exist there. The local
  orchestration scratch dir is gitignored and must appear in no other tracked file
  (Rule D).

## DOCS DISCIPLINE (Rule A)

Any behavior/structure change updates this `AGENTS.md` and `README.md` in the SAME PR.
Keep the parity contract section authoritative — it is the device-integration contract.
