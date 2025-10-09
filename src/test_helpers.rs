#![cfg(test)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio::time::Instant;

use crate::connection::SrtlaConnection;
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
        packet_log: [-1; PKT_LOG_SIZE],
        packet_send_times_ms: [0; PKT_LOG_SIZE],
        packet_idx: 0,
        last_received: Some(Instant::now()),
        last_keepalive_ms: 0,
        last_keepalive_sent_ms: 0,
        waiting_for_keepalive_response: false,
        last_rtt_measurement_ms: 0,
        smooth_rtt_ms: 0.0,
        fast_rtt_ms: 0.0,
        rtt_jitter_ms: 0.0,
        prev_rtt_ms: 0.0,
        rtt_avg_delta_ms: 0.0,
        rtt_min_ms: 200.0,
        estimated_rtt_ms: 0.0,
        nak_count: 0,
        last_nak_time_ms: 0,
        last_window_increase_ms: 0,
        consecutive_acks_without_nak: 0,
        fast_recovery_mode: false,
        fast_recovery_start_ms: 0,
        nak_burst_count: 0,
        nak_burst_start_time_ms: 0,
        last_reconnect_attempt_ms: 0,
        reconnect_failure_count: 0,
        connection_established_ms: now_ms(),
    }
}

pub async fn create_test_connections(count: usize) -> Vec<SrtlaConnection> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEST_CONN_ID: AtomicU64 = AtomicU64::new(1000);

    let mut connections = Vec::new();

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
            packet_log: [-1; PKT_LOG_SIZE],
            packet_send_times_ms: [0; PKT_LOG_SIZE],
            packet_idx: 0,
            last_received: Some(Instant::now()),
            last_keepalive_ms: 0,
            last_keepalive_sent_ms: 0,
            waiting_for_keepalive_response: false,
            last_rtt_measurement_ms: 0,
            smooth_rtt_ms: 0.0,
            fast_rtt_ms: 0.0,
            rtt_jitter_ms: 0.0,
            prev_rtt_ms: 0.0,
            rtt_avg_delta_ms: 0.0,
            rtt_min_ms: 200.0,
            estimated_rtt_ms: 0.0,
            nak_count: 0,
            last_nak_time_ms: 0,
            last_window_increase_ms: 0,
            consecutive_acks_without_nak: 0,
            fast_recovery_mode: false,
            fast_recovery_start_ms: 0,
            nak_burst_count: 0,
            nak_burst_start_time_ms: 0,
            last_reconnect_attempt_ms: 0,
            reconnect_failure_count: 0,
            connection_established_ms: now_ms(),
        };
        connections.push(conn);
    }

    connections
}
