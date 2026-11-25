//! Stall detection, fast silence pull, rejoin ramp/backoff, and Enhanced
//! quality/sole-carrier gate transitions for [`SrtlaConnection`].

use tracing::debug;

use super::SrtlaConnection;
use crate::config_snapshot::ConfigSnapshot;

/// Share of its natural score a link should compete with while ramping back
/// in after a stall gate, rising linearly from
/// [`crate::config_snapshot::STALL_REJOIN_RAMP_FLOOR`] to `1.0` over `ramp_ms`.
///
/// A gated link is the best-looking link in the pool, and that is an artefact.
/// It carries nothing but 1-in-N duplicate probes, so its in-flight count
/// drains to zero, while time-based window recovery keeps growing its window
/// because no traffic means no NAKs — and the score is `window / (in_flight +
/// 1)`. The instant the latch releases it therefore outranks every working
/// link by a wide margin, clears the switch hysteresis without effort, and
/// takes the whole stream. On a marginal link that refills the queue it just
/// drained, so it re-stalls and re-gates, which is a stable oscillation rather
/// than a recovery: librist measured the same loop on bonded cellular at a
/// ~15 s period, for as long as the link stayed marginal.
///
/// Ramping makes the link earn its share back under a growing load, so it
/// reveals congestion on the way up and settles at what it can actually carry.
/// `ramp_start == 0` (never gated) or an elapsed ramp both mean full score.
#[inline]
pub fn rejoin_ramp_multiplier(ramp_start_ms: u64, ramp_ms: u64, now_ms: u64) -> f64 {
    use crate::config_snapshot::STALL_REJOIN_RAMP_FLOOR;
    if ramp_start_ms == 0 || ramp_ms == 0 {
        return 1.0;
    }
    let elapsed = now_ms.saturating_sub(ramp_start_ms);
    if elapsed >= ramp_ms {
        return 1.0;
    }
    let progress = elapsed as f64 / ramp_ms as f64;
    (STALL_REJOIN_RAMP_FLOOR + (1.0 - STALL_REJOIN_RAMP_FLOOR) * progress)
        .clamp(STALL_REJOIN_RAMP_FLOOR, 1.0)
}

/// Rejoin-dwell multiplier to serve after the stall latch engages again, given
/// the one it was serving, how long the last rejoin lasted, and how long it had
/// to last to count as recovered.
///
/// The ramp above prices a rejoining link's share correctly but cannot stop it
/// rejoining in the first place, and that is the half of the oscillation it
/// does not reach: the dwell only asks for sustained delivery proof, which a
/// drained link supplies trivially. A link carrying nothing but probes keeps a
/// fresh `last_ack_or_rtt_sample_ms` from those probes alone, so the rejoin
/// condition is met on schedule no matter what the path can carry under load.
/// A link that genuinely cannot hold its share therefore rejoins, refloods,
/// re-stalls and re-gates on a fixed period, forever.
///
/// Waiting longer is the only lever left, so the dwell doubles every time a
/// rejoin fails to outlast `probation_ms`, capped at
/// [`crate::config_snapshot::STALL_REJOIN_BACKOFF_MAX`]. A rejoin that does
/// hold resets it immediately, so a link recovering from a transient spike is
/// never penalised. `released_at_ms == 0` (never rejoined) is the first
/// engagement and is always free. `0` and `1` both mean 1x.
#[inline]
pub fn stall_rejoin_backoff_next(
    backoff: u32,
    released_at_ms: u64,
    held_for_ms: u64,
    probation_ms: u64,
) -> u32 {
    if released_at_ms == 0 || held_for_ms >= probation_ms {
        return 1;
    }
    let doubled = if backoff <= 1 {
        2
    } else {
        backoff.saturating_mul(2)
    };
    doubled.min(crate::config_snapshot::STALL_REJOIN_BACKOFF_MAX)
}

/// Whether delivery proof measured at this round trip is worth anything,
/// against a one-way delivery deadline of `budget_ms`.
///
/// The rejoin dwell asks a gated link to hold fresh delivery proof, and treats
/// holding it as evidence the link can carry the stream again. On a gated link
/// that evidence is close to free: a completed keepalive round trip stamps
/// proof once a second, and a 38-byte control frame echoes fine on a path that
/// could not deliver a frame of video in time. The backoff in
/// [`stall_rejoin_backoff_next`] slows how often such a link is let back in but
/// cannot tell that it should not be let back in at all.
///
/// So require the round trip to fit the deadline as well as be recent. Half of
/// it is the one-way delay — the symmetric-path assumption used throughout this
/// module — and it has to land inside the receiver's buffer, because a packet
/// arriving after that is dropped rather than played whatever else is true
/// about the link. librist reached the same rule from the receiving end: a leg
/// queued deeper than the buffer keeps delivering packets that can never be
/// output, so those arrivals stopped counting as evidence of anything.
///
/// This gates only the *release*. Engaging the latch stays a question of
/// silence and backlog, so lateness does not become a second, redundant way to
/// gate a link — that verdict already belongs to the weak-link classifier.
/// Nothing here stops RTT being measured, so a link that genuinely recovers
/// crosses back over this bar on its own; one that never recovers simply stops
/// asking to.
///
/// A `budget_ms` of 0 means the peer never told us its buffer depth (see
/// [`crate::config_snapshot::ConfigSnapshot::negotiated_latency_ms`]), and a
/// non-positive RTT means the link has no baseline yet. Neither is grounds to
/// withhold proof, so both pass.
#[inline]
pub fn delivery_proof_is_timely(smooth_rtt_ms: f64, budget_ms: u32) -> bool {
    if budget_ms == 0 || smooth_rtt_ms <= 0.0 {
        return true;
    }
    (smooth_rtt_ms / 2.0) <= budget_ms as f64
}

impl SrtlaConnection {
    /// Whether this link carries enough unanswered payload for silence or
    /// stale proof to mean a stall: an in-flight backlog of at least
    /// `min_in_flight`, or unproven sends that are due (see
    /// [`Self::unproven_sends_due`]). The second arm is what sees a dead path
    /// at moderate rates, where NAKs and cumulative ACKs arriving over the
    /// healthy links keep draining the first. An idle link meets neither, so
    /// its keepalive cadence alone never gates it.
    #[inline]
    pub fn has_stall_load(&self, min_in_flight: i32, now_ms: u64) -> bool {
        self.in_flight_packets >= min_in_flight || self.unproven_sends_due(now_ms)
    }

    /// Whether this link owes proof for
    /// [`crate::config_snapshot::STALL_MIN_UNPROVEN_SENDS`] payload packets,
    /// the oldest of which has had a feedback round trip to be answered.
    ///
    /// The second condition keeps a burst handed to a quiet link (a keyframe
    /// after a lost keepalive echo, say) from counting before any ACK for it
    /// could have come back. On a link that carries traffic steadily the
    /// oldest unproven packet is about as old as its last proof, so this costs
    /// a dead link no detection time.
    #[inline]
    pub fn unproven_sends_due(&self, now_ms: u64) -> bool {
        use crate::config_snapshot::{
            STALL_MIN_UNPROVEN_SENDS, STALL_UNPROVEN_FEEDBACK_FLOOR_MS,
            STALL_UNPROVEN_FEEDBACK_RTT_MULT,
        };
        if self.unproven_sends < STALL_MIN_UNPROVEN_SENDS {
            return false;
        }
        let feedback_ms = ((self.get_smooth_rtt_ms().max(0.0) as u64)
            .saturating_mul(STALL_UNPROVEN_FEEDBACK_RTT_MULT))
        .max(STALL_UNPROVEN_FEEDBACK_FLOOR_MS);
        now_ms.saturating_sub(self.first_unproven_send_ms) >= feedback_ms
    }

    /// Effective delivery-proof staleness window in ms: RTT-adaptive between
    /// [`crate::config_snapshot::STALL_STALE_FLOOR_MS`] and the configured
    /// ceiling. Proof on a loaded link arrives every RTT (earned SRTLA ACKs),
    /// so the window scales with the path instead of always waiting a fixed
    /// 3 s — on a 50 ms link that fixed wait was ~3 s of payload committed to
    /// a black hole. No RTT baseline yet falls back to the ceiling; a ceiling
    /// configured below the floor wins (operator override).
    #[inline]
    pub fn effective_stall_stale_ms(&self, ceiling_ms: u64) -> u64 {
        use crate::config_snapshot::{STALL_STALE_FLOOR_MS, STALL_STALE_RTT_MULT};
        let srtt = self.get_smooth_rtt_ms();
        if srtt <= 0.0 {
            return ceiling_ms;
        }
        ((srtt as u64).saturating_mul(STALL_STALE_RTT_MULT))
            .max(STALL_STALE_FLOOR_MS)
            .min(ceiling_ms)
    }

    /// `stall_deselect` signal (pure read; never mutates). True for a connected
    /// link that carries stall load ([`Self::has_stall_load`]: an in-flight
    /// backlog of at least `min_in_flight`, or enough payload sent since its
    /// last proof) AND whose last delivery proof (earned-ACK or keepalive-RTT
    /// sample) is older than
    /// the effective staleness window ([`Self::effective_stall_stale_ms`] of
    /// `stale_ceiling_ms`). `now_ms` is the selection clock.
    ///
    /// A link that has produced no proof yet (`last_ack_or_rtt_sample_ms == 0`)
    /// is never stalled: a fresh burst before its first ACK must not be
    /// mistaken for a black hole. A genuinely dead-from-birth link is pruned by
    /// `is_timed_out`/`CONN_TIMEOUT`, not here. This is a selection penalty
    /// input ONLY — it never affects `is_timed_out`/re-registration/CONN_TIMEOUT.
    #[inline]
    pub fn is_stalled(&self, now_ms: u64, min_in_flight: i32, stale_ceiling_ms: u64) -> bool {
        self.stall_arm(now_ms, min_in_flight, stale_ceiling_ms)
            .is_some()
    }

    /// Which arm of [`Self::is_stalled`] fires, if any: the in-flight backlog
    /// (judged on the RTT-scaled window) or the unproven-send count (judged on
    /// at least [`crate::config_snapshot::STALL_UNPROVEN_STALE_FLOOR_MS`]).
    pub fn stall_arm(
        &self,
        now_ms: u64,
        min_in_flight: i32,
        stale_ceiling_ms: u64,
    ) -> Option<&'static str> {
        if !self.connected || self.last_ack_or_rtt_sample_ms == 0 {
            return None;
        }
        let proof_age_ms = now_ms.saturating_sub(self.last_ack_or_rtt_sample_ms);
        let stale_ms = self.effective_stall_stale_ms(stale_ceiling_ms);
        if self.in_flight_packets >= min_in_flight && proof_age_ms >= stale_ms {
            return Some("in-flight backlog");
        }
        // The unproven-send arm also fires on links whose only steady proof
        // is the keepalive echo, so its window must clear the keepalive
        // period with room for echo jitter (see
        // `STALL_UNPROVEN_STALE_FLOOR_MS`).
        (self.unproven_sends_due(now_ms)
            && proof_age_ms >= stale_ms.max(crate::config_snapshot::STALL_UNPROVEN_STALE_FLOOR_MS))
        .then_some("unproven sends")
    }

    /// Drive the asymmetric stall latch: quick to drop, conservative to
    /// rejoin. Called by `apply_stall_gate` on every selection pass.
    ///
    /// Engaging is immediate once [`Self::is_stalled`] fires. Rejoining
    /// requires an *uninterrupted* run of fresh delivery proof spanning
    /// [`crate::config_snapshot::STALL_REJOIN_DWELL_MULT`] x the effective
    /// staleness window: a single keepalive echo (or the backlog draining via
    /// cumulative ACKs carried by the healthy links) is not evidence the path
    /// can deliver again, and un-gating on it flaps payload back onto a
    /// still-marginal link. Proof going stale mid-run resets the run.
    pub fn update_stall_latch(&mut self, now_ms: u64, min_in_flight: i32, stale_ceiling_ms: u64) {
        // Escalation from the fast tier: a silence-pulled link receives no
        // payload, so cumulative ACKs drain the backlog and the raw
        // `is_stalled` in-flight precondition can never fire again. If the
        // pull is still held when the delivery proof goes fully stale, the
        // stall is sustained, not a micro-pause — latch it so recovery goes
        // through the dwell and probes instead of a cold readmit.
        //
        // A pull that engaged on unproven sends alone escalates on the same
        // floor as the unproven-send arm: before it, a correlated gap in the
        // link's return traffic (or one late echo) is not yet evidence that
        // the path is dead, and the pull is released as soon as the link
        // speaks again. A pull that engaged on a full backlog keeps the
        // RTT-scaled window even after the backlog drains.
        let mut escalate_after_ms = self.effective_stall_stale_ms(stale_ceiling_ms);
        if !self.silence_pull_on_backlog {
            escalate_after_ms =
                escalate_after_ms.max(crate::config_snapshot::STALL_UNPROVEN_STALE_FLOOR_MS);
        }
        let proof_age_ms = now_ms.saturating_sub(self.last_ack_or_rtt_sample_ms);
        let escalates = self.silence_pulled
            && self.last_ack_or_rtt_sample_ms != 0
            && proof_age_ms >= escalate_after_ms;
        let arm = self
            .stall_arm(now_ms, min_in_flight, stale_ceiling_ms)
            .or(escalates.then_some("silence pull"));
        if let Some(arm) = arm {
            if self.stall_latched_since_ms == 0 {
                // Judge the rejoin that just ended before starting a new gate:
                // one that could not outlast its probation earns a longer wait
                // for the next retry, one that did clears the penalty outright.
                let probation_ms = self
                    .effective_stall_stale_ms(stale_ceiling_ms)
                    .saturating_mul(crate::config_snapshot::STALL_REJOIN_PROBATION_MULT);
                self.stall_rejoin_backoff = stall_rejoin_backoff_next(
                    self.stall_rejoin_backoff,
                    self.stall_released_at_ms,
                    now_ms.saturating_sub(self.stall_released_at_ms),
                    probation_ms,
                );
                debug!(
                    "{}: stall latch engaged ({arm}: {} in flight, {} unproven sends, proof {} ms \
                     old), rejoin dwell now {}x",
                    self.label,
                    self.in_flight_packets,
                    self.unproven_sends,
                    proof_age_ms,
                    self.stall_rejoin_backoff
                );
                self.stall_latched_since_ms = now_ms;
                self.stall_gate_events += 1;
            }
            self.stall_recovery_since_ms = 0;
            return;
        }
        if self.stall_latched_since_ms == 0 {
            return;
        }

        let stale_ms = self.effective_stall_stale_ms(stale_ceiling_ms);
        // A gated link carries no payload, so its steady proof is the keepalive
        // echo and freshness is judged on a window that outlasts one keepalive
        // period (see `STALL_REJOIN_PROOF_FLOOR_MS`).
        let proof_window_ms = stale_ms.max(crate::config_snapshot::STALL_REJOIN_PROOF_FLOOR_MS);
        // Recent *and* fast enough to matter: see `delivery_proof_is_timely`.
        // Timeliness is judged on the recent RTT floor (`rejoin_rtt_ms`), not
        // the smoothed RTT. Both conditions restart the dwell, so a link only
        // rejoins on a run of proof that a packet of real payload could have
        // ridden.
        let proof_fresh = self.last_ack_or_rtt_sample_ms != 0
            && now_ms.saturating_sub(self.last_ack_or_rtt_sample_ms) < proof_window_ms
            && delivery_proof_is_timely(self.rejoin_rtt_ms(), self.delay_budget_ms);
        if !proof_fresh {
            self.stall_recovery_since_ms = 0;
            return;
        }
        if self.stall_recovery_since_ms == 0 {
            self.stall_recovery_since_ms = now_ms;
        }
        let base_dwell_ms =
            stale_ms.saturating_mul(crate::config_snapshot::STALL_REJOIN_DWELL_MULT);
        // The wait stretches while rejoins keep failing to hold, so a link that
        // cannot carry its share is still retried — just far enough apart that
        // the stream stops paying a transition for every attempt. Only the
        // *wait* scales: the ramp below stays at the base dwell, since how
        // gently a link should be reloaded does not depend on how long it sat
        // out, and scaling it too would leave a link at the ramp floor for
        // minutes after it finally recovered.
        let dwell_ms = base_dwell_ms.saturating_mul(self.stall_rejoin_backoff.max(1) as u64);
        if now_ms.saturating_sub(self.stall_recovery_since_ms) >= dwell_ms {
            debug!(
                "{}: stall latch released after sustained proof ({}ms dwell), ramping share back \
                 over {}ms",
                self.label, dwell_ms, base_dwell_ms
            );
            self.stall_latched_since_ms = 0;
            self.stall_recovery_since_ms = 0;
            self.stall_released_at_ms = now_ms;
            // Arm the share ramp: the link has proven it can deliver probes,
            // not that it can carry the stream (see `rejoin_ramp_multiplier`).
            self.arm_rejoin_ramp(now_ms, base_dwell_ms, true);
        }
    }

    /// Start the post-rejoin share ramp, unless one is already running.
    ///
    /// Called wherever a link stops being held out of the payload rotation:
    /// the stall latch releasing, a sole-carrier election ending, or a quality
    /// exclusion lifting. All three leave the link with a drained backlog and
    /// an inflated score it did not earn, which is what the ramp prices out
    /// (see [`rejoin_ramp_multiplier`]).
    ///
    /// An in-progress ramp is never restarted. The gates above can re-arm at
    /// classifier cadence when a verdict flaps, and restarting each time would
    /// pin a link at the ramp floor for as long as the flapping lasts — the
    /// link would never finish earning its share back, which is its own kind
    /// of starvation. `from_stall_gate` records who armed it, so disabling the
    /// stall guard at runtime can drop the ramps that guard created without
    /// touching anyone else's.
    #[inline]
    pub(crate) fn arm_rejoin_ramp(&mut self, now_ms: u64, ramp_ms: u64, from_stall_gate: bool) {
        if self.rejoin_ramp_multiplier(now_ms) < 1.0 {
            return;
        }
        self.stall_rejoin_ramp_start_ms = now_ms;
        self.stall_rejoin_ramp_ms = ramp_ms;
        self.stall_rejoin_ramp_from_stall_gate = from_stall_gate;
    }

    /// Refresh liveness timeout, delivery budget, silence pull and stall latch
    /// for this selection pass. Returns `true` when this link is a healthy
    /// carrier (not timed out, schedulable, not latched, not silence-pulled).
    pub(crate) fn refresh_stall_state(&mut self, now_ms: u64, config: &ConfigSnapshot) -> bool {
        self.conn_timeout_ms = config.conn_timeout_ms;
        self.delay_budget_ms = config.negotiated_latency_ms;
        if !config.stall_deselect {
            self.stall_gated = false;
            self.silence_pulled = false;
            self.clear_stall_latch();
            return false;
        }
        // Pull first: the latch's escalation path reads the fresh pull state.
        self.update_silence_pull(
            now_ms,
            config.stall_min_in_flight,
            config.stall_ack_stale_ms,
        );
        self.update_stall_latch(
            now_ms,
            config.stall_min_in_flight,
            config.stall_ack_stale_ms,
        );
        !self.is_timed_out(now_ms)
            && self.is_schedulable()
            && !self.stall_latched()
            && !self.silence_pulled
    }

    /// Apply the pool-wide stall-gating decision computed after every link ran
    /// [`Self::refresh_stall_state`].
    #[inline]
    pub(crate) fn apply_stall_gate_decision(&mut self, any_healthy: bool) {
        self.stall_gated = any_healthy && (self.stall_latched() || self.silence_pulled);
    }

    /// Update this link's sole-carrier election state for the current pass.
    pub(crate) fn set_sole_carrier(
        &mut self,
        won: bool,
        handover: bool,
        eligible_loser: bool,
        now_ms: u64,
        ramp_ms: u64,
    ) {
        if won {
            if self.sole_carrier_excluded {
                self.arm_rejoin_ramp(now_ms, ramp_ms, false);
            }
            if !self.sole_carrier {
                debug!(
                    "{}: elected sole carrier (every link is quality gated)",
                    self.label
                );
                self.sole_carrier = true;
                if handover || self.sole_carrier_since_ms == 0 {
                    self.sole_carrier_since_ms = now_ms;
                }
                if handover {
                    self.sole_carrier_elections += 1;
                }
            }
            self.sole_carrier_excluded = false;
        } else {
            self.sole_carrier = false;
            if handover {
                self.sole_carrier_since_ms = 0;
            }
            self.sole_carrier_excluded = eligible_loser;
        }
    }

    /// Clear this link's sole-carrier state, arming the rejoin ramp if this
    /// ends an exclusion.
    #[inline]
    pub(crate) fn release_sole_carrier(&mut self, now_ms: u64, ramp_ms: u64) {
        if self.sole_carrier_excluded {
            self.arm_rejoin_ramp(now_ms, ramp_ms, false);
            self.sole_carrier_excluded = false;
        }
        self.sole_carrier = false;
    }

    /// Update this link's quality-exclusion flag, arming the rejoin ramp on
    /// the falling edge.
    #[inline]
    pub(crate) fn set_quality_excluded(&mut self, excluded: bool, now_ms: u64, ramp_ms: u64) {
        if self.quality_excluded && !excluded {
            self.arm_rejoin_ramp(now_ms, ramp_ms, false);
        }
        self.quality_excluded = excluded;
    }

    /// Whether this link is considered late enough to be held off unique
    /// payload while a healthier sibling can carry.
    #[inline(always)]
    pub(crate) fn is_late_for_exclusion(&self) -> bool {
        self.loss_degraded || (self.weak && self.weak_for_delay)
    }

    /// Whether this link is eligible to carry must-land priority traffic
    /// (excluding the transient in-flight cap checked by the caller).
    #[inline(always)]
    pub(crate) fn is_eligible_for_priority_override(&self, now_ms: u64) -> bool {
        self.connected
            && self.is_schedulable()
            && !self.is_timed_out(now_ms)
            && !self.stall_gated
            && !self.quality_excluded
    }

    /// Whether this link is currently held out of the payload rotation on
    /// quality grounds. The shell reads this to decide which links get
    /// duplicate probes; stats and selection read the underlying flags.
    #[inline(always)]
    pub fn is_quality_excluded(&self) -> bool {
        self.quality_excluded
    }

    /// Drop every flag owned by the Enhanced quality gates.
    ///
    /// Only Enhanced selection maintains these, and the scheduling mode is
    /// switchable at runtime, so Classic has to clear them rather than leave
    /// them frozen at whatever they held when the mode changed. The rejoin
    /// ramp is *not* cleared here: it is a scoring de-rate that Classic
    /// ignores anyway, and it should still be running if the mode switches
    /// back before it elapses.
    pub(crate) fn clear_quality_gate_state(&mut self) {
        self.quality_excluded = false;
        self.sole_carrier = false;
        self.sole_carrier_excluded = false;
        self.sole_carrier_since_ms = 0;
    }

    /// Whether this link currently holds the sole-carrier role (stats export).
    #[inline(always)]
    pub fn is_sole_carrier(&self) -> bool {
        self.sole_carrier
    }

    /// Whether this link is currently held out of the rotation because a
    /// sibling holds the sole-carrier role (stats export).
    #[inline(always)]
    pub fn is_sole_carrier_excluded(&self) -> bool {
        self.sole_carrier_excluded
    }

    /// Cumulative sole-carrier handovers *from another link* over its life.
    #[inline(always)]
    pub fn sole_carrier_elections(&self) -> u64 {
        self.sole_carrier_elections
    }

    /// Fraction of its natural score this link should compete with right now,
    /// in `[STALL_REJOIN_RAMP_FLOOR, 1.0]`. `1.0` whenever no ramp is running.
    #[inline]
    pub fn rejoin_ramp_multiplier(&self, now_ms: u64) -> f64 {
        rejoin_ramp_multiplier(
            self.stall_rejoin_ramp_start_ms,
            self.stall_rejoin_ramp_ms,
            now_ms,
        )
    }

    /// Rejoin-dwell multiplier this link is currently serving (stats/tests).
    /// `1` whenever no backoff has been earned.
    #[inline(always)]
    pub fn stall_rejoin_backoff(&self) -> u32 {
        self.stall_rejoin_backoff.max(1)
    }

    /// Whether a post-rejoin share ramp is currently running (stats/telemetry).
    #[inline(always)]
    pub fn is_rejoin_ramping(&self, now_ms: u64) -> bool {
        self.rejoin_ramp_multiplier(now_ms) < 1.0
    }

    /// Whether the stall latch is currently engaged (independent of whether a
    /// healthy alternative exists to make it an active routing gate).
    #[inline(always)]
    pub fn stall_latched(&self) -> bool {
        self.stall_latched_since_ms != 0
    }

    /// Clear the stall latch without touching the event counter. Used when the
    /// guard is disabled at runtime so selection returns to baseline instantly.
    pub(crate) fn clear_stall_latch(&mut self) {
        self.stall_latched_since_ms = 0;
        self.stall_recovery_since_ms = 0;
        // With the guard off there are no rejoins to judge, so the escalation
        // must not survive to lengthen the first dwell if it is turned back on.
        self.stall_released_at_ms = 0;
        self.stall_rejoin_backoff = 0;
        // Including any ramp this guard armed: with it off, its contribution
        // to scoring must be gone. Ramps armed by the sole-carrier election or
        // the quality exclusion survive — those gates are independent of this
        // one and still running, and wiping their ramps here would silently
        // undo them on every selection pass.
        if self.stall_rejoin_ramp_from_stall_gate {
            self.stall_rejoin_ramp_start_ms = 0;
            self.stall_rejoin_ramp_ms = 0;
            self.stall_rejoin_ramp_from_stall_gate = false;
        }
    }

    /// Whether this link is currently stall-gated (routing view; stats export).
    #[inline(always)]
    pub fn is_stall_gated(&self) -> bool {
        self.stall_gated
    }

    /// Cumulative stall-latch engagements over this connection's life.
    #[inline(always)]
    pub fn stall_gate_events(&self) -> u64 {
        self.stall_gate_events
    }

    /// Duplicate-probe cadence on a gated link: true once per
    /// [`crate::config_snapshot::STALL_PROBE_ONE_IN_N`] calls. The caller
    /// invokes this once per routed data packet for each gated link, mirroring
    /// librist's per-leg trickle counter.
    #[inline]
    pub fn stall_probe_due(&mut self) -> bool {
        self.stall_probe_counter += 1;
        if self.stall_probe_counter >= crate::config_snapshot::STALL_PROBE_ONE_IN_N {
            self.stall_probe_counter = 0;
            return true;
        }
        false
    }

    /// The round trip the rejoin timeliness check judges a latched link by:
    /// the lower of the smoothed RTT and the 3 s windowed minimum. Receivers
    /// that batch SRTLA ACKs (BELABOX acknowledges every 10 packets per
    /// connection) hold the ACK for a packet sent before an outage, or for a
    /// latched link's sparse probes, until the batch fills. That sample comes
    /// back as long as the outage or the batch and inflates the smoothed RTT
    /// for seconds. The sample that completes each batch is answered at once,
    /// so the floor stays at the path RTT, while a genuinely late link has no
    /// fast samples and its floor stays high.
    #[inline]
    pub fn rejoin_rtt_ms(&self) -> f64 {
        self.rtt.rejoin_rtt_ms()
    }

    /// RTT-scaled window (ms) of total inbound silence after which a loaded
    /// link is transiently pulled: `max(floor, 2 x smoothed RTT)`, capped at
    /// the effective staleness window (beyond that the latch owns the
    /// decision, so the two tiers cannot disagree about who acts).
    #[inline]
    pub fn silence_pull_window_ms(&self, stale_ceiling_ms: u64) -> u64 {
        use crate::config_snapshot::{SILENCE_PULL_FLOOR_MS, SILENCE_PULL_RTT_MULT};
        let srtt = self.get_smooth_rtt_ms();
        let base = if srtt <= 0.0 {
            SILENCE_PULL_FLOOR_MS
        } else {
            ((srtt as u64).saturating_mul(SILENCE_PULL_RTT_MULT)).max(SILENCE_PULL_FLOOR_MS)
        };
        base.min(self.effective_stall_stale_ms(stale_ceiling_ms))
    }

    /// Fast silence signal (pure read): a connected link with stall load
    /// ([`Self::has_stall_load`]) that has received *nothing* — under
    /// load, SRTLA ACKs arrive continuously, so total silence is abnormal —
    /// for [`Self::silence_pull_window_ms`]. The routing pull this drives is
    /// transient and self-clearing (any inbound byte refreshes
    /// `last_received`), the deliberate opposite of the sticky latch: fast to
    /// react during a stall the link may ride out (e.g. a HARQ pause), free
    /// to readmit the moment the link speaks. Never a liveness signal.
    #[inline]
    pub fn is_briefly_silent(
        &self,
        now_ms: u64,
        min_in_flight: i32,
        stale_ceiling_ms: u64,
    ) -> bool {
        if !self.connected || !self.has_stall_load(min_in_flight, now_ms) {
            return false;
        }
        let Some(last_received) = self.last_received else {
            return false;
        };
        now_ms.saturating_sub(last_received) >= self.silence_pull_window_ms(stale_ceiling_ms)
    }

    /// Drive the silence-pull flag and its rising-edge counter. Called by
    /// `apply_stall_gate` on every selection pass, BEFORE the latch update
    /// (the latch escalates from this flag).
    ///
    /// The pull engages only on the loaded-and-silent signal, but releases
    /// only when the link actually SPEAKS (a fresh inbound byte) — not when
    /// its backlog drains. Cumulative SRT ACKs delivered via the healthy
    /// links clear a pulled link's in-flight within an RTT, so releasing on
    /// drain would readmit the still-silent link, refill it, and re-pull it
    /// in a tight duty cycle that both dribbles payload into the hole and
    /// starves the latch of its in-flight precondition.
    pub(crate) fn update_silence_pull(
        &mut self,
        now_ms: u64,
        min_in_flight: i32,
        stale_ceiling_ms: u64,
    ) {
        if self.is_briefly_silent(now_ms, min_in_flight, stale_ceiling_ms) {
            if !self.silence_pulled {
                self.silence_pulls += 1;
                self.silence_pull_on_backlog = self.in_flight_packets >= min_in_flight;
                debug!(
                    "{}: silence pull engaged ({}: {} in flight, {} unproven sends)",
                    self.label,
                    if self.silence_pull_on_backlog {
                        "in-flight backlog"
                    } else {
                        "unproven sends"
                    },
                    self.in_flight_packets,
                    self.unproven_sends
                );
            }
            self.silence_pulled = true;
            return;
        }
        if !self.silence_pulled {
            return;
        }
        let window = self.silence_pull_window_ms(stale_ceiling_ms);
        let spoke = self
            .last_received
            .is_some_and(|lr| now_ms.saturating_sub(lr) < window);
        if spoke || !self.connected {
            self.silence_pulled = false;
        }
        // Otherwise: drained but still mute — hold the pull until the link
        // speaks or the latch takes over.
    }

    /// Cumulative silence-pull engagements over this connection's life.
    #[inline(always)]
    pub fn silence_pulls(&self) -> u64 {
        self.silence_pulls
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_snapshot::STALL_REJOIN_RAMP_FLOOR;

    #[test]
    fn ramp_is_inert_when_no_ramp_is_running() {
        // Never gated.
        assert_eq!(rejoin_ramp_multiplier(0, 1000, 500), 1.0);
        // Armed with a zero-length dwell (degenerate config).
        assert_eq!(rejoin_ramp_multiplier(100, 0, 500), 1.0);
        // Ramp already elapsed.
        assert_eq!(rejoin_ramp_multiplier(100, 1000, 1100), 1.0);
        assert_eq!(rejoin_ramp_multiplier(100, 1000, 9999), 1.0);
    }

    #[test]
    fn ramp_rises_linearly_from_the_floor_to_full() {
        assert_eq!(
            rejoin_ramp_multiplier(100, 1000, 100),
            STALL_REJOIN_RAMP_FLOOR
        );
        let quarter = rejoin_ramp_multiplier(100, 1000, 350);
        let half = rejoin_ramp_multiplier(100, 1000, 600);
        let three_quarters = rejoin_ramp_multiplier(100, 1000, 850);
        assert!((quarter - (STALL_REJOIN_RAMP_FLOOR + 0.95 * 0.25)).abs() < 1e-9);
        assert!((half - (STALL_REJOIN_RAMP_FLOOR + 0.95 * 0.50)).abs() < 1e-9);
        assert!((three_quarters - (STALL_REJOIN_RAMP_FLOOR + 0.95 * 0.75)).abs() < 1e-9);
        assert!(quarter < half && half < three_quarters && three_quarters < 1.0);
    }

    #[test]
    fn ramp_never_scores_a_rejoining_link_to_zero() {
        // The link has to carry something, or it can never reveal how it
        // behaves under load and the ramp would gate it forever.
        for elapsed in 0..10 {
            let m = rejoin_ramp_multiplier(1000, 100_000, 1000 + elapsed);
            assert!(m >= STALL_REJOIN_RAMP_FLOOR, "ramp dropped to {m}");
        }
    }

    #[test]
    fn ramp_tolerates_a_clock_that_went_backwards() {
        assert_eq!(
            rejoin_ramp_multiplier(1000, 500, 900),
            STALL_REJOIN_RAMP_FLOOR
        );
    }
}
