#![cfg(test)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use tokio::net::UdpSocket;
use tokio::time::Instant;

use crate::connection::SrtlaConnection;
use crate::protocol::{PKT_LOG_SIZE, WINDOW_DEF, WINDOW_MULT};
use crate::utils::now_ms;

/// Create a preconfigured SrtlaConnection bound to a local ephemeral UDP port and targeting 127.0.0.1:8080 for use in tests.

///

/// The returned connection is marked connected and has default/test-friendly initial values for windowing, RTT metrics, packet logs, and timestamps.

///

/// # Examples

///

/// ```

/// let conn = tokio::runtime::Runtime::new().unwrap().block_on(create_test_connection());

/// assert_eq!(conn.remote.ip().to_string(), "127.0.0.1");

/// assert_eq!(conn.remote.port(), 8080);

/// assert!(conn.connected);

/// ```
pub async fn create_test_connection() -> SrtlaConnection {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    let tokio_socket = UdpSocket::from_std(socket).unwrap();
    let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);

    SrtlaConnection {
        socket: tokio_socket,
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

/// Create multiple test SrtlaConnection instances with distinct local addresses and incrementing remote ports.
///
/// Each connection is bound to an ephemeral local UDP port on 127.0.0.1, uses a unique local IP (192.168.1.10 + index),
/// and targets remote port 8080 + index. Connections are pre-initialized with default transport and RTT-related metrics
/// suitable for unit tests.
///
/// # Returns
///
/// A `Vec<SrtlaConnection>` containing `count` constructed connections.
///
/// # Examples
///
/// ```
/// let rt = tokio::runtime::Runtime::new().unwrap();
/// let conns = rt.block_on(create_test_connections(2));
/// assert_eq!(conns.len(), 2);
/// ```
pub async fn create_test_connections(count: usize) -> Vec<SrtlaConnection> {
    let mut connections = Vec::new();

    for i in 0..count {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let tokio_socket = UdpSocket::from_std(socket).unwrap();
        let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080 + i as u16);

        let conn = SrtlaConnection {
            socket: tokio_socket,
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