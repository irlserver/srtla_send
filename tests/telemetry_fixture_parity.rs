//! Golden-fixture parity: the Rust producer golden and the TS-binding golden are
//! a single source kept in lockstep (Task 7).
//!
//! Two committed fixtures encode the same ADR-001 snapshot:
//!   * `tests/fixtures/telemetry-golden.json` — the Rust producer's byte-for-byte
//!     output (round-tripped by `golden_fixture_matches_producer_output` in
//!     `src/telemetry_file.rs`);
//!   * `bindings/typescript/tests/fixtures/telemetry-golden.json` — the copy the
//!     `@ceralive/srtla-send` Zod reader round-trips (`telemetry/index.test.ts`).
//!
//! They MUST stay identical. A contract regression that lands in one but not the
//! other would silently corrupt the CeraUI console + ingest panel that consume
//! this telemetry — exactly the drift these assertions exist to catch. Both
//! paths are anchored at `CARGO_MANIFEST_DIR` and live inside this repo, so the
//! test never reaches above its own checkout (Rule D).

use std::path::PathBuf;

use srtla_send::telemetry_file::TELEMETRY_SCHEMA_VERSION;

/// The frozen per-connection key set the `@ceralive/srtla` Zod reader requires.
/// Sorted; both fixtures must carry exactly these and no others.
const FROZEN_CONN_KEYS: [&str; 7] = [
    "bitrate_bps",
    "conn_id",
    "in_flight",
    "nak_count",
    "rtt_ms",
    "weight_percent",
    "window",
];

fn rust_golden_path() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/telemetry-golden.json"
    ))
}

fn ts_golden_path() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/bindings/typescript/tests/fixtures/telemetry-golden.json"
    ))
}

fn read(path: &PathBuf) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("golden fixture missing at {}: {e}", path.display()))
}

fn parse(path: &PathBuf) -> serde_json::Value {
    serde_json::from_str(&read(path)).unwrap_or_else(|e| {
        panic!(
            "golden fixture at {} is not valid JSON: {e}",
            path.display()
        )
    })
}

/// Sorted top-level object keys.
fn sorted_keys(v: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = v
        .as_object()
        .expect("expected a JSON object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

// ---- Single source: byte-for-byte identical --------------------------------

#[test]
fn rust_and_ts_goldens_are_byte_identical() {
    // The two fixtures are maintained as identical copies (one source of truth).
    // If this fails they have drifted — re-sync them rather than editing one.
    let rust = read(&rust_golden_path());
    let ts = read(&ts_golden_path());
    assert_eq!(
        rust, ts,
        "Rust and TS golden fixtures have drifted; re-sync them (single source)"
    );
    // The shared document is the single-line atomic-publish shape.
    assert!(
        !rust.contains('\n'),
        "golden fixtures must be newline-free: {rust}"
    );
}

// ---- Structural parity (survives even if byte-equality is ever relaxed) -----

#[test]
fn goldens_share_schema_version_and_top_level_keys() {
    let rust = parse(&rust_golden_path());
    let ts = parse(&ts_golden_path());

    // schema_version matches the producer constant on BOTH sides.
    let expected = serde_json::json!(TELEMETRY_SCHEMA_VERSION);
    assert_eq!(
        rust["schema_version"], expected,
        "rust schema_version drift"
    );
    assert_eq!(ts["schema_version"], expected, "ts schema_version drift");

    // Identical top-level key sets.
    assert_eq!(
        sorted_keys(&rust),
        sorted_keys(&ts),
        "top-level key sets differ between the two goldens"
    );
    assert_eq!(
        sorted_keys(&rust),
        vec!["connections", "last_updated_ms", "schema_version"],
        "top-level contract keys changed"
    );
}

#[test]
fn goldens_share_per_connection_key_structure() {
    let rust = parse(&rust_golden_path());
    let ts = parse(&ts_golden_path());

    let rust_conns = rust["connections"].as_array().expect("rust connections[]");
    let ts_conns = ts["connections"].as_array().expect("ts connections[]");
    assert_eq!(
        rust_conns.len(),
        ts_conns.len(),
        "connection counts differ between the two goldens"
    );
    assert!(
        !rust_conns.is_empty(),
        "the golden must exercise at least one connection"
    );

    let frozen: Vec<String> = FROZEN_CONN_KEYS.iter().map(|s| s.to_string()).collect();
    for (i, (r, t)) in rust_conns.iter().zip(ts_conns).enumerate() {
        assert_eq!(
            sorted_keys(r),
            sorted_keys(t),
            "per-connection key sets differ at index {i}"
        );
        assert_eq!(
            sorted_keys(r),
            frozen,
            "connection {i} keys drifted from the frozen ADR-001 contract"
        );
    }
}
