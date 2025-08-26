use std::net::{IpAddr, SocketAddr};

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
// mpsc channel for instant forwarding
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::protocol::*;
use crate::registration::SrtlaRegistrationManager;

pub struct SrtlaIncoming {
    pub forward_to_client: Vec<Vec<u8>>,
    pub ack_numbers: Vec<u32>,
    pub nak_numbers: Vec<u32>,
    pub srtla_ack_numbers: Vec<u32>,
    pub read_any: bool,
}

pub struct SrtlaConnection {
    socket: UdpSocket,
    #[allow(dead_code)]
    remote: SocketAddr,
    #[allow(dead_code)]
    pub local_ip: IpAddr,
    pub label: String,
    connected: bool,
    window: i32,
    in_flight_packets: i32,
    packet_log: [i32; PKT_LOG_SIZE],
    packet_send_times_ms: [u64; PKT_LOG_SIZE],
    packet_idx: usize,
    last_received: Instant,
    last_keepalive_ms: u64,
    // RTT measurement via keepalive
    last_keepalive_sent_ms: u64,
    waiting_for_keepalive_response: bool,
    last_rtt_measurement_ms: u64,
    estimated_rtt_ms: f64,
    // Congestion control state
    nak_count: i32,
    last_nak_time_ms: u64,
    last_window_increase_ms: u64,
    consecutive_acks_without_nak: i32,
    fast_recovery_mode: bool,
    fast_recovery_start_ms: u64,
    // Burst NAK tracking (matches Java implementation)
    nak_burst_count: i32,
    nak_burst_start_time_ms: u64,
    last_reconnect_attempt_ms: u64,
    reconnect_failure_count: u32,
}

impl SrtlaConnection {
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
            connected: true,
            window: WINDOW_DEF * WINDOW_MULT,
            in_flight_packets: 0,
            packet_log: [-1; PKT_LOG_SIZE],
            packet_send_times_ms: [0; PKT_LOG_SIZE],
            packet_idx: 0,
            last_received: Instant::now(),
            last_keepalive_ms: 0,
            last_keepalive_sent_ms: 0,
            waiting_for_keepalive_response: false,
            last_rtt_measurement_ms: 0,
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
        })
    }

    pub fn get_score(&self) -> i32 {
        if !self.connected {
            return -1;
        }
        self.window / (self.in_flight_packets + 1)
    }

    pub async fn send_data_with_tracking(&mut self, data: &[u8], seq: Option<u32>) -> Result<()> {
        self.socket.send(data).await?;
        if let Some(s) = seq {
            self.register_packet(s as i32);
        }
        Ok(())
    }

    pub async fn send_keepalive(&mut self) -> Result<()> {
        let pkt = create_keepalive_packet();
        debug!("keepalive → {} ({} bytes)", self.label, pkt.len());
        self.socket.send(&pkt).await?;
        let now = now_ms();
        self.last_keepalive_ms = now;
        // Only set waiting flag and timestamp when we intend to measure RTT
        if self.waiting_for_keepalive_response == false
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
        let mut forward: Vec<Vec<u8>> = Vec::new();
        let mut acks: Vec<u32> = Vec::new();
        let mut naks: Vec<u32> = Vec::new();
        let mut srtla_acks: Vec<u32> = Vec::new();
        loop {
            match self.socket.try_recv(&mut buf) {
                Ok(n) => {
                    if n == 0 {
                        break;
                    }
                    read_any = true;
                    self.last_received = Instant::now();
                    debug!("recv {} bytes from {}", n, self.label);
                    let pt = get_packet_type(&buf[..n]);
                    if let Some(pt) = pt {
                        // registration first
                        if reg.process_registration_packet(conn_idx, &buf[..n]) {
                            continue;
                        }
                        // Protocol handling
                        if pt == SRT_TYPE_ACK {
                            if let Some(ack) = parse_srt_ack(&buf[..n]) {
                                debug!("ACK {:?} from {}", ack, self.label);
                                acks.push(ack);
                            }
                            // Instantly forward ACKs for minimal RTT
                            let _ = instant_forwarder.send(buf[..n].to_vec());
                            // Also add to regular batch for compatibility
                            forward.push(buf[..n].to_vec());
                        } else if pt == SRT_TYPE_NAK {
                            let nak_list = parse_srt_nak(&buf[..n]);
                            if !nak_list.is_empty() {
                                info!(
                                    "📦 NAK received from {}: {} sequences",
                                    self.label,
                                    nak_list.len()
                                );
                                for seq in nak_list {
                                    debug!("NAK {:?} from {}", seq, self.label);
                                    naks.push(seq);
                                }
                            }
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
                                    debug!("SRTLA ACK {:?} from {}", seq, self.label);
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

    fn register_packet(&mut self, seq: i32) {
        let idx = self.packet_idx % PKT_LOG_SIZE;
        self.packet_log[idx] = seq;
        self.packet_send_times_ms[idx] = now_ms();
        self.packet_idx = (self.packet_idx + 1) % PKT_LOG_SIZE;
        self.in_flight_packets += 1;
    }

    pub fn handle_srt_ack(&mut self, ack: i32) {
        let mut count = 0;
        let mut ack_send_time_ms: Option<u64> = None;
        for i in 0..PKT_LOG_SIZE {
            let idx = (self.packet_idx + PKT_LOG_SIZE - 1 - i) % PKT_LOG_SIZE;
            let val = self.packet_log[idx];
            if val < ack {
                self.packet_log[idx] = -1;
            } else if val > 0 {
                count += 1;
            }
            if val == ack {
                let t = self.packet_send_times_ms[idx];
                if t != 0 {
                    ack_send_time_ms = Some(t);
                }
            }
        }
        self.in_flight_packets = count;

        // RTT from SRT ACK if we have a send timestamp
        if let Some(sent_ms) = ack_send_time_ms {
            let now = now_ms();
            let rtt = now.saturating_sub(sent_ms);
            if rtt > 0 && rtt <= 10_000 {
                self.estimated_rtt_ms = if self.estimated_rtt_ms == 0.0 {
                    rtt as f64
                } else {
                    (self.estimated_rtt_ms * 0.875) + (rtt as f64 * 0.125)
                };
                self.last_rtt_measurement_ms = now;
                // Use info level for SRT ACK RTT since this is the authoritative measurement
                info!(
                    "{}: RTT (SRT ACK) measured: {} ms (avg {:.1} ms)",
                    self.label, rtt, self.estimated_rtt_ms
                );
            }
        }

        // Window recovery with conservative parameters (aligned with Java defaults)
        let now = now_ms();
        if now.saturating_sub(self.last_window_increase_ms) > 200 {
            let utilization_threshold: f64 = if self.fast_recovery_mode { 0.95 } else { 0.85 };
            if (self.in_flight_packets as f64)
                < (self.window as f64 * utilization_threshold / WINDOW_MULT as f64)
            {
                self.consecutive_acks_without_nak += 1;
                let acks_required = if self.fast_recovery_mode { 2 } else { 4 };
                if self.consecutive_acks_without_nak >= acks_required {
                    let old = self.window;
                    self.window += WINDOW_INCR;
                    if self.window > WINDOW_MAX * WINDOW_MULT {
                        self.window = WINDOW_MAX * WINDOW_MULT;
                    }
                    self.last_window_increase_ms = now;
                    self.consecutive_acks_without_nak = 0;
                    if old <= 10_000 {
                        info!(
                            "{}: ACK increased window {} → {}",
                            self.label, old, self.window
                        );
                    }
                }
            }
        }
        if self.fast_recovery_mode && self.window >= 12_000 {
            self.fast_recovery_mode = false;
            info!(
                "{}: Disabling FAST RECOVERY MODE - window {}",
                self.label, self.window
            );
        }
    }

    pub fn handle_nak(&mut self, seq: i32) {
        for i in 0..PKT_LOG_SIZE.min(64) {
            let idx = (self.packet_idx + PKT_LOG_SIZE - 1 - i) % PKT_LOG_SIZE;
            if self.packet_log[idx] == seq {
                self.packet_log[idx] = -1;
                break;
            }
        }

        // Track NAK statistics and bursts (matches Java implementation)
        self.nak_count += 1;
        let current_time = now_ms();

        // Detect NAK bursts (multiple NAKs within 1 second)
        if current_time.saturating_sub(self.last_nak_time_ms) < 1000 {
            if self.nak_burst_count == 0 {
                self.nak_burst_start_time_ms = self.last_nak_time_ms; // Start of burst
            }
            self.nak_burst_count += 1;
        } else {
            // End of burst - log if it was significant
            if self.nak_burst_count >= 3 {
                warn!(
                    "{}: NAK burst ended - {} NAKs in {}ms",
                    self.label,
                    self.nak_burst_count,
                    self.last_nak_time_ms
                        .saturating_sub(self.nak_burst_start_time_ms)
                );
            }
            self.nak_burst_count = 1; // Start new burst count
        }

        self.last_nak_time_ms = current_time;
        self.consecutive_acks_without_nak = 0;

        // Decrease window (congestion response)
        let old = self.window;
        self.window = (self.window - WINDOW_DECR).max(WINDOW_MIN * WINDOW_MULT);

        // Log window reduction for diagnosis (include burst info)
        if self.window <= 3000 {
            let burst_info = if self.nak_burst_count > 1 {
                format!(" [BURST: {} NAKs]", self.nak_burst_count)
            } else {
                String::new()
            };
            warn!(
                "{}: NAK reduced window {} → {} (seq={}, total_naks={}{})",
                self.label, old, self.window, seq, self.nak_count, burst_info
            );
        }

        if self.window <= 2000 && !self.fast_recovery_mode {
            self.fast_recovery_mode = true;
            self.fast_recovery_start_ms = self.last_nak_time_ms;
            warn!(
                "{}: Enabling FAST RECOVERY MODE - window {}",
                self.label, self.window
            );
        }
    }

    pub fn handle_srtla_ack_specific(&mut self, seq: i32) -> bool {
        // Find the specific packet in the log and handle targeted logic
        // This mimics the first phase of the original implementation's register_srtla_ack
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
                
                // Window increase logic from original implementation
                if self.in_flight_packets * WINDOW_MULT > self.window {
                    let old = self.window;
                    self.window += WINDOW_INCR - 1;
                    if self.window > WINDOW_MAX * WINDOW_MULT {
                        self.window = WINDOW_MAX * WINDOW_MULT;
                    }
                    debug!(
                        "{}: SRTLA ACK specific increased window {} → {} (seq={}, in_flight={})",
                        self.label, old, self.window, seq, self.in_flight_packets
                    );
                }
                break;
            }
        }

        found
    }

    pub fn handle_srtla_ack_global(&mut self) {
        // Global +1 window increase for active connections (from original implementation)
        // This is the second phase applied to ALL connections for each SRTLA ACK
        if self.connected {
            let old = self.window;
            self.window += 1;
            if self.window > WINDOW_MAX * WINDOW_MULT {
                self.window = WINDOW_MAX * WINDOW_MULT;
            }
            if old < self.window {
                debug!(
                    "{}: SRTLA ACK global recovery {} → {}",
                    self.label, old, self.window
                );
            }
        }
    }

    pub fn needs_rtt_measurement(&self) -> bool {
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
                // Keepalive RTT is informational only; prefer SRT ACK-based RTT for accuracy
                debug!("{}: Keepalive RTT observed: {} ms", self.label, rtt);
                // still update timestamp to throttle RTT measurements
                self.last_rtt_measurement_ms = now;
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
            Some(now.saturating_sub(self.last_nak_time_ms))
        } else {
            None
        };

        // Conservative recovery timing (aligned with Java)
        let min_wait_time = if self.fast_recovery_mode { 500 } else { 2000 };
        let increment_wait = if self.fast_recovery_mode { 300 } else { 1000 };

        if time_since_last_nak.unwrap_or(u64::MAX) > min_wait_time
            && now.saturating_sub(self.last_window_increase_ms) > increment_wait
        {
            let old = self.window;
            let fast_mode_bonus = if self.fast_recovery_mode { 2 } else { 1 };

            if let Some(tsn) = time_since_last_nak {
                if tsn > 10_000 {
                    self.window += WINDOW_INCR * 2 * fast_mode_bonus;
                } else if tsn > 7_000 {
                    self.window += WINDOW_INCR * 1 * fast_mode_bonus;
                } else if tsn > 5_000 {
                    self.window += WINDOW_INCR * 1 * fast_mode_bonus;
                } else {
                    self.window += WINDOW_INCR * fast_mode_bonus;
                }
            } else {
                // No NAKs yet: minimal recovery step
                self.window += WINDOW_INCR * fast_mode_bonus;
            }

            let maxw = WINDOW_MAX * WINDOW_MULT;
            if self.window > maxw {
                self.window = maxw;
            }
            self.last_window_increase_ms = now;

            if self.window > old {
                if let Some(tsn) = time_since_last_nak {
                    info!(
                        "{}: Time-based window recovery {} → {} (no NAKs for {:.1}s, fast_mode={})",
                        self.label,
                        old,
                        self.window,
                        (tsn as f64) / 1000.0,
                        self.fast_recovery_mode
                    );
                } else {
                    debug!(
                        "{}: Time-based window recovery {} → {} (no NAKs yet)",
                        self.label, old, self.window
                    );
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn is_timed_out(&self) -> bool {
        !self.connected || self.last_received.elapsed().as_secs() >= CONN_TIMEOUT
    }

    pub fn mark_disconnected(&mut self) {
        self.connected = false;
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

    pub fn should_attempt_reconnect(&self) -> bool {
        const BASE_RECONNECT_DELAY_MS: u64 = 5000; // 5 seconds
        const MAX_BACKOFF_DELAY_MS: u64 = 120000; // 2 minutes max
        const MAX_BACKOFF_COUNT: u32 = 5;

        let now = now_ms();

        // First attempt is always allowed
        if self.last_reconnect_attempt_ms == 0 {
            return true;
        }

        let time_since_last_attempt = now.saturating_sub(self.last_reconnect_attempt_ms);
        let current_backoff_count = self.reconnect_failure_count.min(MAX_BACKOFF_COUNT);
        let min_interval = BASE_RECONNECT_DELAY_MS * (1u64 << current_backoff_count);
        let backoff_delay = min_interval.min(MAX_BACKOFF_DELAY_MS);

        time_since_last_attempt >= backoff_delay
    }

    pub fn record_reconnect_attempt(&mut self) {
        self.last_reconnect_attempt_ms = now_ms();
        self.reconnect_failure_count += 1;

        const MAX_BACKOFF_COUNT: u32 = 5;
        let capped_count = self.reconnect_failure_count.min(MAX_BACKOFF_COUNT);
        let delay_s = 5u64 * (1u64 << capped_count);

        info!(
            "{}: Reconnect attempt #{}, next attempt in {}s",
            self.label, self.reconnect_failure_count, delay_s
        );
    }

    pub fn mark_reconnect_success(&mut self) {
        if self.reconnect_failure_count > 0 {
            info!("{}: Reconnection successful, resetting backoff", self.label);
            self.reconnect_failure_count = 0;
        }
    }

    #[allow(dead_code)]
    pub async fn reconnect(&mut self) -> Result<()> {
        let sock = bind_from_ip(self.local_ip, 0)?;
        sock.connect(&self.remote.into())?;
        let std_sock: std::net::UdpSocket = sock.into();
        std_sock.set_nonblocking(true)?;
        let socket = UdpSocket::from_std(std_sock)?;
        self.socket = socket;
        self.connected = true;
        self.last_received = Instant::now();
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
        self.estimated_rtt_ms = 0.0;
        self.last_keepalive_ms = 0;
        self.last_keepalive_sent_ms = 0;
        self.waiting_for_keepalive_response = false;
        self.packet_send_times_ms = [0; PKT_LOG_SIZE];
        self.fast_recovery_start_ms = 0;
        self.last_reconnect_attempt_ms = now_ms();
        self.reconnect_failure_count = 0;
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

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
