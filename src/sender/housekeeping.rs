use std::collections::HashMap;

use anyhow::{Result, anyhow};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use super::uplink::{ConnectionId, ReaderHandle, UplinkPacket, restart_reader_for};
use crate::connection::{STARTUP_GRACE_MS, SrtlaConnection};
use crate::registration::SrtlaRegistrationManager;
use crate::utils::now_ms;

pub const GLOBAL_TIMEOUT_MS: u64 = 10_000;

/// Handle periodic housekeeping tasks.
///
/// With the ring buffer sequence tracker, we no longer need periodic cleanup
/// since old entries are naturally overwritten.
#[allow(clippy::too_many_arguments)]
pub async fn handle_housekeeping(
    connections: &mut [SrtlaConnection],
    reg: &mut SrtlaRegistrationManager,
    classic: bool,
    all_failed_at: &mut Option<Instant>,
    reader_handles: &mut HashMap<ConnectionId, ReaderHandle>,
    packet_tx: &UnboundedSender<UplinkPacket>,
) -> Result<()> {
    // If we're waiting on a REG2 response past the timeout, proactively retry REG1
    let current_ms = now_ms();
    let _ = reg.clear_pending_if_timed_out(current_ms);

    if reg.is_probing() {
        let was_probing = true;
        reg.check_probing_complete();
        // If probing just completed, reset grace period for the selected connection
        if !reg.is_probing() && was_probing {
            if let Some(idx) = reg.get_selected_connection_idx() {
                if let Some(conn) = connections.get_mut(idx) {
                    conn.reconnection.startup_grace_deadline_ms = current_ms + STARTUP_GRACE_MS;
                    debug!(
                        "{}: Reset grace period after being selected for initial registration",
                        conn.label
                    );
                }
            }
        }
    }

    // housekeeping: drive registration, send keepalives
    for (i, conn) in connections.iter_mut().enumerate() {
        // Simple reconnect-on-timeout, then allow reg driver to proceed
        if conn.is_timed_out() {
            if conn.should_attempt_reconnect() {
                let label = conn.label.clone();
                conn.record_reconnect_attempt();
                if conn.connection_established_ms() == 0 {
                    debug!("{} initial registration timed out; retrying", label);
                } else {
                    warn!("{} timed out; attempting full socket reconnection", label);
                }
                // Perform full socket reconnection
                if let Err(e) = conn.reconnect().await {
                    warn!("{} failed to reconnect: {}", label, e);
                    // Fall back to mark_for_recovery if reconnect fails
                    conn.mark_for_recovery();
                } else {
                    restart_reader_for(conn, reader_handles, packet_tx);
                }

                match reg.pending_reg2_idx() {
                    Some(idx) if idx == i => {
                        info!("{} marked for recovery; re-sending REG1", label);
                        reg.send_reg1_to(i, conn).await;
                    }
                    Some(_) => {
                        debug!(
                            "{} timed out but another uplink is awaiting REG2; deferring",
                            label
                        );
                    }
                    None => {
                        info!("{} marked for recovery; re-sending REG2", label);
                        reg.send_reg2_to(i, conn).await;
                    }
                }
            } else {
                debug!("{} timed out but in retry interval", conn.label);
            }
            continue;
        }

        if conn.needs_keepalive() {
            let _ = conn.send_keepalive().await;
        }
        if conn.needs_rtt_measurement() {
            let _ = conn.send_keepalive().await;
        }
        if !classic {
            conn.perform_window_recovery();
        }
        // Update bitrate calculation (from Android C implementation)
        conn.calculate_bitrate();
    }

    // Update active connections count (matches C implementation behavior)
    // C code resets active_connections=0 then counts non-timed-out connections
    reg.update_active_connections(connections);

    // drive registration (send REG1/REG2 as needed)
    reg.reg_driver_send_if_needed(connections).await;

    // Check for connection failures and output appropriate error messages
    // This matches the C implementation's connection_housekeeping logic
    let active_connections = connections.iter().filter(|c| !c.is_timed_out()).count();

    if active_connections == 0 {
        if all_failed_at.is_none() {
            *all_failed_at = Some(Instant::now());
        }

        if reg.has_connected {
            error!("warning: no available connections");
        }

        // Timeout when all connections have failed
        if let Some(failed_at) = all_failed_at
            && crate::utils::instant_to_elapsed_ms(*failed_at) > GLOBAL_TIMEOUT_MS
        {
            if reg.has_connected {
                error!("Failed to re-establish any connections");
                return Err(anyhow!("Failed to re-establish any connections"));
            } else {
                error!("Failed to establish any initial connections");
                return Err(anyhow!("Failed to establish any initial connections"));
            }
        }
    } else {
        *all_failed_at = None;
    }

    // NOTE: With the ring buffer sequence tracker, no cleanup is needed.
    // Old entries are naturally overwritten when the buffer wraps around.

    Ok(())
}
