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
use smallvec::SmallVec;
pub use socket::{bind_from_ip, resolve_remote};
use tokio::net::UdpSocket;
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::protocol::*;
use crate::registration::{RegistrationEvent, SrtlaRegistrationManager};
use crate::utils::now_ms;

const STARTUP_GRACE_MS: u64 = 1_500;

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
    #[cfg(feature = "test-internals")]
    pub packet_log: [i32; PKT_LOG_SIZE],
    #[cfg(not(feature = "test-internals"))]
    pub(crate) packet_log: [i32; PKT_LOG_SIZE],
    #[cfg(feature = "test-internals")]
    pub packet_send_times_ms: [u64; PKT_LOG_SIZE],
    #[cfg(not(feature = "test-internals"))]
    pub(crate) packet_send_times_ms: [u64; PKT_LOG_SIZE],
    #[cfg(feature = "test-internals")]
    pub packet_idx: usize,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) packet_idx: usize,
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
}

impl SrtlaConnection {
    pub async fn connect_from_ip(ip: IpAddr, host: &str, port: u16) -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

        let remote = resolve_remote(host, port).await?;
        let sock = bind_from_ip(ip, 0)?;
        sock.connect(&remote.into())?;
        let std_sock: std::net::UdpSocket = sock.into();
        std_sock.set_nonblocking(true)?;
        let socket = Arc::new(UdpSocket::from_std(std_sock)?);
        let startup_deadline = now_ms() + STARTUP_GRACE_MS;
        Ok(Self {
            conn_id: NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed),
            socket,
            remote,
            local_ip: ip,
            label: format!("{}:{} via {}", host, port, ip),
            connected: false,
            window: WINDOW_DEF * WINDOW_MULT,
            in_flight_packets: 0,
            packet_log: [-1; PKT_LOG_SIZE],
            packet_send_times_ms: [0; PKT_LOG_SIZE],
            packet_idx: 0,
            last_received: None,
            last_sent: None,
            rtt: RttTracker::default(),
            congestion: CongestionControl::default(),
            bitrate: BitrateTracker::default(),
            reconnection: ReconnectionState {
                startup_grace_deadline_ms: startup_deadline,
                ..Default::default()
            },
        })
    }

    #[inline]
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
    pub async fn send_data_with_tracking(&mut self, data: &[u8], seq: Option<u32>) -> Result<()> {
        self.socket.send(data).await?;
        // Track bytes sent for bitrate calculation
        self.bitrate.update_on_send(data.len() as u64);
        // Update last_sent timestamp
        self.last_sent = Some(Instant::now());
        if let Some(s) = seq {
            self.register_packet(s as i32);
        }
        Ok(())
    }

    pub async fn send_keepalive(&mut self) -> Result<()> {
        let pkt = create_keepalive_packet();
        self.socket.send(&pkt).await?;
        let now_instant = Instant::now();
        let now = now_ms();
        self.last_sent = Some(now_instant); // Track all sends, including keepalives
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
        self.rtt.last_keepalive_sent_ms = sent_at;
        self.rtt.waiting_for_keepalive_response = true;
        self.reconnection.startup_grace_deadline_ms = sent_at + STARTUP_GRACE_MS;
        Ok(sent_at)
    }

    pub async fn drain_incoming(
        &mut self,
        conn_idx: usize,
        reg: &mut SrtlaRegistrationManager,
        instant_forwarder: &std::sync::mpsc::Sender<SmallVec<u8, 64>>,
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
                        instant_forwarder,
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
        instant_forwarder: &std::sync::mpsc::Sender<SmallVec<u8, 64>>,
        data: &[u8],
    ) -> Result<SrtlaIncoming> {
        let mut incoming = SrtlaIncoming::default();
        self.process_packet_internal(conn_idx, reg, instant_forwarder, data, &mut incoming)
            .await?;
        Ok(incoming)
    }

    async fn process_packet_internal(
        &mut self,
        conn_idx: usize,
        reg: &mut SrtlaRegistrationManager,
        instant_forwarder: &std::sync::mpsc::Sender<SmallVec<u8, 64>>,
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
                let ack_packet = SmallVec::from_slice(data);
                let _ = instant_forwarder.send(ack_packet.clone());
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
                incoming.forward_to_client.push(SmallVec::from_slice(data));
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
                incoming.forward_to_client.push(SmallVec::from_slice(data));
            }
        }
        Ok(())
    }

    pub fn register_packet(&mut self, seq: i32) {
        let idx = self.packet_idx % PKT_LOG_SIZE;
        self.packet_log[idx] = seq;
        self.packet_send_times_ms[idx] = now_ms();

        // Debug when packet log wraps around
        let old_idx = self.packet_idx;
        self.packet_idx = self.packet_idx.wrapping_add(1) % PKT_LOG_SIZE;
        if old_idx == PKT_LOG_SIZE - 1 && self.packet_idx == 0 {
            debug!("{}: packet log wrapped around (seq={})", self.label, seq);
        }

        self.in_flight_packets = self.in_flight_packets.saturating_add(1);
    }

    pub fn handle_srt_ack(&mut self, ack: i32) {
        let mut ack_send_time_ms: Option<u64> = None;
        let mut remaining_count = 0;

        for i in 0..PKT_LOG_SIZE {
            let idx = (self.packet_idx + PKT_LOG_SIZE - 1 - i) % PKT_LOG_SIZE;
            let val = self.packet_log[idx];
            if val == ack {
                self.packet_log[idx] = -1;
                let t = self.packet_send_times_ms[idx];
                if t != 0 {
                    ack_send_time_ms = Some(t);
                }
            } else if val > 0 && val < ack {
                self.packet_log[idx] = -1;
            } else if val > 0 {
                remaining_count += 1;
            }
        }

        self.in_flight_packets = remaining_count;

        if let Some(sent_ms) = ack_send_time_ms {
            let now = now_ms();
            let rtt = now.saturating_sub(sent_ms);
            if rtt > 0 && rtt <= 10_000 {
                self.rtt.update_estimate(rtt);
            }
        }
    }

    pub fn handle_nak(&mut self, seq: i32) -> bool {
        let mut found = false;
        for i in 0..PKT_LOG_SIZE {
            let idx = (self.packet_idx + PKT_LOG_SIZE - 1 - i) % PKT_LOG_SIZE;
            if self.packet_log[idx] == seq {
                self.packet_log[idx] = -1;
                found = true;
                break;
            }
        }

        if !found {
            return false;
        }

        self.congestion
            .handle_nak(&mut self.window, seq, &self.label)
    }

    pub fn handle_srtla_ack_specific(&mut self, seq: i32, classic_mode: bool) -> bool {
        // Find the specific packet in the log and handle targeted logic
        // This mimics the first phase of the original implementation's
        // register_srtla_ack
        let mut found = false;

        // Search backwards through the packet log (most recent first)
        for i in 0..PKT_LOG_SIZE {
            let idx = (self.packet_idx + PKT_LOG_SIZE - 1 - i) % PKT_LOG_SIZE;
            if self.packet_log[idx] == seq {
                found = true;
                if self.in_flight_packets > 0 {
                    self.in_flight_packets -= 1;
                }
                self.packet_log[idx] = -1;

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
                break;
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

    pub fn is_timed_out(&self) -> bool {
        if !self.connected {
            return true;
        }
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
        self.packet_log = [-1; PKT_LOG_SIZE];
        self.reconnection.reset_startup_grace();
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
        self.last_received = Some(Instant::now());
        self.window = WINDOW_DEF * WINDOW_MULT;
        self.in_flight_packets = 0;
        self.packet_log = [-1; PKT_LOG_SIZE];
        self.packet_idx = 0;
        self.congestion.nak_count = 0;
        self.congestion.last_nak_time_ms = 0;
        self.congestion.nak_burst_count = 0;
        self.congestion.nak_burst_start_time_ms = 0;
        self.congestion.last_window_increase_ms = 0;
        self.congestion.consecutive_acks_without_nak = 0;
        self.congestion.fast_recovery_mode = false;
        self.rtt.last_rtt_measurement_ms = 0;
        self.rtt.smooth_rtt_ms = 0.0;
        self.rtt.fast_rtt_ms = 0.0;
        self.rtt.rtt_jitter_ms = 0.0;
        self.rtt.prev_rtt_ms = 0.0;
        self.rtt.rtt_avg_delta_ms = 0.0;
        self.rtt.rtt_min_ms = 200.0;
        self.rtt.estimated_rtt_ms = 0.0;
        self.rtt.last_keepalive_sent_ms = 0;
        self.rtt.waiting_for_keepalive_response = false;
        self.packet_send_times_ms = [0; PKT_LOG_SIZE];
        self.congestion.fast_recovery_start_ms = 0;
        self.reconnection.last_reconnect_attempt_ms = now_ms();
        self.reconnection.reconnect_failure_count = 0;
        // Reset bitrate tracker to start fresh measurement window after reconnect
        self.bitrate.reset();
        // Don't reset connection_established_ms for reconnections - only set when REG3
        // is received
        self.mark_reconnect_success();
        self.reconnection.reset_startup_grace();
        Ok(())
    }
}
