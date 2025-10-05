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
    /// Creates a new SrtlaConnection by binding a UDP socket to the given local IP and connecting it to the resolved remote host:port.
    ///
    /// The returned connection is initialized with default congestion and RTT state, a bound and connected UDP socket, and `connected` set to `false`. The connection's `connection_established_ms` remains zero until the REG3 handshake is completed.
    ///
    /// # Parameters
    ///
    /// - `ip`: local IP address to bind the UDP socket to.
    /// - `host`: remote hostname (DNS will be resolved).
    /// - `port`: remote UDP port.
    ///
    /// # Returns
    ///
    /// An initialized `SrtlaConnection` with a UDP socket bound to `ip` and connected to the resolved remote address.
    ///
    /// # Examples
    ///
    /// ```
    /// # tokio::runtime::Runtime::new().unwrap().block_on(async {
    /// use std::net::IpAddr;
    /// // Replace with a reachable host/ip when actually running the example.
    /// let local_ip: IpAddr = "0.0.0.0".parse().unwrap();
    /// let conn = SrtlaConnection::connect_from_ip(local_ip, "example.com", 9000).await.unwrap();
    /// assert!(!conn.connected);
    /// # });
    /// ```
    pub async fn connect_from_ip(ip: IpAddr, host: &str, port: u16) -> Result<Self> {
        let remote = resolve_remote(host, port).await?;
        let sock = bind_from_ip(ip, 0)?;
        sock.connect(&remote.into())?;
        let std_sock: std::net::UdpSocket = sock.into();
        std_sock.set_nonblocking(true)?;
        let socket = UdpSocket::from_std(std_sock)?;
        Ok(Self {
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

    /// Computes a priority score for the connection based on congestion window and current load.
    ///
    /// The score is the integer division of the connection window by the in-flight packet load (with the
    /// load treated as at least 1). If the connection is not established, returns -1.
    ///
    /// # Returns
    ///
    /// The computed score as `window / max(in_flight_packets + 1, 1)`, or `-1` when the connection is not established.
    ///
    /// # Examples
    ///
    /// ```
    /// // Equivalent calculation used by `get_score`.
    /// let window = 100;
    /// let in_flight = 4;
    /// let denom = in_flight.saturating_add(1).max(1);
    /// let score = window / denom;
    /// assert_eq!(score, 20);
    /// ```
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

    /// Send the provided bytes to the remote endpoint and, if a sequence number is given, record the packet for tracking.
    ///
    /// When `seq` is provided, the sequence number is registered so the connection can track in-flight packets and measure RTTs.
    ///
    /// # Parameters
    ///
    /// - `data`: bytes to send to the remote.
    /// - `seq`: optional packet sequence number to register for tracking; if `None`, no tracking entry is recorded.
    ///
    /// # Returns
    ///
    /// `Ok(())` on success, `Err(...)` if the underlying socket send fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(conn: &mut crate::SrtlaConnection) -> Result<(), Box<dyn std::error::Error>> {
    /// conn.send_data_with_tracking(b"payload", Some(42)).await?;
    /// conn.send_data_with_tracking(b"fire-and-forget", None).await?;
    /// # Ok(())
    /// # }
    /// ```
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

    /// Drain the connection's UDP socket and collect incoming SRT/SRTLA packets and control events.
    ///
    /// Processes all currently available packets from the socket, updating registration and
    /// connection state, handling keepalive responses, extracting ACK/NAK/SRTLA-ACK sequence
    /// numbers, and batching packets that should be forwarded to the local SRT client.
    /// ACK packets are also forwarded immediately via the provided `instant_forwarder`.
    ///
    /// # Parameters
    ///
    /// - `conn_idx`: Index identifying this connection within the registration manager; used when
    ///   processing registration events.
    ///
    /// # Returns
    ///
    /// A `SrtlaIncoming` struct containing:
    /// - `forward_to_client`: packets to forward to the SRT client,
    /// - `ack_numbers`: extracted SRT ACK sequence numbers,
    /// - `nak_numbers`: extracted SRT NAK sequence numbers,
    /// - `srtla_ack_numbers`: extracted SRTLA ACK sequence numbers,
    /// - `read_any`: `true` if at least one packet was read, `false` otherwise.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(conn: &mut crate::connection::SrtlaConnection,
    /// #                  idx: usize,
    /// #                  reg: &mut crate::registration::SrtlaRegistrationManager,
    /// #                  sender: &std::sync::mpsc::Sender<Vec<u8>>) -> Result<(), Box<dyn std::error::Error>> {
    /// let incoming = conn.drain_incoming(idx, reg, sender).await?;
    /// if incoming.read_any {
    ///     // process incoming.forward_to_client, incoming.ack_numbers, etc.
    /// }
    /// # Ok(()) }
    /// ```
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

    /// Process an SRT ACK sequence number and update connection state.
    ///
    /// Marks the matching sent packet as acknowledged, clears any sent-packet
    /// records with sequence numbers older than the acknowledged sequence,
    /// updates the in-flight packet count, and, if the original send timestamp
    /// is available and the measured RTT is within 1–10,000 ms, updates the RTT estimate.
    ///
    /// # Parameters
    ///
    /// - `ack`: ACK sequence number to apply to the send log.
    ///
    /// # Examples
    ///
    /// ```
    /// // Given a mutable SrtlaConnection `conn` that has previously registered
    /// // sent packets, report receipt of ACK 42:
    /// conn.handle_srt_ack(42);
    /// ```
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

    /// Handle a received SRT NAK (negative-acknowledgement) for a sent packet.
    ///
    /// Updates internal packet tracking, NAK counters and burst detection, and
    /// reduces the congestion window as a reaction to the reported lost packet.
    /// If the window falls below a fast-recovery threshold, enables fast recovery mode.
    ///
    /// # Parameters
    ///
    /// - `seq`: the sequence number reported as lost by the remote peer.
    ///
    /// # Examples
    ///
    /// ```
    /// // Obtain a mutable SrtlaConnection from your connection manager or constructor,
    /// // then notify it of a missing packet:
    /// // let mut conn = SrtlaConnection::connect_from_ip(...).await.unwrap();
    /// // conn.handle_nak(42);
    /// ```
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

    /// Handle a received SRTLA ACK for a specific packet sequence and adjust congestion state.
    ///
    /// Searches the recent packet log for `seq`. If found, marks the packet as acknowledged,
    /// decrements the in-flight packet count, and applies window increase behavior.
    /// When `classic_mode` is `true` the legacy (C-style) window increment is applied;
    /// otherwise the enhanced ACK handling path is invoked.
    ///
    /// # Parameters
    ///
    /// - `seq`: The sequence number to acknowledge.
    /// - `classic_mode`: If `true`, apply legacy window-increase logic; if `false`, use enhanced handling.
    ///
    /// # Returns
    ///
    /// `true` if the sequence was found in the packet log and processed, `false` otherwise.
    ///
    /// # Examples
    ///
    /// ```
    /// // Assuming `conn` is a mutable SrtlaConnection with a recent packet log:
    /// // let mut conn = ...;
    /// // let handled = conn.handle_srtla_ack_specific(42, false);
    /// // assert!(handled || !handled); // demonstrates calling the method
    /// ```
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
                        self.window += WINDOW_INCR - 1; // Note: WINDOW_INCR - 1 in C code
                        if self.window > WINDOW_MAX * WINDOW_MULT {
                            self.window = WINDOW_MAX * WINDOW_MULT;
                        }
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

    /// Performs enhanced ACK-driven congestion window recovery.
    ///
    /// Updates the connection's congestion window when sustained ACKs indicate available
    /// capacity. Uses utilization thresholds and a consecutive-ACKs counter to apply a
    /// conservative, time-gated window increase; respects fast-recovery mode and caps the
    /// window to configured maximums. Resets the consecutive-ACKs counter after a
    /// successful increase and disables fast-recovery mode if the window grows beyond the
    /// fast-recovery disable threshold.
    ///
    /// The `_seq` parameter is unused and present for API symmetry with other ACK handlers.
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
                    self.window += WINDOW_INCR;
                    if self.window > WINDOW_MAX * WINDOW_MULT {
                        self.window = WINDOW_MAX * WINDOW_MULT;
                    }
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

    /// Increases the congestion window by one when the connection has recently received data.
    ///
    /// If the connection is marked as connected and a receipt timestamp exists, the window
    /// is incremented by 1 and clamped to WINDOW_MAX * WINDOW_MULT. Large single-step
    /// increases are logged for visibility.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Given a connected SrtlaConnection that has received data:
    /// // let mut conn = SrtlaConnection::new(...);
    /// // conn.connected = true;
    /// // conn.last_received = Some(std::time::Instant::now());
    /// // let old = conn.window;
    /// // conn.handle_srtla_ack_global();
    /// // assert!(conn.window >= old);
    /// ```
    pub fn handle_srtla_ack_global(&mut self) {
        // Global +1 window increase for connections that have received data (from
        // original implementation)
        // This matches C version: if (c->last_rcvd != 0)
        // In Rust, we check if last_received is Some (i.e., has been set when data was
        // received)
        if self.connected && self.last_received.is_some() {
            let old = self.window;
            self.window += 1;
            if self.window > WINDOW_MAX * WINDOW_MULT {
                self.window = WINDOW_MAX * WINDOW_MULT;
            }
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

    /// Update the connection's RTT statistics using a new round-trip time measurement.
    ///
    /// This updates internal RTT estimates (smooth, fast, min, jitter, average delta,
    /// and legacy estimated RTT) and records the time of the measurement. May emit
    /// a debug log when the measured change or jitter is large.
    ///
    /// # Parameters
    ///
    /// - `rtt_ms`: Measured round-trip time in milliseconds.
    ///
    /// # Examples
    ///
    /// ```
    /// // Assume `conn` is a mutable SrtlaConnection obtained from your connection manager.
    /// // A new RTT measurement (in milliseconds) can be applied like this:
    /// // conn.update_rtt_estimate(120);
    /// ```
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

    /// Reports whether recent RTT measurements are stable.
    ///
    /// The RTT is considered stable when the absolute average RTT delta is less than 1.0 ms.
    ///
    /// # Examples
    ///
    /// ```
    /// // Assuming `conn` is an initialized `SrtlaConnection`:
    /// // conn.rtt_avg_delta_ms = 0.5;
    /// // assert!(conn.is_rtt_stable());
    /// ```
    pub fn is_rtt_stable(&self) -> bool {
        self.rtt_avg_delta_ms.abs() < 1.0
    }

    /// Returns the current smoothed round-trip time estimate in milliseconds.
    ///
    /// # Returns
    ///
    /// The smoothed RTT estimate in milliseconds.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Given a `SrtlaConnection` instance `conn`, read the smoothed RTT:
    /// let rtt_ms = conn.get_smooth_rtt_ms();
    /// assert!(rtt_ms >= 0.0);
    /// ```
    pub fn get_smooth_rtt_ms(&self) -> f64 {
        self.smooth_rtt_ms
    }

    /// Access the connection's fast RTT estimate in milliseconds.
    ///
    /// # Returns
    ///
    /// The current fast RTT estimate in milliseconds.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Given a `SrtlaConnection` instance `conn`, read the fast RTT:
    /// let _fast_rtt_ms = conn.get_fast_rtt_ms();
    /// ```
    pub fn get_fast_rtt_ms(&self) -> f64 {
        self.fast_rtt_ms
    }

    /// Returns the current RTT jitter estimate in milliseconds.
    ///
    /// The value represents the exponentially-smoothed estimate of round-trip time variability.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // given a SrtlaConnection `conn`
    /// let jitter_ms = conn.get_rtt_jitter_ms();
    /// println!("RTT jitter: {} ms", jitter_ms);
    /// ```
    pub fn get_rtt_jitter_ms(&self) -> f64 {
        self.rtt_jitter_ms
    }

    /// Reports the minimum observed round-trip time (RTT) measurement.
    ///
    /// # Returns
    ///
    /// The minimum observed RTT in milliseconds.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Given an existing `SrtlaConnection` instance `conn`:
    /// // let min_rtt = conn.get_rtt_min_ms();
    /// // println!("min RTT = {} ms", min_rtt);
    /// ```
    #[allow(dead_code)]
    pub fn get_rtt_min_ms(&self) -> f64 {
        self.rtt_min_ms
    }

    /// Average change between consecutive RTT measurements, expressed in milliseconds.
    ///
    /// This value reflects how much RTT has been drifting between samples; positive values
    /// indicate RTT has been increasing on average, negative values indicate it has been
    /// decreasing.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use crate::SrtlaConnection;
    /// let conn = /* obtain SrtlaConnection */ unimplemented!();
    /// let delta_ms = conn.get_rtt_avg_delta_ms();
    /// println!("RTT delta (ms): {}", delta_ms);
    /// ```
    #[allow(dead_code)]
    pub fn get_rtt_avg_delta_ms(&self) -> f64 {
        self.rtt_avg_delta_ms
    }

    /// Determines whether an RTT (round-trip time) measurement should be performed.
    ///
    /// Returns `true` when the connection is established (REG3 completed), the connection is
    /// marked connected, there is not already a pending keepalive response, and either no prior
    /// RTT measurement exists or the last measurement was more than 3000 ms ago.
    ///
    /// # Returns
    ///
    /// `true` if an RTT measurement should be performed, `false` otherwise.
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

    /// Process a received keepalive response and update RTT state if appropriate.
    ///
    /// If the connection is currently awaiting a keepalive response, this extracts the
    /// timestamp from the provided packet payload, computes the round-trip time (RTT),
    /// and updates the connection's RTT estimate when the measured RTT is 10,000 ms or less.
    /// In all cases clears the awaiting flag so subsequent responses are ignored until a new
    /// keepalive is sent.
    ///
    /// # Parameters
    ///
    /// - `data`: The raw keepalive packet payload to extract the remote timestamp from.
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

    /// Attempt to recover the congestion window based on elapsed time since the last NAK.
    ///
    /// When the connection is established and the window is below the configured maximum,
    /// this method increases the window at controlled intervals if no NAKs have been observed
    /// for a configured cooldown. The increase amount and timing depend on whether fast
    /// recovery mode is active; the method also caps the window and may disable fast recovery
    /// once a safe threshold is reached.
    ///
    /// # Examples
    ///
    /// ```
    /// // Example usage: call on a mutable SrtlaConnection to trigger time-based recovery.
    /// // (This is a usage example; constructing a real `SrtlaConnection` requires the surrounding context.)
    /// # use crate::SrtlaConnection;
    /// # unsafe {
    /// #     // Placeholder conn for demonstration; real code should use a properly-initialized connection.
    /// #     let mut conn: SrtlaConnection = std::mem::zeroed();
    /// #     conn.connected = true;
    /// #     conn.perform_window_recovery();
    /// # }
    /// ```
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
            let fast_mode_bonus = if self.fast_recovery_mode { 2 } else { 1 };

            if time_since_last_nak > 10_000 {
                self.window += WINDOW_INCR * 2 * fast_mode_bonus;
            } else if time_since_last_nak > 7_000 {
                self.window += WINDOW_INCR * fast_mode_bonus;
            } else if time_since_last_nak > 5_000 {
                self.window += WINDOW_INCR * fast_mode_bonus;
            } else {
                self.window += WINDOW_INCR * fast_mode_bonus;
            }

            if self.window > WINDOW_MAX * WINDOW_MULT {
                self.window = WINDOW_MAX * WINDOW_MULT;
            }
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

    #[allow(dead_code)]
    pub fn is_timed_out(&self) -> bool {
        !self.connected
            || if let Some(lr) = self.last_received {
                lr.elapsed().as_secs() >= CONN_TIMEOUT
            } else {
                true
            }
    }

    /// Mark the connection as needing full recovery and reset transient state.
    ///
    /// This clears the last-received timestamp, cancels any pending keepalive RTT probe,
    /// marks the connection as not connected, resets the congestion window and in-flight
    /// packet tracking, and clears the packet log so the connection can ramp anew after
    /// a subsequent reconnect/registration.
    ///
    /// # Examples
    ///
    /// ```
    /// // Given a mutable `SrtlaConnection` named `conn`
    /// conn.mark_for_recovery();
    /// assert!(!conn.connected);
    /// assert_eq!(conn.in_flight_packets, 0);
    /// ```
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

    /// Returns the millisecond timestamp when REG3 completed and the connection became established.
    ///
    /// The value is a Unix-like millisecond timestamp indicating when the connection was marked established; `0` indicates the connection has not yet been established.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Given an existing SrtlaConnection `conn`, read the establishment timestamp:
    /// let ts_ms = conn.connection_established_ms();
    /// // `ts_ms == 0` if REG3 has not completed.
    /// ```
    pub fn connection_established_ms(&self) -> u64 {
        self.connection_established_ms
    }

    /// Determines whether a reconnect attempt should be performed now.
    ///
    /// This uses two modes:
    /// - During initial registration (before REG3), it retries roughly once per housekeeping pass (~1s).
    /// - After a successful registration, it applies exponential backoff based on the number of consecutive reconnect failures,
    ///   clamped by a maximum delay and a maximum backoff exponent.
    ///
    /// # Returns
    ///
    /// `true` if enough time has elapsed to attempt a reconnect, `false` otherwise.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Assuming `conn` is an instance of `SrtlaConnection`,
    /// // call `conn.should_attempt_reconnect()` to decide whether to try reconnecting now.
    /// // This example is illustrative and intentionally marked `no_run`.
    /// # let conn = /* SrtlaConnection instance */ panic!();
    /// let _ = conn.should_attempt_reconnect();
    /// ```
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

    /// Record that a reconnect attempt was made and update backoff bookkeeping.
    ///
    /// Updates the connection's `last_reconnect_attempt_ms`. If the connection has not
    /// completed initial registration (`connection_established_ms == 0`), the method
    /// leaves the failure counter unchanged to preserve a fast retry cadence. Otherwise
    /// it increments `reconnect_failure_count` and computes the next backoff delay
    /// (exponential with upper bounds), which is emitted via logging.
    ///
    /// # Examples
    ///
    /// ```
    /// let mut conn = SrtlaConnection::default(); // assume Default exists for example
    /// conn.record_reconnect_attempt();
    /// assert!(conn.last_reconnect_attempt_ms > 0);
    /// ```
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

    /// Re-establishes the UDP connection to the remote endpoint and reinitializes connection state.
    ///
    /// Rebinds a socket from the connection's local IP, connects it to the configured remote address,
    /// replaces the internal socket, marks the connection as established, and resets congestion-control
    /// and RTT-related state used for a fresh connection attempt. The timestamp of prior REG3
    /// establishment is preserved and not cleared by this operation.
    ///
    /// # Errors
    ///
    /// Returns an error if socket creation, binding, or configuration fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use tokio::runtime::Runtime;
    /// # fn main() {
    /// # let rt = Runtime::new().unwrap();
    /// # rt.block_on(async {
    /// // `conn` is an existing `SrtlaConnection` value.
    /// // conn.reconnect().await.unwrap();
    /// # });
    /// # }
    /// ```
    #[allow(dead_code)]
    pub async fn reconnect(&mut self) -> Result<()> {
        let sock = bind_from_ip(self.local_ip, 0)?;
        sock.connect(&self.remote.into())?;
        let std_sock: std::net::UdpSocket = sock.into();
        std_sock.set_nonblocking(true)?;
        let socket = UdpSocket::from_std(std_sock)?;
        self.socket = socket;
        self.connected = true;
        self.last_received = Some(Instant::now());
        self.window = WINDOW_MIN * WINDOW_MULT;
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