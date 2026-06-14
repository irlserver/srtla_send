//! Edge-case hardening for the ADR-001 sender telemetry serializer (Task 7).
//!
//! These pin the producer side of the `--stats-file` telemetry contract at its
//! boundaries — the cases a fixture/contract regression would silently slip
//! through into the CeraUI console + ingest panel that consume this telemetry:
//!   * zero connections (a live-but-idle process, distinct from an absent file);
//!   * an established link with zero traffic (`bitrate_bps` is a hard 0, not absent);
//!   * a satellite-grade very-high RTT (5000 ms) carried verbatim;
//!   * `schema_version` pinned to `1` — a bump must be deliberate (ADR-001);
//!   * `bitrate_bps == wire_bytes × 8` on fixed, human-checkable inputs.
//!
//! ## Deterministic clock seam
//!
//! [`build_telemetry_json`] takes `last_updated_ms` as an argument — only
//! `TelemetryWriter::publish` reads the real wall clock (`utils::now_ms`). Every
//! assertion below feeds a fixed timestamp through that seam, so the suite is
//! reproducible and never flaky on `last_updated_ms`. They complement the
//! in-module unit tests in `src/telemetry_file.rs`.

use std::net::{IpAddr, Ipv4Addr};

use srtla_send::stats::{LinkStats, StatsSnapshot};
use srtla_send::telemetry_file::{
    TELEMETRY_SCHEMA_VERSION, TelemetryConn, build_telemetry_json, conns_from_stats,
};

/// Fixed publish timestamp fed through the deterministic clock seam. Matches the
/// committed golden's `last_updated_ms` so the serialized timestamp is stable.
const FIXED_MS: u64 = 1_749_556_546_000;

/// A fully-populated baseline record; edge cases override one field via `..`.
fn base_conn() -> TelemetryConn {
    TelemetryConn {
        conn_id: 0,
        rtt_ms: 42,
        nak_count: 3,
        weight_percent: 85,
        window: 8192,
        in_flight: 100,
        bitrate_bytes_per_sec: 312_500,
    }
}

/// One link in the shared stats snapshot, parameterized on the fields the edge
/// cases vary. Inactive links set `timed_out` so `conns_from_stats` zeroes them.
fn link(connected: bool, bitrate_bytes_per_sec: u32, rtt_ms: u32, base_score: i32) -> LinkStats {
    LinkStats {
        ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
        label: "test".to_string(),
        connected,
        timed_out: !connected,
        window: 1000,
        in_flight: 0,
        rtt_ms,
        nak_count: 0,
        bitrate_bps: bitrate_bytes_per_sec,
        rtt_min_ms: 0.0,
        rtt_velocity: 0.0,
        base_score,
        quality_multiplier: 1.0,
    }
}

// ---- The deterministic seam itself -----------------------------------------

#[test]
fn last_updated_ms_comes_from_the_argument_seam() {
    // The same input must yield a byte-identical document — proof the serializer
    // reads no wall clock and the suite cannot be flaky on `last_updated_ms`.
    let a = build_telemetry_json(FIXED_MS, &[base_conn()]);
    let b = build_telemetry_json(FIXED_MS, &[base_conn()]);
    assert_eq!(
        a, b,
        "build_telemetry_json must be a pure function of its args"
    );
    assert!(
        a.contains("\"last_updated_ms\":1749556546000"),
        "the fixed timestamp must round-trip verbatim: {a}"
    );
}

// ---- Zero connections (running-but-idle) -----------------------------------

#[test]
fn zero_connections_serialize_to_empty_array() {
    let json = build_telemetry_json(FIXED_MS, &[]);
    assert!(json.contains("\"connections\":[]"), "got {json}");
    // Idle is distinct from absent: the version tag + timestamp still ship.
    assert!(json.contains("\"schema_version\":1"), "got {json}");
    assert!(
        json.contains("\"last_updated_ms\":1749556546000"),
        "got {json}"
    );
    assert!(!json.contains('\n'), "telemetry must be one line: {json}");
}

#[test]
fn zero_connections_via_stats_projection() {
    // A live process with no links projects to an empty list, never `null`.
    let conns = conns_from_stats(&StatsSnapshot::default());
    assert!(conns.is_empty(), "no links must project to no connections");
    let json = build_telemetry_json(FIXED_MS, &conns);
    assert!(json.contains("\"connections\":[]"), "got {json}");
}

// ---- A link with zero traffic ----------------------------------------------

#[test]
fn active_link_with_zero_traffic_reports_zero_bitrate() {
    // A freshly-registered uplink that has not sent yet: `bitrate_bps` is a hard
    // 0 (×8 of 0 bytes), and the record is still present (not dropped/absent).
    let conn = TelemetryConn {
        bitrate_bytes_per_sec: 0,
        ..base_conn()
    };
    let json = build_telemetry_json(FIXED_MS, &[conn]);
    assert!(json.contains("\"bitrate_bps\":0"), "got {json}");
    assert!(json.contains("\"conn_id\":\"0\""), "got {json}");
}

#[test]
fn zero_traffic_link_stays_active_in_projection() {
    // Active link, no capacity signal yet (base_score 0, 0 B/s). conns_from_stats
    // keeps it active with the equal-share fallback and a zero bitrate, rather
    // than reporting the only uplink as all-zero weight.
    let snap = StatsSnapshot {
        links: vec![link(true, 0, 20, 0)],
        ..StatsSnapshot::default()
    };
    let conns = conns_from_stats(&snap);
    assert_eq!(conns.len(), 1);
    assert_eq!(conns[0].bitrate_bytes_per_sec, 0);
    assert_eq!(
        conns[0].weight_percent, 100,
        "sole active link gets full share"
    );
    let json = build_telemetry_json(FIXED_MS, &conns);
    assert!(json.contains("\"bitrate_bps\":0"), "got {json}");
}

// ---- Very high RTT ---------------------------------------------------------

#[test]
fn very_high_rtt_serializes_intact() {
    // A 5000 ms (satellite-grade) RTT must serialize verbatim: no clamp, no
    // truncation, no overflow to a smaller value.
    let conn = TelemetryConn {
        rtt_ms: 5000,
        ..base_conn()
    };
    let json = build_telemetry_json(FIXED_MS, &[conn]);
    assert!(json.contains("\"rtt_ms\":5000"), "got {json}");
}

#[test]
fn very_high_rtt_survives_stats_projection() {
    let snap = StatsSnapshot {
        links: vec![link(true, 100, 5000, 10)],
        ..StatsSnapshot::default()
    };
    let conns = conns_from_stats(&snap);
    assert_eq!(
        conns[0].rtt_ms, 5000,
        "high RTT must pass through unaltered"
    );
    let json = build_telemetry_json(FIXED_MS, &conns);
    assert!(json.contains("\"rtt_ms\":5000"), "got {json}");
}

// ---- schema_version stability (a bump must be deliberate) -------------------

#[test]
fn schema_version_is_pinned_to_one() {
    // Guard the on-wire `schema_version` against an accidental change: the TS
    // reader validates `z.literal(1)`, so any drift silently breaks every
    // consumer. Change this assertion ONLY alongside a versioned telemetry
    // contract change (ADR-001).
    assert_eq!(
        TELEMETRY_SCHEMA_VERSION, 1,
        "schema_version bump must be deliberate (ADR-001)"
    );

    let json = build_telemetry_json(FIXED_MS, &[]);
    // Present, an integer, and leading the document.
    assert!(
        json.starts_with("{\"schema_version\":1,"),
        "schema_version must lead the doc as an integer: {json}"
    );
    // Never a string.
    assert!(
        !json.contains("\"schema_version\":\"1\""),
        "schema_version must be a JSON number, not a string: {json}"
    );
}

// ---- bitrate_bps == wire_bytes × 8 on a known input ------------------------

#[test]
fn bitrate_bps_is_exactly_wire_bytes_times_eight() {
    // Fixed, human-checkable inputs: the serialized bits/s is exactly bytes × 8,
    // the single mandated conversion (ADR-001). 312_500 B/s is the canonical
    // 2_500_000 bps example.
    let cases: [(u32, u64); 5] = [
        (0, 0),
        (1, 8),
        (150_000, 1_200_000),
        (312_500, 2_500_000),
        (1_000_000, 8_000_000),
    ];
    for (wire_bytes, expected_bps) in cases {
        let conn = TelemetryConn {
            bitrate_bytes_per_sec: wire_bytes,
            ..base_conn()
        };
        let json = build_telemetry_json(FIXED_MS, &[conn]);
        let needle = format!("\"bitrate_bps\":{expected_bps}");
        assert!(
            json.contains(&needle),
            "{wire_bytes} B/s must serialize as {expected_bps} bps: {json}"
        );
    }

    // The raw wire-bytes/s value must never leak into the document.
    let json = build_telemetry_json(
        FIXED_MS,
        &[TelemetryConn {
            bitrate_bytes_per_sec: 312_500,
            ..base_conn()
        }],
    );
    assert!(!json.contains("312500"), "raw wire bytes/s leaked: {json}");
}
