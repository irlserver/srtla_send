//! Two-link simulations of a path that dies while its link stays registered.
//!
//! The unit tests in `stall_deselect_tests` set a link's backlog and proof
//! stamps by hand. These drive the real sender logic with a small model of
//! the rest of the system instead, because the failure they guard against
//! only shows up in the interplay: NAKs and cumulative SRT ACKs that reach the
//! sender over the healthy link keep draining the dead link's packet log, so
//! its in-flight count never builds the backlog the stall guard used to
//! require, and the scheduler kept feeding the dead path for as long as the
//! link stayed registered.
//!
//! The sender side is production code on a simulated clock: link choice with
//! the best-path override for retransmits (`choose_link`), the 15 ms send
//! batching, duplicate probes on held-out links, the sequence tracker, ACK and
//! NAK dispatch (`apply_link_feedback`) and the 1 s housekeeping tick.
//!
//! The model follows `srtla_rec` and a live-mode SRT receiver:
//! - every data packet a link delivers counts toward that link's SRTLA ACK,
//!   sent back on the same link after every tenth one;
//! - keepalives are echoed on the link they arrived on;
//! - SRT ACKs (cumulative, every 10 ms) are broadcast on every link the
//!   receiver still has registered, including one whose inbound path is dead;
//! - NAKs go out on the link that delivered most recently, once on gap
//!   detection and again every 50 ms while the packet is still missing;
//! - a packet still missing when its 2000 ms latency budget runs out is
//!   dropped by the receiver. Those drops are what freezes the picture.
//!
//! Return paths can carry random extra delay, separately for keepalive echoes
//! and for the receiver's ACKs. With unequal round trips that reproduces the
//! race real links run: the broadcast SRT ACK over the faster path prunes the
//! slower link's packets before that link's own SRTLA ACK arrives. A link can
//! also have a capacity, which queues (and past 300 ms drops) what is sent on
//! it, so a faster link cannot take the whole stream.
//!
//! Loss and jitter use a fixed-seed generator so every run is identical.

#[cfg(test)]
mod tests {
    use std::cmp::Reverse;
    use std::collections::{BTreeMap, BinaryHeap, HashMap};

    use smallvec::SmallVec;
    use srtla_core::connection::{SrtlaConnection, SrtlaIncoming};
    use srtla_core::mode::SchedulingMode;
    use srtla_core::utils::now_ms;

    use crate::config::ConfigSnapshot;
    use crate::sender::{SequenceTracker, apply_link_feedback, choose_link};
    use crate::test_helpers::create_test_connections;

    /// SRT receive latency, as CeraUI configures it.
    const LATENCY_MS: u64 = 2000;
    /// CeraUI's `--conn-timeout-ms`.
    const CONN_TIMEOUT_MS: u64 = 15_000;
    /// `srtla_rec` forgets a link after 10 s of silence, checked every 3 s.
    const RECEIVER_CONN_TIMEOUT_MS: u64 = 11_000;
    const SRT_ACK_PERIOD_MS: u64 = 10;
    const NAK_REPORT_PERIOD_MS: u64 = 50;
    const RECV_ACK_INT: usize = 10;
    /// `HOUSEKEEPING_INTERVAL_MS` in the sender loop.
    const HOUSEKEEPING_MS: u64 = 1000;
    /// A capacity-limited link drops what would wait longer than this.
    const QUEUE_LIMIT_MS: f64 = 300.0;

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum PathState {
        /// Delivers both ways, each direction losing `loss_permille` packets.
        Alive { loss_permille: u32 },
        /// Nothing from the sender reaches the receiver, but the receiver's
        /// own packets (SRT ACK broadcasts) still reach the sender until the
        /// receiver times the link out. This is a cut on the receiver's input,
        /// or an uplink that only fails outbound.
        DeadInbound,
        /// Nothing gets through in either direction: an AP that vanished
        /// without a deauth, or an upstream that went dark.
        Blackhole,
    }

    #[derive(Clone, Copy)]
    struct Step {
        at_ms: u64,
        link: usize,
        state: PathState,
    }

    #[derive(Debug)]
    enum Ev {
        Arrive { link: usize, seq: u32 },
        KeepaliveArrive { link: usize, pkt: Vec<u8> },
        SrtlaAck { link: usize, seqs: Vec<u32> },
        KeepaliveEcho { link: usize, pkt: Vec<u8> },
        SrtAck { link: usize, last_acked: u32 },
        Nak { link: usize, seq: u32 },
    }

    struct Rng(u64);
    impl Rng {
        fn permille(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % 1000) as u32
        }
    }

    #[derive(Default, Debug)]
    struct Outcome {
        /// Simulated ms from the first cut until the dead link was held out.
        gated_after_ms: Option<u64>,
        /// Payload packets (not probes) put on the cut link while it was cut.
        payload_into_dead_path: u64,
        /// Of those, sent after it was first held out: new packets and SRT
        /// retransmits.
        fresh_after_gate: u64,
        retransmits_after_gate: u64,
        /// Packets the receiver had to drop because they missed the latency.
        receiver_drops: u64,
        /// Simulated ms from the path's recovery until it was readmitted.
        readmitted_after_ms: Option<u64>,
        stall_gate_events: u64,
        /// Simulated ms from the cut to the last payload packet (new or
        /// retransmit, not probe) put on the cut link before it healed.
        last_payload_after_cut_ms: Option<u64>,
        /// Payload packets put on the cut link more than 5 s after the cut.
        payload_after_5s: u64,
    }

    struct Sim {
        conns: SmallVec<SrtlaConnection, 4>,
        classic: bool,
        rtt_ms: [u64; 2],
        state: [PathState; 2],
        /// Packets per second each link can carry; 0 = unlimited.
        cap_pps: [u64; 2],
        link_free_at: [f64; 2],
        last_arrival: [u64; 2],
        rng: Rng,
        events: BinaryHeap<Reverse<(u64, u64)>>,
        payloads: HashMap<u64, Ev>,
        next_ev: u64,
        // Sender side.
        tracker: SequenceTracker,
        next_seq: u32,
        last_idx: Option<usize>,
        // Receiver side.
        first_sent: BTreeMap<u32, u64>,
        missing: BTreeMap<u32, u64>,
        received_upto: u32,
        max_seen: Option<u32>,
        recv_batch: [Vec<u32>; 2],
        last_active_link: usize,
        receiver_drops: u64,
        /// Extra random delay (0..=n ms) on keepalive echoes and on the
        /// receiver's ACKs, per packet.
        echo_jitter_ms: u64,
        ack_jitter_ms: u64,
        /// Per-link payload packets sent, ms spent held out (latched or
        /// pulled), and worst rejoin backoff seen.
        payload_sent: [u64; 2],
        held_out_ms: [u64; 2],
        max_backoff: [u32; 2],
        /// Longest a link went without delivery proof after the first 5 s.
        max_proof_age_ms: [u64; 2],
        /// Set once the watched link has been held out during a cut.
        watch_gated: bool,
        watch_cut: bool,
        watch: usize,
        fresh_after_gate: u64,
        retransmits_after_gate: u64,
        cut_started: u64,
        last_payload_after_cut_ms: Option<u64>,
        dead_payload_ms: Vec<u64>,
        /// Per link, a window (simulated ms from the start, end exclusive) in
        /// which the receiver's SRTLA ACKs on that link are lost while
        /// everything else on it gets through.
        srtla_ack_gap: [Option<(u64, u64)>; 2],
        /// Per link, a window in which nothing the receiver sends back on
        /// that link arrives (SRTLA ACKs, keepalive echoes, SRT ACK and NAK
        /// reports), while what the sender puts on it still gets through.
        return_gap: [Option<(u64, u64)>; 2],
        /// Per link, each hold-out as (start, end) in simulated ms from the
        /// start; `None` while it lasts.
        gate_spans: [Vec<(u64, Option<u64>)>; 2],
        t0: u64,
    }

    impl Sim {
        fn new(rtt_ms: [u64; 2], seed: u64) -> Self {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let conns = rt.block_on(create_test_connections(2));
            Self {
                conns,
                classic: false,
                rtt_ms,
                state: [PathState::Alive { loss_permille: 0 }; 2],
                cap_pps: [0; 2],
                link_free_at: [0.0; 2],
                last_arrival: [0; 2],
                rng: Rng(seed),
                events: BinaryHeap::new(),
                payloads: HashMap::new(),
                next_ev: 0,
                tracker: SequenceTracker::new(),
                next_seq: 0,
                last_idx: None,
                first_sent: BTreeMap::new(),
                missing: BTreeMap::new(),
                received_upto: 0,
                max_seen: None,
                recv_batch: [Vec::new(), Vec::new()],
                last_active_link: 0,
                receiver_drops: 0,
                echo_jitter_ms: 0,
                ack_jitter_ms: 0,
                payload_sent: [0; 2],
                held_out_ms: [0; 2],
                max_backoff: [0; 2],
                max_proof_age_ms: [0; 2],
                watch_gated: false,
                watch_cut: false,
                watch: 0,
                fresh_after_gate: 0,
                retransmits_after_gate: 0,
                cut_started: 0,
                last_payload_after_cut_ms: None,
                dead_payload_ms: Vec::new(),
                srtla_ack_gap: [None; 2],
                return_gap: [None; 2],
                gate_spans: [Vec::new(), Vec::new()],
                t0: 0,
            }
        }

        fn config(&self) -> ConfigSnapshot {
            ConfigSnapshot {
                mode: if self.classic {
                    SchedulingMode::Classic
                } else {
                    SchedulingMode::Enhanced
                },
                quality_enabled: !self.classic,
                conn_timeout_ms: CONN_TIMEOUT_MS,
                negotiated_latency_ms: LATENCY_MS as u32,
                ..ConfigSnapshot::default()
            }
        }

        fn schedule(&mut self, at: u64, ev: Ev) {
            self.next_ev += 1;
            self.payloads.insert(self.next_ev, ev);
            self.events.push(Reverse((at, self.next_ev)));
        }

        fn jitter(&mut self, max_ms: u64) -> u64 {
            if max_ms == 0 {
                0
            } else {
                self.rng.permille() as u64 % (max_ms + 1)
            }
        }

        /// One-way delay back to the sender on `link` for a receiver ACK.
        fn ack_delay(&mut self, link: usize) -> u64 {
            self.rtt_ms[link] / 2 + self.jitter(self.ack_jitter_ms)
        }

        /// Whether one packet makes it from sender to receiver on `link`.
        fn forward_ok(&mut self, link: usize) -> bool {
            match self.state[link] {
                PathState::Alive { loss_permille } => self.rng.permille() >= loss_permille,
                _ => false,
            }
        }

        /// Whether one packet makes it from receiver to sender on `link`.
        fn return_ok(&mut self, link: usize, now: u64) -> bool {
            let ms = now.saturating_sub(self.t0);
            if self.return_gap[link].is_some_and(|(from, to)| (from..to).contains(&ms)) {
                return false;
            }
            match self.state[link] {
                PathState::Alive { loss_permille } => self.rng.permille() >= loss_permille,
                PathState::DeadInbound => {
                    now.saturating_sub(self.last_arrival[link]) < RECEIVER_CONN_TIMEOUT_MS
                }
                PathState::Blackhole => false,
            }
        }

        /// When a packet sent on `link` now reaches the receiver, or `None`
        /// if the link loses it or its queue is full.
        fn forward_arrival(&mut self, link: usize, now: u64) -> Option<u64> {
            let mut leave = now as f64;
            if self.cap_pps[link] > 0 {
                let start = self.link_free_at[link].max(now as f64);
                if start - now as f64 > QUEUE_LIMIT_MS {
                    return None;
                }
                self.link_free_at[link] = start + 1000.0 / self.cap_pps[link] as f64;
                leave = start;
            }
            if !self.forward_ok(link) {
                return None;
            }
            Some(leave.ceil() as u64 + self.rtt_ms[link] / 2)
        }

        fn data_packet(seq: u32, retransmit: bool) -> [u8; 16] {
            let mut pkt = [0u8; 16];
            pkt[..4].copy_from_slice(&seq.to_be_bytes());
            if retransmit {
                srtla_protocol::set_srt_data_retransmit(&mut pkt);
            }
            pkt
        }

        /// Drain `link`'s send batch onto the wire.
        fn flush(&mut self, link: usize, now: u64) {
            let batch = self.conns[link].take_batch(now);
            if batch.is_empty() {
                return;
            }
            self.conns[link].note_sent(now);
            for (data, _, _) in batch.iter() {
                let Some(seq) = srtla_protocol::get_srt_sequence_number(data) else {
                    continue;
                };
                if let Some(at) = self.forward_arrival(link, now) {
                    self.schedule(at, Ev::Arrive { link, seq });
                }
            }
        }

        /// Route one payload packet the way the shell does: choose a link
        /// (retransmits get the best-path override), queue it, and queue a
        /// duplicate probe on each held-out link when its cadence is due.
        fn send_payload(&mut self, now: u64, cfg: &ConfigSnapshot, seq: u32, rtx: bool) -> usize {
            let idx = choose_link(&mut self.conns, self.last_idx, now, cfg, rtx)
                .expect("a link is always selectable");
            self.last_idx = Some(idx);
            let pkt = Self::data_packet(seq, rtx);
            self.tracker.insert(seq, self.conns[idx].conn_id, now);
            self.payload_sent[idx] += 1;
            if idx == self.watch && self.watch_cut {
                self.last_payload_after_cut_ms = Some(now - self.cut_started);
                self.dead_payload_ms.push(now - self.cut_started);
            }
            if idx == self.watch && self.watch_cut && self.watch_gated {
                if rtx {
                    self.retransmits_after_gate += 1;
                } else {
                    self.fresh_after_gate += 1;
                }
            }
            if self.conns[idx].queue_data_packet(&pkt, Some(seq), now) {
                self.flush(idx, now);
            }
            for i in 0..self.conns.len() {
                let c = &self.conns[i];
                if i == idx || !(c.is_stall_gated() || c.is_quality_excluded()) || !c.connected {
                    continue;
                }
                if !self.conns[i].stall_probe_due() {
                    continue;
                }
                let probe = Self::data_packet(seq, true);
                if self.conns[i].queue_probe_packet(&probe, seq, now) {
                    self.flush(i, now);
                }
            }
            idx
        }

        fn feedback(&mut self, link: usize, now: u64, incoming: SrtlaIncoming) {
            self.conns[link].last_received = Some(now);
            let classic = self.classic;
            apply_link_feedback(
                link,
                &mut self.conns,
                &self.tracker,
                classic,
                &incoming,
                now,
            );
        }

        fn receiver_tick(&mut self, now: u64, ms: u64) {
            // Latency deadline: a packet still missing is dropped for good.
            let late: Vec<u32> = self
                .missing
                .keys()
                .copied()
                .filter(|s| self.first_sent[s] + LATENCY_MS <= now)
                .collect();
            for s in late {
                self.missing.remove(&s);
                self.receiver_drops += 1;
            }
            self.advance_cumulative();
            if ms.is_multiple_of(SRT_ACK_PERIOD_MS) && self.received_upto > 0 {
                let last_acked = self.received_upto - 1;
                for link in 0..2 {
                    if self.return_ok(link, now) {
                        let at = now + self.ack_delay(link);
                        self.schedule(at, Ev::SrtAck { link, last_acked });
                    }
                }
            }
            if ms.is_multiple_of(NAK_REPORT_PERIOD_MS) {
                let link = self.last_active_link;
                let due: Vec<u32> = self
                    .missing
                    .iter()
                    .filter(|(_, last)| now.saturating_sub(**last) >= NAK_REPORT_PERIOD_MS)
                    .map(|(s, _)| *s)
                    .collect();
                for s in due {
                    self.missing.insert(s, now);
                    if self.return_ok(link, now) {
                        let at = now + self.ack_delay(link);
                        self.schedule(at, Ev::Nak { link, seq: s });
                    }
                }
            }
        }

        fn advance_cumulative(&mut self) {
            let Some(max) = self.max_seen else { return };
            while self.received_upto <= max && !self.missing.contains_key(&self.received_upto) {
                self.received_upto += 1;
            }
        }

        fn on_arrive(&mut self, now: u64, link: usize, seq: u32) {
            self.last_arrival[link] = now;
            self.last_active_link = link;
            self.recv_batch[link].push(seq);
            if self.recv_batch[link].len() == RECV_ACK_INT {
                let seqs = std::mem::take(&mut self.recv_batch[link]);
                let ms = now - self.t0;
                let gap =
                    self.srtla_ack_gap[link].is_some_and(|(from, to)| (from..to).contains(&ms));
                if self.return_ok(link, now) && !gap {
                    let at = now + self.ack_delay(link);
                    self.schedule(at, Ev::SrtlaAck { link, seqs });
                }
            }
            match self.max_seen {
                Some(max) if seq <= max => {
                    self.missing.remove(&seq);
                }
                prev => {
                    let from = prev.map_or(0, |m| m + 1);
                    for gap in from..seq {
                        if seq.saturating_sub(gap) < 4096 && !self.missing.contains_key(&gap) {
                            self.missing.insert(gap, now);
                            if self.return_ok(link, now) {
                                let at = now + self.ack_delay(link);
                                self.schedule(at, Ev::Nak { link, seq: gap });
                            }
                        }
                    }
                    self.max_seen = Some(seq);
                }
            }
        }

        fn deliver(&mut self, now: u64, cfg: &ConfigSnapshot, ev: Ev) {
            match ev {
                Ev::Arrive { link, seq } => self.on_arrive(now, link, seq),
                Ev::KeepaliveArrive { link, pkt } => {
                    self.last_arrival[link] = now;
                    if self.return_ok(link, now) {
                        let at = now + self.rtt_ms[link] / 2 + self.jitter(self.echo_jitter_ms);
                        self.schedule(at, Ev::KeepaliveEcho { link, pkt });
                    }
                }
                Ev::SrtlaAck { link, seqs } => {
                    let incoming = SrtlaIncoming {
                        srtla_ack_numbers: SmallVec::from_slice_copy(&seqs),
                        read_any: true,
                        ..Default::default()
                    };
                    self.feedback(link, now, incoming);
                }
                Ev::KeepaliveEcho { link, pkt } => {
                    self.conns[link].last_received = Some(now);
                    self.conns[link].handle_keepalive_echo(&pkt, now);
                }
                Ev::SrtAck { link, last_acked } => {
                    // The wire field names the first sequence still owed.
                    let incoming = SrtlaIncoming {
                        ack_numbers: smallvec::smallvec![last_acked + 1],
                        read_any: true,
                        ..Default::default()
                    };
                    self.feedback(link, now, incoming);
                }
                Ev::Nak { link, seq } => {
                    let incoming = SrtlaIncoming {
                        nak_numbers: smallvec::smallvec![seq],
                        read_any: true,
                        ..Default::default()
                    };
                    self.feedback(link, now, incoming);
                    // SRT retransmits each NAKed sequence once per report.
                    if self.missing.contains_key(&seq) {
                        self.send_payload(now, cfg, seq, true);
                    }
                }
            }
        }

        /// The sender's 1 s housekeeping tick, as far as it touches the links:
        /// keepalives (and the extra one an RTT measurement asks for), window
        /// recovery and phase transitions.
        fn housekeeping(&mut self, now: u64) {
            for link in 0..self.conns.len() {
                let mut kas = Vec::new();
                if self.conns[link].needs_keepalive(now) {
                    kas.push(self.conns[link].keepalive_packet(now).to_vec());
                }
                if self.conns[link].needs_rtt_measurement(now) {
                    kas.push(self.conns[link].keepalive_packet(now).to_vec());
                }
                for pkt in kas {
                    if let Some(at) = self.forward_arrival(link, now) {
                        self.schedule(at, Ev::KeepaliveArrive { link, pkt });
                    }
                }
                if !self.classic {
                    self.conns[link].perform_window_recovery(now);
                }
                self.conns[link].calculate_bitrate(now);
                self.conns[link].update_phase(now);
            }
        }

        /// Run `duration_ms` at `rate_pps` payload packets per second.
        /// `watch` is the link whose cut/recovery the outcome describes.
        fn run(
            &mut self,
            rate_pps: u64,
            duration_ms: u64,
            steps: &[Step],
            watch: usize,
        ) -> Outcome {
            let cfg = self.config();
            self.watch = watch;
            let t0 = now_ms() + 1_000;
            self.t0 = t0;
            for c in self.conns.iter_mut() {
                c.last_received = Some(t0);
            }
            let cut_at = steps
                .iter()
                .find(|s| s.link == watch && !matches!(s.state, PathState::Alive { .. }))
                .map(|s| s.at_ms);
            let heal_at = steps
                .iter()
                .find(|s| {
                    s.link == watch
                        && cut_at.is_some_and(|c| s.at_ms > c)
                        && matches!(s.state, PathState::Alive { .. })
                })
                .map(|s| s.at_ms);
            let mut out = Outcome::default();
            let mut sent_acc: u64 = 0;
            for ms in 0..duration_ms {
                let now = t0 + ms;
                for s in steps.iter().filter(|s| s.at_ms == ms) {
                    self.state[s.link] = s.state;
                }
                self.watch_cut = cut_at.is_some_and(|c| ms >= c) && heal_at.is_none_or(|h| ms < h);
                if cut_at == Some(ms) {
                    self.cut_started = now;
                }
                while let Some(Reverse((at, id))) = self.events.peek().copied() {
                    if at > now {
                        break;
                    }
                    self.events.pop();
                    let ev = self.payloads.remove(&id).unwrap();
                    self.deliver(now, &cfg, ev);
                }
                if ms % HOUSEKEEPING_MS == HOUSEKEEPING_MS / 2 {
                    self.housekeeping(now);
                }
                sent_acc += rate_pps;
                while sent_acc >= 1000 {
                    sent_acc -= 1000;
                    let seq = self.next_seq;
                    self.next_seq += 1;
                    self.first_sent.insert(seq, now);
                    let idx = self.send_payload(now, &cfg, seq, false);
                    if self.watch_cut && idx == watch {
                        out.payload_into_dead_path += 1;
                    }
                }
                for link in 0..2 {
                    if self.conns[link].needs_batch_flush(now) {
                        self.flush(link, now);
                    }
                }
                self.receiver_tick(now, ms);
                for l in 0..2 {
                    let gated = self.conns[l].is_stall_gated();
                    if gated {
                        self.held_out_ms[l] += 1;
                    }
                    match self.gate_spans[l].last_mut() {
                        Some((_, end @ None)) if !gated => *end = Some(ms),
                        Some((_, None)) => {}
                        _ if gated => self.gate_spans[l].push((ms, None)),
                        _ => {}
                    }
                    self.max_backoff[l] =
                        self.max_backoff[l].max(self.conns[l].stall_rejoin_backoff());
                    let proof = self.conns[l].last_ack_or_rtt_sample_ms;
                    if ms > 5_000 && proof != 0 {
                        self.max_proof_age_ms[l] = self.max_proof_age_ms[l].max(now - proof);
                    }
                }
                let gated = self.conns[watch].is_stall_gated();
                if self.watch_cut && gated {
                    self.watch_gated = true;
                }
                if let Some(c) = cut_at
                    && ms >= c
                    && out.gated_after_ms.is_none()
                    && gated
                {
                    out.gated_after_ms = Some(ms - c);
                }
                if let Some(h) = heal_at
                    && ms >= h
                    && out.readmitted_after_ms.is_none()
                    && !gated
                {
                    out.readmitted_after_ms = Some(ms - h);
                }
            }
            out.last_payload_after_cut_ms = self.last_payload_after_cut_ms;
            out.payload_after_5s =
                self.dead_payload_ms.iter().filter(|&&t| t > 5_000).count() as u64;
            out.fresh_after_gate = self.fresh_after_gate;
            out.retransmits_after_gate = self.retransmits_after_gate;
            out.receiver_drops = self.receiver_drops;
            out.stall_gate_events = self.conns[watch].stall_gate_events();
            out
        }

        fn share(&self, link: usize) -> f64 {
            self.payload_sent[link] as f64 / (self.payload_sent[0] + self.payload_sent[1]) as f64
        }
    }

    /// A 30 s outage of link 0, as in the field runs.
    fn cut_and_heal(state: PathState) -> Vec<Step> {
        vec![
            Step {
                at_ms: 20_000,
                link: 0,
                state,
            },
            Step {
                at_ms: 50_000,
                link: 0,
                state: PathState::Alive { loss_permille: 0 },
            },
        ]
    }

    /// Both links lossy from 5 s on.
    fn lossy(loss_permille: [u32; 2]) -> Vec<Step> {
        (0..2)
            .map(|link| Step {
                at_ms: 5_000,
                link,
                state: PathState::Alive {
                    loss_permille: loss_permille[link],
                },
            })
            .collect()
    }

    /// Keepalive echoes up to 20 ms late, receiver ACKs up to 10 ms late.
    fn jittered(rtts: [u64; 2], seed: u64) -> Sim {
        let mut sim = Sim::new(rtts, seed);
        sim.echo_jitter_ms = 20;
        sim.ack_jitter_ms = 10;
        sim
    }

    /// Payload rates from the field runs (0.3 to 1 Mbit/s of 1316-byte
    /// packets is about 30 to 95 packets/s) and above.
    const RATES_PPS: [u64; 3] = [30, 100, 400];

    fn assert_never_held_out(sim: &Sim, what: &str) {
        assert_eq!(
            sim.held_out_ms,
            [0, 0],
            "{what}: a live link was held out (latch or pull) for {:?} ms, {:?} latches",
            sim.held_out_ms,
            [
                sim.conns[0].stall_gate_events(),
                sim.conns[1].stall_gate_events()
            ]
        );
    }

    // --- A dead path ---

    /// Rates at which the dead link stops getting payload within the
    /// staleness floor: about 0.7 Mbit/s and up. Below, the count of 50
    /// unproven packets takes longer to fill (see the low-rate test).
    const FIELD_RATES_PPS: [u64; 3] = [67, 100, 400];

    /// The cut link stopped getting payload within `bound_ms` of the cut. A
    /// link the scheduler had (nearly) stopped using passes trivially.
    fn assert_fed_at_most(out: &Outcome, bound_ms: u64, what: &str) {
        if let Some(last) = out.last_payload_after_cut_ms {
            assert!(
                last <= bound_ms,
                "{what}: the dead link still got payload {last} ms after the cut, {out:?}"
            );
        }
    }

    #[test]
    fn a_path_that_dies_inbound_stops_getting_payload_within_1600_ms() {
        for rate in FIELD_RATES_PPS {
            let mut sim = Sim::new([7, 7], 0x5eed_0001 ^ rate);
            let out = sim.run(rate, 70_000, &cut_and_heal(PathState::DeadInbound), 0);
            assert!(out.gated_after_ms.is_some(), "{rate} pkt/s: {out:?}");
            assert_fed_at_most(&out, 1_600, &format!("{rate} pkt/s"));
            assert_eq!(out.receiver_drops, 0, "{rate} pkt/s: {out:?}");
        }
    }

    #[test]
    fn a_blackholed_path_stops_getting_payload_within_1600_ms() {
        for rate in FIELD_RATES_PPS {
            let mut sim = Sim::new([7, 7], 0x5eed_0002 ^ rate);
            let out = sim.run(rate, 70_000, &cut_and_heal(PathState::Blackhole), 0);
            assert!(out.gated_after_ms.is_some(), "{rate} pkt/s: {out:?}");
            assert_fed_at_most(&out, 1_600, &format!("{rate} pkt/s"));
            assert_eq!(out.receiver_drops, 0, "{rate} pkt/s: {out:?}");
        }
    }

    #[test]
    fn a_dead_path_gets_no_payload_once_held_out() {
        // Neither new packets nor SRT retransmits may go there once it is
        // held out while its sibling is usable; only duplicate probes do.
        for state in [PathState::DeadInbound, PathState::Blackhole] {
            for rate in [30, 67, 100, 400] {
                let mut sim = Sim::new([7, 25], 0x5eed_0003 ^ rate);
                let out = sim.run(rate, 70_000, &cut_and_heal(state), 0);
                assert_eq!(
                    (out.fresh_after_gate, out.retransmits_after_gate),
                    (0, 0),
                    "{state:?} {rate} pkt/s: payload after the hold-out, {out:?}"
                );
            }
        }
    }

    #[test]
    fn a_recovered_path_is_readmitted_once_after_the_dwell() {
        // An 8 s outage: shorter than both the receiver's and the sender's
        // link timeouts, so the stall gate alone decides when the link
        // comes back (a longer one goes through re-registration instead).
        let steps = |state| {
            vec![
                Step {
                    at_ms: 20_000,
                    link: 0,
                    state,
                },
                Step {
                    at_ms: 28_000,
                    link: 0,
                    state: PathState::Alive { loss_permille: 0 },
                },
            ]
        };
        for state in [PathState::DeadInbound, PathState::Blackhole] {
            for rate in FIELD_RATES_PPS {
                let mut sim = Sim::new([7, 7], 0x5eed_0004 ^ rate);
                let out = sim.run(rate, 60_000, &steps(state), 0);
                let back = out
                    .readmitted_after_ms
                    .expect("the healed link must be readmitted");
                // First echo within a keepalive period, then the 2 s dwell.
                assert!(
                    (2_000..=4_000).contains(&back),
                    "{state:?} {rate} pkt/s: readmitted {back} ms after healing, {out:?}"
                );
                assert_eq!(out.stall_gate_events, 1, "{state:?} {rate} pkt/s, {out:?}");
                assert_eq!(out.receiver_drops, 0, "{state:?} {rate} pkt/s, {out:?}");
            }
        }
    }

    #[test]
    fn a_dead_link_is_caught_on_jittered_unequal_paths() {
        // Either link of a 7/25 ms pair dies, inbound only or both ways.
        for (watch, rate) in [(0, 67), (0, 103), (1, 103), (0, 401)] {
            for state in [PathState::DeadInbound, PathState::Blackhole] {
                for seed in 0..4u64 {
                    let mut sim = jittered([7, 25], 0xd00d ^ rate ^ (seed << 20));
                    let steps = [Step {
                        at_ms: 20_000 + seed * 131,
                        link: watch,
                        state,
                    }];
                    let out = sim.run(rate, 40_000, &steps, watch);
                    assert_fed_at_most(
                        &out,
                        1_600,
                        &format!("link {watch} {state:?} at {rate} pkt/s, seed {seed}"),
                    );
                    assert!(out.stall_gate_events <= 1, "{out:?}");
                }
            }
        }
    }

    #[test]
    fn a_dead_link_at_low_rates_stops_getting_payload_within_five_seconds() {
        // 20 to 50 packets/s in total. Here the count of 50 unproven packets,
        // not the 1.5 s floor, sets the pace. A link the scheduler has mostly
        // abandoned by then (its window cut by NAKs) may still get a stray
        // packet now and then until it times out; bound that trickle.
        for rtts in [[30, 30], [7, 25]] {
            for state in [PathState::DeadInbound, PathState::Blackhole] {
                for rate in [20, 30, 40, 50] {
                    let (mut into, mut late) = (0, 0);
                    for seed in 0..8u64 {
                        let mut sim = jittered(rtts, 0xdead ^ rate ^ (seed << 20));
                        let steps = [Step {
                            at_ms: 20_000 + seed * 131,
                            link: 0,
                            state,
                        }];
                        let out = sim.run(rate, 40_000, &steps, 0);
                        into += out.payload_into_dead_path;
                        late += out.payload_after_5s;
                    }
                    assert!(
                        late * 10 <= into,
                        "{rtts:?} ms {state:?} at {rate} pkt/s: {late} of {into} packets into the \
                         dead link came more than 5 s after the cut"
                    );
                }
            }
        }
    }

    #[test]
    fn classic_mode_holds_out_a_dead_link_and_keeps_live_ones() {
        for rate in FIELD_RATES_PPS {
            let mut sim = jittered([7, 25], 0xc1a5 ^ rate);
            sim.classic = true;
            let _ = sim.run(rate, 60_000, &lossy([20, 20]), 0);
            assert_never_held_out(&sim, &format!("classic, 2 ‰ loss, {rate} pkt/s"));

            let mut sim = jittered([7, 25], 0xc1a6 ^ rate);
            sim.classic = true;
            let steps = [Step {
                at_ms: 20_000,
                link: 0,
                state: PathState::DeadInbound,
            }];
            let out = sim.run(rate, 40_000, &steps, 0);
            assert_fed_at_most(&out, 1_600, &format!("classic {rate} pkt/s"));
        }
    }

    // --- Live links that must keep their place ---

    #[test]
    fn a_lossy_but_alive_path_is_never_held_out() {
        // 5 and 10 % loss each way on one link, two simulated minutes.
        for loss_permille in [50, 100] {
            for rate in RATES_PPS {
                let mut sim = Sim::new([7, 7], 0x5eed_0005 ^ rate ^ loss_permille as u64);
                let _ = sim.run(rate, 125_000, &lossy([loss_permille, 0]), 0);
                assert_never_held_out(&sim, &format!("{loss_permille}‰ at {rate} pkt/s"));
            }
        }
    }

    #[test]
    fn a_very_lossy_link_is_rarely_held_out() {
        // 20 % loss each way (36 % of round trips fail): an occasional
        // hold-out is acceptable, a steady one is not. Bound the share of
        // time across seeds rather than pinning one seed's count.
        let mut held = 0u64;
        let mut total = 0u64;
        for rate in [67, 103, 401] {
            for seed in 0..4u64 {
                let mut sim = jittered([7, 25], 0x2020 ^ rate ^ (seed << 20));
                let _ = sim.run(rate, 125_000, &lossy([200, 0]), 0);
                held += sim.held_out_ms[0];
                total += 120_000;
            }
        }
        assert!(
            held * 100 <= total,
            "a 20 %-loss link was held out {held} of {total} ms"
        );
    }

    #[test]
    fn both_lossy_links_at_low_rates_keep_their_place() {
        // Two cellular links in poor coverage, 5 or 10 % loss each way on
        // both, 20 to 67 packets/s in total: the band where the count and the
        // floor are reached at about the same time. 144 runs of about three
        // simulated minutes; a threshold of 30 unproven sends held a live
        // link out 17 times here and 40 held one out 3 times.
        //
        // A link's proof is its SRTLA ACKs, one per ten delivered packets,
        // and a keepalive echo once a second, so holding out a live link
        // takes an echo and the ACK batches of the same stretch going missing
        // together. With the independent loss modelled here that is rare but
        // within reach of a sweep this size; correlated loss on a real link
        // makes it more likely. Such a hold-out is a false one the heuristic
        // accepts: the path was delivering and only the evidence for it was
        // lost (see the return gap test above for how it plays out). On this
        // tree one seed does it once, for 2.5 s. Bound it rather than pin
        // zero.
        let mut latches = 0;
        let mut held_ms = 0;
        for rtts in [[30, 30], [7, 25]] {
            for loss in [50, 100] {
                for rate in [20, 25, 30, 40, 50, 67] {
                    for seed in 0..6u64 {
                        let mut sim = jittered(rtts, 0xf00d ^ rate ^ (seed << 20) ^ loss as u64);
                        let _ = sim.run(rate, 185_000, &lossy([loss, loss]), 0);
                        latches +=
                            sim.conns[0].stall_gate_events() + sim.conns[1].stall_gate_events();
                        held_ms += sim.held_out_ms[0] + sim.held_out_ms[1];
                    }
                }
            }
        }
        assert!(
            latches <= 2 && held_ms <= 2 * 5_000,
            "live lossy links were held out {latches} times, {held_ms} ms in total"
        );
    }

    /// Link 0 at 40 pkt/s next to a lossless sibling, with nothing coming
    /// back on it for `gap` ms from 20 s on (or, with `acks_only`, only its
    /// SRTLA ACKs missing) while everything it sends still arrives.
    fn return_gap_run(rtts: [u64; 2], gap: u64, acks_only: bool) -> Sim {
        let mut sim = Sim::new(rtts, 0x9a90 ^ gap);
        let window = Some((20_000, 20_000 + gap));
        if acks_only {
            sim.srtla_ack_gap[0] = window;
        } else {
            sim.return_gap[0] = window;
        }
        let _ = sim.run(40, 50_000, &[], 0);
        sim
    }

    #[test]
    fn missing_srtla_acks_alone_do_not_hold_out_a_link_whose_echoes_arrive() {
        // Keepalives go out on every link once a second, busy or not, and
        // their echoes are proof too. In this setup (40 pkt/s in total, no
        // other loss) the proof never got 1.5 s old.
        for gap in [1_000, 3_000, 6_000] {
            for rtts in [[7, 25], [30, 30]] {
                let sim = return_gap_run(rtts, gap, true);
                assert_never_held_out(&sim, &format!("{gap} ms without SRTLA ACKs, {rtts:?}"));
            }
        }
    }

    #[test]
    fn a_gap_in_a_live_links_return_traffic_holds_it_out_once_and_it_rejoins() {
        // The false hold-out the lossy sweep below accepts, made controlled:
        // the path delivers, only the evidence for it is missing. What this
        // pins is what this setup measured (40 pkt/s in total, no other
        // loss, these two pairs of round trips), not general timing:
        // - gaps of 1 s or less: no hold-out;
        // - gaps of 2 s or more: one latch, once the link owes 50 unproven
        //   sends (pulled at once, since it is also silent, then latched on
        //   the 1.5 s floor). At 20 pkt/s per link that can come just after
        //   the gap ends, before the next echo or ACK batch is back;
        // - the hold-out completes and the link rejoins 1.5 to 3 s after the
        //   configured gap ends, without a longer backoff;
        // - the sibling carries the stream meanwhile and the model's
        //   receiver drops nothing.
        for rtts in [[7, 25], [30, 30]] {
            for gap in [500, 1_000] {
                let sim = return_gap_run(rtts, gap, false);
                assert_never_held_out(&sim, &format!("{gap} ms return gap, {rtts:?}"));
            }
            for gap in [2_000, 3_000, 6_000] {
                let sim = return_gap_run(rtts, gap, false);
                let what = format!("{gap} ms return gap, {rtts:?}");
                let [(start, Some(end))] = sim.gate_spans[0][..] else {
                    panic!("{what}: expected one hold-out, got {:?}", sim.gate_spans[0]);
                };
                assert!(
                    (21_000..21_000 + gap).contains(&start),
                    "{what}: held out at {start} ms"
                );
                let rejoin_after = end - (20_000 + gap);
                assert!(
                    (1_500..=3_000).contains(&rejoin_after),
                    "{what}: rejoined {rejoin_after} ms after its return traffic resumed"
                );
                assert_eq!(sim.conns[0].stall_gate_events(), 1, "{what}");
                assert_eq!(sim.max_backoff, [1, 1], "{what}");
                assert!(
                    sim.gate_spans[1].is_empty(),
                    "{what}: the sibling was held out"
                );
                assert_eq!(sim.receiver_drops, 0, "{what}");
            }
        }
    }

    #[test]
    fn a_healthy_link_with_the_slower_return_path_is_never_held_out() {
        // Wi-Fi next to cellular, or two carriers: lossless but 18 to 40 ms
        // apart, at non-round packet rates. The faster link's capacity is
        // capped so the slower one carries a real share.
        for (rtts, rate, cap) in [
            ([20, 40], 97, 60),
            ([7, 25], 103, 60),
            ([30, 30], 67, 0),
            ([40, 80], 151, 90),
        ] {
            for seed in 0..4u64 {
                let mut sim = jittered(rtts, 0xabc0_0000 ^ rate ^ (seed << 20));
                sim.cap_pps = [cap, 0];
                let _ = sim.run(rate, 120_000, &[], 0);
                let what = format!("{rtts:?} ms at {rate} pkt/s, seed {seed}");
                assert_never_held_out(&sim, &what);
                assert!(
                    sim.share(1) >= 0.2,
                    "{what}: slower link share {:.2}",
                    sim.share(1)
                );
                // Carrying real load, it must keep earning SRTLA-ACK proof
                // between keepalive echoes rather than living on the echoes.
                assert!(
                    sim.max_proof_age_ms[1] < 800,
                    "{what}: the slower link went {} ms without proof",
                    sim.max_proof_age_ms[1]
                );
            }
        }
    }

    #[test]
    fn a_much_slower_lossy_link_keeps_its_place() {
        // 7 ms Wi-Fi next to 150 ms cellular with 1 to 3 % loss each way. The
        // receiver NAKs the slower link's packets as gaps before they arrive,
        // so their retransmits move to the faster link; the slower link's own
        // SRTLA ACKs must still count for it.
        for (loss, rate) in [([20, 20], 400), ([10, 10], 800), ([30, 20], 400)] {
            for seed in 0..2u64 {
                let mut sim = jittered([7, 150], 0x7150 ^ rate ^ (seed << 20));
                sim.cap_pps = [rate * 3 / 4, 0];
                let _ = sim.run(rate, 125_000, &lossy(loss), 0);
                assert_never_held_out(&sim, &format!("{loss:?}‰ at {rate} pkt/s, seed {seed}"));
            }
        }
    }

    #[test]
    fn a_slower_healthy_sibling_does_not_hold_out_the_fast_link() {
        // Unequal round trips (7 ms Wi-Fi next to 60 ms cellular) reorder
        // packets across the links and draw NAKs for packets that do arrive.
        for rate in RATES_PPS {
            let mut sim = Sim::new([7, 60], 0x5eed_0006 ^ rate);
            let _ = sim.run(rate, 60_000, &[], 0);
            assert_never_held_out(&sim, &format!("{rate} pkt/s"));
        }
    }
}
