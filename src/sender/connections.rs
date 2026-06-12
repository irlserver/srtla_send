use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

use smallvec::SmallVec;
use tracing::{info, warn};

use super::sequence::SequenceTracker;
use crate::connection::SrtlaConnection;

pub struct PendingConnectionChanges {
    pub new_ips: Option<SmallVec<IpAddr, 4>>,
    pub receiver_host: String,
    pub receiver_port: u16,
}

/// Apply a reloaded uplink IP list to the live connection pool.
///
/// The pool is rebuilt in **ips-file order**: a surviving uplink keeps its
/// existing socket and registration state (no re-handshake, no disconnect) but
/// is repositioned to match the file, and a removed uplink is torn down. Because
/// the telemetry `conn_id` is the link's index in this vector (Task 10 ADR-001
/// contract), reordering the file reorders `conn_id` consistently — matching the
/// C sender's "reload reassigns by file order" semantics.
pub async fn apply_connection_changes(
    connections: &mut SmallVec<SrtlaConnection, 4>,
    new_ips: &[IpAddr],
    receiver_host: &str,
    receiver_port: u16,
    last_selected_idx: &mut Option<usize>,
    seq_tracker: &mut SequenceTracker,
) {
    let mut seen = HashSet::<IpAddr>::new();
    let desired: SmallVec<(IpAddr, String), 4> = new_ips
        .iter()
        .copied()
        .filter(|ip| seen.insert(*ip))
        .map(|ip| (ip, format!("{receiver_host}:{receiver_port} via {ip}")))
        .collect();
    let desired_labels: HashSet<&str> = desired.iter().map(|(_, label)| label.as_str()).collect();
    let current_labels: HashSet<&str> = connections.iter().map(|c| c.label.as_str()).collect();

    // Drop the leaving links' sequence entries so a stale NAK can't be
    // misattributed to whichever uplink later reuses that index.
    let removed_conn_ids: SmallVec<u64, 4> = connections
        .iter()
        .filter(|c| !desired_labels.contains(c.label.as_str()))
        .map(|c| c.conn_id)
        .collect();
    let removed = removed_conn_ids.len();
    for conn_id in &removed_conn_ids {
        seq_tracker.remove_connection(*conn_id);
    }

    let new_ip_list: SmallVec<IpAddr, 4> = desired
        .iter()
        .filter(|(_, label)| !current_labels.contains(label.as_str()))
        .map(|(ip, _)| *ip)
        .collect();
    let attempted_new = new_ip_list.len();
    let mut fresh: HashMap<String, SrtlaConnection> = if new_ip_list.is_empty() {
        HashMap::new()
    } else {
        create_connections_from_ips(&new_ip_list, receiver_host, receiver_port)
            .await
            .into_iter()
            .map(|c| (c.label.clone(), c))
            .collect()
    };
    let added = fresh.len();

    let previous_order: SmallVec<u64, 4> = connections.iter().map(|c| c.conn_id).collect();
    let mut survivors: HashMap<String, SrtlaConnection> = std::mem::take(connections)
        .into_iter()
        .filter(|c| desired_labels.contains(c.label.as_str()))
        .map(|c| (c.label.clone(), c))
        .collect();

    for (_, label) in &desired {
        if let Some(conn) = survivors.remove(label) {
            connections.push(conn);
        } else if let Some(conn) = fresh.remove(label) {
            connections.push(conn);
        }
    }

    if removed > 0 {
        info!("removed {removed} stale connection(s)");
    }
    if added > 0 {
        info!("added {added} new connection(s)");
    } else if attempted_new > 0 {
        warn!("failed to add any new connections (attempted {attempted_new})");
    }

    // A reorder invalidates the cached selection index, not just a removal.
    let new_order: SmallVec<u64, 4> = connections.iter().map(|c| c.conn_id).collect();
    if previous_order != new_order {
        *last_selected_idx = None;
    }
}

pub async fn create_connections_from_ips(
    ips: &[IpAddr],
    receiver_host: &str,
    receiver_port: u16,
) -> SmallVec<SrtlaConnection, 4> {
    let mut connections = SmallVec::new();
    for ip in ips {
        match SrtlaConnection::connect_from_ip(*ip, receiver_host, receiver_port).await {
            Ok(conn) => {
                info!("added uplink {}", conn.label);
                connections.push(conn);
            }
            Err(e) => warn!(
                "failed to add uplink {} -> {}:{}: {}",
                ip, receiver_host, receiver_port, e
            ),
        }
    }
    connections
}
