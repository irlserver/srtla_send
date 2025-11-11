use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use smallvec::SmallVec;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;
use tracing::{debug, warn};

use super::selection::select_connection_idx;
use super::sequence::{MAX_SEQUENCE_TRACKING, SequenceTrackingEntry};
use super::uplink::UplinkPacket;
use crate::connection::{SrtlaConnection, SrtlaIncoming};
use crate::protocol;
use crate::registration::SrtlaRegistrationManager;
use crate::toggles::DynamicToggles;
use crate::utils::now_ms;

#[allow(clippy::too_many_arguments)]
pub async fn process_connection_events(
    idx: usize,
    connections: &mut [SrtlaConnection],
    reg: &mut SrtlaRegistrationManager,
    instant_tx: &std::sync::mpsc::Sender<SmallVec<u8, 64>>,
    last_client_addr: Option<SocketAddr>,
    local_listener: &UdpSocket,
    seq_to_conn: &mut HashMap<u32, SequenceTrackingEntry>,
    _seq_order: &mut VecDeque<u32>,
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
            .drain_incoming(idx, reg, instant_tx)
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

    for ack in incoming.ack_numbers.iter() {
        for c in connections.iter_mut() {
            c.handle_srt_ack(*ack as i32);
        }
    }

    for srtla_ack in incoming.srtla_ack_numbers.iter() {
        for c in connections.iter_mut() {
            if c.handle_srtla_ack_specific(*srtla_ack as i32, classic) {
                break;
            }
        }
        for c in connections.iter_mut() {
            c.handle_srtla_ack_global();
        }
    }

    for nak in incoming.nak_numbers.iter() {
        let mut handled = false;
        if let Some(entry) = seq_to_conn.get(nak) {
            let current_time = now_ms();
            if !entry.is_expired(current_time) {
                if let Some(conn) = connections.iter_mut().find(|c| c.conn_id == entry.conn_id) {
                    conn.handle_nak(*nak as i32);
                    handled = true;
                }
            }
        }

        if !handled {
            for conn in connections.iter_mut() {
                if conn.handle_nak(*nak as i32) {
                    break;
                }
            }
        }
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
    instant_tx: &std::sync::mpsc::Sender<SmallVec<u8, 64>>,
    last_client_addr: Option<SocketAddr>,
    local_listener: &UdpSocket,
    seq_to_conn: &mut HashMap<u32, SequenceTrackingEntry>,
    seq_order: &mut VecDeque<u32>,
    toggles: &DynamicToggles,
) {
    if packet.bytes.is_empty() {
        return;
    }
    if let Some(idx) = connections.iter().position(|c| c.conn_id == packet.conn_id) {
        match connections[idx]
            .process_packet(idx, reg, instant_tx, &packet.bytes)
            .await
        {
            Ok(incoming) => {
                let classic = toggles
                    .classic_mode
                    .load(std::sync::atomic::Ordering::Relaxed);
                if let Err(err) = process_connection_events(
                    idx,
                    connections,
                    reg,
                    instant_tx,
                    last_client_addr,
                    local_listener,
                    seq_to_conn,
                    seq_order,
                    classic,
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

#[allow(clippy::too_many_arguments)]
pub async fn drain_packet_queue(
    packet_rx: &mut UnboundedReceiver<UplinkPacket>,
    connections: &mut [SrtlaConnection],
    reg: &mut SrtlaRegistrationManager,
    instant_tx: &std::sync::mpsc::Sender<SmallVec<u8, 64>>,
    last_client_addr: Option<SocketAddr>,
    local_listener: &UdpSocket,
    seq_to_conn: &mut HashMap<u32, SequenceTrackingEntry>,
    seq_order: &mut VecDeque<u32>,
    toggles: &DynamicToggles,
) {
    while let Ok(packet) = packet_rx.try_recv() {
        handle_uplink_packet(
            packet,
            connections,
            reg,
            instant_tx,
            last_client_addr,
            local_listener,
            seq_to_conn,
            seq_order,
            toggles,
        )
        .await;
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_srt_packet(
    res: Result<(usize, SocketAddr), std::io::Error>,
    recv_buf: &mut [u8],
    connections: &mut [SrtlaConnection],
    last_selected_idx: &mut Option<usize>,
    seq_to_conn: &mut HashMap<u32, SequenceTrackingEntry>,
    seq_order: &mut VecDeque<u32>,
    last_client_addr: &mut Option<SocketAddr>,
    shared_client_addr: &Arc<Mutex<Option<SocketAddr>>>,
    registration_complete: bool,
    toggles: &DynamicToggles,
) {
    match res {
        Ok((n, src)) => {
            if n == 0 {
                return;
            }
            let pkt = &recv_buf[..n];
            let seq = protocol::get_srt_sequence_number(pkt);
            if !registration_complete {
                let sel_idx = if let Some(idx) = last_selected_idx {
                    if let Some(conn) = connections.get(*idx) {
                        if conn.connected && !conn.is_timed_out() {
                            Some(*idx)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    connections
                        .iter()
                        .enumerate()
                        .find(|(_, c)| !c.is_timed_out())
                        .map(|(i, _)| i)
                        .or_else(|| {
                            connections
                                .first()
                                .and_then(|c| if !c.is_timed_out() { Some(0) } else { None })
                        })
                };
                if let Some(sel_idx) = sel_idx {
                    forward_via_connection(
                        sel_idx,
                        pkt,
                        seq,
                        connections,
                        last_selected_idx,
                        seq_to_conn,
                        seq_order,
                        last_client_addr,
                        shared_client_addr,
                        src,
                    )
                    .await;
                }
                return;
            }
            let enable_quality = toggles
                .quality_scoring_enabled
                .load(std::sync::atomic::Ordering::Relaxed);
            let enable_explore = toggles
                .exploration_enabled
                .load(std::sync::atomic::Ordering::Relaxed);
            let classic = toggles
                .classic_mode
                .load(std::sync::atomic::Ordering::Relaxed);

            let effective_enable_quality = enable_quality && !classic;
            let effective_enable_explore = enable_explore && !classic;

            let sel_idx = select_connection_idx(
                connections,
                *last_selected_idx,
                effective_enable_quality,
                effective_enable_explore,
                classic,
                Instant::now(),
            );
            if let Some(sel_idx) = sel_idx {
                forward_via_connection(
                    sel_idx,
                    pkt,
                    seq,
                    connections,
                    last_selected_idx,
                    seq_to_conn,
                    seq_order,
                    last_client_addr,
                    shared_client_addr,
                    src,
                )
                .await;
            } else {
                warn!("no available connection to forward packet from {}", src);
            }
            *last_client_addr = Some(src);
            // Update shared client address for instant forwarding
            if let Ok(mut addr_guard) = shared_client_addr.lock() {
                *addr_guard = Some(src);
            }
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
    seq_to_conn: &mut HashMap<u32, SequenceTrackingEntry>,
    seq_order: &mut VecDeque<u32>,
    last_client_addr: &mut Option<SocketAddr>,
    shared_client_addr: &Arc<Mutex<Option<SocketAddr>>>,
    src: SocketAddr,
) {
    if sel_idx >= connections.len() {
        return;
    }
    if *last_selected_idx != Some(sel_idx) {
        if let Some(prev_idx) = *last_selected_idx {
            if prev_idx < connections.len() {
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
    let conn = &mut connections[sel_idx];
    if let Err(e) = conn.send_data_with_tracking(pkt, seq).await {
        warn!(
            "{}: sendto() failed, marking for recovery: {}",
            conn.label, e
        );
        conn.mark_for_recovery();
    }
    if let Some(s) = seq {
        if seq_to_conn.len() >= MAX_SEQUENCE_TRACKING
            && let Some(old) = seq_order.pop_front()
        {
            seq_to_conn.remove(&old);
        }
        seq_to_conn.insert(
            s,
            SequenceTrackingEntry {
                conn_id: connections[sel_idx].conn_id,
                timestamp_ms: now_ms(),
            },
        );
        seq_order.push_back(s);
    }
    *last_client_addr = Some(src);
    if let Ok(mut addr_guard) = shared_client_addr.lock() {
        *addr_guard = Some(src);
    }
}
