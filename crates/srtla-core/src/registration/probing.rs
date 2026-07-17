use smallvec::SmallVec;
use srtla_protocol::{SRTLA_ID_LEN, SRTLA_TYPE_REG2_LEN};
use tracing::{info, warn};

use super::SrtlaRegistrationManager;
use crate::connection::SrtlaConnection;
use crate::utils::now_ms;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProbingState {
    NotStarted,
    Probing,
    WaitingForProbes,
    Complete,
}

#[derive(Debug, Clone)]
pub(super) struct ProbeResult {
    pub conn_idx: usize,
    pub probe_sent_ms: u64,
    pub rtt_ms: Option<u64>,
}

impl SrtlaRegistrationManager {
    /// Build a REG2 probe for every connection and advance into
    /// `WaitingForProbes`. Pure: returns `(conn_idx, packet)` for each probe;
    /// the shell transmits them on the matching sockets. Each probe is recorded
    /// as sent at `now` (optimistic — a probe datagram the kernel drops is
    /// handled by the probe-response timeout, not a send error). Returns empty
    /// when probing has already started or is not applicable.
    pub fn start_probing(
        &mut self,
        connections: &mut [SrtlaConnection],
        now: u64,
    ) -> SmallVec<(usize, [u8; SRTLA_TYPE_REG2_LEN]), 4> {
        let mut probes = SmallVec::new();
        if self.probing_state != ProbingState::NotStarted || self.active_connections > 0 {
            return probes;
        }

        info!("Starting RTT probing for {} connections", connections.len());
        self.probing_state = ProbingState::Probing;
        self.probe_results.clear();

        for (idx, conn) in connections.iter_mut().enumerate() {
            let pkt = conn.probe_reg2_packet(&self.probe_id, now);
            probes.push((idx, pkt));
            self.probe_results.push(ProbeResult {
                conn_idx: idx,
                probe_sent_ms: now,
                rtt_ms: None,
            });
        }

        if !self.probe_results.is_empty() {
            self.probing_state = ProbingState::WaitingForProbes;
            self.pending_timeout_at_ms = now + 2000;
            info!(
                "Waiting for probe responses from {} connections",
                self.probe_results.len()
            );
        } else {
            warn!("No connections available for probing - using first connection as fallback");
            self.probing_state = ProbingState::Complete;
        }

        probes
    }

    pub fn handle_probe_response(&mut self, conn_idx: usize, now: u64) {
        if self.probing_state != ProbingState::WaitingForProbes {
            return;
        }

        if let Some(result) = self
            .probe_results
            .iter_mut()
            .find(|r| r.conn_idx == conn_idx)
            && result.rtt_ms.is_none()
        {
            let rtt = now.saturating_sub(result.probe_sent_ms);
            result.rtt_ms = Some(rtt);
            info!(
                "Probe response from connection #{} (RTT: {}ms)",
                conn_idx, rtt
            );
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

// Test-only accessor methods for probing. Gated on `test-internals` (not just
// `test`) so the parent srtla_send crate can reach them cross-crate.
#[cfg(any(test, feature = "test-internals"))]
#[allow(dead_code)]
impl SrtlaRegistrationManager {
    pub fn probe_results_count(&self) -> usize {
        self.probe_results.len()
    }

    pub fn simulate_probe_result(&mut self, conn_idx: usize, rtt_ms: u64) {
        let now = now_ms();
        self.probe_results.push(ProbeResult {
            conn_idx,
            probe_sent_ms: now.saturating_sub(rtt_ms),
            rtt_ms: Some(rtt_ms),
        });
    }

    pub fn set_probing_state_waiting(&mut self) {
        self.probing_state = ProbingState::WaitingForProbes;
        self.pending_timeout_at_ms = now_ms() + 2000;
    }
}

/// Initialize probing fields for SrtlaRegistrationManager
pub(super) fn new_probe_id() -> [u8; SRTLA_ID_LEN] {
    use rand::RngCore;
    let mut probe_id = [0u8; SRTLA_ID_LEN];
    rand::rng().fill_bytes(&mut probe_id);
    probe_id
}

pub(super) fn new_probe_results() -> SmallVec<ProbeResult, 4> {
    SmallVec::new()
}

pub(super) fn default_probing_state() -> ProbingState {
    ProbingState::NotStarted
}
