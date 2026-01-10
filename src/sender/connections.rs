use std::collections::HashSet;
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

pub async fn apply_connection_changes(
    connections: &mut SmallVec<SrtlaConnection, 4>,
    new_ips: &[IpAddr],
    receiver_host: &str,
    receiver_port: u16,
    last_selected_idx: &mut Option<usize>,
    seq_tracker: &mut SequenceTracker,
) {
    let current_labels: HashSet<String> = connections.iter().map(|c| c.label.clone()).collect();
    let desired_labels: HashSet<String> = new_ips
        .iter()
        .map(|ip| format!("{}:{} via {}", receiver_host, receiver_port, ip))
        .collect();

    // Remove stale connections
    let old_len = connections.len();
    let removed_conn_ids: Vec<u64> = connections
        .iter()
        .filter(|c| !desired_labels.contains(&c.label))
        .map(|c| c.conn_id)
        .collect();

    connections.retain(|c| desired_labels.contains(&c.label));

    // If connections were removed, reset selection state and clean up sequence tracker
    if connections.len() != old_len {
        info!("removed {} stale connections", old_len - connections.len());
        *last_selected_idx = None;

        // Remove entries for removed connections from the ring buffer
        for conn_id in removed_conn_ids {
            seq_tracker.remove_connection(conn_id);
        }
    }

    // Add new connections
    let mut seen = HashSet::<IpAddr>::new();
    let new_ips_needed: SmallVec<IpAddr, 4> = new_ips
        .iter()
        .copied()
        .filter(|ip| seen.insert(*ip))
        .filter(|ip| {
            let label = format!("{}:{} via {}", receiver_host, receiver_port, ip);
            !current_labels.contains(&label)
        })
        .collect();

    if !new_ips_needed.is_empty() {
        let mut new_connections =
            create_connections_from_ips(&new_ips_needed, receiver_host, receiver_port).await;
        let added_count = new_connections.len();
        connections.append(&mut new_connections);

        if added_count > 0 {
            info!("added {} new connections", added_count);
        } else {
            warn!(
                "failed to add any new connections (attempted {})",
                new_ips_needed.len()
            );
        }
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
