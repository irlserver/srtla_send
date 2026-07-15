use std::net::SocketAddr;

use anyhow::Result;
use smallvec::SmallVec;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, trace, warn};

use super::selection::select_connection_idx;
use super::sequence::SequenceTracker;
use super::uplink::UplinkPacket;
use crate::config::ConfigSnapshot;
use crate::connection::{SrtlaConnection, SrtlaIncoming};
use crate::protocol;
use crate::registration::SrtlaRegistrationManager;

/// Type alias for instant ACK forwarding: (client_addr, packet_data)
pub type InstantForwarder = UnboundedSender<(SocketAddr, SmallVec<u8, 64>)>;

/// Attribute a NAK to the uplink that sent the lost packet and shrink its window.
///
/// Prefers the O(1) sequence-tracker mapping (the link that actually sent `nak`);
/// once that link is found we never fall through, so a duplicate NAK for an
/// already-cleared sequence can't be re-counted against a different link. Only
/// when the tracker has no record do we fall back to the first link that still
/// recognizes the sequence in its own packet log. Returns the index of the link
/// that counted the NAK, or `None` if none did. Production ignores the return;
/// it exists so the attribution path is unit-testable directly instead of mirrored.
pub(crate) fn attribute_nak(
    connections: &mut [SrtlaConnection],
    seq_tracker: &SequenceTracker,
    nak: u32,
    current_time_ms: u64,
) -> Option<usize> {
    if let Some(conn_id) = seq_tracker.get(nak, current_time_ms)
        && let Some(pos) = connections.iter().position(|c| c.conn_id == conn_id)
    {
        return connections[pos]
            .handle_nak(nak as i32, current_time_ms)
            .then_some(pos);
    }

    for (i, conn) in connections.iter_mut().enumerate() {
        if conn.handle_nak(nak as i32, current_time_ms) {
            return Some(i);
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
pub async fn process_connection_events(
    idx: usize,
    connections: &mut [SrtlaConnection],
    reg: &mut SrtlaRegistrationManager,
    instant_tx: &InstantForwarder,
    last_client_addr: Option<SocketAddr>,
    local_listener: &UdpSocket,
    seq_tracker: &SequenceTracker,
    classic: bool,
    incoming_override: Option<SrtlaIncoming>,
) -> Result<()> {
    if idx >= connections.len() {
        return Ok(());
    }

    let incoming = if let Some(overridden) = incoming_override {
        overridden
    } else {
        connections[idx]
            .drain_incoming(idx, reg, local_listener, instant_tx, last_client_addr)
            .await?
    };

    if !incoming.read_any
        && incoming.ack_numbers.is_empty()
        && incoming.nak_numbers.is_empty()
        && incoming.srtla_ack_numbers.is_empty()
        && incoming.forward_to_client.is_empty()
    {
        return Ok(());
    }

    // One monotonic read drives every ACK/NAK handler in this receive batch.
    let current_time_ms = crate::utils::now_ms();

    for ack in incoming.ack_numbers.iter() {
        for c in connections.iter_mut() {
            c.handle_srt_ack(*ack as i32, current_time_ms);
        }
    }

    for srtla_ack in incoming.srtla_ack_numbers.iter() {
        for c in connections.iter_mut() {
            if c.handle_srtla_ack_specific(*srtla_ack as i32, classic, current_time_ms) {
                break;
            }
        }
        for c in connections.iter_mut() {
            c.handle_srtla_ack_global();
        }
    }

    for nak in incoming.nak_numbers.iter() {
        attribute_nak(connections, seq_tracker, *nak, current_time_ms);
    }

    if let Some(client) = last_client_addr {
        for pkt in incoming.forward_to_client.iter() {
            let _ = local_listener.send_to(pkt, client).await;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_uplink_packet(
    packet: UplinkPacket,
    connections: &mut [SrtlaConnection],
    reg: &mut SrtlaRegistrationManager,
    instant_tx: &InstantForwarder,
    last_client_addr: Option<SocketAddr>,
    local_listener: &UdpSocket,
    seq_tracker: &SequenceTracker,
    config_snap: &ConfigSnapshot,
) {
    if packet.bytes.is_empty() {
        return;
    }
    if let Some(idx) = connections.iter().position(|c| c.conn_id == packet.conn_id) {
        match connections[idx]
            .process_packet(
                idx,
                reg,
                local_listener,
                instant_tx,
                last_client_addr,
                &packet.bytes,
            )
            .await
        {
            Ok(incoming) => {
                if let Err(err) = process_connection_events(
                    idx,
                    connections,
                    reg,
                    instant_tx,
                    last_client_addr,
                    local_listener,
                    seq_tracker,
                    config_snap.mode.is_classic(),
                    Some(incoming),
                )
                .await
                {
                    warn!("failed to apply uplink {} packet: {err}", packet.conn_id);
                }
            }
            Err(err) => warn!(
                "failed to process packet for uplink {}: {}",
                packet.conn_id, err
            ),
        }
    }
}

/// Maximum number of packets to process per drain call.
/// This prevents CPU spikes from processing large accumulated queues in one burst.
/// At ~1000 packets/sec typical rate, 64 packets = ~64ms worth of traffic.
const MAX_DRAIN_PACKETS: usize = 64;

#[allow(clippy::too_many_arguments)]
pub async fn drain_packet_queue(
    packet_rx: &mut UnboundedReceiver<UplinkPacket>,
    connections: &mut [SrtlaConnection],
    reg: &mut SrtlaRegistrationManager,
    instant_tx: &InstantForwarder,
    last_client_addr: Option<SocketAddr>,
    local_listener: &UdpSocket,
    seq_tracker: &SequenceTracker,
    config_snap: &ConfigSnapshot,
) {
    // Process up to MAX_DRAIN_PACKETS to prevent CPU spikes from large queue bursts.
    // Remaining packets will be processed on the next event loop iteration.
    let mut processed = 0;
    while processed < MAX_DRAIN_PACKETS {
        match packet_rx.try_recv() {
            Ok(packet) => {
                handle_uplink_packet(
                    packet,
                    connections,
                    reg,
                    instant_tx,
                    last_client_addr,
                    local_listener,
                    seq_tracker,
                    config_snap,
                )
                .await;
                processed += 1;
            }
            Err(_) => break, // No more packets available
        }
    }
}

/// Selects a connection to use during the pre-registration phase.
///
/// Selection priority:
/// 1. Last selected connection (if still connected and not timed out)
/// 2. Any non-timed-out connection
/// 3. None (if all connections are timed out)
fn select_pre_registration_connection(
    connections: &[SrtlaConnection],
    last_selected_idx: Option<usize>,
    now_ms: u64,
) -> Option<usize> {
    // Try to reuse the last selected connection if it's still valid
    if let Some(idx) = last_selected_idx
        && let Some(conn) = connections.get(idx)
        && conn.connected
        && !conn.is_timed_out(now_ms)
    {
        return Some(idx);
    }

    // Otherwise, find any non-timed-out connection
    connections
        .iter()
        .enumerate()
        .find(|(_, c)| !c.is_timed_out(now_ms))
        .map(|(i, _)| i)
}

/// Handle incoming SRT packet
///
/// Uses a pre-cached `ConfigSnapshot` to avoid atomic loads per packet.
/// The caller should create a snapshot once per select iteration for optimal performance.
///
/// When a keyframe burst is detected (runs of consecutive max-MTU 1316-byte data
/// packets), the scheduler overrides normal selection and routes to the
/// highest-quality link. This ensures I-frame data — which is critical for
/// decoder recovery — travels over the most reliable path.
#[allow(clippy::too_many_arguments)]
pub async fn handle_srt_packet(
    res: Result<(usize, SocketAddr), std::io::Error>,
    recv_buf: &mut [u8],
    connections: &mut [SrtlaConnection],
    last_selected_idx: &mut Option<usize>,
    seq_tracker: &mut SequenceTracker,
    last_client_addr: &mut Option<SocketAddr>,
    registration_complete: bool,
    config_snap: &ConfigSnapshot,
    critical_window: &crate::priority::CriticalWindow,
) {
    match res {
        Ok((n, src)) => {
            if n == 0 {
                return;
            }
            // Capture timestamp once at packet entry - reduces syscalls from 3-5 to 1 per packet
            let packet_time_ms = crate::utils::now_ms();

            let pkt = &recv_buf[..n];
            let seq = protocol::get_srt_sequence_number(pkt);
            if !registration_complete {
                let sel_idx =
                    select_pre_registration_connection(connections, *last_selected_idx, packet_time_ms);
                if let Some(sel_idx) = sel_idx {
                    forward_via_connection(
                        sel_idx,
                        pkt,
                        seq,
                        connections,
                        last_selected_idx,
                        seq_tracker,
                        packet_time_ms,
                    )
                    .await;
                }
                *last_client_addr = Some(src);
                return;
            }

            // Normal scheduler selection
            let mut sel_idx =
                select_connection_idx(connections, *last_selected_idx, packet_time_ms, config_snap);

            // Keyframe priority: route critical packets to the highest-quality
            // link. The critical time window is opened over the priority
            // sidecar by the encoder front-end, which parses NAL units and
            // knows exactly when a keyframe / parameter set is in flight (see
            // crate::priority). srtla_send sees only opaque SRT payloads, so it
            // never guesses at keyframes itself.
            //
            // Only data packets have seq != None (control packets have MSB set).
            if seq.is_some()
                && critical_window.is_critical_now(packet_time_ms)
                && let Some(best_idx) = crate::priority::select_best_quality_idx(connections)
                && sel_idx != Some(best_idx)
            {
                trace!(
                    "critical override (window): link {} -> {}",
                    sel_idx.map_or(-1, |i| i as i64),
                    best_idx as i64
                );
                sel_idx = Some(best_idx);
            }

            if let Some(sel_idx) = sel_idx {
                forward_via_connection(
                    sel_idx,
                    pkt,
                    seq,
                    connections,
                    last_selected_idx,
                    seq_tracker,
                    packet_time_ms,
                )
                .await;
            } else {
                warn!("no available connection to forward packet from {}", src);
            }
            *last_client_addr = Some(src);
        }
        Err(e) => warn!("error reading local SRT: {}", e),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn forward_via_connection(
    sel_idx: usize,
    pkt: &[u8],
    seq: Option<u32>,
    connections: &mut [SrtlaConnection],
    last_selected_idx: &mut Option<usize>,
    seq_tracker: &mut SequenceTracker,
    packet_time_ms: u64,
) {
    if sel_idx >= connections.len() {
        return;
    }
    if *last_selected_idx != Some(sel_idx) {
        if let Some(prev_idx) = *last_selected_idx {
            if prev_idx < connections.len() {
                // Deliberately does not flush the previous link's batch. Each
                // connection owns its BatchSender and drains it with a single
                // `sendmmsg` on its own size threshold or 15ms timer, so
                // interleaved routing just fills several per-link batches
                // concurrently instead of one serially — no syscall is lost.
                //
                // Flushing here emitted a one-packet batch on every switch,
                // which made per-packet scheduling expensive and is what the
                // `MIN_SWITCH_INTERVAL_MS` cooldown existed to suppress. That
                // cooldown freezes the selector for ~15ms, and since
                // `get_score()` counts queued packets as in-flight precisely so
                // routing a packet immediately de-prioritises its link, freezing
                // it opens that feedback loop and lets in-flight run away on one
                // link.
                debug!(
                    "Connection switch: {} → {} (seq: {:?})",
                    connections[prev_idx].label, connections[sel_idx].label, seq
                );
            }
        } else {
            debug!(
                "Initial connection selected: {} (seq: {:?})",
                connections[sel_idx].label, seq
            );
        }
        *last_selected_idx = Some(sel_idx);
    }

    // Get conn_id before mutable borrow for seq_tracker
    let conn_id = connections[sel_idx].conn_id;

    // Queue the packet for batched sending
    let needs_flush = connections[sel_idx].queue_data_packet(pkt, seq, packet_time_ms);

    // O(1) insert into ring buffer - no allocation
    // Track immediately when queued (not when flushed) for accurate NAK attribution
    if let Some(s) = seq {
        seq_tracker.insert(s, conn_id, packet_time_ms);
    }

    // Flush if batch threshold reached
    if needs_flush {
        let conn = &mut connections[sel_idx];
        if let Err(e) = conn.flush_batch().await {
            warn!(
                "{}: batch flush failed, marking for recovery: {}",
                conn.label, e
            );
            conn.mark_for_recovery();
        }
    }
}

/// Flush all connection batches (called on timer or when needed)
///
/// Optimized with early exit: first check if any connection has queued packets
/// before iterating. This avoids work on the 15ms timer when traffic is idle.
pub async fn flush_all_batches(connections: &mut [SrtlaConnection]) {
    // One monotonic read drives the flush-window check for every connection.
    let now = crate::utils::now_ms();

    // Quick scan to check if any connection has work to do
    // This is a fast read-only check that avoids the flush logic entirely when idle
    let has_work = connections
        .iter()
        .any(|c| c.has_queued_packets() || c.needs_batch_flush(now));

    if !has_work {
        return;
    }

    // Now do the actual flush for connections that need it
    for conn in connections.iter_mut() {
        if (conn.needs_batch_flush(now) || conn.has_queued_packets())
            && let Err(e) = conn.flush_batch().await
        {
            warn!("{}: periodic batch flush failed: {}", conn.label, e);
        }
    }
}
