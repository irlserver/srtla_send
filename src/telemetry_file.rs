//! ADR-001 sender telemetry stats-file emitter (opt-in via `--stats-file`).
//!
//! Mirrors the C reference serializer (`srtla/src/sender_telemetry.h`): a
//! per-uplink JSON snapshot written atomically (temp sibling -> fsync ->
//! `rename(2)`) so a concurrent reader never observes a torn document. The file
//! is published on a fixed cadence and removed on clean shutdown.
//!
//! The module is **fully opt-in**: with no `--stats-file` flag a
//! [`TelemetryWriter`] is never constructed and zero filesystem writes happen.
//!
//! # Schema (ADR-001)
//!
//! A single newline-free JSON object, parsed unchanged by the frozen
//! `@ceralive/srtla` Zod reader (`bindings/typescript/src/telemetry`):
//!
//! ```json
//! {"schema_version":1,"last_updated_ms":1749556546000,"connections":[
//!   {"conn_id":"0","rtt_ms":42,"nak_count":3,"weight_percent":85,
//!    "window":8192,"in_flight":100,"bitrate_bps":2500000}]}
//! ```
//!
//! Divergences from the C producer, both additive / strictly-better:
//! - `schema_version` is emitted (C omits it); the Zod reader strips unknown
//!   keys, so the consumer is unaffected.
//! - `rtt_ms` carries the Kalman-smoothed RTT (C hardcodes 0).
//! - `weight_percent` is each link's normalized share of selection weight
//!   (C reports a constant 100); the receiver-side scoring is not ported.
//!
//! `conn_id` is the uplink's 0-based index in the IP-list order (stable until a
//! SIGHUP reload reorders the file). `bitrate_bps` is wire bytes/s x 8 — the
//! mandated bits/s conversion has its single home in [`build_telemetry_json`].

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tracing::warn;

use crate::stats::StatsSnapshot;
use crate::utils::now_ms;

/// JSON schema version. Additive over the C producer; the Zod reader strips it.
pub const TELEMETRY_SCHEMA_VERSION: u32 = 1;

/// One per-uplink telemetry record in wire units.
///
/// Field names / units mirror the C `TelemetrySnapshot` (`sender_telemetry.h`).
/// `bitrate_bytes_per_sec` is the wire byte rate; the mandated x8 -> bits/s
/// conversion happens only at serialization, in [`build_telemetry_json`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TelemetryConn {
    pub conn_id: u32,
    pub rtt_ms: u32,
    pub nak_count: u32,
    pub weight_percent: u8,
    pub window: i32,
    pub in_flight: i32,
    pub bitrate_bytes_per_sec: u32,
}

/// Serialized per-connection record. `conn_id` is a string and `bitrate_bps` is
/// bits/s (the x8 conversion), matching the ADR-001 schema and the Zod reader.
/// Field order is fixed to mirror the C golden fixture.
#[derive(Serialize)]
struct ConnRecord {
    conn_id: String,
    rtt_ms: u32,
    nak_count: u32,
    weight_percent: u8,
    window: i32,
    in_flight: i32,
    bitrate_bps: u64,
}

impl From<&TelemetryConn> for ConnRecord {
    fn from(c: &TelemetryConn) -> Self {
        Self {
            conn_id: c.conn_id.to_string(),
            rtt_ms: c.rtt_ms,
            nak_count: c.nak_count,
            weight_percent: c.weight_percent,
            window: c.window,
            in_flight: c.in_flight,
            // The single, testable home of the mandated bytes/s -> bits/s x8.
            bitrate_bps: u64::from(c.bitrate_bytes_per_sec) * 8,
        }
    }
}

/// Whole-document shape. `schema_version` first so the on-disk object leads with
/// the version tag; the rest mirrors the ADR-001 / C golden field order.
#[derive(Serialize)]
struct TelemetryDoc {
    schema_version: u32,
    last_updated_ms: u64,
    connections: Vec<ConnRecord>,
}

/// Serialize one snapshot to the exact ADR-001 JSON object (compact,
/// newline-free). This is the single place the bytes/s -> bits/s x8 conversion
/// lives, so the mandated unit transform has one testable home.
pub fn build_telemetry_json(last_updated_ms: u64, conns: &[TelemetryConn]) -> String {
    let doc = TelemetryDoc {
        schema_version: TELEMETRY_SCHEMA_VERSION,
        last_updated_ms,
        connections: conns.iter().map(ConnRecord::from).collect(),
    };
    // The doc is plain scalars / strings, so serialization cannot fail; fall back
    // to an empty object defensively rather than panicking on the hot path.
    serde_json::to_string(&doc).unwrap_or_else(|_| "{}".to_string())
}

/// Project the shared stats snapshot into per-uplink telemetry records.
///
/// `conn_id` is the link's 0-based position in IP-list order. `weight_percent`
/// is the link's share of total selection weight (`base_score x quality`) among
/// active links, normalized to 100; inactive links report 0. Active links with
/// no capacity signal yet fall back to an equal share so a freshly-registered
/// group is not reported as all-zero.
pub fn conns_from_stats(stats: &StatsSnapshot) -> Vec<TelemetryConn> {
    let weights: Vec<f64> = stats
        .links
        .iter()
        .map(|l| {
            if l.connected && !l.timed_out {
                f64::from(l.base_score.max(0)) * l.quality_multiplier
            } else {
                0.0
            }
        })
        .collect();
    let total: f64 = weights.iter().sum();
    let active = stats
        .links
        .iter()
        .filter(|l| l.connected && !l.timed_out)
        .count();

    stats
        .links
        .iter()
        .enumerate()
        .map(|(idx, l)| {
            let is_active = l.connected && !l.timed_out;
            let weight_percent = if !is_active {
                0
            } else if total > 0.0 {
                weight_share_percent(weights[idx], total)
            } else {
                equal_share_percent(active)
            };
            TelemetryConn {
                conn_id: idx as u32,
                rtt_ms: l.rtt_ms,
                nak_count: l.nak_count.max(0) as u32,
                weight_percent,
                window: l.window,
                in_flight: l.in_flight,
                // LinkStats.bitrate_bps is already wire bytes/s; the x8 to bits/s
                // is applied once, at JSON serialization.
                bitrate_bytes_per_sec: l.bitrate_bps,
            }
        })
        .collect()
}

/// One link's percentage of the total selection weight, rounded and clamped to
/// the schema's `0..=100` range.
fn weight_share_percent(weight: f64, total: f64) -> u8 {
    let pct = (weight / total * 100.0).round();
    pct.clamp(0.0, 100.0) as u8
}

/// Equal share among `active` links (the no-capacity-signal fallback).
fn equal_share_percent(active: usize) -> u8 {
    100usize
        .checked_div(active)
        .map_or(0, |share| share.min(100) as u8)
}

/// Sibling temp path used by the atomic publish (`<path>.tmp`).
fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Write `json` to the temp sibling and fsync it before the rename so the bytes
/// are durable. The `File` is closed at the end of this scope, before the caller
/// renames it into place.
fn write_tmp(tmp: &Path, json: &str) -> io::Result<()> {
    let mut file = File::create(tmp)?;
    file.write_all(json.as_bytes())?;
    file.sync_all()
}

/// Atomically publish `json` to `path`: write a `.tmp` sibling, fsync, then
/// `rename(2)` over the live path. On the same filesystem rename is atomic, so a
/// concurrent reader only ever sees a complete previous-or-next document. On any
/// I/O error the temp sibling is removed and the previous snapshot is left in
/// place (to go stale) rather than vanishing.
pub fn write_atomic(path: &Path, json: &str) -> io::Result<()> {
    let tmp = tmp_path(path);
    match write_tmp(&tmp, json).and_then(|()| fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Best-effort removal of the live file and any leftover temp sibling.
pub fn remove(path: &Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(tmp_path(path));
}

/// Opt-in telemetry sink bound to a single stats-file path.
///
/// Constructed only when `--stats-file` is supplied. Publishing is best-effort:
/// an I/O failure is logged and dropped, never fatal to the stream. The live
/// file is removed when the writer is dropped (clean shutdown), so any graceful
/// exit — the SIGTERM/SIGINT handler, the channel closing, or a fatal stream
/// error — unlinks it via RAII.
pub struct TelemetryWriter {
    path: PathBuf,
    period: Duration,
}

impl TelemetryWriter {
    pub fn new(path: impl Into<PathBuf>, interval_ms: u64) -> Self {
        Self {
            path: path.into(),
            period: Duration::from_millis(interval_ms.max(1)),
        }
    }

    /// The publish cadence (`--stats-file-interval`, floored at 1 ms).
    pub fn period(&self) -> Duration {
        self.period
    }

    /// Serialize the current snapshot and atomically publish it. Best-effort: an
    /// I/O error is logged at WARN and otherwise ignored (telemetry never stalls
    /// or fails the stream).
    pub fn publish(&self, stats: &StatsSnapshot) {
        let json = build_telemetry_json(now_ms(), &conns_from_stats(stats));
        if let Err(e) = write_atomic(&self.path, &json) {
            let path = self.path.display();
            warn!("telemetry stats-file write failed: {path}: {e}");
        }
    }

    /// Remove the live file and temp sibling now (idempotent).
    pub fn remove(&self) {
        remove(&self.path);
    }
}

impl Drop for TelemetryWriter {
    fn drop(&mut self) {
        self.remove();
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::thread;

    use super::*;
    use crate::stats::{LinkStats, StatsSnapshot};

    fn sample_conn() -> TelemetryConn {
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

    fn link(score: i32, active: bool, bytes_per_sec: u32) -> LinkStats {
        LinkStats {
            ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            label: "test".to_string(),
            connected: active,
            timed_out: !active,
            window: 100,
            in_flight: 0,
            rtt_ms: 20,
            nak_count: 0,
            bitrate_bps: bytes_per_sec,
            rtt_min_ms: 0.0,
            rtt_velocity: 0.0,
            base_score: score,
            quality_multiplier: 1.0,
        }
    }

    // ---- Schema serialization: field names + types ------------------------

    #[test]
    fn schema_version_is_integer_one() {
        let json = build_telemetry_json(1, &[]);
        assert!(json.contains("\"schema_version\":1"), "got {json}");
        // It must be a number, never a string.
        assert!(!json.contains("\"schema_version\":\"1\""));
    }

    #[test]
    fn document_is_newline_free() {
        let json = build_telemetry_json(1, &[sample_conn()]);
        assert!(
            !json.contains('\n'),
            "telemetry must be a single line: {json}"
        );
    }

    #[test]
    fn empty_connections_serialize_to_array() {
        let json = build_telemetry_json(1_749_556_546_000, &[]);
        assert!(json.contains("\"connections\":[]"), "got {json}");
        assert!(json.contains("\"last_updated_ms\":1749556546000"));
    }

    #[test]
    fn conn_id_is_stringified() {
        let json = build_telemetry_json(0, &[sample_conn()]);
        assert!(json.contains("\"conn_id\":\"0\""), "got {json}");
    }

    #[test]
    fn all_schema_fields_present_and_typed() {
        let json = build_telemetry_json(7, &[sample_conn()]);
        for needle in [
            "\"rtt_ms\":42",
            "\"nak_count\":3",
            "\"weight_percent\":85",
            "\"window\":8192",
            "\"in_flight\":100",
            "\"bitrate_bps\":2500000",
        ] {
            assert!(json.contains(needle), "missing {needle} in {json}");
        }
    }

    // ---- The mandated x8 bytes/s -> bits/s conversion ---------------------

    #[test]
    fn bitrate_is_bytes_times_eight_bits_per_second() {
        // 312500 B/s -> 2500000 bps (the ADR-001 canonical example).
        let json = build_telemetry_json(0, &[sample_conn()]);
        assert!(json.contains("\"bitrate_bps\":2500000"), "got {json}");
        // The raw bytes/s value must never leak into the JSON.
        assert!(!json.contains("312500"), "raw bytes/s leaked: {json}");
    }

    #[test]
    fn bitrate_conversion_is_exactly_times_eight() {
        let cases = [
            (0u32, 0u64),
            (1, 8),
            (150_000, 1_200_000),
            (312_500, 2_500_000),
        ];
        for (bytes, bits) in cases {
            let conn = TelemetryConn {
                bitrate_bytes_per_sec: bytes,
                ..sample_conn()
            };
            let record = ConnRecord::from(&conn);
            assert_eq!(record.bitrate_bps, bits, "{bytes} B/s should be {bits} bps");
        }
    }

    // ---- Weight normalization --------------------------------------------

    #[test]
    fn weight_share_normalizes_to_one_hundred() {
        assert_eq!(weight_share_percent(5.0, 10.0), 50);
        assert_eq!(weight_share_percent(10.0, 10.0), 100);
        // 2:1 split rounds to 67 / 33.
        assert_eq!(weight_share_percent(2.0, 3.0), 67);
        assert_eq!(weight_share_percent(1.0, 3.0), 33);
    }

    #[test]
    fn equal_share_fallback_distributes_evenly() {
        assert_eq!(equal_share_percent(0), 0);
        assert_eq!(equal_share_percent(1), 100);
        assert_eq!(equal_share_percent(2), 50);
        assert_eq!(equal_share_percent(4), 25);
    }

    #[test]
    fn conns_from_stats_indexes_and_normalizes() {
        let snap = StatsSnapshot {
            // two equal active links + one timed-out link
            links: vec![link(10, true, 100), link(10, true, 200), link(0, false, 0)],
            ..Default::default()
        };
        let conns = conns_from_stats(&snap);

        assert_eq!(conns.len(), 3);
        // conn_id is the 0-based IP-list index.
        assert_eq!(conns[0].conn_id, 0);
        assert_eq!(conns[1].conn_id, 1);
        assert_eq!(conns[2].conn_id, 2);
        // Two equal active links split 50/50; the inactive link reports 0.
        assert_eq!(conns[0].weight_percent, 50);
        assert_eq!(conns[1].weight_percent, 50);
        assert_eq!(conns[2].weight_percent, 0);
        // Wire bytes/s carried through verbatim (x8 applied only at serialization).
        assert_eq!(conns[1].bitrate_bytes_per_sec, 200);
    }

    #[test]
    fn conns_from_stats_equal_share_when_no_capacity_signal() {
        // Active links whose base_score is 0 still get a non-zero equal share.
        let snap = StatsSnapshot {
            links: vec![link(0, true, 0), link(0, true, 0)],
            ..Default::default()
        };
        let conns = conns_from_stats(&snap);
        assert_eq!(conns[0].weight_percent, 50);
        assert_eq!(conns[1].weight_percent, 50);
    }

    // ---- Atomicity: reader never sees a partial document ------------------

    #[test]
    fn write_atomic_roundtrips_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.json");
        let json = build_telemetry_json(111, &[sample_conn()]);

        write_atomic(&path, &json).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), json);
        assert!(!tmp_path(&path).exists(), "temp sibling left behind");
    }

    #[test]
    fn write_atomic_replaces_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.json");

        write_atomic(&path, &build_telemetry_json(111, &[])).unwrap();
        write_atomic(&path, &build_telemetry_json(222, &[])).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"last_updated_ms\":222"));
        assert!(
            !content.contains("\"last_updated_ms\":111"),
            "a publish must replace the previous snapshot, not append"
        );
    }

    #[test]
    fn concurrent_reader_never_sees_torn_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.json");
        // Seed one complete snapshot so the reader always finds a live file.
        write_atomic(&path, &build_telemetry_json(1, &[])).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let writes = Arc::new(AtomicU64::new(0));

        // Alternate small/large snapshots to maximize the byte-length delta, so a
        // non-atomic publish would be caught as an unparseable document.
        let small = vec![sample_conn()];
        let big: Vec<TelemetryConn> = (0..64u32)
            .map(|i| TelemetryConn {
                conn_id: i,
                rtt_ms: i,
                nak_count: i,
                weight_percent: 100,
                window: i as i32 * 100,
                in_flight: i as i32,
                bitrate_bytes_per_sec: i * 1000,
            })
            .collect();

        let writer = {
            let path = path.clone();
            let stop = stop.clone();
            let writes = writes.clone();
            thread::spawn(move || {
                let mut t = 2u64;
                while !stop.load(Ordering::Relaxed) {
                    let v = if t & 1 == 1 { &small } else { &big };
                    let _ = write_atomic(&path, &build_telemetry_json(t, v));
                    writes.fetch_add(1, Ordering::Relaxed);
                    t += 1;
                }
            })
        };

        let mut parse_errors = 0;
        for _ in 0..1000 {
            if let Ok(content) = fs::read_to_string(&path)
                && serde_json::from_str::<serde_json::Value>(&content).is_err()
            {
                parse_errors += 1;
            }
        }

        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();

        assert_eq!(parse_errors, 0, "reader observed a torn/partial document");
        assert!(writes.load(Ordering::Relaxed) > 0, "writer never ran");
    }

    // ---- Opt-in + unlink-on-exit semantics -------------------------------

    #[test]
    fn constructing_writer_creates_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.json");
        let _writer = TelemetryWriter::new(&path, 1000);
        // No publish() call -> nothing on disk (opt-in: construction is inert).
        assert!(
            !path.exists(),
            "constructing a writer must not create the file"
        );
    }

    #[test]
    fn drop_unlinks_live_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.json");
        {
            let writer = TelemetryWriter::new(&path, 1000);
            writer.publish(&StatsSnapshot::default());
            assert!(path.exists(), "publish should create the live file");
        } // writer dropped here
        assert!(!path.exists(), "the live file must be unlinked on drop");
    }

    #[test]
    fn explicit_remove_clears_live_and_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stats.json");
        write_atomic(&path, &build_telemetry_json(1, &[])).unwrap();
        // A stray temp sibling (e.g. from a crashed write) is also cleared.
        fs::write(tmp_path(&path), b"partial").unwrap();

        remove(&path);

        assert!(!path.exists(), "live file must be gone");
        assert!(!tmp_path(&path).exists(), "temp sibling must be gone");
    }
}
