use rand::RngCore;
use tracing::{debug, info, warn};

use crate::connection::SrtlaConnection;
use crate::protocol::*;
use crate::utils::now_ms;

#[derive(Debug)]
pub enum RegistrationEvent {
    RegNgp,
    Reg2,
    Reg3,
    RegErr,
}

pub struct SrtlaRegistrationManager {
    pub srtla_id: [u8; SRTLA_ID_LEN],
    pending_reg2_idx: Option<usize>,
    pending_timeout_at_ms: u64,
    active_connections: usize,
    pub has_connected: bool,
    broadcast_reg2_pending: bool,
    reg1_target_idx: Option<usize>,
    reg1_next_send_at_ms: u64,
    probing_state: ProbingState,
    probe_id: [u8; SRTLA_ID_LEN],
    probe_results: Vec<ProbeResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbingState {
    NotStarted,
    Probing,
    WaitingForProbes,
    Complete,
}

#[derive(Debug, Clone)]
struct ProbeResult {
    conn_idx: usize,
    probe_sent_ms: u64,
    rtt_ms: Option<u64>,
}

impl Default for SrtlaRegistrationManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SrtlaRegistrationManager {
    #[allow(deprecated)]
    pub fn new() -> Self {
        let mut id = [0u8; SRTLA_ID_LEN];
        rand::thread_rng().fill_bytes(&mut id);
        let mut probe_id = [0u8; SRTLA_ID_LEN];
        rand::thread_rng().fill_bytes(&mut probe_id);
        Self {
            srtla_id: id,
            pending_reg2_idx: None,
            pending_timeout_at_ms: 0,
            active_connections: 0,
            has_connected: false,
            broadcast_reg2_pending: false,
            reg1_target_idx: None,
            reg1_next_send_at_ms: 0,
            probing_state: ProbingState::NotStarted,
            probe_id,
            probe_results: Vec::new(),
        }
    }

    pub async fn send_reg1_to(&mut self, conn_idx: usize, conn: &mut SrtlaConnection) {
        let pkt = create_reg1_packet(&self.srtla_id);
        debug!("queueing REG1 for uplink #{}", conn_idx);
        info!("REG1 → uplink #{} ({} bytes)", conn_idx, pkt.len());
        if let Err(e) = conn.send_srtla_packet(&pkt).await {
            warn!("Failed to send REG1 to uplink #{}: {:?}", conn_idx, e);
        }

        let now = now_ms();
        self.pending_reg2_idx = Some(conn_idx);
        self.reg1_target_idx = Some(conn_idx);
        self.pending_timeout_at_ms = now + REG2_TIMEOUT * 1000;
        // Allow the driver to retry at a 1s cadence while waiting on REG2
        self.reg1_next_send_at_ms = now + 1000;
    }

    pub async fn send_reg2_to(&mut self, conn_idx: usize, conn: &mut SrtlaConnection) {
        let pkt = create_reg2_packet(&self.srtla_id);
        debug!("queueing REG2 for uplink #{}", conn_idx);
        info!("REG2 → uplink #{} ({} bytes)", conn_idx, pkt.len());
        if let Err(e) = conn.send_srtla_packet(&pkt).await {
            warn!("Failed to send REG2 to uplink #{}: {:?}", conn_idx, e);
        }
    }

    pub fn process_registration_packet(
        &mut self,
        conn_idx: usize,
        buf: &[u8],
    ) -> Option<RegistrationEvent> {
        match get_packet_type(buf) {
            Some(SRTLA_TYPE_REG_NGP) => {
                debug!("REG_NGP from uplink #{}", conn_idx);
                self.handle_reg_ngp(conn_idx);
                Some(RegistrationEvent::RegNgp)
            }
            Some(SRTLA_TYPE_REG2) => {
                debug!("REG2 from uplink #{} (len={})", conn_idx, buf.len());
                self.handle_reg2(conn_idx, buf);
                Some(RegistrationEvent::Reg2)
            }
            Some(SRTLA_TYPE_REG3) => {
                debug!("REG3 from uplink #{}", conn_idx);
                self.handle_reg3(conn_idx);
                Some(RegistrationEvent::Reg3)
            }
            Some(SRTLA_TYPE_REG_ERR) => {
                debug!("REG_ERR from uplink #{}", conn_idx);
                self.handle_reg_err(conn_idx);
                Some(RegistrationEvent::RegErr)
            }
            _ => None,
        }
    }

    pub async fn reg_driver_send_if_needed(&mut self, connections: &mut [SrtlaConnection]) {
        // If nothing connected yet, send REG1. Prefer target from REG_NGP; otherwise,
        // pick the first uplink.
        if self.active_connections == 0 {
            if let Some(idx) = self.reg1_target_idx {
                let now = now_ms();
                if self.pending_reg2_idx.is_none() && now >= self.reg1_next_send_at_ms {
                    let pkt = create_reg1_packet(&self.srtla_id);
                    info!("REG1 → uplink #{} ({} bytes)", idx, pkt.len());
                    if let Err(e) = connections[idx].send_srtla_packet(&pkt).await {
                        warn!("Failed to send REG1 to uplink #{}: {:?}", idx, e);
                    }
                    self.pending_reg2_idx = Some(idx);
                    self.pending_timeout_at_ms = now + REG2_TIMEOUT * 1000;
                    // throttle retries until next REG_NGP/timeout
                    self.reg1_next_send_at_ms = now + REG2_TIMEOUT * 1000;
                } else if self.pending_reg2_idx.is_some() {
                    debug!(
                        "REG1 pending for uplink #{} (timeout at {}), skipping send",
                        idx, self.pending_timeout_at_ms
                    );
                }
            } else {
                debug!("No REG1 target selected; awaiting REG_NGP");
            }
        }

        // If flagged by REG2 reception, broadcast REG2 once to all uplinks
        if self.broadcast_reg2_pending {
            let pkt = create_reg2_packet(&self.srtla_id);
            info!(
                "broadcast REG2 to {} uplinks ({} bytes)",
                connections.len(),
                pkt.len()
            );
            for (i, c) in connections.iter_mut().enumerate() {
                let _ = c.send_srtla_packet(&pkt).await;
                debug!("REG2 → uplink #{} sent", i);
            }
            self.broadcast_reg2_pending = false;
        }
    }

    fn handle_reg_ngp(&mut self, conn_idx: usize) {
        if self.probing_state == ProbingState::WaitingForProbes {
            self.handle_probe_response(conn_idx);
            return;
        }

        if self.active_connections == 0 && self.pending_reg2_idx.is_none() {
            debug!("REG_NGP from uplink #{} accepted as REG1 target", conn_idx);
            self.reg1_target_idx = Some(conn_idx);
            self.reg1_next_send_at_ms = now_ms();
        } else {
            debug!(
                "REG_NGP from uplink #{} ignored (active connections present or pending)",
                conn_idx
            );
        }
    }

    fn handle_reg2(&mut self, conn_idx: usize, buf: &[u8]) {
        if buf.len() < 2 + SRTLA_ID_LEN {
            return;
        }
        if self.pending_reg2_idx == Some(conn_idx) {
            // server returns full id starting at byte 2
            self.srtla_id.copy_from_slice(&buf[2..2 + SRTLA_ID_LEN]);
            debug!(
                "REG2 from uplink #{} accepted; broadcasting to peers",
                conn_idx
            );
            self.pending_reg2_idx = None;
            self.pending_timeout_at_ms = now_ms() + REG3_TIMEOUT * 1000;
            self.broadcast_reg2_pending = true;
            // stop sending REG1 until next REG_NGP
            self.reg1_target_idx = None;
            self.reg1_next_send_at_ms = 0;
        }
    }

    fn handle_reg3(&mut self, _conn_idx: usize) {
        self.has_connected = true;
    }

    fn handle_reg_err(&mut self, conn_idx: usize) {
        if self.pending_reg2_idx == Some(conn_idx) {
            debug!("REG_ERR for uplink #{} while awaiting REG2", conn_idx);
        } else {
            debug!("REG_ERR for uplink #{} (no pending REG2)", conn_idx);
        }

        self.pending_reg2_idx = None;
        self.pending_timeout_at_ms = 0;
        self.reg1_target_idx = None;
        // Wait for a fresh REG_NGP to select the next REG1 target
        self.reg1_next_send_at_ms = now_ms() + REG2_TIMEOUT * 1000;

        warn!("registration failed for connection {}", conn_idx);
    }

    pub async fn try_send_reg1_immediately(&mut self, conn_idx: usize, conn: &mut SrtlaConnection) {
        if self.active_connections == 0
            && self.pending_reg2_idx.is_none()
            && self.reg1_target_idx == Some(conn_idx)
            && now_ms() >= self.reg1_next_send_at_ms
        {
            debug!("REG_NGP immediate send for uplink #{}", conn_idx);
            self.send_reg1_to(conn_idx, conn).await;
        }
    }

    pub fn update_active_connections(&mut self, connections: &[SrtlaConnection]) {
        // Match C/Bond Bunny implementation: recalculate from scratch each housekeeping cycle
        // Bond Bunny (line 235): activeConnections = 0; then counts CONNECTED state connections
        // We count connections that are actually connected (received REG3), not just within grace period
        let new_count = connections.iter().filter(|c| c.connected).count();

        // Log any changes in connection count
        if new_count != self.active_connections {
            if new_count > self.active_connections {
                info!("connection established (active={})", new_count);
            } else {
                info!("connection(s) lost - active connections: {}", new_count);
            }
        }

        // Reset and recalculate - this is the authoritative count
        self.active_connections = new_count;
    }
    pub(crate) fn pending_reg2_idx(&self) -> Option<usize> {
        self.pending_reg2_idx
    }

    pub fn clear_pending_if_timed_out(&mut self, now_ms_value: u64) -> Option<usize> {
        if let Some(idx) = self.pending_reg2_idx
            && self.pending_timeout_at_ms != 0
            && now_ms_value >= self.pending_timeout_at_ms
        {
            warn!(
                "REG2 wait exceeded {}ms for uplink #{}; clearing pending handshake",
                REG2_TIMEOUT * 1000,
                idx
            );
            self.pending_reg2_idx = None;
            self.pending_timeout_at_ms = 0;
            self.reg1_target_idx = None;
            self.reg1_next_send_at_ms = now_ms_value;
            return Some(idx);
        }
        None
    }

    pub async fn start_probing(&mut self, connections: &mut [SrtlaConnection]) {
        if self.probing_state != ProbingState::NotStarted || self.active_connections > 0 {
            return;
        }

        info!("Starting RTT probing for {} connections", connections.len());
        self.probing_state = ProbingState::Probing;
        self.probe_results.clear();

        let probe_start_ms = now_ms();

        for (idx, conn) in connections.iter_mut().enumerate() {
            match conn.send_probe_reg2(&self.probe_id).await {
                Ok(sent_ms) => {
                    debug!(
                        "Probe REG2 sent to connection #{} at T+{}ms",
                        idx,
                        sent_ms.saturating_sub(probe_start_ms)
                    );
                    self.probe_results.push(ProbeResult {
                        conn_idx: idx,
                        probe_sent_ms: sent_ms,
                        rtt_ms: None,
                    });
                }
                Err(e) => {
                    warn!("Failed to send probe to connection #{}: {}", idx, e);
                }
            }
        }

        if !self.probe_results.is_empty() {
            self.probing_state = ProbingState::WaitingForProbes;
            self.pending_timeout_at_ms = now_ms() + 2000;
            info!(
                "Waiting for probe responses from {} connections",
                self.probe_results.len()
            );
        } else {
            warn!("No connections available for probing - using first connection as fallback");
            self.probing_state = ProbingState::Complete;
            if !connections.is_empty() {
                self.reg1_target_idx = Some(0);
                self.reg1_next_send_at_ms = probe_start_ms;
            }
        }
    }

    pub fn handle_probe_response(&mut self, conn_idx: usize) {
        if self.probing_state != ProbingState::WaitingForProbes {
            return;
        }

        let now = now_ms();
        if let Some(result) = self
            .probe_results
            .iter_mut()
            .find(|r| r.conn_idx == conn_idx)
        {
            if result.rtt_ms.is_none() {
                let rtt = now.saturating_sub(result.probe_sent_ms);
                result.rtt_ms = Some(rtt);
                info!(
                    "Probe response from connection #{} (RTT: {}ms)",
                    conn_idx, rtt
                );
            }
        }
    }

    pub fn check_probing_complete(&mut self) -> bool {
        if self.probing_state != ProbingState::WaitingForProbes {
            return false;
        }

        let now = now_ms();
        let all_responded = self.probe_results.iter().all(|r| r.rtt_ms.is_some());
        let timed_out = now >= self.pending_timeout_at_ms;

        if all_responded || timed_out {
            let responded_count = self
                .probe_results
                .iter()
                .filter(|r| r.rtt_ms.is_some())
                .count();

            if timed_out {
                info!(
                    "Probe timeout reached - {} of {} connections responded",
                    responded_count,
                    self.probe_results.len()
                );
            } else {
                info!("All {} probe responses received", responded_count);
            }

            if let Some(best) = self
                .probe_results
                .iter()
                .filter(|r| r.rtt_ms.is_some())
                .min_by_key(|r| r.rtt_ms.unwrap())
            {
                self.reg1_target_idx = Some(best.conn_idx);
                self.reg1_next_send_at_ms = now;
                info!(
                    "Selected connection #{} for initial registration (RTT: {}ms)",
                    best.conn_idx,
                    best.rtt_ms.unwrap()
                );
            } else {
                warn!("No connections responded to probes - will use first connection");
                self.reg1_target_idx = Some(0);
                self.reg1_next_send_at_ms = now;
            }

            self.probing_state = ProbingState::Complete;
            self.pending_timeout_at_ms = 0;
            return true;
        }

        false
    }

    pub fn is_probing(&self) -> bool {
        matches!(
            self.probing_state,
            ProbingState::Probing | ProbingState::WaitingForProbes
        )
    }
}

// no extra trait needed; driver directly awaits on `send_srtla_packet`

impl SrtlaRegistrationManager {
    pub fn get_selected_connection_idx(&self) -> Option<usize> {
        self.reg1_target_idx
    }
}

// Test-only accessor methods for controlled field access
#[cfg(test)]
#[allow(dead_code)]
impl SrtlaRegistrationManager {
    pub(crate) fn srtla_id(&self) -> &[u8; SRTLA_ID_LEN] {
        &self.srtla_id
    }

    pub(crate) fn active_connections(&self) -> usize {
        self.active_connections
    }

    pub(crate) fn has_connected(&self) -> bool {
        self.has_connected
    }

    pub(crate) fn broadcast_reg2_pending(&self) -> bool {
        self.broadcast_reg2_pending
    }

    pub(crate) fn reg1_target_idx(&self) -> Option<usize> {
        self.reg1_target_idx
    }

    pub(crate) fn set_reg1_target_idx(&mut self, value: Option<usize>) {
        self.reg1_target_idx = value;
    }

    pub(crate) fn reg1_next_send_at_ms(&self) -> u64 {
        self.reg1_next_send_at_ms
    }

    pub(crate) fn pending_timeout_at_ms(&self) -> u64 {
        self.pending_timeout_at_ms
    }

    // Mutable accessors for tests that need to modify state
    pub(crate) fn set_pending_reg2_idx(&mut self, value: Option<usize>) {
        self.pending_reg2_idx = value;
    }

    pub(crate) fn set_pending_timeout_at_ms(&mut self, value: u64) {
        self.pending_timeout_at_ms = value;
    }

    pub(crate) fn set_reg1_next_send_at_ms(&mut self, value: u64) {
        self.reg1_next_send_at_ms = value;
    }

    pub(crate) fn set_broadcast_reg2_pending(&mut self, value: bool) {
        self.broadcast_reg2_pending = value;
    }

    pub(crate) fn probe_results_count(&self) -> usize {
        self.probe_results.len()
    }

    pub(crate) fn simulate_probe_result(&mut self, conn_idx: usize, rtt_ms: u64) {
        use crate::utils::now_ms;
        let now = now_ms();
        self.probe_results.push(ProbeResult {
            conn_idx,
            probe_sent_ms: now.saturating_sub(rtt_ms),
            rtt_ms: Some(rtt_ms),
        });
    }

    pub(crate) fn set_probing_state_waiting(&mut self) {
        self.probing_state = ProbingState::WaitingForProbes;
        self.pending_timeout_at_ms = now_ms() + 2000;
    }
}
