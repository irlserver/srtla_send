mod probing;

use probing::{ProbeResult, ProbingState, default_probing_state, new_probe_id, new_probe_results};
use rand::RngCore;
use smallvec::SmallVec;
use srtla_protocol::*;
use tracing::{debug, info, warn};

use crate::connection::SrtlaConnection;

#[derive(Debug)]
pub enum RegistrationEvent {
    RegNgp,
    Reg2,
    Reg3,
    RegErr,
}

/// Registration control frames carry no authentication, and the uplink sockets
/// are deliberately unconnected (accept-any-source, for C-reference and NAT
/// interop), so anything that can reach an uplink's source port can forge one.
/// The manager therefore accepts a REG3 or a REG_ERR only while the addressed
/// uplink genuinely has that phase of the handshake in flight; anything else is
/// counted here and reported to the shell as "not a registration packet"
/// (`None`), which is a no-op for connection state.
#[derive(Debug, Default, Clone, Copy)]
struct OutOfPhaseCounters {
    reg3: u64,
    reg_err: u64,
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
    /// Uplink indices we have queued a REG2 for. A REG3 is honored only for a
    /// member, and the grant is ONE-SHOT: `handle_reg3` consumes it, so a
    /// duplicate or replayed REG3 cannot re-fire the "just registered" effects
    /// (`clear_pre_registration_state`, which wipes a live link's packet log,
    /// in-flight count, congestion state and batch queue). A legitimate
    /// reconnect re-arms the gate through [`Self::build_reg2`].
    ///
    /// Indices are positional into the shell's connection vector, exactly like
    /// `pending_reg2_idx` / `reg1_target_idx`.
    awaiting_reg3: SmallVec<usize, 4>,
    /// `connected` flags snapshotted by [`Self::update_active_connections`],
    /// indexed the same way. Used only to keep a REG2 broadcast from re-arming
    /// the one-shot gate on an already-established, forwarding uplink.
    connected_snapshot: SmallVec<bool, 4>,
    out_of_phase: OutOfPhaseCounters,
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
            awaiting_reg3: SmallVec::new(),
            connected_snapshot: SmallVec::new(),
            out_of_phase: OutOfPhaseCounters::default(),
        }
    }

    /// Wind the handshake all the way back to its pre-REG1 state so the bond can
    /// register from scratch — used when the shell has repointed every uplink at
    /// a different receiver instance (whole-bond re-home).
    ///
    /// The receiver-issued half of `srtla_id` is discarded (it only ever meant
    /// something to the instance that minted it) but the sender's own half — the
    /// first half, the only part any receiver reads out of a REG1 — is **kept
    /// deliberately**. Receivers that derive their half deterministically from
    /// ours (HKDF over the client bytes) then reissue the identical `full_id`,
    /// so a load balancer tracking the bond by that id sees one continuous group
    /// across the move instead of a brand-new stream. A receiver that mints its
    /// half randomly is unaffected: it overwrites those bytes in its REG2
    /// exactly as it does for a first-time sender.
    ///
    /// The discarded half is re-randomized rather than zeroed, so the REG1 that
    /// follows is byte-for-byte the same *shape* a freshly started sender emits
    /// — the move must be indistinguishable on the wire from a new sender.
    ///
    /// Everything else — pending REG1/REG2 slots, the one-shot REG3 grants, the
    /// broadcast flag, the connected snapshot and the RTT probing state machine
    /// — is cleared, so the next housekeeping tick re-probes and re-registers
    /// over the plain REG1/REG2/REG3 flow. `has_connected` survives: it records
    /// that this *process* has streamed before, which only drives log wording.
    pub fn reset_for_rehome(&mut self) {
        rand::rng().fill_bytes(&mut self.srtla_id[SRTLA_ID_LEN / 2..]);
        self.pending_reg2_idx = None;
        self.pending_timeout_at_ms = 0;
        self.active_connections = 0;
        self.broadcast_reg2_pending = false;
        self.reg1_target_idx = None;
        self.reg1_next_send_at_ms = 0;
        self.awaiting_reg3.clear();
        self.connected_snapshot.clear();
        // A fresh probe id: the probe REG2 is deliberately an id the receiver
        // cannot know, and the old one was minted against the old instance.
        self.probing_state = default_probing_state();
        self.probe_id = new_probe_id();
        self.probe_results = new_probe_results();
    }

    /// Arm the one-shot REG3 grant for `conn_idx`.
    ///
    /// Sans-IO caveat: the manager builds packets and the shell transmits them,
    /// so the grant is armed when a REG2 is *queued*, not when it provably left
    /// the host (the fork this is ported from can gate on the send result). A
    /// REG2 the kernel then drops therefore leaves a grant armed for one
    /// handshake window — deliberately the liveness-safe direction: over-arming
    /// can only admit a REG3 for an uplink we did try to register, whereas
    /// under-arming would silently refuse a legitimate registration.
    fn arm_reg3_grant(&mut self, conn_idx: usize) {
        if !self.awaiting_reg3.contains(&conn_idx) {
            self.awaiting_reg3.push(conn_idx);
        }
    }

    fn revoke_reg3_grant(&mut self, conn_idx: usize) {
        self.awaiting_reg3.retain(|&idx| idx != conn_idx);
    }

    fn is_connected_snapshot(&self, conn_idx: usize) -> bool {
        self.connected_snapshot
            .get(conn_idx)
            .copied()
            .unwrap_or(false)
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

    /// Build a REG2 packet from the current SRTLA id and arm this uplink's
    /// one-shot REG3 grant. The caller transmits it on the uplink.
    ///
    /// This is the per-uplink *reconnect* resend, so it arms unconditionally:
    /// a link that reached this path has been reset (`connected == false`) and
    /// must be able to complete a fresh handshake. Only the broadcast in
    /// [`Self::reg_driver_pending_sends`] skips established links.
    pub fn build_reg2(&mut self, conn_idx: usize) -> [u8; SRTLA_TYPE_REG2_LEN] {
        let pkt = create_reg2_packet(&self.srtla_id);
        debug!("queueing REG2 for uplink #{}", conn_idx);
        info!("REG2 → uplink #{} ({} bytes)", conn_idx, pkt.len());
        self.arm_reg3_grant(conn_idx);
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
            // An out-of-phase REG3/REG_ERR reports `None`: the shell's
            // registration effects are keyed on the returned event, and "no
            // event" is the only way to say "consumed, change nothing" without
            // widening `RegistrationEvent` (its match in the sender shell is
            // exhaustive). The shell then treats the frame as an unrecognized
            // datagram, which is exactly what it already does for any other
            // spoofable junk that reaches an uplink's source port.
            Some(SRTLA_TYPE_REG3) => {
                debug!("REG3 from uplink #{}", conn_idx);
                self.handle_reg3(conn_idx)
                    .then_some(RegistrationEvent::Reg3)
            }
            Some(SRTLA_TYPE_REG_ERR) => {
                debug!("REG_ERR from uplink #{}", conn_idx);
                self.handle_reg_err(conn_idx, now_ms)
                    .then_some(RegistrationEvent::RegErr)
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
            for idx in 0..connection_count {
                // Arming an already-established uplink would re-open the
                // one-shot gate `handle_reg3` consumed, so the REG3 this
                // broadcast provokes would wipe a live, forwarding link's
                // state. The link is already registered; it needs no grant.
                // (The shell still transmits to it — the receiver re-admits
                // the address either way — only our bookkeeping abstains.)
                if self.is_connected_snapshot(idx) {
                    debug!("REG2 → uplink #{} not re-armed (already connected)", idx);
                    continue;
                }
                self.arm_reg3_grant(idx);
            }
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

    /// Returns `true` only when this REG3 answers a REG2 this uplink actually
    /// has in flight. The grant is consumed on the way through, so a duplicate
    /// or replayed REG3 is rejected: without that, every later REG3 on an
    /// already-connected uplink re-fires the "just registered" effects and
    /// wipes a live, forwarding link's packet log, in-flight count, congestion
    /// state and batch queue.
    ///
    /// A legitimate receiver only ever emits REG3 in reply to a REG2 from that
    /// exact source address, so the gate cannot reject a real one — including
    /// from a NAT-remapped receiver, since the grant is keyed on our uplink,
    /// not on the receiver's address.
    fn handle_reg3(&mut self, conn_idx: usize) -> bool {
        if !self.awaiting_reg3.contains(&conn_idx) {
            self.out_of_phase.reg3 = self.out_of_phase.reg3.saturating_add(1);
            self.log_out_of_phase("REG3", conn_idx, self.out_of_phase.reg3);
            return false;
        }
        self.revoke_reg3_grant(conn_idx);
        self.has_connected = true;
        true
    }

    /// Returns `true` only when this REG_ERR answers a handshake this uplink
    /// actually has in flight (awaiting REG2, or awaiting REG3).
    ///
    /// A forged 2-byte REG_ERR is the cheapest remote DoS against the sender:
    /// ungated, it force-disconnects an established, forwarding uplink and
    /// collaterally clears the single-slot global REG1/REG2 fields, aborting an
    /// *unrelated* uplink's concurrent handshake. Phase-gating is a strictly
    /// better mitigation than rate-limiting: a real receiver only emits REG_ERR
    /// as a direct reply to a REG1/REG2 it just received from that address, so
    /// nothing legitimate is ever discarded, and a flood of forged ones is
    /// bounded at zero effect rather than "one teardown per rate-limit window".
    fn handle_reg_err(&mut self, conn_idx: usize, now_ms: u64) -> bool {
        let awaiting_reg2 = self.pending_reg2_idx == Some(conn_idx);
        let awaiting_reg3 = self.awaiting_reg3.contains(&conn_idx);

        if !awaiting_reg2 && !awaiting_reg3 {
            self.out_of_phase.reg_err = self.out_of_phase.reg_err.saturating_add(1);
            self.log_out_of_phase("REG_ERR", conn_idx, self.out_of_phase.reg_err);
            return false;
        }

        if awaiting_reg2 {
            debug!("REG_ERR for uplink #{} while awaiting REG2", conn_idx);
            // Scoped to the uplink that owns the single in-flight REG1/REG2
            // slot: clearing these for any other index is what let one forged
            // frame abort a different uplink's handshake.
            self.pending_reg2_idx = None;
            self.pending_timeout_at_ms = 0;
            self.reg1_target_idx = None;
            // Wait for a fresh REG_NGP to select the next REG1 target
            self.reg1_next_send_at_ms = now_ms + REG2_TIMEOUT * 1000;
        } else {
            debug!("REG_ERR for uplink #{} while awaiting REG3", conn_idx);
        }
        // An awaiting-REG3 REG_ERR revokes only its own grant.
        self.revoke_reg3_grant(conn_idx);

        warn!("registration failed for connection {}", conn_idx);
        true
    }

    /// Log the first rejection of each kind loudly, then drop to `debug!`: a
    /// spoofed flood must not turn into a log-amplification DoS of its own.
    fn log_out_of_phase(&self, kind: &str, conn_idx: usize, count: u64) {
        if count == 1 {
            warn!(
                "{} for uplink #{} ignored: no matching registration in flight (further \
                 out-of-phase frames logged at debug)",
                kind, conn_idx
            );
        } else {
            debug!(
                "{} for uplink #{} ignored: no matching registration in flight ({} so far)",
                kind, conn_idx, count
            );
        }
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

        // Snapshot the per-index `connected` flags for the REG2 broadcast's
        // re-arm decision. Refreshed from the live slice every housekeeping
        // tick, immediately before `reg_driver_pending_sends` consumes it.
        self.connected_snapshot.clear();
        for conn in connections {
            self.connected_snapshot.push(conn.connected);
        }
    }

    pub fn pending_reg2_idx(&self) -> Option<usize> {
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

// Test-only accessor methods for controlled field access. Gated on
// `test-internals` (not just `test`) so the parent srtla_send crate can reach
// them from its own cross-crate registration tests.
#[cfg(any(test, feature = "test-internals"))]
#[allow(dead_code)]
impl SrtlaRegistrationManager {
    pub fn srtla_id(&self) -> &[u8; SRTLA_ID_LEN] {
        &self.srtla_id
    }

    pub fn active_connections(&self) -> usize {
        self.active_connections
    }

    pub fn has_connected(&self) -> bool {
        self.has_connected
    }

    pub fn broadcast_reg2_pending(&self) -> bool {
        self.broadcast_reg2_pending
    }

    pub fn reg1_target_idx(&self) -> Option<usize> {
        self.reg1_target_idx
    }

    pub fn set_reg1_target_idx(&mut self, value: Option<usize>) {
        self.reg1_target_idx = value;
    }

    pub fn reg1_next_send_at_ms(&self) -> u64 {
        self.reg1_next_send_at_ms
    }

    pub fn pending_timeout_at_ms(&self) -> u64 {
        self.pending_timeout_at_ms
    }

    // Mutable accessors for tests that need to modify state
    pub fn set_pending_reg2_idx(&mut self, value: Option<usize>) {
        self.pending_reg2_idx = value;
    }

    pub fn set_pending_timeout_at_ms(&mut self, value: u64) {
        self.pending_timeout_at_ms = value;
    }

    pub fn set_reg1_next_send_at_ms(&mut self, value: u64) {
        self.reg1_next_send_at_ms = value;
    }

    pub fn set_broadcast_reg2_pending(&mut self, value: bool) {
        self.broadcast_reg2_pending = value;
    }

    /// REG3 frames rejected by the phase gate.
    pub fn out_of_phase_reg3(&self) -> u64 {
        self.out_of_phase.reg3
    }

    /// REG_ERR frames rejected by the phase gate.
    pub fn out_of_phase_reg_err(&self) -> u64 {
        self.out_of_phase.reg_err
    }

    pub fn is_awaiting_reg3(&self, conn_idx: usize) -> bool {
        self.awaiting_reg3.contains(&conn_idx)
    }

    /// Arm the one-shot REG3 grant without building a REG2, for tests that
    /// want to start from "a REG2 is in flight on this uplink".
    pub fn arm_reg3_gate(&mut self, conn_idx: usize) {
        self.arm_reg3_grant(conn_idx);
    }
}
