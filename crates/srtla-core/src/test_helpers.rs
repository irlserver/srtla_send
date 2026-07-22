#![cfg(any(test, feature = "test-internals"))]
#![allow(dead_code)] // Allow unused helpers - they're used by library tests but not binary tests

use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU64, Ordering};

use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use srtla_protocol::{PKT_LOG_SIZE, WINDOW_DEF, WINDOW_MULT};
use tokio::time::Duration;

use crate::connection::{
    BatchSender, BitrateTracker, CachedQuality, CongestionControl, LinkPhase, ReconnectionState,
    RttTracker, SrtlaConnection,
};
use crate::utils::now_ms;

/// Shared test connection ID counter for all helper functions
static NEXT_TEST_CONN_ID: AtomicU64 = AtomicU64::new(1000);

/// Build a socket-free test connection in the connected/`Live` state.
///
/// The socket lifted out of `SrtlaConnection` in the sans-IO refactor, so tests
/// that only exercise protocol logic (selection, congestion, NAK handling, …)
/// no longer need a real UDP socket. Tests that need to drive I/O construct a
/// `ConnIo` separately (that half lives in the parent srtla_send crate).
fn build_connection(local_ip: IpAddr, label: String) -> SrtlaConnection {
    SrtlaConnection {
        conn_id: NEXT_TEST_CONN_ID.fetch_add(1, Ordering::Relaxed),
        local_ip,
        label,
        connected: true,
        window: WINDOW_DEF * WINDOW_MULT,
        in_flight_packets: 0,
        packet_log: FxHashMap::with_capacity_and_hasher(PKT_LOG_SIZE, Default::default()),
        highest_acked_seq: i32::MIN,
        last_received: Some(now_ms()),
        last_sent: None,
        last_keepalive_sent: None,
        last_ack_or_rtt_sample_ms: 0,
        stall_gated: false,
        stall_latched_since_ms: 0,
        stall_recovery_since_ms: 0,
        stall_gate_events: 0,
        stall_probe_counter: 0,
        silence_pulled: false,
        silence_pulls: 0,
        conn_timeout_ms: crate::config_snapshot::CONN_TIMEOUT_MS,
        rtt: RttTracker::default(),
        congestion: CongestionControl::default(),
        bitrate: BitrateTracker::new(now_ms()),
        reconnection: ReconnectionState {
            connection_established_ms: now_ms(),
            startup_grace_deadline_ms: now_ms(),
            ..Default::default()
        },
        quality_cache: CachedQuality::default(),
        batch_sender: BatchSender::new(),
        phase: LinkPhase::Live,
        weak: false,
        cc_backing_off: false,
        cc_target_bps: 0,
        loss_degraded: false,
    }
}

pub async fn create_test_connection() -> SrtlaConnection {
    build_connection(
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        "test-connection".to_string(),
    )
}

pub async fn create_test_connections(count: usize) -> SmallVec<SrtlaConnection, 4> {
    let mut connections = SmallVec::new();

    for i in 0..count {
        let local_ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10 + i as u8));
        connections.push(build_connection(local_ip, format!("test-connection-{}", i)));
    }

    connections
}

/// Advance the paused Tokio virtual clock by `by`.
///
/// Only meaningful inside `#[tokio::test(start_paused = true)]`. Connection
/// liveness timing (`last_received`, `last_keepalive_sent`, the `all_failed_at`
/// timer, `is_timed_out`) runs on the `now_ms()` monotonic clock, which is NOT
/// tokio-controlled: those tests stamp explicit past timestamps instead of
/// advancing this clock. The send-coalescing layer no longer uses
/// `tokio::time::Instant` either (`BatchSender` now runs on the injected
/// monotonic clock), so this seam only remains for tests that gate on tokio
/// timers directly.
pub async fn advance_test_clock(by: Duration) {
    tokio::time::advance(by).await;
}
