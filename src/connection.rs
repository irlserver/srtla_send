use std::cmp::min;
use std::net::{IpAddr, SocketAddr};

use anyhow::{Context, Result};
use smallvec::SmallVec;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::protocol::*;
use crate::registration::{RegistrationEvent, SrtlaRegistrationManager};
use crate::utils::now_ms;

const NAK_SEARCH_LIMIT: usize = 64;
const ACK_SEARCH_LIMIT: usize = 128;
const NAK_BURST_WINDOW_MS: u64 = 1000;
const NAK_BURST_LOG_THRESHOLD: i32 = 3;
const NORMAL_UTILIZATION_THRESHOLD: f64 = 0.85;
const FAST_UTILIZATION_THRESHOLD: f64 = 0.95;
const NORMAL_ACKS_REQUIRED: i32 = 4;
const FAST_ACKS_REQUIRED: i32 = 2;
const NORMAL_MIN_WAIT_MS: u64 = 2000;
const FAST_MIN_WAIT_MS: u64 = 500;
const NORMAL_INCREMENT_WAIT_MS: u64 = 1000;
const FAST_INCREMENT_WAIT_MS: u64 = 300;
const FAST_RECOVERY_DISABLE_WINDOW: i32 = 12_000;

pub struct SrtlaIncoming {
    pub forward_to_client: SmallVec<Vec<u8>, 4>,
    pub ack_numbers: SmallVec<u32, 4>,
    pub nak_numbers: SmallVec<u32, 4>,
    pub srtla_ack_numbers: SmallVec<u32, 4>,
    pub read_any: bool,
}

pub struct SrtlaConnection {
    pub(crate) conn_id: u64,
    #[cfg(feature = "test-internals")]
    pub socket: UdpSocket,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) socket: UdpSocket,
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
    pub last_keepalive_ms: u64,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) last_keepalive_ms: u64,
    // RTT measurement via keepalive
    #[cfg(feature = "test-internals")]
    pub last_keepalive_sent_ms: u64,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) last_keepalive_sent_ms: u64,
    #[cfg(feature = "test-internals")]
    pub waiting_for_keepalive_response: bool,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) waiting_for_keepalive_response: bool,
    #[cfg(feature = "test-internals")]
    pub last_rtt_measurement_ms: u64,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) last_rtt_measurement_ms: u64,
    #[cfg(feature = "test-internals")]
    pub smooth_rtt_ms: f64,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) smooth_rtt_ms: f64,
    #[cfg(feature = "test-internals")]
    pub fast_rtt_ms: f64,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) fast_rtt_ms: f64,
    #[cfg(feature = "test-internals")]
    pub rtt_jitter_ms: f64,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) rtt_jitter_ms: f64,
    #[cfg(feature = "test-internals")]
    pub prev_rtt_ms: f64,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) prev_rtt_ms: f64,
    #[cfg(feature = "test-internals")]
    pub rtt_avg_delta_ms: f64,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) rtt_avg_delta_ms: f64,
    #[cfg(feature = "test-internals")]
    pub rtt_min_ms: f64,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) rtt_min_ms: f64,
    #[cfg(feature = "test-internals")]
    pub estimated_rtt_ms: f64,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) estimated_rtt_ms: f64,
    // Congestion control state
    #[cfg(feature = "test-internals")]
    pub nak_count: i32,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) nak_count: i32,
    #[cfg(feature = "test-internals")]
    pub last_nak_time_ms: u64,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) last_nak_time_ms: u64,
    #[cfg(feature = "test-internals")]
    pub last_window_increase_ms: u64,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) last_window_increase_ms: u64,
    #[cfg(feature = "test-internals")]
    pub consecutive_acks_without_nak: i32,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) consecutive_acks_without_nak: i32,
    #[cfg(feature = "test-internals")]
    pub fast_recovery_mode: bool,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) fast_recovery_mode: bool,
    #[cfg(feature = "test-internals")]
    pub fast_recovery_start_ms: u64,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) fast_recovery_start_ms: u64,
    // Burst NAK tracking (matches Java implementation)
    #[cfg(feature = "test-internals")]
    pub nak_burst_count: i32,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) nak_burst_count: i32,
    #[cfg(feature = "test-internals")]
    pub nak_burst_start_time_ms: u64,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) nak_burst_start_time_ms: u64,
    #[cfg(feature = "test-internals")]
    pub last_reconnect_attempt_ms: u64,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) last_reconnect_attempt_ms: u64,
    #[cfg(feature = "test-internals")]
    pub reconnect_failure_count: u32,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) reconnect_failure_count: u32,
    // Connection establishment timestamp for startup grace period
    #[cfg(feature = "test-internals")]
    pub connection_established_ms: u64,
    #[cfg(not(feature = "test-internals"))]
    pub(crate) connection_established_ms: u64,
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
        let socket = UdpSocket::from_std(std_sock)?;
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
            connection_established_ms: 0, // Will be set when REG3 is received
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
        if let Some(s) = seq {
            self.register_packet(s as i32);
        }
        Ok(())
    }

    pub async fn send_keepalive(&mut self) -> Result<()> {
        let pkt = create_keepalive_packet();
        self.socket.send(&pkt).await?;
        let now = now_ms();
        self.last_keepalive_ms = now;
        // Only set waiting flag and timestamp when we intend to measure RTT
        if !self.waiting_for_keepalive_response
            && (self.last_rtt_measurement_ms == 0
                || now.saturating_sub(self.last_rtt_measurement_ms) > 3000)
        {
            self.record_keepalive_sent();
        }
        Ok(())
    }

    pub async fn send_srtla_packet(&self, pkt: &[u8]) -> Result<()> {
        self.socket.send(pkt).await?;
        Ok(())
    }

    pub async fn drain_incoming(
        &mut self,
        conn_idx: usize,
        reg: &mut SrtlaRegistrationManager,
        instant_forwarder: &std::sync::mpsc::Sender<Vec<u8>>,
    ) -> Result<SrtlaIncoming> {
        let mut buf = [0u8; MTU];
        let mut read_any = false;
        let mut forward: SmallVec<Vec<u8>, 4> = SmallVec::new();
        let mut acks: SmallVec<u32, 4> = SmallVec::new();
        let mut naks: SmallVec<u32, 4> = SmallVec::new();
        let mut srtla_acks: SmallVec<u32, 4> = SmallVec::new();
        loop {
            match self.socket.try_recv(&mut buf) {
                Ok(n) => {
                    if n == 0 {
                        break;
                    }
                    read_any = true;
                    let recv_time = Instant::now();
                    let pt = get_packet_type(&buf[..n]);
                    if let Some(pt) = pt {
                        // registration first
                        if let Some(event) = reg.process_registration_packet(conn_idx, &buf[..n]) {
                            match event {
                                RegistrationEvent::RegNgp => {
                                    reg.try_send_reg1_immediately(conn_idx, self).await;
                                }
                                RegistrationEvent::Reg3 => {
                                    self.connected = true;
                                    self.last_received = Some(recv_time);
                                    if self.connection_established_ms == 0 {
                                        self.connection_established_ms = crate::utils::now_ms();
                                    }
                                    self.mark_reconnect_success();
                                }
                                RegistrationEvent::RegErr => {
                                    self.connected = false;
                                    self.last_received = None;
                                }
                                RegistrationEvent::Reg2 => {
                                    // No state updates required for intermediate registration packets
                                }
                            }
                            continue;
                        }
                        self.last_received = Some(recv_time);
                        // Protocol handling
                        if pt == SRT_TYPE_ACK {
                            if let Some(ack) = parse_srt_ack(&buf[..n]) {
                                acks.push(ack);
                            }
                            // Create packet once and share it
                            let ack_packet = buf[..n].to_vec();
                            // Instantly forward ACKs for minimal RTT
                            let _ = instant_forwarder.send(ack_packet.clone());
                            // Also add to regular batch for compatibility
                            forward.push(ack_packet);
                        } else if pt == SRT_TYPE_NAK {
                            let nak_list = parse_srt_nak(&buf[..n]);
                            if !nak_list.is_empty() {
                                info!(
                                    "📦 NAK received from {}: {} sequences",
                                    self.label,
                                    nak_list.len()
                                );
                                for seq in nak_list {
                                    naks.push(seq);
                                }
                            }
                            // Forward NAKs to SRT client (like C implementation does)
                            forward.push(buf[..n].to_vec());
                        } else if pt == SRTLA_TYPE_ACK {
                            // Collect SRTLA ACK packets for global processing
                            let ack_list = parse_srtla_ack(&buf[..n]);
                            if !ack_list.is_empty() {
                                debug!(
                                    "🎯 SRTLA ACK received from {}: {} sequences",
                                    self.label,
                                    ack_list.len()
                                );
                                for seq in ack_list {
                                    srtla_acks.push(seq);
                                }
                            }
                            // Don't forward SRTLA ACKs to SRT client
                        } else if pt == SRTLA_TYPE_KEEPALIVE {
                            // handle keepalive as RTT response
                            self.handle_keepalive_response(&buf[..n]);
                        } else {
                            // Forward other SRT packets (handshake, shutdown, data, other control)
                            // If control bit (0x8000) is set or data/handshake/shutdown, forward
                            forward.push(buf[..n].to_vec());
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    warn!("read uplink error: {}", e);
                    break;
                }
            }
        }
        Ok(SrtlaIncoming {
            forward_to_client: forward,
            ack_numbers: acks,
            nak_numbers: naks,
            srtla_ack_numbers: srtla_acks,
            read_any,
        })
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

        for i in 0..ACK_SEARCH_LIMIT {
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
                self.update_rtt_estimate(rtt);
            }
        }
    }

    pub fn handle_nak(&mut self, seq: i32) {
        let mut found = false;
        for i in 0..NAK_SEARCH_LIMIT {
            let idx = (self.packet_idx + PKT_LOG_SIZE - 1 - i) % PKT_LOG_SIZE;
            if self.packet_log[idx] == seq {
                self.packet_log[idx] = -1;
                found = true;
                break;
            }
        }

        if !found {
            debug!("{}: NAK seq {} not found in packet log", self.label, seq);
        }

        // Always track NAK statistics for monitoring, regardless of whether packet was
        // found in our log (could be attributed to wrong connection or very old)
        self.nak_count = self.nak_count.saturating_add(1);
        let current_time = now_ms();

        if current_time.saturating_sub(self.last_nak_time_ms) < NAK_BURST_WINDOW_MS {
            if self.nak_burst_count == 0 {
                self.nak_burst_start_time_ms = self.last_nak_time_ms;
            }
            self.nak_burst_count = self.nak_burst_count.saturating_add(1);
        } else {
            if self.nak_burst_count >= NAK_BURST_LOG_THRESHOLD {
                let burst_duration = if self.nak_burst_start_time_ms > 0 {
                    current_time.saturating_sub(self.nak_burst_start_time_ms)
                } else {
                    (self.nak_burst_count as u64) * 100
                };
                warn!(
                    "{}: NAK burst ended - {} NAKs in {}ms",
                    self.label, self.nak_burst_count, burst_duration
                );
            }
            self.nak_burst_count = 1;
            self.nak_burst_start_time_ms = current_time;
        }

        self.last_nak_time_ms = current_time;
        self.consecutive_acks_without_nak = 0;

        // Only reduce window if we actually sent this packet (matches Belabox/Moblin
        // behavior). This prevents unfair window reduction from:
        // - NAKs for packets sent on other connections (before sequence attribution)
        // - NAKs for very old packets outside our search window
        // - Spurious/misattributed NAKs from network issues
        if !found {
            return;
        }

        let old_window = self.window;
        self.window = (self.window - WINDOW_DECR).max(WINDOW_MIN * WINDOW_MULT);

        if self.window <= 3000 {
            let burst_info = if self.nak_burst_count > 1 {
                format!(" [BURST: {} NAKs]", self.nak_burst_count)
            } else {
                String::new()
            };
            warn!(
                "{}: NAK reduced window {} → {} (seq={}, total_naks={}{})",
                self.label, old_window, self.window, seq, self.nak_count, burst_info
            );
        }

        if self.window <= 2000 && !self.fast_recovery_mode {
            self.fast_recovery_mode = true;
            self.fast_recovery_start_ms = current_time;
            warn!(
                "{}: Enabling FAST RECOVERY MODE - window {}",
                self.label, self.window
            );
        }
    }

    pub fn handle_srtla_ack_specific(&mut self, seq: i32, classic_mode: bool) -> bool {
        // Find the specific packet in the log and handle targeted logic
        // This mimics the first phase of the original implementation's
        // register_srtla_ack
        let mut found = false;

        // Search backwards through the packet log (most recent first)
        for i in 0..ACK_SEARCH_LIMIT {
            let idx = (self.packet_idx + PKT_LOG_SIZE - 1 - i) % PKT_LOG_SIZE;
            if self.packet_log[idx] == seq {
                found = true;
                if self.in_flight_packets > 0 {
                    self.in_flight_packets -= 1;
                }
                self.packet_log[idx] = -1;

                if classic_mode {
                    // CLASSIC MODE: Exact C implementation
                    // Window increase logic from C version (lines 291-293)
                    // Only increase if in_flight_pkts*WINDOW_MULT > window
                    if self.in_flight_packets * WINDOW_MULT > self.window {
                        let old = self.window;
                        // Note: WINDOW_INCR - 1 in C code
                        self.window = min(self.window + WINDOW_INCR - 1, WINDOW_MAX * WINDOW_MULT);
                        debug!(
                            "{}: SRTLA ACK specific increased window {} → {} (seq={}, \
                             in_flight={}) [CLASSIC]",
                            self.label, old, self.window, seq, self.in_flight_packets
                        );
                    }
                } else {
                    // ENHANCED MODE: Sophisticated ACK handling with utilization thresholds
                    self.handle_srtla_ack_enhanced(seq);
                }
                break;
            }
        }

        if !found {
            debug!("{}: ACK seq {} not found in packet log", self.label, seq);
        }

        found
    }

    fn handle_srtla_ack_enhanced(&mut self, _seq: i32) {
        // Enhanced mode ACK handling with utilization thresholds and consecutive ACK
        // tracking
        let old_window = self.window;
        let mut window_increased = false;
        let current_time = now_ms();

        // Only increase window if we haven't increased recently (slower recovery)
        if current_time.saturating_sub(self.last_window_increase_ms) > 200 {
            // Utilization thresholds for enhanced mode
            let utilization_threshold = if self.fast_recovery_mode {
                FAST_UTILIZATION_THRESHOLD
            } else {
                NORMAL_UTILIZATION_THRESHOLD
            };

            if (self.in_flight_packets as f64)
                < (self.window as f64 * utilization_threshold / WINDOW_MULT as f64)
            {
                self.consecutive_acks_without_nak =
                    self.consecutive_acks_without_nak.saturating_add(1);

                // Conservative recovery - require more ACKs
                let acks_required = if self.fast_recovery_mode {
                    FAST_ACKS_REQUIRED
                } else {
                    NORMAL_ACKS_REQUIRED
                };

                if self.consecutive_acks_without_nak >= acks_required {
                    self.window = min(self.window + WINDOW_INCR, WINDOW_MAX * WINDOW_MULT);
                    window_increased = true;
                    self.last_window_increase_ms = current_time;
                    self.consecutive_acks_without_nak = 0; // Reset counter to prevent burst increases
                }
            }
        }

        // Log window recovery for diagnosis
        if window_increased && old_window <= 10000 {
            debug!(
                "{}: ACK increased window {} → {} (in_flight={}, consec_acks={}, fast_mode={}) \
                 [ENHANCED]",
                self.label,
                old_window,
                self.window,
                self.in_flight_packets,
                self.consecutive_acks_without_nak,
                self.fast_recovery_mode
            );
        }

        if self.fast_recovery_mode && self.window >= FAST_RECOVERY_DISABLE_WINDOW {
            self.fast_recovery_mode = false;
            let recovery_duration = current_time.saturating_sub(self.fast_recovery_start_ms);
            debug!(
                "{}: Disabling FAST RECOVERY MODE after enhanced ACK recovery (window={}, \
                 duration={}ms)",
                self.label, self.window, recovery_duration
            );
        }
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

    fn update_rtt_estimate(&mut self, rtt_ms: u64) {
        let current_rtt = rtt_ms as f64;

        // Initialize on first measurement
        if self.smooth_rtt_ms == 0.0 {
            self.smooth_rtt_ms = current_rtt;
            self.fast_rtt_ms = current_rtt;
            self.prev_rtt_ms = current_rtt;
            self.estimated_rtt_ms = current_rtt;
            self.last_rtt_measurement_ms = now_ms();
            return;
        }

        // Asymmetric smoothing for smooth RTT: fast decrease, slow increase
        if self.smooth_rtt_ms > current_rtt {
            self.smooth_rtt_ms = self.smooth_rtt_ms * 0.60 + current_rtt * 0.40;
        } else {
            self.smooth_rtt_ms = self.smooth_rtt_ms * 0.96 + current_rtt * 0.04;
        }

        // Asymmetric smoothing for fast RTT: catches spikes quickly
        if self.fast_rtt_ms > current_rtt {
            self.fast_rtt_ms = self.fast_rtt_ms * 0.70 + current_rtt * 0.30;
        } else {
            self.fast_rtt_ms = self.fast_rtt_ms * 0.90 + current_rtt * 0.10;
        }

        // Track RTT change rate
        let delta_rtt = current_rtt - self.prev_rtt_ms;
        self.rtt_avg_delta_ms = self.rtt_avg_delta_ms * 0.8 + delta_rtt * 0.2;
        self.prev_rtt_ms = current_rtt;

        // Track minimum RTT with slow decay, only update when stable
        self.rtt_min_ms *= 1.001;
        if current_rtt < self.rtt_min_ms && self.rtt_avg_delta_ms.abs() < 1.0 {
            self.rtt_min_ms = current_rtt;
        }

        // Track peak deviation with exponential decay
        self.rtt_jitter_ms *= 0.99;
        if delta_rtt.abs() > self.rtt_jitter_ms {
            self.rtt_jitter_ms = delta_rtt.abs();
        }

        // Update legacy field for backwards compatibility
        self.estimated_rtt_ms = self.smooth_rtt_ms;
        self.last_rtt_measurement_ms = now_ms();

        // Log significant RTT changes for debugging
        if delta_rtt.abs() > 20.0 || self.rtt_jitter_ms > 50.0 {
            debug!(
                "{}: RTT update - raw={:.1}ms, smooth={:.1}ms, fast={:.1}ms, jitter={:.1}ms, \
                 delta={:.1}ms, stable={}",
                self.label,
                current_rtt,
                self.smooth_rtt_ms,
                self.fast_rtt_ms,
                self.rtt_jitter_ms,
                self.rtt_avg_delta_ms,
                self.is_rtt_stable()
            );
        }
    }

    pub fn is_rtt_stable(&self) -> bool {
        self.rtt_avg_delta_ms.abs() < 1.0
    }

    pub fn get_smooth_rtt_ms(&self) -> f64 {
        self.smooth_rtt_ms
    }

    pub fn get_fast_rtt_ms(&self) -> f64 {
        self.fast_rtt_ms
    }

    pub fn get_rtt_jitter_ms(&self) -> f64 {
        self.rtt_jitter_ms
    }

    #[allow(dead_code)]
    pub fn get_rtt_min_ms(&self) -> f64 {
        self.rtt_min_ms
    }

    #[allow(dead_code)]
    pub fn get_rtt_avg_delta_ms(&self) -> f64 {
        self.rtt_avg_delta_ms
    }

    pub fn needs_rtt_measurement(&self) -> bool {
        // Stay lightweight during initial registration: defer RTT probing until the
        // connection has been fully established via REG3. This mirrors the C
        // implementation, which does not measure RTT until a link is active, and
        // avoids spamming keepalives while the uplink is still handshaking.
        if self.connection_established_ms == 0 {
            return false;
        }

        self.connected
            && !self.waiting_for_keepalive_response
            && (self.last_rtt_measurement_ms == 0
                || now_ms().saturating_sub(self.last_rtt_measurement_ms) > 3000)
    }

    fn record_keepalive_sent(&mut self) {
        self.last_keepalive_sent_ms = now_ms();
        self.waiting_for_keepalive_response = true;
    }

    fn handle_keepalive_response(&mut self, data: &[u8]) {
        if !self.waiting_for_keepalive_response {
            return;
        }
        if let Some(ts) = extract_keepalive_timestamp(data) {
            let now = now_ms();
            let rtt = now.saturating_sub(ts);
            if rtt <= 10_000 {
                self.update_rtt_estimate(rtt);
                info!(
                    "{}: RTT from keepalive: {}ms (smooth: {:.1}ms, fast: {:.1}ms, jitter: \
                     {:.1}ms)",
                    self.label, rtt, self.smooth_rtt_ms, self.fast_rtt_ms, self.rtt_jitter_ms
                );
            }
        }
        self.waiting_for_keepalive_response = false;
    }

    pub fn needs_keepalive(&self) -> bool {
        let now = now_ms();
        self.connected
            && (self.last_keepalive_ms == 0
                || now.saturating_sub(self.last_keepalive_ms) >= IDLE_TIME * 1000)
    }

    pub fn perform_window_recovery(&mut self) {
        if !self.connected || self.window >= WINDOW_MAX * WINDOW_MULT {
            return;
        }

        let now = now_ms();
        let time_since_last_nak = if self.last_nak_time_ms > 0 {
            now.saturating_sub(self.last_nak_time_ms)
        } else {
            u64::MAX
        };

        let min_wait_time = if self.fast_recovery_mode {
            FAST_MIN_WAIT_MS
        } else {
            NORMAL_MIN_WAIT_MS
        };
        let increment_wait = if self.fast_recovery_mode {
            FAST_INCREMENT_WAIT_MS
        } else {
            NORMAL_INCREMENT_WAIT_MS
        };

        if time_since_last_nak > min_wait_time
            && now.saturating_sub(self.last_window_increase_ms) > increment_wait
        {
            let old_window = self.window;
            // Conservative recovery multipliers (using cached values)
            let fast_mode_bonus = if self.fast_recovery_mode { 2 } else { 1 };

            // More conservative recovery based on how long since last NAK
            if time_since_last_nak > 10_000 {
                // No NAKs for 10+ seconds: moderate recovery (was 5s)
                self.window += WINDOW_INCR * 2 * fast_mode_bonus;
            } else if time_since_last_nak > 7_000 {
                // No NAKs for 7+ seconds: slow recovery (was 3s)
                self.window += WINDOW_INCR * fast_mode_bonus;
            } else if time_since_last_nak > 5_000 {
                // No NAKs for 5+ seconds: very slow recovery (was 1.5s)
                self.window += WINDOW_INCR * fast_mode_bonus;
            } else {
                // Recent NAKs: minimal recovery (keep same as before)
                self.window += WINDOW_INCR * fast_mode_bonus;
            }

            self.window = min(self.window, WINDOW_MAX * WINDOW_MULT);
            self.last_window_increase_ms = now;

            if self.window > old_window {
                debug!(
                    "{}: Time-based window recovery {} → {} (no NAKs for {:.1}s, fast_mode={})",
                    self.label,
                    old_window,
                    self.window,
                    (time_since_last_nak as f64) / 1000.0,
                    self.fast_recovery_mode
                );
            }

            if self.fast_recovery_mode && self.window >= FAST_RECOVERY_DISABLE_WINDOW {
                self.fast_recovery_mode = false;
                debug!(
                    "{}: Disabling FAST RECOVERY MODE after time-based recovery (window={})",
                    self.label, self.window
                );
            }
        }
    }

    pub fn is_timed_out(&self) -> bool {
        !self.connected
            || if let Some(lr) = self.last_received {
                lr.elapsed().as_secs() >= CONN_TIMEOUT
            } else {
                true
            }
    }

    /// Mark connection for recovery (C-style), similar to setting last_rcvd = 1
    pub fn mark_for_recovery(&mut self) {
        self.last_received = None;
        self.last_keepalive_ms = 0;
        self.last_keepalive_sent_ms = 0;
        self.waiting_for_keepalive_response = false;
        self.connected = false;
        // Reset connection state like C version does
        // Reset to default window like classic implementation so reconnecting
        // links can ramp quickly once registration completes.
        self.window = WINDOW_DEF * WINDOW_MULT;
        self.in_flight_packets = 0;
        self.packet_log = [-1; PKT_LOG_SIZE];
    }

    pub fn time_since_last_nak_ms(&self) -> Option<u64> {
        if self.last_nak_time_ms == 0 {
            None
        } else {
            Some(now_ms().saturating_sub(self.last_nak_time_ms))
        }
    }

    pub fn total_nak_count(&self) -> i32 {
        self.nak_count
    }

    pub fn nak_burst_count(&self) -> i32 {
        self.nak_burst_count
    }

    pub fn connection_established_ms(&self) -> u64 {
        self.connection_established_ms
    }

    pub fn should_attempt_reconnect(&self) -> bool {
        const BASE_RECONNECT_DELAY_MS: u64 = 5000;
        const MAX_BACKOFF_DELAY_MS: u64 = 120_000;
        const MAX_BACKOFF_COUNT: u32 = 5;

        let now = now_ms();

        if self.connection_established_ms == 0 {
            // Match the C implementation during initial registration by retrying
            // roughly once per housekeeping pass (~1s cadence).
            if self.last_reconnect_attempt_ms == 0 {
                return true;
            }
            return now.saturating_sub(self.last_reconnect_attempt_ms) >= 1000;
        }

        if self.last_reconnect_attempt_ms == 0 {
            return true;
        }

        let time_since_last_attempt = now.saturating_sub(self.last_reconnect_attempt_ms);
        let current_backoff = self.reconnect_failure_count.min(MAX_BACKOFF_COUNT);
        let min_interval = BASE_RECONNECT_DELAY_MS.saturating_mul(1u64 << current_backoff);
        let backoff_delay = min_interval.min(MAX_BACKOFF_DELAY_MS);

        time_since_last_attempt >= backoff_delay
    }

    pub fn record_reconnect_attempt(&mut self) {
        self.last_reconnect_attempt_ms = now_ms();

        // For initial registration we keep retry cadence fast and skip backoff
        if self.connection_established_ms == 0 {
            debug!(
                "{}: Initial registration retry scheduled (next attempt in ~1s)",
                self.label
            );
            return;
        }

        self.reconnect_failure_count = self.reconnect_failure_count.saturating_add(1);

        const BASE_RECONNECT_DELAY_MS: u64 = 5000;
        const MAX_BACKOFF_DELAY_MS: u64 = 120_000;
        const MAX_BACKOFF_COUNT: u32 = 5;

        let current_backoff = self.reconnect_failure_count.min(MAX_BACKOFF_COUNT);
        let min_interval = BASE_RECONNECT_DELAY_MS.saturating_mul(1u64 << current_backoff);
        let next_delay = min_interval.min(MAX_BACKOFF_DELAY_MS);

        info!(
            "{}: Reconnect attempt #{}, next attempt in {}s",
            self.label,
            self.reconnect_failure_count,
            next_delay / 1000
        );
    }

    pub fn mark_reconnect_success(&mut self) {
        if self.reconnect_failure_count > 0 {
            info!("{}: Reconnection successful, resetting backoff", self.label);
            self.reconnect_failure_count = 0;
        }
    }

    pub async fn reconnect(&mut self) -> Result<()> {
        let sock = bind_from_ip(self.local_ip, 0)?;
        sock.connect(&self.remote.into())?;
        let std_sock: std::net::UdpSocket = sock.into();
        std_sock.set_nonblocking(true)?;
        let socket = UdpSocket::from_std(std_sock)?;
        self.socket = socket;
        self.connected = false;
        self.last_received = None;
        self.window = WINDOW_DEF * WINDOW_MULT;
        self.in_flight_packets = 0;
        self.packet_log = [-1; PKT_LOG_SIZE];
        self.packet_idx = 0;
        self.nak_count = 0;
        self.last_nak_time_ms = 0;
        self.last_window_increase_ms = 0;
        self.consecutive_acks_without_nak = 0;
        self.fast_recovery_mode = false;
        self.last_rtt_measurement_ms = 0;
        self.smooth_rtt_ms = 0.0;
        self.fast_rtt_ms = 0.0;
        self.rtt_jitter_ms = 0.0;
        self.prev_rtt_ms = 0.0;
        self.rtt_avg_delta_ms = 0.0;
        self.rtt_min_ms = 200.0;
        self.estimated_rtt_ms = 0.0;
        self.last_keepalive_ms = 0;
        self.last_keepalive_sent_ms = 0;
        self.waiting_for_keepalive_response = false;
        self.packet_send_times_ms = [0; PKT_LOG_SIZE];
        self.fast_recovery_start_ms = 0;
        self.last_reconnect_attempt_ms = now_ms();
        self.reconnect_failure_count = 0;
        // Don't reset connection_established_ms for reconnections - only set when REG3
        // is received
        self.mark_reconnect_success();
        Ok(())
    }
}

fn bind_from_ip(ip: IpAddr, port: u16) -> Result<Socket> {
    let domain = match ip {
        IpAddr::V4(_) => Domain::IPV4,
        IpAddr::V6(_) => Domain::IPV6,
    };
    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)).context("create socket")?;
    sock.set_nonblocking(true).context("set nonblocking")?;

    // Set send buffer size (100MB)
    const SEND_BUF_SIZE: usize = 100 * 1024 * 1024;
    if let Err(e) = sock.set_send_buffer_size(SEND_BUF_SIZE) {
        warn!("Failed to set send buffer size to {}: {}", SEND_BUF_SIZE, e);
        if let Ok(actual_size) = sock.send_buffer_size() {
            warn!("Effective send buffer size: {}", actual_size);
        }
    }

    // Set receive buffer size to handle large SRT packets (100MB)
    const RECV_BUF_SIZE: usize = 100 * 1024 * 1024;
    if let Err(e) = sock.set_recv_buffer_size(RECV_BUF_SIZE) {
        warn!(
            "Failed to set receive buffer size to {}: {}",
            RECV_BUF_SIZE, e
        );
        if let Ok(actual_size) = sock.recv_buffer_size() {
            warn!("Effective receive buffer size: {}", actual_size);
        }
    }

    let addr = SocketAddr::new(ip, port);
    sock.bind(&addr.into()).context("bind socket")?;
    Ok(sock)
}

async fn resolve_remote(host: &str, port: u16) -> Result<SocketAddr> {
    let mut addrs = tokio::net::lookup_host((host, port))
        .await
        .context("dns lookup")?;
    addrs
        .next()
        .ok_or_else(|| anyhow::anyhow!("no DNS result for {}", host))
}
