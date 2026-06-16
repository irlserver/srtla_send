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

> **Status:** v1.0.0 — CeraLive parity layer complete. Fork created from upstream HEAD;
> nightly pinned; full gate green on the pinned toolchain. Landed: CLI parity contract
> (Task 9: `--verbose`/`--dry-run`/`--stats-file`/`--stats-file-interval`), the opt-in
> ADR-001 telemetry sink (Task 10: `src/telemetry_file.rs`), signal/startup parity
> (Task 11: SIGHUP reload guard, empty-start, clean SIGTERM/SIGINT), PR#19 behavior
> verification (Task 12: keepalive cadence + jitter demotion), CI/packaging (Task 13:
> aarch64 + x86_64 cross-build producing pipeline-compatible `.deb`s -- see CI / PACKAGING),
> telemetry test hardening (Task 7: `tests/telemetry_edge_cases.rs` + `tests/telemetry_fixture_parity.rs`),
> TS binding test hardening (Task 8: `bindings/typescript/tests/telemetry-reader.test.ts`, 52 tests total),
> and sendmmsg triage (Task 25: TODO converted to tracked DEFERRED note).
> CeraUI integration lands in follow-up tasks.

**Relationship to `srtla/`:** this is the **sender** engine (Rust). The existing
`srtla/` repo holds the C `srtla_send`/`srtla_rec` pair plus the bonding receiver and
its own **TypeScript bindings** (`@ceralive/srtla`, consumed by CeraUI via the sibling
`link:`). This repo additionally ships its **own** pure-TS sender binding,
`@ceralive/srtla-send`, under `bindings/typescript/` — published to the **public npm
registry** (`@ceralive` scope) via npm **OIDC trusted publishing** and consumed
**registry-only**: **no sibling `link:`, no `.tgz` vendoring**. It is a thin,
registry-distributed helper layer over this repo's binary
(args/validation/telemetry reader); the binary itself remains the primary artifact.
The `@ceralive/srtla-send` sender/telemetry exports mirror `@ceralive/srtla`'s
`./sender` + `./telemetry` subpaths and must not import or share types with
`@ceralive/cerastream`.

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
- **IP-list reload (`SIGHUP`, Unix):** reloads `BIND_IPS_FILE` without restart.
  Surviving uplinks keep their socket + registration (no re-handshake, zero
  disconnect); the pool is rebuilt in **ips-file order** so `conn_id` tracks the
  file (reorder reorders `conn_id`, matching the telemetry contract above). A
  reload that resolves to **zero valid source IPs** — missing/unreadable, empty,
  or all-garbage — is **refused** (the stream keeps running on the existing
  links) with a specific log: `ips file not found/unreadable`, `ips file is
  empty`, `invalid IP on line N` (mixed valid+invalid still applies), or `no
  valid source IPs … keeping existing connections`. Mirrors the C reload guard
  (`srtla/src/sender_logic.h`).
- **Empty start:** a missing / empty / all-invalid `BIND_IPS_FILE` at startup is
  **not** fatal — the sender binds the local listener, starts with an empty
  uplink pool, and waits for a `SIGHUP` (CeraUI writes the file and signals once
  interfaces appear). It must not crash-loop the device.
- **Clean shutdown (`SIGTERM`/`SIGINT`, Unix):** exit `0` well within CeraUI's
  10s SIGKILL window; the `--stats-file` telemetry file (and its `.tmp` sibling)
  is unlinked so no stale snapshot outlives the process.
- **NAT-keepalive control padding (`MIN_CONTROL_PKT_LEN = 32`,
  `src/connection/packet_io.rs`):** every control-plane send (keepalive,
  REG1/REG2) routes through `send_control_padded`, which zero-pads frames
  smaller than 32 bytes up to a 32-byte wire frame — parity with the C
  `pad_sendto` (`srtla/src/protocol/pad_sendto.h`) so cellular/carrier NAT
  keepalive thresholds don't silently drop tiny control frames. **DATA is never
  padded** — the batch DATA path (`src/connection/batch_send.rs`) deliberately
  bypasses it; padding DATA would corrupt the SRT byte stream. Current control
  frames are already ≥32 B (extended keepalive 38 B, REG1/REG2 258 B) so this is
  a passthrough today, but the floor is now enforced. Pinned by
  `control_packet_padded_to_32b` + `data_not_padded` (`src/tests/connection_tests.rs`).
- **Link-liveness timeout (`CONN_TIMEOUT = 15`, `src/protocol/constants.rs`):**
  seconds of inbound silence before an established uplink is declared failed and
  re-registered. It is deliberately set to **15**, matching the bonding receiver's
  `CONN_TIMEOUT` (`srtla/src/receiver_config.h:28`) and the C sender's
  `SENDER_CONN_TIMEOUT` (`srtla/src/sender_logic.h:66`). **Upstream irlserver ships
  5 s** — that value was inherited verbatim at fork time and is the accidental drift
  T12 reconciled (the receiver holds a link for 15 s while echoing keepalives, so a
  sender that gives up at 5 s falsely re-registers and resets the window on a link
  that is merely mid radio-stall). Real dead-link detection is unaffected — the
  send-failure path (`sender/packet_handler.rs` → `mark_for_recovery`) and quality
  scoring drop a dead/struggling link in ~1 s. **Do not let an upstream merge revert
  15 → 5**; the value (and the sender == receiver relationship) is pinned by
  `conn_timeout_value_pinned` (`src/tests/integration_tests.rs`).

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

The `@ceralive/srtla-send` TS binding (`bindings/typescript/`) has its own gate:

```bash
cd bindings/typescript && bun install && bun tsc --noEmit && bun test
```

`tsc --noEmit` typechecks **everything** via `tsconfig.json` (tests included). The
shipped build, however, emits via `tsconfig.build.json` (`extends tsconfig.json`,
`exclude: ["src/**/*.test.ts"]`) so compiled tests never land in `dist/` — the
published tarball is `dist/` non-test output + `package.json` only. Control tarball
contents at the **build-emit** layer, not `.npmignore`: with `files: ["dist"]` an
allowlist, `.npmignore`'s test-source pattern can't strip already-compiled
`dist/**/*.test.js`.

## CI / PACKAGING

Three workflows. The two Rust `.deb` workflows build on the **pinned nightly**
(`setup-rust-toolchain` with no `toolchain` input reads `rust-toolchain.toml`); the
binding-publish workflow is Node/Bun and shares no triggers with them:

- **`ci.yml`** (push/PR) — the gate (`fmt`, `clippy -D warnings` lib+bin, `check`,
  the full test fan-out, `cargo audit`) **plus** a `build-deb` matrix that
  cross-compiles `aarch64-unknown-linux-gnu` (device) and `x86_64-unknown-linux-gnu`
  and packages each `.deb` so a packaging break is caught before any tag. Upstream's
  stable/beta/windows/macOS jobs are kept; under the pin they must call `cargo +<channel>`
  (explicit `+` outranks `rust-toolchain.toml`) to actually exercise that channel.
- **`release.yml`** (tag push `v*`) — rebuilds both arches, packages, runs the glob
  gate, and attaches both `.deb`s + `.sha256`s to the GitHub release for the tag.
  No crates.io publish; no scheduled upstream-sync.
- **`publish-bindings.yml`** (tag push **`bindings-v*`**) — publishes
  `@ceralive/srtla-send` to the **public npm registry** (`@ceralive` scope,
  `registry-url: https://registry.npmjs.org/`) via npm **OIDC trusted publishing**
  (`permissions: id-token: write`, `npm publish --access public` — **no `NODE_AUTH_TOKEN`**;
  requires npm ≥ 11.5.1 / Node ≥ 22.14). Mirrors `@ceralive/cerastream`'s publish flow.
  Gated by `bun run typecheck && bun test` then `bun run build` (+ a tarball guard that
  only compiled `dist/` ships) before publish. A `workflow_dispatch` run with
  `dry_run=true` performs a registry dry-run. **Binding version source:** the binding
  ships on its **own** tag namespace `bindings-vYYYY.M.P` (CalVer, matching
  `@ceralive/cerastream`), deliberately distinct from the Rust crate's `v*` release tags
  — the two namespaces keep the `.deb` release and the binding publish fully decoupled
  (no shared trigger). A `-rc.N` suffix publishes under the `next` dist-tag; a plain
  version under `latest`. The published version **is** the committed
  `bindings/typescript/package.json` `version`; the tag does not mint it. A guard step
  refuses to publish unless the tag's version equals `package.json` `version`, so a tag
  can never ship a stale/mismatched version. Cut a binding release: bump `package.json`
  `version` → commit → `git tag bindings-vYYYY.M.P && git push --tags`.

**`ci/build-deb.sh` is the single source of truth** for the `.deb` and is called by both
workflows. It pins the contract the device image depends on:

- **Package:** `srtla-send-rs`; **binary at** `/usr/bin/srtla_send`; **Architecture**
  `arm64` (aarch64 build) / `amd64` (x86_64 build).
- **Filename** `srtla-send-rs_<ver>_<arch>.deb` — matches `image-building-pipeline`
  `fetch-debs.sh`'s `*${ARCH}*.deb` glob; the script re-runs that exact glob as a
  self-test so a rename fails the build, not the image fetch.
- **`Conflicts: srtla (<< <cutover>)`** (and matching `Replaces:`) — pre-cutover srtla
  shipped the C `/usr/bin/srtla_send`, so the two packages file-conflict for any srtla
  release that still ships it. The bound is `SRTLA_CUTOVER_VERSION` (default `2026.6.2`),
  the first **receiver-only** srtla release (ADR-003 accepted): srtla `<< 2026.6.2`
  conflicts (C sender present); `2026.6.2` and later coexist (receiver only).

aarch64 cross-build env (mirrors the PINNED TOOLCHAIN note): linker
`CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc`, apt
`gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libc6-dev-arm64-cross binutils-aarch64-linux-gnu pkg-config`,
`PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig`.

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
ci/build-deb.sh      single-source .deb packager (control + filename + glob self-test)
.github/workflows/   ci.yml (gate + cross-build/package) + release.yml (tag-triggered)
```

Conventions (enforced by the gate): edition 2024, `anyhow::Result`, `tracing` macros,
Tokio async, imports grouped std → external → crate (module granularity), constants
`SCREAMING_SNAKE_CASE`. Three scheduling modes; enhanced (default) adds NAK-decay
quality scoring + optional exploration. See `README.md` for the full operator/runtime
reference (modes, runtime commands, tuning constants).

## ANTI-PATTERNS

- **Bindings are registry-only.** This repo ships its own pure-TS sender binding
  `@ceralive/srtla-send` under `bindings/typescript/` (public npm, `@ceralive`
  scope). It is consumed **registry-only** — **never add a sibling `link:` for it and
  never vendor a `.tgz`** (that is the `srtla/` → CeraUI pattern, not this one).
  `@ceralive/srtla` (the C-pair bindings) still lives in `srtla/`; do not duplicate it
  here.
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

## TEST HARDENING (Tasks 7-8, 25)

### Task 7 — Telemetry Rust test hardening

Two new integration-level test files complement the in-module unit tests in
`src/telemetry_file.rs`:

- **`tests/telemetry_edge_cases.rs`** (9 tests): zero connections (`connections:[]`
  idle-not-absent), active link with zero traffic (`bitrate_bps:0` present not absent),
  very-high RTT 5000 ms verbatim, `schema_version==1` pinned (constant + JSON,
  number-not-string, leads the document), `bitrate_bps == wire_bytes*8` on fixed
  inputs (0, 1, 150k, 312.5k, 1M bytes/s).
- **`tests/telemetry_fixture_parity.rs`** (3 tests): Rust golden
  `tests/fixtures/telemetry-golden.json` vs TS-binding golden
  `bindings/typescript/tests/fixtures/telemetry-golden.json` asserted byte-identical
  + structural (top-level keys, `schema_version==constant`, frozen 7-key per-conn set).
  Both anchored at `CARGO_MANIFEST_DIR` -- inside the repo, Rule D clean.

Key seam: `build_telemetry_json(last_updated_ms, conns)` takes an explicit ms arg.
Tests call it with a fixed timestamp (`1_749_556_546_000`) -- never `publish()` --
to stay non-flaky. Do not conflate with the tokio virtual-clock seam
(`advance_test_clock`), which is for timeout/keepalive tests only.

Gate note: `cargo clippy --features test-internals` is NOT a gate command (fails on
a pre-existing `tokio::time::advance` issue in `src/test_helpers.rs`). The real gate
is `cargo clippy -- -D warnings` (lib+bin only, matches `ci.yml:32`).

### Task 8 -- TS binding test hardening

New `bindings/typescript/tests/telemetry-reader.test.ts` (24 tests, 52 total after
adding to the existing 28):

- Valid golden fixture: full ADR-001 typed shape (both uplinks, all 7 per-link fields),
  `bitrate_bps` x8 invariant.
- Malformed input: non-JSON, truncated, empty string, non-object, absent file, each
  missing required field via `test.each`, wrong types, out-of-domain numerics -- all
  return graceful `null`.
- Schema version: `schema_version` 2/0/missing/non-numeric all return `null`.

`tsconfig.json` fix: added `tests/**/*` to `include`; moved `rootDir: "src"` into
`tsconfig.build.json` only. This ensures `bun tsc --noEmit` typechecks tests (not
just `src/`), while `bun run build` still emits only `dist/{index,sender/index,
telemetry/index}.js` with no test files. Tarball stays clean (`files: ["dist"]`
allowlist + build-emit excludes `*.test.ts`).

### Task 25 -- sendmmsg triage

The `// TODO: On Linux, could use sendmmsg ...` in `src/connection/batch_send.rs`
`flush()` has been converted to a tracked DEFERRED note. The note captures:

- What `sendmmsg(2)` would do (multi-datagram single syscall)
- Why it is deferred: marginal gain at current rates (~60-67 flushes/s at 10 Mbps,
  already a ~15x reduction from raw per-packet sends), Linux-only unsafe FFI, and
  complexity not justified without profiling evidence on the Jetson Nano target
- When to revisit: profiling shows syscall overhead, or Tokio adds native support

Full rationale in `openspec/changes/rust-sender-adoption/sendmmsg-deferred.md`.
**Do not implement sendmmsg** without profiling evidence and a deliberate PR.

## TS BINDING TOOLING

The `bindings/typescript/` package uses Biome 2.5 via `@ceralive/biome-config` as its first linter/formatter. The `biome.json` in `bindings/typescript/` extends `@ceralive/biome-config` (`"extends": ["@ceralive/biome-config"]`). ESLint and Prettier are not used. Run `biome check .` from `bindings/typescript/` (check) or `biome check --write .` (apply fixes). The binding gate includes `bun tsc --noEmit && bun test` — Biome is not a separate gate step but is expected clean before PR.

**Golden fixtures are excluded from Biome** — `biome.json` sets `files.includes` to `["**", "!**/tests/fixtures/**"]`. `tests/fixtures/telemetry-golden.json` is a deliberately byte-identical copy of the Rust producer golden (`tests/fixtures/telemetry-golden.json` at the crate root): the single-line, newline-free atomic-publish telemetry shape (ADR-001). If Biome pretty-prints it (multi-line + trailing newline), the cross-language parity test (`tests/telemetry_fixture_parity.rs` — `rust_and_ts_goldens_are_byte_identical` plus the newline-free assertion) fails every Rust test job in CI. **Do not remove this exclude, and never `biome check --write` the fixtures** — re-sync the two goldens by editing both byte-for-byte instead.

## DOCS DISCIPLINE (Rule A)

Any behavior/structure change updates this `AGENTS.md` and `README.md` in the SAME PR.
Keep the parity contract section authoritative — it is the device-integration contract.
