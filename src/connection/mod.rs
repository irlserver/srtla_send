mod bitrate;
mod congestion;
mod incoming;
mod reconnection;
mod rtt;
mod socket;

use std::cmp::min;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use anyhow::Result;
pub use bitrate::BitrateTracker;
pub use congestion::CongestionControl;
pub use incoming::SrtlaIncoming;
pub use reconnection::ReconnectionState;
pub use rtt::RttTracker;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
pub use socket::{bind_from_ip, resolve_remote};
use tokio::net::UdpSocket;
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::protocol::*;
use crate::registration::{RegistrationEvent, SrtlaRegistrationManager};
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
    pub socket: Arc<UdpSocket>,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) socket: Arc<UdpSocket>,
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
    #[cfg(feature = "test-internals")]
    pub last_received: Option<Instant>,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) last_received: Option<Instant>,
    #[cfg(feature = "test-internals")]
    pub last_sent: Option<Instant>,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) last_sent: Option<Instant>,
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
}

impl SrtlaConnection {
    pub async fn connect_from_ip(ip: IpAddr, host: &str, port: u16) -> Result<Self> {
        use rand::RngCore;

        let remote = resolve_remote(host, port).await?;
        let sock = bind_from_ip(ip, 0)?;
        sock.connect(&remote.into())?;
        let std_sock: std::net::UdpSocket = sock.into();
        std_sock.set_nonblocking(true)?;
        let socket = Arc::new(UdpSocket::from_std(std_sock)?);
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
            last_received: None,
            last_sent: None,
            rtt: RttTracker::default(),
            congestion: CongestionControl::default(),
            bitrate: BitrateTracker::default(),
            reconnection: ReconnectionState {
                startup_grace_deadline_ms: startup_deadline,
                ..Default::default()
            },
            quality_cache: CachedQuality::default(),
        })
    }

    #[inline(always)]
    pub fn get_score(&self) -> i32 {
        if !self.connected {
            return -1;
        }
        // Mirror classic implementations: score is window divided by in-flight load.
        // Use saturating_add to avoid overflow when the queue is extremely large and
        // clamp the denominator to at least 1 to prevent division by zero.
        let denom = self.in_flight_packets.saturating_add(1).max(1);
        self.window / denom
    }

    #[inline]
    pub async fn send_data_with_tracking(
        &mut self,
        data: &[u8],
        seq: Option<u32>,
        send_time_ms: u64,
    ) -> Result<()> {
        self.socket.send(data).await?;
        // Track bytes sent for bitrate calculation
        self.bitrate.update_on_send(data.len() as u64);
        // Update last_sent timestamp
        self.last_sent = Some(Instant::now());
        if let Some(s) = seq {
            self.register_packet(s as i32, send_time_ms);
        }
        Ok(())
    }

    pub async fn send_keepalive(&mut self) -> Result<()> {
        // Create extended keepalive with connection info
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
        let now = now_ms();
        self.last_sent = Some(Instant::now()); // Track all sends, including keepalives
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

    pub async fn drain_incoming(
        &mut self,
        conn_idx: usize,
        reg: &mut SrtlaRegistrationManager,
        local_listener: &UdpSocket,
        instant_forwarder: &tokio::sync::mpsc::UnboundedSender<(SocketAddr, SmallVec<u8, 64>)>,
        client_addr: Option<SocketAddr>,
    ) -> Result<SrtlaIncoming> {
        let mut buf = [0u8; MTU];
        let mut incoming = SrtlaIncoming::default();
        loop {
            match self.socket.try_recv(&mut buf) {
                Ok(n) => {
                    if n == 0 {
                        break;
                    }
                    self.process_packet_internal(
                        conn_idx,
                        reg,
                        local_listener,
                        instant_forwarder,
                        client_addr,
                        &buf[..n],
                        &mut incoming,
                    )
                    .await?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    warn!("read uplink error: {}", e);
                    break;
                }
            }
        }
        Ok(incoming)
    }

    pub async fn process_packet(
        &mut self,
        conn_idx: usize,
        reg: &mut SrtlaRegistrationManager,
        local_listener: &UdpSocket,
        instant_forwarder: &tokio::sync::mpsc::UnboundedSender<(SocketAddr, SmallVec<u8, 64>)>,
        client_addr: Option<SocketAddr>,
        data: &[u8],
    ) -> Result<SrtlaIncoming> {
        let mut incoming = SrtlaIncoming::default();
        self.process_packet_internal(
            conn_idx,
            reg,
            local_listener,
            instant_forwarder,
            client_addr,
            data,
            &mut incoming,
        )
        .await?;
        Ok(incoming)
    }

    async fn process_packet_internal(
        &mut self,
        conn_idx: usize,
        reg: &mut SrtlaRegistrationManager,
        local_listener: &UdpSocket,
        instant_forwarder: &tokio::sync::mpsc::UnboundedSender<(SocketAddr, SmallVec<u8, 64>)>,
        client_addr: Option<SocketAddr>,
        data: &[u8],
        incoming: &mut SrtlaIncoming,
    ) -> Result<()> {
        incoming.read_any = true;
        let recv_time = Instant::now();
        let pt = get_packet_type(data);
        if let Some(pt) = pt {
            if let Some(event) = reg.process_registration_packet(conn_idx, data) {
                match event {
                    RegistrationEvent::RegNgp => {
                        reg.try_send_reg1_immediately(conn_idx, self).await;
                    }
                    RegistrationEvent::Reg3 => {
                        self.connected = true;
                        self.last_received = Some(recv_time);
                        if self.reconnection.connection_established_ms == 0 {
                            self.reconnection.connection_established_ms = crate::utils::now_ms();
                        }
                        self.reconnection.mark_success(&self.label);
                    }
                    RegistrationEvent::RegErr => {
                        self.connected = false;
                        self.last_received = None;
                    }
                    RegistrationEvent::Reg2 => {}
                }
                return Ok(());
            }

            self.last_received = Some(recv_time);

            if pt == SRT_TYPE_ACK {
                if let Some(ack) = parse_srt_ack(data) {
                    incoming.ack_numbers.push(ack);
                }
                let ack_packet = SmallVec::from_slice_copy(data);
                // Try synchronous send first (avoids task context switch)
                // Only fall back to channel if socket would block
                if let Some(addr) = client_addr {
                    match local_listener.try_send_to(&ack_packet, addr) {
                        Ok(_) => {} // Fast path: sent directly
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            // Slow path: socket busy, use channel
                            let _ = instant_forwarder.send((addr, ack_packet.clone()));
                        }
                        Err(_) => {} // Other errors: drop silently (same as before)
                    }
                }
                incoming.forward_to_client.push(ack_packet);
            } else if pt == SRT_TYPE_NAK {
                let nak_list = parse_srt_nak(data);
                if !nak_list.is_empty() {
                    debug!(
                        "📦 NAK received from {}: {} sequences",
                        self.label,
                        nak_list.len()
                    );
                    for seq in nak_list {
                        incoming.nak_numbers.push(seq);
                    }
                }
                incoming
                    .forward_to_client
                    .push(SmallVec::from_slice_copy(data));
            } else if pt == SRTLA_TYPE_ACK {
                let ack_list = parse_srtla_ack(data);
                if !ack_list.is_empty() {
                    debug!(
                        "🎯 SRTLA ACK received from {}: {} sequences",
                        self.label,
                        ack_list.len()
                    );
                    for seq in ack_list {
                        incoming.srtla_ack_numbers.push(seq);
                    }
                }
            } else if pt == SRTLA_TYPE_KEEPALIVE {
                self.rtt.handle_keepalive_response(data, &self.label);
            } else {
                incoming
                    .forward_to_client
                    .push(SmallVec::from_slice_copy(data));
            }
        }
        Ok(())
    }

    /// Register a packet as in-flight. O(1) insert.
    #[inline]
    pub fn register_packet(&mut self, seq: i32, send_time_ms: u64) {
        self.packet_log.insert(seq, send_time_ms);
        self.in_flight_packets = self.packet_log.len() as i32;
    }

    /// Handle SRT cumulative ACK - clears all packets with seq <= ack.
    /// This is O(n) due to cumulative ACK semantics, but uses efficient retain().
    pub fn handle_srt_ack(&mut self, ack: i32) {
        // Get send time for RTT calculation before removing
        let ack_send_time_ms = self.packet_log.get(&ack).copied();

        // Remove all packets with seq <= ack (cumulative ACK)
        self.packet_log.retain(|&seq, _| seq > ack);
        self.in_flight_packets = self.packet_log.len() as i32;

        // Update RTT estimate if we found the acked packet
        if let Some(sent_ms) = ack_send_time_ms {
            let now = now_ms();
            let rtt = now.saturating_sub(sent_ms);
            if rtt > 0 && rtt <= 10_000 {
                self.rtt.update_estimate(rtt);
            }
        }
    }

    /// Handle NAK for a specific sequence. O(1) remove.
    #[inline]
    pub fn handle_nak(&mut self, seq: i32) -> bool {
        let found = self.packet_log.remove(&seq).is_some();
        if found {
            self.in_flight_packets = self.packet_log.len() as i32;
            self.congestion
                .handle_nak(&mut self.window, seq, &self.label);
        }
        found
    }

    /// Handle SRTLA ACK for a specific sequence. O(1) remove.
    #[inline]
    pub fn handle_srtla_ack_specific(&mut self, seq: i32, classic_mode: bool) -> bool {
        let found = self.packet_log.remove(&seq).is_some();
        if found {
            self.in_flight_packets = self.packet_log.len() as i32;

            if classic_mode {
                self.congestion.handle_srtla_ack_specific_classic(
                    &mut self.window,
                    self.in_flight_packets,
                    seq,
                    &self.label,
                );
            } else {
                self.congestion.handle_srtla_ack_enhanced(
                    &mut self.window,
                    self.in_flight_packets,
                    &self.label,
                );
            }
        }
        found
    }

    pub fn handle_srtla_ack_global(&mut self) {
        // Global +1 window increase for connections that have received data (from
        // original implementation)
        // This matches C version: if (c->last_rcvd != 0)
        // In Rust, we check if last_received is Some (i.e., has been set when data was
        // received)
        if self.connected && self.last_received.is_some() {
            let old = self.window;
            self.window = min(self.window + 1, WINDOW_MAX * WINDOW_MULT);

            if old < self.window && (self.window - old) > 100 {
                debug!(
                    "{}: Major window recovery {} → {} (+{})",
                    self.label,
                    old,
                    self.window,
                    self.window - old
                );
            }
        }
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

    pub fn get_rtt_jitter_ms(&self) -> f64 {
        self.rtt.rtt_jitter_ms
    }

    pub fn needs_rtt_measurement(&self) -> bool {
        self.rtt
            .needs_measurement(self.connected, self.reconnection.connection_established_ms)
    }

    pub fn needs_keepalive(&self) -> bool {
        // Match C implementation: send keepalive if we haven't sent ANY data (not just
        // keepalives) in IDLE_TIME. This prevents unnecessary keepalives during active
        // transmission.
        let now = Instant::now();
        if !self.connected {
            return false;
        }

        match self.last_sent {
            None => true, // Never sent anything, need keepalive
            Some(last) => now.duration_since(last).as_secs() >= IDLE_TIME,
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

    /// Mark connection for recovery (C-style), similar to setting last_rcvd = 1
    pub fn mark_for_recovery(&mut self) {
        self.last_received = None;
        self.rtt.last_keepalive_sent_ms = 0;
        self.rtt.waiting_for_keepalive_response = false;
        self.connected = false;
        // Reset connection state like C version does
        // Reset to default window like classic implementation so reconnecting
        // links can ramp quickly once registration completes.
        self.window = WINDOW_DEF * WINDOW_MULT;
        self.in_flight_packets = 0;
        self.packet_log.clear();
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

    pub async fn reconnect(&mut self) -> Result<()> {
        let sock = bind_from_ip(self.local_ip, 0)?;
        sock.connect(&self.remote.into())?;
        let std_sock: std::net::UdpSocket = sock.into();
        std_sock.set_nonblocking(true)?;
        let socket = UdpSocket::from_std(std_sock)?;
        self.socket = Arc::new(socket);
        self.connected = false;
        self.last_received = None;
        self.window = WINDOW_DEF * WINDOW_MULT;
        self.in_flight_packets = 0;
        self.packet_log.clear();

        // Use encapsulated reset methods for submodules
        self.congestion.reset();
        self.rtt.reset();
        self.bitrate.reset();

        // Reset reconnection tracking
        self.reconnection.last_reconnect_attempt_ms = now_ms();
        self.reconnection.reconnect_failure_count = 0;

        // Don't reset connection_established_ms for reconnections - only set when REG3
        // is received
        self.mark_reconnect_success();
        self.reconnection.reset_startup_grace();
        Ok(())
    }
}
