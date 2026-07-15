mod probing;

use probing::{ProbeResult, ProbingState, default_probing_state, new_probe_id, new_probe_results};
use rand::RngCore;
use smallvec::SmallVec;
use tracing::{debug, info, warn};

use crate::connection::SrtlaConnection;
use crate::protocol::*;

#[derive(Debug)]
pub enum RegistrationEvent {
    RegNgp,
    Reg2,
    Reg3,
    RegErr,
}

/// Packets the registration driver decided to send this tick, for the shell to
/// transmit. Keeps the manager sans-IO: `reg_driver_pending_sends` mutates the
/// handshake state machine and returns *what* to send; the caller owns the
/// sockets and does the sending.
#[derive(Default)]
pub struct RegDriverSends {
    /// REG1 to a single target uplink: `(conn_idx, packet)`.
    pub reg1: Option<(usize, [u8; SRTLA_TYPE_REG1_LEN])>,
    /// REG2 to broadcast to every uplink.
    pub broadcast_reg2: Option<[u8; SRTLA_TYPE_REG2_LEN]>,
}

pub struct SrtlaRegistrationManager {
    pub srtla_id: [u8; SRTLA_ID_LEN],
    pending_reg2_idx: Option<usize>,
    pub(crate) pending_timeout_at_ms: u64,
    pub(crate) active_connections: usize,
    pub has_connected: bool,
    broadcast_reg2_pending: bool,
    pub(crate) reg1_target_idx: Option<usize>,
    pub(crate) reg1_next_send_at_ms: u64,
    probing_state: ProbingState,
    probe_id: [u8; SRTLA_ID_LEN],
    probe_results: SmallVec<ProbeResult, 4>,
}

impl Default for SrtlaRegistrationManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SrtlaRegistrationManager {
    pub fn new() -> Self {
        let mut id = [0u8; SRTLA_ID_LEN];
        rand::rng().fill_bytes(&mut id);
        Self {
            srtla_id: id,
            pending_reg2_idx: None,
            pending_timeout_at_ms: 0,
            active_connections: 0,
            has_connected: false,
            broadcast_reg2_pending: false,
            reg1_target_idx: None,
            reg1_next_send_at_ms: 0,
            probing_state: default_probing_state(),
            probe_id: new_probe_id(),
            probe_results: new_probe_results(),
        }
    }

    /// Build a REG1 packet for `conn_idx` and advance into the "awaiting REG2"
    /// state. Pure: the caller transmits the returned bytes on the uplink and
    /// stamps [`SrtlaConnection::note_sent`].
    pub fn build_reg1_for(&mut self, conn_idx: usize, now: u64) -> [u8; SRTLA_TYPE_REG1_LEN] {
        let pkt = create_reg1_packet(&self.srtla_id);
        debug!("queueing REG1 for uplink #{}", conn_idx);
        info!("REG1 → uplink #{} ({} bytes)", conn_idx, pkt.len());

        self.pending_reg2_idx = Some(conn_idx);
        self.reg1_target_idx = Some(conn_idx);
        self.pending_timeout_at_ms = now + REG2_TIMEOUT * 1000;
        // Allow the driver to retry at a 1s cadence while waiting on REG2
        self.reg1_next_send_at_ms = now + 1000;
        pkt
    }

    /// Build a REG2 packet from the current SRTLA id. Pure and stateless; the
    /// caller transmits it on the uplink.
    pub fn build_reg2(&self, conn_idx: usize) -> [u8; SRTLA_TYPE_REG2_LEN] {
        let pkt = create_reg2_packet(&self.srtla_id);
        debug!("queueing REG2 for uplink #{}", conn_idx);
        info!("REG2 → uplink #{} ({} bytes)", conn_idx, pkt.len());
        pkt
    }

    pub fn process_registration_packet(
        &mut self,
        conn_idx: usize,
        buf: &[u8],
        now_ms: u64,
    ) -> Option<RegistrationEvent> {
        match get_packet_type(buf) {
            Some(SRTLA_TYPE_REG_NGP) => {
                debug!("REG_NGP from uplink #{}", conn_idx);
                self.handle_reg_ngp(conn_idx, now_ms);
                Some(RegistrationEvent::RegNgp)
            }
            Some(SRTLA_TYPE_REG2) => {
                debug!("REG2 from uplink #{} (len={})", conn_idx, buf.len());
                self.handle_reg2(conn_idx, buf, now_ms);
                Some(RegistrationEvent::Reg2)
            }
            Some(SRTLA_TYPE_REG3) => {
                debug!("REG3 from uplink #{}", conn_idx);
                self.handle_reg3(conn_idx);
                Some(RegistrationEvent::Reg3)
            }
            Some(SRTLA_TYPE_REG_ERR) => {
                debug!("REG_ERR from uplink #{}", conn_idx);
                self.handle_reg_err(conn_idx, now_ms);
                Some(RegistrationEvent::RegErr)
            }
            _ => None,
        }
    }

    /// Decide the registration driver's sends for this tick and advance the
    /// handshake state machine. Pure: returns the packets; the caller (which
    /// owns the sockets) transmits REG1 to `reg1.0` and broadcasts REG2 to
    /// every uplink. `connection_count` is used only for logging.
    pub fn reg_driver_pending_sends(
        &mut self,
        connection_count: usize,
        now: u64,
    ) -> RegDriverSends {
        let mut sends = RegDriverSends::default();

        // If nothing connected yet, send REG1. Prefer target from REG_NGP; otherwise,
        // pick the first uplink.
        if self.active_connections == 0 {
            if let Some(idx) = self.reg1_target_idx {
                if self.pending_reg2_idx.is_none() && now >= self.reg1_next_send_at_ms {
                    let pkt = create_reg1_packet(&self.srtla_id);
                    info!("REG1 → uplink #{} ({} bytes)", idx, pkt.len());
                    self.pending_reg2_idx = Some(idx);
                    self.pending_timeout_at_ms = now + REG2_TIMEOUT * 1000;
                    // throttle retries until next REG_NGP/timeout
                    self.reg1_next_send_at_ms = now + REG2_TIMEOUT * 1000;
                    sends.reg1 = Some((idx, pkt));
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
                connection_count,
                pkt.len()
            );
            sends.broadcast_reg2 = Some(pkt);
            self.broadcast_reg2_pending = false;
        }

        sends
    }

    fn handle_reg_ngp(&mut self, conn_idx: usize, now_ms: u64) {
        if self.probing_state == ProbingState::WaitingForProbes {
            self.handle_probe_response(conn_idx, now_ms);
            return;
        }

        if self.active_connections == 0 && self.pending_reg2_idx.is_none() {
            debug!("REG_NGP from uplink #{} accepted as REG1 target", conn_idx);
            self.reg1_target_idx = Some(conn_idx);
            self.reg1_next_send_at_ms = now_ms;
        } else {
            debug!(
                "REG_NGP from uplink #{} ignored (active connections present or pending)",
                conn_idx
            );
        }
    }

    fn handle_reg2(&mut self, conn_idx: usize, buf: &[u8], now_ms: u64) {
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
            self.pending_timeout_at_ms = now_ms + REG3_TIMEOUT * 1000;
            self.broadcast_reg2_pending = true;
            // stop sending REG1 until next REG_NGP
            self.reg1_target_idx = None;
            self.reg1_next_send_at_ms = 0;
        }
    }

    fn handle_reg3(&mut self, _conn_idx: usize) {
        self.has_connected = true;
    }

    fn handle_reg_err(&mut self, conn_idx: usize, now_ms: u64) {
        if self.pending_reg2_idx == Some(conn_idx) {
            debug!("REG_ERR for uplink #{} while awaiting REG2", conn_idx);
        } else {
            debug!("REG_ERR for uplink #{} (no pending REG2)", conn_idx);
        }

        self.pending_reg2_idx = None;
        self.pending_timeout_at_ms = 0;
        self.reg1_target_idx = None;
        // Wait for a fresh REG_NGP to select the next REG1 target
        self.reg1_next_send_at_ms = now_ms + REG2_TIMEOUT * 1000;

        warn!("registration failed for connection {}", conn_idx);
    }

    /// If a REG_NGP just arrived and we should immediately answer this uplink
    /// with REG1, build it (advancing state) and return it for the shell to
    /// send. Pure; returns `None` when no immediate REG1 is due.
    pub fn reg1_if_ngp_immediate(
        &mut self,
        conn_idx: usize,
        now: u64,
    ) -> Option<[u8; SRTLA_TYPE_REG1_LEN]> {
        if self.active_connections == 0
            && self.pending_reg2_idx.is_none()
            && self.reg1_target_idx == Some(conn_idx)
            && now >= self.reg1_next_send_at_ms
        {
            debug!("REG_NGP immediate send for uplink #{}", conn_idx);
            Some(self.build_reg1_for(conn_idx, now))
        } else {
            None
        }
    }

    pub fn update_active_connections(&mut self, connections: &[SrtlaConnection]) {
        // Match C implementation: recalculate from scratch each housekeeping cycle
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
}
