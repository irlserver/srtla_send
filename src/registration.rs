use rand::RngCore;
use tracing::{debug, info, warn};

use crate::connection::SrtlaConnection;
use crate::protocol::*;
use crate::utils::now_ms;

pub struct SrtlaRegistrationManager {
    pub srtla_id: [u8; SRTLA_ID_LEN],
    pending_reg2_idx: Option<usize>,
    pending_timeout_at_ms: u64,
    active_connections: usize,
    pub has_connected: bool,
    broadcast_reg2_pending: bool,
    // New: drive REG1 only after REG_NGP, and avoid spamming
    reg1_target_idx: Option<usize>,
    reg1_next_send_at_ms: u64,
}

impl SrtlaRegistrationManager {
    #[allow(deprecated)]
    pub fn new() -> Self {
        let mut id = [0u8; SRTLA_ID_LEN];
        rand::thread_rng().fill_bytes(&mut id);
        Self {
            srtla_id: id,
            pending_reg2_idx: None,
            pending_timeout_at_ms: 0,
            active_connections: 0,
            has_connected: false,
            broadcast_reg2_pending: false,
            reg1_target_idx: None,
            reg1_next_send_at_ms: 0,
        }
    }

    pub async fn trigger_broadcast_reg2(&mut self, connections: &mut [SrtlaConnection]) {
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
    }

    pub fn process_registration_packet(&mut self, conn_idx: usize, buf: &[u8]) -> bool {
        match get_packet_type(buf) {
            Some(SRTLA_TYPE_REG_NGP) => {
                debug!("REG_NGP from uplink #{}", conn_idx);
                self.handle_reg_ngp(conn_idx);
                true
            }
            Some(SRTLA_TYPE_REG2) => {
                debug!("REG2 from uplink #{} (len={})", conn_idx, buf.len());
                self.handle_reg2(conn_idx, buf);
                true
            }
            Some(SRTLA_TYPE_REG3) => {
                debug!("REG3 from uplink #{}", conn_idx);
                self.handle_reg3(conn_idx);
                true
            }
            Some(SRTLA_TYPE_REG_ERR) => {
                debug!("REG_ERR from uplink #{}", conn_idx);
                self.handle_reg_err(conn_idx);
                true
            }
            _ => false,
        }
    }

    pub async fn reg_driver_send_if_needed(&mut self, connections: &mut [SrtlaConnection]) {
        // If nothing connected yet, send REG1. Prefer target from REG_NGP; otherwise,
        // pick the first uplink.
        if self.active_connections == 0 {
            let target_idx = if let Some(idx) = self.reg1_target_idx {
                Some(idx)
            } else {
                if !connections.is_empty() {
                    Some(0)
                } else {
                    None
                }
            };
            if let Some(idx) = target_idx {
                let now = now_ms();
                if self.pending_reg2_idx.is_none() && now >= self.reg1_next_send_at_ms {
                    let pkt = create_reg1_packet(&self.srtla_id);
                    info!("REG1 → uplink #{} ({} bytes)", idx, pkt.len());
                    let _ = connections[idx].send_srtla_packet(&pkt).await;
                    self.pending_reg2_idx = Some(idx);
                    self.pending_timeout_at_ms = now + REG2_TIMEOUT * 1000;
                    // schedule retry in case of REG_ERR/timeout
                    self.reg1_next_send_at_ms = now + REG2_TIMEOUT * 1000;
                }
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
        if self.active_connections == 0 {
            // Select target to receive REG1; driver will send
            self.reg1_target_idx = Some(conn_idx);
            self.reg1_next_send_at_ms = now_ms();
        }
    }

    fn handle_reg2(&mut self, conn_idx: usize, buf: &[u8]) {
        if buf.len() < 2 + SRTLA_ID_LEN {
            return;
        }
        if self.pending_reg2_idx == Some(conn_idx) {
            // server returns full id starting at byte 2
            self.srtla_id.copy_from_slice(&buf[2..2 + SRTLA_ID_LEN]);
            self.pending_reg2_idx = None;
            self.pending_timeout_at_ms = now_ms() + REG3_TIMEOUT * 1000;
            self.broadcast_reg2_pending = true;
            // stop sending REG1
            self.reg1_target_idx = None;
        }
    }

    fn handle_reg3(&mut self, _conn_idx: usize) {
        self.has_connected = true;
        self.active_connections += 1;
        info!(
            "connection established (active={})",
            self.active_connections
        );
    }

    fn handle_reg_err(&mut self, conn_idx: usize) {
        if self.pending_reg2_idx == Some(conn_idx) {
            self.pending_reg2_idx = None;
            self.pending_timeout_at_ms = 0;
        }
        // Do not retry REG1 immediately; wait for next REG_NGP to select target again
        self.reg1_target_idx = None;
        warn!("registration failed for connection {}", conn_idx);
    }
}

// no extra trait needed; driver directly awaits on `send_srtla_packet`

// Test-only accessor methods for controlled field access
#[cfg(test)]
impl SrtlaRegistrationManager {
    pub(crate) fn srtla_id(&self) -> &[u8; SRTLA_ID_LEN] {
        &self.srtla_id
    }

    pub(crate) fn pending_reg2_idx(&self) -> Option<usize> {
        self.pending_reg2_idx
    }

    pub(crate) fn pending_timeout_at_ms(&self) -> u64 {
        self.pending_timeout_at_ms
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

    pub(crate) fn reg1_next_send_at_ms(&self) -> u64 {
        self.reg1_next_send_at_ms
    }

    // Mutable accessors for tests that need to modify state
    pub(crate) fn set_pending_reg2_idx(&mut self, value: Option<usize>) {
        self.pending_reg2_idx = value;
    }

    pub(crate) fn set_pending_timeout_at_ms(&mut self, value: u64) {
        self.pending_timeout_at_ms = value;
    }

    pub(crate) fn set_reg1_target_idx(&mut self, value: Option<usize>) {
        self.reg1_target_idx = value;
    }

    pub(crate) fn set_reg1_next_send_at_ms(&mut self, value: u64) {
        self.reg1_next_send_at_ms = value;
    }

    pub(crate) fn set_broadcast_reg2_pending(&mut self, value: bool) {
        self.broadcast_reg2_pending = value;
    }
}


