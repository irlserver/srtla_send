#![cfg(any(test, feature = "test-internals"))]
#![allow(dead_code)] // Allow unused helpers - they're used by library tests but not binary tests

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::time::Instant;

use crate::connection::{
    BatchSender, BatchUdpSocket, BitrateTracker, CachedQuality, CongestionControl,
    ReconnectionState, RttTracker, SrtlaConnection,
};
use crate::protocol::{PKT_LOG_SIZE, WINDOW_DEF, WINDOW_MULT};
use crate::utils::now_ms;

/// Shared test connection ID counter for all helper functions
static NEXT_TEST_CONN_ID: AtomicU64 = AtomicU64::new(1000);

/// Create a test UDP socket bound to localhost.
fn create_test_socket() -> Socket {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
    socket
        .bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into())
        .unwrap();
    socket.set_nonblocking(true).unwrap();
    socket
}

/// Create a SrtlaConnection from a socket and connection parameters.
fn create_connection_from_socket(
    socket: Socket,
    remote: SocketAddr,
    local_ip: IpAddr,
    label: String,
) -> SrtlaConnection {
    let batch_socket = BatchUdpSocket::new(socket).unwrap();

    SrtlaConnection {
        conn_id: NEXT_TEST_CONN_ID.fetch_add(1, Ordering::Relaxed),
        socket: Arc::new(batch_socket),
        remote,
        local_ip,
        label,
        connected: true,
        window: WINDOW_DEF * WINDOW_MULT,
        in_flight_packets: 0,
        packet_log: FxHashMap::with_capacity_and_hasher(PKT_LOG_SIZE, Default::default()),
        highest_acked_seq: i32::MIN,
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
        batch_sender: BatchSender::new(),
    }
}

pub async fn create_test_connection() -> SrtlaConnection {
    let socket = create_test_socket();
    let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
    let local_ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));

    create_connection_from_socket(socket, remote, local_ip, "test-connection".to_string())
}

pub async fn create_test_connections(count: usize) -> SmallVec<SrtlaConnection, 4> {
    let mut connections = SmallVec::new();

    for i in 0..count {
        let socket = create_test_socket();
        let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080 + i as u16);
        let local_ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10 + i as u8));
        let label = format!("test-connection-{}", i);

        connections.push(create_connection_from_socket(
            socket, remote, local_ip, label,
        ));
    }

    connections
}
