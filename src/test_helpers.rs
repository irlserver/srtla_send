#![cfg(any(test, feature = "test-internals"))]
#![allow(dead_code)] // Allow unused helpers - they're used by library tests but not binary tests

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use tokio::net::UdpSocket;
use tokio::time::Instant;

use crate::connection::{
    BitrateTracker, CachedQuality, CongestionControl, ReconnectionState, RttTracker,
    SrtlaConnection,
};
use crate::protocol::{PKT_LOG_SIZE, WINDOW_DEF, WINDOW_MULT};
use crate::utils::now_ms;

pub async fn create_test_connection() -> SrtlaConnection {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEST_CONN_ID: AtomicU64 = AtomicU64::new(1000);

    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    let tokio_socket = UdpSocket::from_std(socket).unwrap();
    let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);

    SrtlaConnection {
        conn_id: NEXT_TEST_CONN_ID.fetch_add(1, Ordering::Relaxed),
        socket: Arc::new(tokio_socket),
        remote,
        local_ip: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        label: "test-connection".to_string(),
        connected: true,
        window: WINDOW_DEF * WINDOW_MULT,
        in_flight_packets: 0,
        packet_log: FxHashMap::with_capacity_and_hasher(PKT_LOG_SIZE, Default::default()),
        last_received: Some(Instant::now()),
        last_sent: None,
        last_keepalive_sent: None,
        rtt: RttTracker::default(),
        congestion: CongestionControl::default(),
        bitrate: BitrateTracker::default(),
        reconnection: ReconnectionState {
            connection_established_ms: now_ms(),
            startup_grace_deadline_ms: now_ms(),
            ..Default::default()
        },
        quality_cache: CachedQuality::default(),
    }
}

pub async fn create_test_connections(count: usize) -> SmallVec<SrtlaConnection, 4> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEST_CONN_ID: AtomicU64 = AtomicU64::new(1000);

    let mut connections = SmallVec::new();

    for i in 0..count {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let tokio_socket = UdpSocket::from_std(socket).unwrap();
        let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080 + i as u16);

        let conn = SrtlaConnection {
            conn_id: NEXT_TEST_CONN_ID.fetch_add(1, Ordering::Relaxed),
            socket: Arc::new(tokio_socket),
            remote,
            local_ip: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10 + i as u8)),
            label: format!("test-connection-{}", i),
            connected: true,
            window: WINDOW_DEF * WINDOW_MULT,
            in_flight_packets: 0,
            packet_log: FxHashMap::with_capacity_and_hasher(PKT_LOG_SIZE, Default::default()),
            last_received: Some(Instant::now()),
            last_sent: None,
            last_keepalive_sent: None,
            rtt: RttTracker::default(),
            congestion: CongestionControl::default(),
            bitrate: BitrateTracker::default(),
            reconnection: ReconnectionState {
                connection_established_ms: now_ms(),
                startup_grace_deadline_ms: now_ms(),
                ..Default::default()
            },
            quality_cache: CachedQuality::default(),
        };
        connections.push(conn);
    }

    connections
}
