//! EDPF (Earliest Delivery Path First) link selection.
//!
//! Selects the link with the lowest predicted arrival time, considering
//! in-flight data, link capacity, loss rate, and base RTT.

use crate::connection::SrtlaConnection;

/// SRT payload packet size in bytes.
const SRT_PKT_SIZE: usize = 1316;

/// Velocity penalty scaling factor.
///
/// When the Kalman velocity is positive (RTT rising), we add a penalty term
/// proportional to the velocity. This penalises links with building congestion
/// before loss manifests, giving EDPF a proactive avoidance signal.
/// The factor converts ms/sample velocity into seconds of penalty.
const VELOCITY_PENALTY_FACTOR: f64 = 0.005;

/// Compute predicted arrival time for a connection.
///
/// Returns `None` if the connection lacks valid capacity or RTT data.
fn predicted_arrival(conn: &SrtlaConnection, pkt_size: usize) -> Option<f64> {
    if !conn.connected {
        return None;
    }

    let bitrate_bps = conn.bitrate.current_bitrate_bps;
    if bitrate_bps <= 0.0 {
        return None;
    }
    let capacity_bytes_per_sec = bitrate_bps / 8.0;

    // Loss from quality multiplier
    let loss = (1.0 - conn.quality_cache.multiplier).clamp(0.0, 0.99);
    let effective_capacity = capacity_bytes_per_sec * (1.0 - loss);
    if effective_capacity <= 0.0 {
        return None;
    }

    let in_flight_bytes = (conn.in_flight_packets.max(0) as usize * SRT_PKT_SIZE) as f64;

    // Use Kalman-smoothed RTT as propagation delay estimate.
    // Falls back to rtt_min_ms if Kalman hasn't initialized yet.
    let smooth_rtt = conn.rtt.kalman_rtt.value();
    let propagation_s = if smooth_rtt > 0.0 {
        smooth_rtt / 1000.0
    } else {
        conn.rtt.rtt_min_ms / 1000.0
    };

    // Velocity penalty: penalise links with rising RTT (positive velocity)
    // to proactively avoid congestion before it manifests as loss.
    let velocity = conn.rtt.kalman_rtt.velocity();
    let velocity_penalty_s = if velocity > 0.0 {
        velocity * VELOCITY_PENALTY_FACTOR
    } else {
        0.0
    };

    Some(
        (in_flight_bytes + pkt_size as f64) / effective_capacity
            + propagation_s
            + velocity_penalty_s,
    )
}

/// Select the connection with lowest predicted arrival time from all connections.
pub fn select_from(conns: &[SrtlaConnection], pkt_size: usize) -> Option<usize> {
    let mut best_idx = None;
    let mut best_arrival = f64::MAX;

    for (i, conn) in conns.iter().enumerate() {
        if let Some(arrival) = predicted_arrival(conn, pkt_size)
            && arrival < best_arrival
        {
            best_arrival = arrival;
            best_idx = Some(i);
        }
    }

    best_idx
}

/// Select the connection with lowest predicted arrival time from a filtered subset.
///
/// `indices` contains the indices of candidate connections in `conns`.
pub fn select_from_indices(
    conns: &[SrtlaConnection],
    indices: &[usize],
    pkt_size: usize,
) -> Option<usize> {
    let mut best_idx = None;
    let mut best_arrival = f64::MAX;

    for &i in indices {
        if i < conns.len()
            && let Some(arrival) = predicted_arrival(&conns[i], pkt_size)
            && arrival < best_arrival
        {
            best_arrival = arrival;
            best_idx = Some(i);
        }
    }

    best_idx
}

/// Compute predicted arrival time for a connection (public for IoDS integration).
pub fn arrival_time(conn: &SrtlaConnection, pkt_size: usize) -> Option<f64> {
    predicted_arrival(conn, pkt_size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::create_test_connections;

    #[test]
    fn test_select_prefers_lower_arrival() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(3));

        // Make conn 1 have lowest arrival (low in-flight, high bitrate, low RTT)
        conns[0].in_flight_packets = 10;
        conns[0].bitrate.current_bitrate_bps = 1_000_000.0;
        conns[0].rtt.rtt_min_ms = 50.0;

        conns[1].in_flight_packets = 0;
        conns[1].bitrate.current_bitrate_bps = 2_000_000.0;
        conns[1].rtt.rtt_min_ms = 20.0;

        conns[2].in_flight_packets = 20;
        conns[2].bitrate.current_bitrate_bps = 500_000.0;
        conns[2].rtt.rtt_min_ms = 100.0;

        let result = select_from(&conns, SRT_PKT_SIZE);
        assert_eq!(
            result,
            Some(1),
            "Should pick conn with lowest predicted arrival"
        );
    }

    #[test]
    fn test_select_skips_disconnected() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(2));

        conns[0].connected = false;
        conns[0].bitrate.current_bitrate_bps = 10_000_000.0;

        conns[1].in_flight_packets = 5;
        conns[1].bitrate.current_bitrate_bps = 1_000_000.0;
        conns[1].rtt.rtt_min_ms = 50.0;

        let result = select_from(&conns, SRT_PKT_SIZE);
        assert_eq!(result, Some(1));
    }

    #[test]
    fn test_select_empty() {
        let conns: Vec<SrtlaConnection> = vec![];
        assert_eq!(select_from(&conns, SRT_PKT_SIZE), None);
    }

    #[test]
    fn test_select_from_indices() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(3));

        conns[0].in_flight_packets = 0;
        conns[0].bitrate.current_bitrate_bps = 5_000_000.0;
        conns[0].rtt.rtt_min_ms = 10.0;

        conns[1].in_flight_packets = 0;
        conns[1].bitrate.current_bitrate_bps = 1_000_000.0;
        conns[1].rtt.rtt_min_ms = 50.0;

        conns[2].in_flight_packets = 0;
        conns[2].bitrate.current_bitrate_bps = 2_000_000.0;
        conns[2].rtt.rtt_min_ms = 20.0;

        // Only consider indices 1 and 2 (exclude the best one, 0)
        let result = select_from_indices(&conns, &[1, 2], SRT_PKT_SIZE);
        assert_eq!(result, Some(2), "Should pick best from subset");
    }
}
