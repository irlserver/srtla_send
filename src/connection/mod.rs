mod ack_nak;
pub mod batch_recv;
pub mod batch_send;
mod bitrate;
mod congestion;
mod incoming;
mod packet_io;
mod reconnection;
mod rtt;
mod socket;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use anyhow::Result;
pub use batch_recv::BatchUdpSocket;
pub use batch_send::BatchSender;
pub use bitrate::BitrateTracker;
pub use congestion::CongestionControl;
pub use incoming::SrtlaIncoming;
pub use reconnection::ReconnectionState;
pub use rtt::RttTracker;
use rustc_hash::FxHashMap;
pub use socket::{bind_from_ip, resolve_remote};
use tokio::time::Instant;

use crate::protocol::*;
use crate::utils::now_ms;

const STARTUP_GRACE_MS: u64 = 1_500;

/// Interval in milliseconds between quality multiplier recalculations.
/// Caching reduces expensive exp() calls from every packet to ~20 times per second.
pub const QUALITY_CACHE_INTERVAL_MS: u64 = 50;

/// Cached quality multiplier to avoid expensive recalculations on every packet.
#[derive(Clone, Copy, Debug)]
pub struct CachedQuality {
    /// The cached quality multiplier value
    pub multiplier: f64,
    /// Timestamp when the multiplier was last calculated
    pub last_calculated_ms: u64,
}

impl Default for CachedQuality {
    fn default() -> Self {
        Self {
            multiplier: 1.0,
            last_calculated_ms: 0,
        }
    }
}

pub struct SrtlaConnection {
    pub(crate) conn_id: u64,
    #[cfg(feature = "test-internals")]
    pub socket: Arc<BatchUdpSocket>,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) socket: Arc<BatchUdpSocket>,
    #[allow(dead_code)]
    #[cfg(feature = "test-internals")]
    pub remote: SocketAddr,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) remote: SocketAddr,
    #[allow(dead_code)]
    #[cfg(feature = "test-internals")]
    pub local_ip: IpAddr,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) local_ip: IpAddr,
    pub label: String,
    #[cfg(feature = "test-internals")]
    pub connected: bool,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) connected: bool,
    #[cfg(feature = "test-internals")]
    pub window: i32,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) window: i32,
    #[cfg(feature = "test-internals")]
    pub in_flight_packets: i32,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) in_flight_packets: i32,
    /// Packet log: maps sequence number -> send timestamp (ms).
    /// Uses FxHashMap for O(1) insert/remove instead of O(256) linear scan.
    #[cfg(feature = "test-internals")]
    pub packet_log: FxHashMap<i32, u64>,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) packet_log: FxHashMap<i32, u64>,
    /// Highest sequence number that has been cumulatively ACKed.
    /// Used to optimize cumulative ACK processing by skipping already-ACKed sequences.
    #[cfg(feature = "test-internals")]
    pub highest_acked_seq: i32,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) highest_acked_seq: i32,
    #[cfg(feature = "test-internals")]
    pub last_received: Option<Instant>,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) last_received: Option<Instant>,
    #[cfg(feature = "test-internals")]
    pub last_sent: Option<Instant>,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) last_sent: Option<Instant>,
    /// Timestamp of the last keepalive sent (for periodic telemetry)
    pub(crate) last_keepalive_sent: Option<Instant>,
    // Sub-structs for organized state management
    #[cfg(feature = "test-internals")]
    pub rtt: RttTracker,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) rtt: RttTracker,
    #[cfg(feature = "test-internals")]
    pub congestion: CongestionControl,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) congestion: CongestionControl,
    #[cfg(feature = "test-internals")]
    pub bitrate: BitrateTracker,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) bitrate: BitrateTracker,
    #[cfg(feature = "test-internals")]
    pub reconnection: ReconnectionState,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) reconnection: ReconnectionState,
    /// Cached quality multiplier for performance optimization.
    /// Recalculated every 50ms instead of on every packet.
    pub(crate) quality_cache: CachedQuality,
    /// Batch sender for optimized packet transmission.
    /// Buffers up to 16 packets before flushing, reducing syscall overhead.
    pub(crate) batch_sender: BatchSender,
}

impl SrtlaConnection {
    pub async fn connect_from_ip(ip: IpAddr, host: &str, port: u16) -> Result<Self> {
        use rand::RngCore;

        let remote = resolve_remote(host, port).await?;
        let sock = bind_from_ip(ip, 0)?;
        sock.connect(&remote.into())?;
        sock.set_nonblocking(true)?;
        let socket = Arc::new(BatchUdpSocket::new(sock)?);
        let startup_deadline = now_ms() + STARTUP_GRACE_MS;
        Ok(Self {
            conn_id: rand::rng().next_u64(),
            socket,
            remote,
            local_ip: ip,
            label: format!("{}:{} via {}", host, port, ip),
            connected: false,
            window: WINDOW_DEF * WINDOW_MULT,
            in_flight_packets: 0,
            packet_log: FxHashMap::with_capacity_and_hasher(PKT_LOG_SIZE, Default::default()),
            highest_acked_seq: i32::MIN,
            last_received: None,
            last_sent: None,
            last_keepalive_sent: None,
            rtt: RttTracker::default(),
            congestion: CongestionControl::default(),
            bitrate: BitrateTracker::default(),
            reconnection: ReconnectionState {
                startup_grace_deadline_ms: startup_deadline,
                ..Default::default()
            },
            quality_cache: CachedQuality::default(),
            batch_sender: BatchSender::new(),
        })
    }

    #[inline(always)]
    pub fn get_score(&self) -> i32 {
        if !self.connected {
            return -1;
        }
        // Mirror C's select_conn(): score = window / (in_flight + 1).
        // Include queued (not-yet-flushed) packets so the score drops immediately
        // when a packet is queued, matching C's reg_pkt() which increments
        // in_flight_pkts per packet before the next select_conn() call.
        let total_in_flight = self
            .in_flight_packets
            .saturating_add(self.batch_sender.queued_count());
        let denom = total_in_flight.saturating_add(1).max(1);
        self.window / denom
    }

    /// Queue a data packet for batched sending.
    ///
    /// Returns true if the batch queue is full and needs to be flushed.
    /// The caller should call `flush_batch()` when this returns true or when
    /// the flush timer fires.
    #[inline]
    pub fn queue_data_packet(&mut self, data: &[u8], seq: Option<u32>, send_time_ms: u64) -> bool {
        // Track bytes for bitrate calculation (tracked at queue time)
        self.bitrate.update_on_send(data.len() as u64);
        self.batch_sender.queue_packet(data, seq, send_time_ms)
    }

    /// Check if the batch queue needs time-based flushing (15ms interval)
    #[inline]
    pub fn needs_batch_flush(&self) -> bool {
        self.batch_sender.needs_time_flush()
    }

    /// Check if there are any packets in the batch queue
    #[inline]
    pub fn has_queued_packets(&self) -> bool {
        self.batch_sender.has_queued_packets()
    }

    /// Flush the batch queue, sending all queued packets.
    ///
    /// This registers all sent packets for in-flight tracking.
    pub async fn flush_batch(&mut self) -> Result<()> {
        if !self.batch_sender.has_queued_packets() {
            return Ok(());
        }

        match self.batch_sender.flush(&self.socket).await {
            Ok(tracking_info) => {
                // Register all sent packets for in-flight tracking
                for (seq, send_time_ms) in tracking_info {
                    if let Some(s) = seq {
                        self.register_packet(s as i32, send_time_ms);
                    }
                }
                self.last_sent = Some(Instant::now());
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!("batch flush failed: {}", e)),
        }
    }

    pub async fn send_keepalive(&mut self) -> Result<()> {
        // Create extended keepalive with connection info (telemetry for receiver)
        let info = ConnectionInfo {
            conn_id: self.conn_id as u32,
            window: self.window,
            in_flight: self.in_flight_packets,
            rtt_ms: self.rtt.smooth_rtt_ms as u32,
            nak_count: self.congestion.nak_count as u32,
            bitrate_bytes_per_sec: (self.bitrate.current_bitrate_bps / 8.0) as u32,
        };
        let pkt = create_keepalive_packet_ext(info);
        self.socket.send(&pkt).await?;
        let now_instant = Instant::now();
        let now = now_ms();
        self.last_sent = Some(now_instant);
        self.last_keepalive_sent = Some(now_instant);
        // Only set waiting flag and timestamp when we intend to measure RTT
        if !self.rtt.waiting_for_keepalive_response
            && (self.rtt.last_rtt_measurement_ms == 0
                || now.saturating_sub(self.rtt.last_rtt_measurement_ms) > 3000)
        {
            self.rtt.record_keepalive_sent();
        }
        Ok(())
    }

    pub async fn send_srtla_packet(&mut self, pkt: &[u8]) -> Result<()> {
        self.socket.send(pkt).await?;
        self.last_sent = Some(Instant::now());
        Ok(())
    }

    pub async fn send_probe_reg2(&mut self, probe_id: &[u8; SRTLA_ID_LEN]) -> Result<u64> {
        let pkt = create_reg2_packet(probe_id);
        let sent_at = now_ms();
        self.socket.send(&pkt).await?;
        self.reconnection.startup_grace_deadline_ms = sent_at + STARTUP_GRACE_MS;
        Ok(sent_at)
    }

    pub fn is_rtt_stable(&self) -> bool {
        self.rtt.is_stable()
    }

    pub fn get_smooth_rtt_ms(&self) -> f64 {
        self.rtt.smooth_rtt_ms
    }

    pub fn get_fast_rtt_ms(&self) -> f64 {
        self.rtt.fast_rtt_ms
    }

    pub fn get_rtt_min_ms(&self) -> f64 {
        self.rtt.rtt_min_ms
    }

    pub fn get_rtt_jitter_ms(&self) -> f64 {
        self.rtt.rtt_jitter_ms
    }

    pub fn needs_rtt_measurement(&self) -> bool {
        self.rtt
            .needs_measurement(self.connected, self.reconnection.connection_established_ms)
    }

    pub fn needs_keepalive(&self) -> bool {
        // Send keepalive every IDLE_TIME (1s) unconditionally on all connections.
        // Moblin does this with standard 10-byte keepalives; we use extended 38-byte
        // keepalives to provide the receiver with telemetry (window, RTT, NAKs, bitrate).
        if !self.connected {
            return false;
        }

        match self.last_keepalive_sent {
            None => true,
            Some(last) => last.elapsed().as_secs() >= IDLE_TIME,
        }
    }

    pub fn perform_window_recovery(&mut self) {
        self.congestion
            .perform_window_recovery(&mut self.window, self.connected, &self.label);
    }

    #[inline(always)]
    pub fn is_timed_out(&self) -> bool {
        // During initial registration (not yet connected), allow grace period
        if !self.connected {
            // If this connection was never established (connection_established_ms == 0),
            // check if we're still within the startup grace period
            if self.reconnection.connection_established_ms == 0 {
                let now = now_ms();
                if now < self.reconnection.startup_grace_deadline_ms {
                    return false;
                }
            }
            // After grace period, or for connections that were previously established,
            // if we've never received anything or haven't received in a while, consider it timed out
            return self.last_received.is_none()
                || self
                    .last_received
                    .map(|lr| lr.elapsed().as_secs() >= CONN_TIMEOUT)
                    .unwrap_or(true);
        }

        // For established connections, check normal timeout
        if let Some(lr) = self.last_received {
            lr.elapsed().as_secs() >= CONN_TIMEOUT
        } else {
            false
        }
    }

    /// Reset core connection state (window, packet tracking, batch queue).
    /// Used by both mark_for_recovery and reset_state.
    fn reset_core_state(&mut self) {
        self.connected = false;
        self.window = WINDOW_DEF * WINDOW_MULT;
        self.in_flight_packets = 0;
        self.packet_log.clear();
        self.highest_acked_seq = i32::MIN;
        self.batch_sender.reset();
    }

    /// Mark connection for recovery (C-style), similar to setting last_rcvd = 1.
    /// Soft reset: clears packet state but preserves congestion/bitrate stats.
    pub fn mark_for_recovery(&mut self) {
        self.last_received = None;
        self.last_keepalive_sent = None;
        self.rtt.last_keepalive_sent_ms = 0;
        self.rtt.waiting_for_keepalive_response = false;
        self.reset_core_state();
        // Set grace deadline to 0 to indicate this connection is immediately timed out
        // (matches C behavior of setting last_rcvd = 1)
        self.reconnection.startup_grace_deadline_ms = 0;
    }

    pub fn time_since_last_nak_ms(&self) -> Option<u64> {
        self.congestion.time_since_last_nak_ms()
    }

    pub fn total_nak_count(&self) -> i32 {
        self.congestion.nak_count
    }

    pub fn nak_burst_count(&self) -> i32 {
        self.congestion.nak_burst_count
    }

    pub fn connection_established_ms(&self) -> u64 {
        self.reconnection.connection_established_ms
    }

    /// Get the cached quality multiplier, recalculating if stale.
    ///
    /// This is more efficient than calling `calculate_quality_multiplier()` on every packet
    /// because it only recalculates every 50ms.
    #[inline(always)]
    pub fn get_cached_quality_multiplier(&mut self, current_time_ms: u64) -> f64 {
        use crate::sender::calculate_quality_multiplier;

        if current_time_ms.saturating_sub(self.quality_cache.last_calculated_ms)
            >= QUALITY_CACHE_INTERVAL_MS
        {
            self.quality_cache.multiplier = calculate_quality_multiplier(self, current_time_ms);
            self.quality_cache.last_calculated_ms = current_time_ms;
        }
        self.quality_cache.multiplier
    }

    pub fn should_attempt_reconnect(&self) -> bool {
        self.reconnection.should_attempt_reconnect()
    }

    pub fn record_reconnect_attempt(&mut self) {
        self.reconnection.record_attempt(&self.label);
    }

    pub fn mark_reconnect_success(&mut self) {
        self.reconnection.mark_success(&self.label);
    }

    /// Calculate current bitrate
    pub fn calculate_bitrate(&mut self) {
        self.bitrate.calculate();
    }

    /// Get current bitrate in Mbps
    pub fn current_bitrate_mbps(&self) -> f64 {
        self.bitrate.mbps()
    }

    /// Reset connection state after socket replacement.
    /// Full reset: clears all state including congestion/bitrate stats.
    fn reset_state(&mut self) {
        self.last_received = None;
        self.reset_core_state();

        // Reset submodule state
        self.congestion.reset();
        self.rtt.reset();
        self.bitrate.reset();

        // Reset reconnection tracking
        self.reconnection.last_reconnect_attempt_ms = now_ms();
        self.reconnection.reconnect_failure_count = 0;
    }

    pub async fn reconnect(&mut self) -> Result<()> {
        let sock = bind_from_ip(self.local_ip, 0)?;
        sock.connect(&self.remote.into())?;
        sock.set_nonblocking(true)?;
        let socket = BatchUdpSocket::new(sock)?;
        self.socket = Arc::new(socket);

        self.reset_state();

        // Don't reset connection_established_ms for reconnections - only set when REG3
        // is received
        self.mark_reconnect_success();
        self.reconnection.reset_startup_grace();
        Ok(())
    }
}
