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
    // New: drive REG1 only after REG_NGP, and avoid spamming
    reg1_target_idx: Option<usize>,
    reg1_next_send_at_ms: u64,
}

impl Default for SrtlaRegistrationManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SrtlaRegistrationManager {
    /// Create a new SrtlaRegistrationManager with a random SRTLA identifier and default initial state.
    ///
    /// The returned manager has a randomly generated `srtla_id` and all control fields set to their
    /// initial values (no pending REG2, zero timeouts, zero active connections, not connected,
    /// no pending broadcasts, and no REG1 target).
    ///
    /// # Examples
    ///
    /// ```
    /// let _mgr = SrtlaRegistrationManager::new();
    /// ```
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

    /// Queues and sends a REG1 packet to the specified uplink and starts the REG2 wait/ retry state.
    ///
    /// After sending the REG1 packet, this method records the target uplink as the pending REG2
    /// responder, sets a REG2 timeout based on the current time and `REG2_TIMEOUT`, and schedules
    /// the next allowed REG1 retry at a 1-second cadence.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use crate::{SrtlaRegistrationManager, SrtlaConnection};
    /// # async fn doc_example(mut manager: SrtlaRegistrationManager, mut conn: SrtlaConnection) {
    /// manager.send_reg1_to(0, &mut conn).await;
    /// # }
    /// ```
    pub async fn send_reg1_to(&mut self, conn_idx: usize, conn: &mut SrtlaConnection) {
        let pkt = create_reg1_packet(&self.srtla_id);
        debug!("queueing REG1 for uplink #{}", conn_idx);
        info!("REG1 → uplink #{} ({} bytes)", conn_idx, pkt.len());
        let _ = conn.send_srtla_packet(&pkt).await;

        let now = now_ms();
        self.pending_reg2_idx = Some(conn_idx);
        self.reg1_target_idx = Some(conn_idx);
        self.pending_timeout_at_ms = now + REG2_TIMEOUT * 1000;
        // Allow the driver to retry at a 1s cadence while waiting on REG2
        self.reg1_next_send_at_ms = now + 1000;
    }

    /// Sends a REG2 registration packet to the given uplink using the manager's current SRTLA identifier.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// async fn example(mut manager: SrtlaRegistrationManager, mut conn: SrtlaConnection) {
    ///     // Send a REG2 to uplink index 0
    ///     manager.send_reg2_to(0, &mut conn).await;
    /// }
    /// ```
    pub async fn send_reg2_to(&mut self, conn_idx: usize, conn: &mut SrtlaConnection) {
        let pkt = create_reg2_packet(&self.srtla_id);
        debug!("queueing REG2 for uplink #{}", conn_idx);
        info!("REG2 → uplink #{} ({} bytes)", conn_idx, pkt.len());
        let _ = conn.send_srtla_packet(&pkt).await;
    }

    /// Handle an incoming registration packet and invoke the appropriate registration handler.
    ///
    /// This inspects `buf` to determine the registration packet type and dispatches to the
    /// corresponding internal handler, returning an event describing the handled packet.
    ///
    /// # Returns
    ///
    /// `Some(RegistrationEvent::RegNgp|Reg2|Reg3|RegErr)` when `buf` contains a recognized registration
    /// packet type, `None` if the buffer does not represent a registration packet.
    ///
    /// # Examples
    ///
    /// ```
    /// // Example: process a REG_NGP-like buffer (first byte indicates packet type)
    /// let mut mgr = SrtlaRegistrationManager::new();
    /// let buf = [SRTLA_TYPE_REG_NGP]; // buffer whose packet type is REG_NGP
    /// let ev = mgr.process_registration_packet(0, &buf);
    /// assert_eq!(ev, Some(RegistrationEvent::RegNgp));
    /// ```
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

    /// Drives REG1/REG2 sends based on the manager's state and connected uplinks.
    ///
    /// When there are no active connections and a REG1 target is set, this will send a REG1
    /// to that target if no REG2 is currently pending and the next-send time has been reached;
    /// it then marks a REG2 as pending and sets the REG2 timeout and next-send cadence.
    /// When `broadcast_reg2_pending` is set, this will broadcast a single REG2 to all uplinks
    /// and then clear the pending flag.
    ///
    /// State changes:
    /// - On REG1 send: sets `pending_reg2_idx`, `pending_timeout_at_ms`, and `reg1_next_send_at_ms`.
    /// - On REG2 broadcast: clears `broadcast_reg2_pending`.
    ///
    /// # Examples
    ///
    /// ```
    /// # async fn example(mut manager: crate::SrtlaRegistrationManager) {
    /// let mut connections: Vec<crate::SrtlaConnection> = Vec::new();
    /// manager.reg_driver_send_if_needed(&mut connections).await;
    /// # }
    /// ```
    pub async fn reg_driver_send_if_needed(&mut self, connections: &mut [SrtlaConnection]) {
        // If nothing connected yet, send REG1. Prefer target from REG_NGP; otherwise,
        // pick the first uplink.
        if self.active_connections == 0 {
            if let Some(idx) = self.reg1_target_idx {
                let now = now_ms();
                if self.pending_reg2_idx.is_none() && now >= self.reg1_next_send_at_ms {
                    let pkt = create_reg1_packet(&self.srtla_id);
                    info!("REG1 → uplink #{} ({} bytes)", idx, pkt.len());
                    let _ = connections[idx].send_srtla_packet(&pkt).await;
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

    /// Accepts a REG_NGP from an uplink as the REG1 target when no active connections or pending REG2 exist.
    ///
    /// When accepted, sets the internal REG1 target to `conn_idx` and allows an immediate REG1 send
    /// by updating the next-send timestamp to the current time. If there are active connections or a
    /// REG2 is already pending, the REG_NGP is ignored.
    ///
    /// # Examples
    ///
    /// ```
    /// let mut mgr = SrtlaRegistrationManager::new();
    /// // simulate an uplink announcing presence
    /// mgr.handle_reg_ngp(0);
    /// assert_eq!(mgr.reg1_target_idx, Some(0));
    /// ```
    fn handle_reg_ngp(&mut self, conn_idx: usize) {
        if self.active_connections == 0 && self.pending_reg2_idx.is_none() {
            debug!("REG_NGP from uplink #{} accepted as REG1 target", conn_idx);
            self.reg1_target_idx = Some(conn_idx);
            // Allow immediate REG1 send; driver will enforce minimum cadence later
            self.reg1_next_send_at_ms = now_ms();
        } else {
            debug!(
                "REG_NGP from uplink #{} ignored (active connections present or pending)",
                conn_idx
            );
        }
    }

    /// Accepts a REG2 packet from a specific uplink and, if it matches an outstanding REG1 target,
    /// copies the returned SRTLA identifier into the manager, schedules a REG3 timeout, and flags the
    /// REG2 for broadcast to peers while suspending further REG1 sends until the next REG_NGP.
    ///
    /// The function does nothing if the buffer is too short to contain a full REG2 payload or the
    /// packet did not originate from the uplink currently awaited for REG2.
    ///
    /// # Parameters
    ///
    /// - `conn_idx`: index of the uplink that sent the packet.
    /// - `buf`: raw packet bytes; expected to contain the SRTLA id starting at byte offset 2.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Assume `mgr` is a SrtlaRegistrationManager and `buf` is a received REG2 packet.
    /// // If `mgr.pending_reg2_idx == Some(conn_idx)` and `buf` is long enough,
    /// // this call will copy the id from `buf[2..]` into `mgr.srtla_id`.
    /// let mut mgr = SrtlaRegistrationManager::new();
    /// let conn_idx = 0usize;
    /// let buf: Vec<u8> = vec![0u8; 2 + SRTLA_ID_LEN]; // synthetic REG2 with placeholder id
    /// mgr.handle_reg2(conn_idx, &buf);
    /// ```
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

    /// Marks the manager as having completed a successful registration/connection.
    ///
    /// # Examples
    ///
    /// ```
    /// let mut mgr = SrtlaRegistrationManager::new();
    /// // record that a REG3 (final registration) was received from connection 0
    /// mgr.handle_reg3(0);
    /// ```
    fn handle_reg3(&mut self, _conn_idx: usize) {
        self.has_connected = true;
    }

    /// Handle a REG_ERR response from the given uplink and reset registration state.
    ///
    /// Clears any pending REG2 tracking for the specified connection, cancels the REG2 timeout,
    /// clears the REG1 target, and schedules the next REG1 send for now + REG2_TIMEOUT seconds
    /// to wait for a fresh REG_NGP. Also records the registration failure for the connection.
    ///
    /// # Parameters
    ///
    /// - `conn_idx`: Index of the uplink that sent the REG_ERR.
    ///
    /// # Examples
    ///
    /// ```
    /// let mut mgr = SrtlaRegistrationManager::new();
    /// // simulate an error from uplink 0
    /// mgr.handle_reg_err(0);
    /// assert!(mgr.pending_reg2_idx.is_none());
    /// ```
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

    /// Attempts to send a REG1 to the specified uplink immediately if registration state allows it.
    ///
    /// This will send REG1 only when there are no active connections, there is no pending REG2,
    /// the given uplink is the current REG1 target, and the next-send time has been reached.
    ///
    /// # Examples
    ///
    /// ```
    /// # async fn example(mut manager: SrtlaRegistrationManager, mut conn: SrtlaConnection, idx: usize) {
    /// manager.try_send_reg1_immediately(idx, &mut conn).await;
    /// # }
    /// ```
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

    /// Recomputes and updates the count of active (non-timed-out) uplink connections.
    ///
    /// Recounts the provided `connections` slice, treating a connection as active if it is not
    /// timed out, logs any increase or decrease, and replaces the manager's authoritative
    /// `active_connections` value with the new count.
    ///
    /// # Parameters
    ///
    /// - `connections`: slice of `SrtlaConnection` instances to evaluate for activity.
    pub fn update_active_connections(&mut self, connections: &[SrtlaConnection]) {
        // Match C/Bond Bunny implementation: recalculate from scratch each housekeeping cycle
        // C code (line 653): active_connections = 0; then counts non-timed-out connections
        // Bond Bunny (line 235): activeConnections = 0; then counts CONNECTED state connections
        let new_count = connections.iter().filter(|c| !c.is_timed_out()).count();

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
    /// Returns the index of the uplink that is currently awaiting a REG2 response, if one exists.
    ///
    /// # Returns
    ///
    /// `Some(index)` with the uplink index awaiting REG2, or `None` if no REG2 is pending.
    ///
    /// # Examples
    ///
    /// ```
    /// let mgr = SrtlaRegistrationManager::new();
    /// assert!(mgr.pending_reg2_idx().is_none());
    /// ```
    pub(crate) fn pending_reg2_idx(&self) -> Option<usize> {
        self.pending_reg2_idx
    }

    /// Clears a pending REG2 handshake if its timeout has been reached and resets related registration state.
    ///
    /// If a pending REG2 exists and `now_ms_value` is greater than or equal to the pending timeout, this will:
    /// - log a timeout,
    /// - clear `pending_reg2_idx` and `pending_timeout_at_ms`,
    /// - clear `reg1_target_idx`,
    /// - set `reg1_next_send_at_ms` to `now_ms_value`,
    /// and return `Some(idx)` where `idx` is the uplink index that timed out. If no pending REG2 has timed out, returns `None`.
    ///
    /// # Examples
    ///
    /// ```
    /// let mut mgr = SrtlaRegistrationManager::new();
    /// // No pending REG2, so nothing to clear.
    /// assert_eq!(mgr.clear_pending_if_timed_out(0), None);
    /// ```
    pub fn clear_pending_if_timed_out(&mut self, now_ms_value: u64) -> Option<usize> {
        if let Some(idx) = self.pending_reg2_idx
            && self.pending_timeout_at_ms != 0 && now_ms_value >= self.pending_timeout_at_ms {
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
}

// no extra trait needed; driver directly awaits on `send_srtla_packet`

// Test-only accessor methods for controlled field access
#[cfg(test)]
#[allow(dead_code)]
impl SrtlaRegistrationManager {
    /// Accesses the manager's current SRTLA identifier.
    ///
    /// # Returns
    ///
    /// A reference to the manager's 16-byte SRTLA identifier.
    ///
    /// # Examples
    ///
    /// ```
    /// let mgr = SrtlaRegistrationManager::new();
    /// let id_ref: &[u8; SRTLA_ID_LEN] = mgr.srtla_id();
    /// assert_eq!(id_ref.len(), SRTLA_ID_LEN);
    /// ```
    pub(crate) fn srtla_id(&self) -> &[u8; SRTLA_ID_LEN] {
        &self.srtla_id
    }

    /// Returns the number of active (non-timed-out) connections tracked by the manager.
    ///
    /// # Examples
    ///
    /// ```
    /// let mgr = SrtlaRegistrationManager::new();
    /// let count = mgr.active_connections();
    /// assert_eq!(count, 0);
    /// ```
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

    /// Reports the scheduled timestamp (in milliseconds) for when the next REG1 may be sent.
    ///
    /// Returns the timestamp in milliseconds that was last set for the next allowed REG1 transmission.
    ///
    /// # Examples
    ///
    /// ```
    /// let mgr = SrtlaRegistrationManager::new();
    /// let ts = mgr.reg1_next_send_at_ms();
    /// // timestamp is a u64 millisecond value
    /// assert!(ts >= 0);
    /// ```
    pub(crate) fn reg1_next_send_at_ms(&self) -> u64 {
        self.reg1_next_send_at_ms
    }

    /// Timestamp (ms) when the pending REG2 will time out, or 0 if none is set.
    ///
    /// # Returns
    ///
    /// The timeout timestamp in milliseconds; `0` indicates there is no pending REG2 timeout.
    ///
    /// # Examples
    ///
    /// ```
    /// let mgr = SrtlaRegistrationManager::new();
    /// let timeout = mgr.pending_timeout_at_ms();
    /// assert_eq!(timeout, 0);
    /// ```
    pub(crate) fn pending_timeout_at_ms(&self) -> u64 {
        self.pending_timeout_at_ms
    }

    // Mutable accessors for tests that need to modify state
    /// Set the pending REG2 uplink index or clear it.
    ///
    /// Passing `Some(idx)` marks the uplink at `idx` as awaiting a REG2 response.
    /// Passing `None` clears any pending REG2 state.
    ///
    /// # Examples
    ///
    /// ```
    /// let mut mgr = SrtlaRegistrationManager::new();
    /// mgr.set_pending_reg2_idx(Some(2));
    /// assert_eq!(mgr.pending_reg2_idx(), Some(2));
    /// mgr.set_pending_reg2_idx(None);
    /// assert_eq!(mgr.pending_reg2_idx(), None);
    /// ```
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