use std::collections::HashMap;

use anyhow::{Result, anyhow};
use srtla_core::connection::{STARTUP_GRACE_MS, SrtlaConnection};
use srtla_core::registration::SrtlaRegistrationManager;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, info, warn};

use super::connections::reconnect_uplink;
use super::uplink::{ConnIoMap, ConnectionId, ReaderHandle, UplinkPacket, restart_reader_for};

pub const GLOBAL_TIMEOUT_MS: u64 = 10_000;

/// Handle periodic housekeeping tasks.
///
/// With the ring buffer sequence tracker, we no longer need periodic cleanup
/// since old entries are naturally overwritten.
#[allow(clippy::too_many_arguments)]
pub async fn handle_housekeeping(
    connections: &mut [SrtlaConnection],
    conn_io: &mut ConnIoMap,
    reg: &mut SrtlaRegistrationManager,
    classic: bool,
    now_ms: u64,
    all_failed_at: &mut Option<u64>,
    reader_handles: &mut HashMap<ConnectionId, ReaderHandle>,
    packet_tx: &UnboundedSender<UplinkPacket>,
) -> Result<()> {
    // If we're waiting on a REG2 response past the timeout, proactively retry REG1
    let current_ms = now_ms;
    let _ = reg.clear_pending_if_timed_out(current_ms);

    if reg.is_probing() {
        let was_probing = true;
        reg.check_probing_complete();
        // If probing just completed, reset grace period for the selected connection
        if !reg.is_probing()
            && was_probing
            && let Some(idx) = reg.get_selected_connection_idx()
            && let Some(conn) = connections.get_mut(idx)
        {
            conn.reconnection.startup_grace_deadline_ms = current_ms + STARTUP_GRACE_MS;
            debug!(
                "{}: Reset grace period after being selected for initial registration",
                conn.label
            );
        }
    }

    // housekeeping: drive registration, send keepalives
    for (i, conn) in connections.iter_mut().enumerate() {
        // Simple reconnect-on-timeout, then allow reg driver to proceed
        if conn.is_timed_out(current_ms) {
            if conn.should_attempt_reconnect(current_ms) {
                let label = conn.label.clone();
                conn.record_reconnect_attempt(current_ms);
                if conn.connection_established_ms() == 0 {
                    debug!("{} initial registration timed out; retrying", label);
                } else {
                    warn!("{} timed out; attempting full socket reconnection", label);
                }
                // Perform full socket reconnection (re-opens the socket in the
                // I/O map; the connection itself owns no socket).
                match conn_io.get_mut(&conn.conn_id) {
                    Some(io) => {
                        if let Err(e) = reconnect_uplink(conn, io, current_ms).await {
                            warn!("{} failed to reconnect: {}", label, e);
                            // Fall back to mark_for_recovery if reconnect fails
                            conn.mark_for_recovery();
                        } else {
                            restart_reader_for(conn, io.socket.clone(), reader_handles, packet_tx);
                        }
                    }
                    None => {
                        warn!("{} has no I/O entry; marking for recovery", label);
                        conn.mark_for_recovery();
                    }
                }

                match reg.pending_reg2_idx() {
                    Some(idx) if idx == i => {
                        info!("{} marked for recovery; re-sending REG1", label);
                        let pkt = reg.build_reg1_for(i, current_ms);
                        if let Some(io) = conn_io.get(&conn.conn_id) {
                            match io.socket.send(&pkt).await {
                                Ok(_) => conn.note_sent(current_ms),
                                Err(e) => warn!("Failed to send REG1 to uplink #{i}: {e:?}"),
                            }
                        }
                    }
                    Some(_) => {
                        debug!(
                            "{} timed out but another uplink is awaiting REG2; deferring",
                            label
                        );
                    }
                    None => {
                        info!("{} marked for recovery; re-sending REG2", label);
                        let pkt = reg.build_reg2(i);
                        if let Some(io) = conn_io.get(&conn.conn_id) {
                            match io.socket.send(&pkt).await {
                                Ok(_) => conn.note_sent(current_ms),
                                Err(e) => warn!("Failed to send REG2 to uplink #{i}: {e:?}"),
                            }
                        }
                    }
                }
            } else {
                debug!("{} timed out but in retry interval", conn.label);
            }
            continue;
        }

        if conn.needs_keepalive(current_ms) {
            let ka = conn.keepalive_packet(current_ms);
            if let Some(io) = conn_io.get(&conn.conn_id) {
                let _ = io.socket.send(&ka).await;
            }
        }
        if conn.needs_rtt_measurement(current_ms) {
            let ka = conn.keepalive_packet(current_ms);
            if let Some(io) = conn_io.get(&conn.conn_id) {
                let _ = io.socket.send(&ka).await;
            }
        }
        if !classic {
            conn.perform_window_recovery(current_ms);
        }
        // Update bitrate calculation
        conn.calculate_bitrate(current_ms);
        // Drive link lifecycle phase transitions
        conn.update_phase(current_ms);
        // Adapt the per-connection batch-send regime to the observed
        // load. Cheap; no-op when the regime hasn't changed.
        conn.recompute_batch_regime();

        // The reader task self-heals via CONN_TIMEOUT, but a task death
        // (panic / early return) would otherwise go undetected until that window.
        // Poll its handle cheaply each tick and respawn it for a still-active link
        // so inbound ACK/NAK/keepalive traffic resumes immediately, not seconds later.
        let reader_dead = reader_handles
            .get(&conn.conn_id)
            .is_some_and(|reader| reader.handle.is_finished());
        if reader_dead && let Some(io) = conn_io.get(&conn.conn_id) {
            warn!("{}: uplink reader task ended; restarting", conn.label);
            restart_reader_for(conn, io.socket.clone(), reader_handles, packet_tx);
        }
    }

    // Update active connections count (matches C implementation behavior)
    // C code resets active_connections=0 then counts non-timed-out connections
    reg.update_active_connections(connections);

    // drive registration (send REG1/REG2 as needed)
    let sends = reg.reg_driver_pending_sends(connections.len(), current_ms);
    if let Some((idx, pkt)) = sends.reg1
        && let Some(conn) = connections.get_mut(idx)
        && let Some(io) = conn_io.get(&conn.conn_id)
    {
        match io.socket.send(&pkt).await {
            Ok(_) => conn.note_sent(current_ms),
            Err(e) => warn!("Failed to send REG1 to uplink #{idx}: {e:?}"),
        }
    }
    if let Some(pkt) = sends.broadcast_reg2 {
        for (i, conn) in connections.iter_mut().enumerate() {
            if let Some(io) = conn_io.get(&conn.conn_id)
                && io.socket.send(&pkt).await.is_ok()
            {
                conn.note_sent(current_ms);
                debug!("REG2 → uplink #{i} sent");
            }
        }
    }

    // Check for connection failures and output appropriate error messages
    // This matches the C implementation's connection_housekeeping logic
    let active_connections = connections
        .iter()
        .filter(|c| !c.is_timed_out(current_ms))
        .count();

    if active_connections == 0 {
        if all_failed_at.is_none() {
            // Monotonic ms stamp on the single now_ms() clock; the all-links-failed
            // timeout below is a plain difference against the per-tick current_ms.
            *all_failed_at = Some(current_ms);
        }

        if reg.has_connected {
            error!("warning: no available connections");
        }

        // Timeout when all connections have failed. Measure elapsed-time-since-failure
        // so a transient all-down blip only trips after a full GLOBAL_TIMEOUT_MS of
        // sustained failure, not the instant uptime exceeds it.
        if let Some(failed_at) = all_failed_at
            && current_ms.saturating_sub(*failed_at) > GLOBAL_TIMEOUT_MS
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

#[cfg(test)]
mod tests {
    use srtla_core::utils::now_ms;

    use super::*;
    use crate::sender::uplink::{create_uplink_channel, sync_readers};
    use crate::test_helpers::{
        create_test_conn_io_map, create_test_connection, create_test_connections,
    };

    #[tokio::test]
    async fn dead_reader_is_restarted_for_active_connection() {
        let mut connections = vec![create_test_connection().await];
        let conn_id = connections[0].conn_id;
        let mut conn_io = create_test_conn_io_map(&connections);
        let mut reg = SrtlaRegistrationManager::new();
        let mut all_failed_at: Option<u64> = None;

        let (packet_tx, _packet_rx) = create_uplink_channel();
        let mut reader_handles: HashMap<ConnectionId, ReaderHandle> = HashMap::new();
        sync_readers(&connections, &conn_io, &mut reader_handles, &packet_tx);

        // Abort the reader and let the runtime drive cancellation to completion,
        // reproducing a silently dead task (a handle that reports is_finished()).
        reader_handles.get(&conn_id).unwrap().handle.abort();
        for _ in 0..1000 {
            if reader_handles.get(&conn_id).unwrap().handle.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            reader_handles.get(&conn_id).unwrap().handle.is_finished(),
            "reader task should be dead after abort"
        );

        handle_housekeeping(
            &mut connections,
            &mut conn_io,
            &mut reg,
            false,
            now_ms(),
            &mut all_failed_at,
            &mut reader_handles,
            &packet_tx,
        )
        .await
        .expect("housekeeping on an active connection must not fail");

        // A finished handle can never un-finish itself; a live handle proves
        // housekeeping spawned a fresh reader in its place.
        assert!(
            !reader_handles.get(&conn_id).unwrap().handle.is_finished(),
            "housekeeping must respawn the dead reader for the still-active connection"
        );
    }

    /// The all-uplinks-failed timeout must measure time *since* the links failed,
    /// not the uptime captured at the moment of failure. With the buggy
    /// uptime-at-failure measure, the timer tripped on the first all-down pass as
    /// soon as total uptime exceeded `GLOBAL_TIMEOUT_MS`, erroring on a transient
    /// blip. Arming and the first re-check must not error; only a full
    /// `GLOBAL_TIMEOUT_MS` of sustained failure may fire it.
    ///
    /// `handle_housekeeping` takes `now` as an argument, so the elapsed-since-
    /// failure window is driven by the explicit timestamps passed here — no tokio
    /// virtual clock. `now_ms()` is monotonic and not tokio-controlled, so a paused
    /// clock could not drive this anymore.
    #[tokio::test]
    async fn all_failed_timeout_measures_elapsed_since_failure() {
        let mut connections = create_test_connections(2).await;
        let mut conn_io = create_test_conn_io_map(&connections);
        let mut reg = SrtlaRegistrationManager::new();
        // Models a stream that was established and then lost every link.
        reg.has_connected = true;
        let mut reader_handles: HashMap<ConnectionId, ReaderHandle> = HashMap::new();
        let (packet_tx, _packet_rx) = tokio::sync::mpsc::unbounded_channel::<UplinkPacket>();
        let mut all_failed_at: Option<u64> = None;

        let t0 = now_ms();

        // Drop all uplinks. Pin the reconnect backoff well past the whole test
        // window (max failure count -> 120s backoff) so housekeeping reaches the
        // timeout branch instead of attempting a socket reconnection.
        for conn in connections.iter_mut() {
            conn.mark_for_recovery();
            conn.reconnection.last_reconnect_attempt_ms = t0;
            conn.reconnection.reconnect_failure_count = 5;
        }

        // Arm: first all-down pass. Uptime is irrelevant (only now - failed_at
        // matters), so arming must not error however long the process has run.
        let armed = handle_housekeeping(
            &mut connections,
            &mut conn_io,
            &mut reg,
            false,
            t0,
            &mut all_failed_at,
            &mut reader_handles,
            &packet_tx,
        )
        .await;
        assert!(armed.is_ok(), "arming the all-failed timer must not error");
        assert!(all_failed_at.is_some(), "the failure timer should be armed");

        // Still within the window: no error until a full GLOBAL_TIMEOUT_MS elapses.
        let within = handle_housekeeping(
            &mut connections,
            &mut conn_io,
            &mut reg,
            false,
            t0 + GLOBAL_TIMEOUT_MS - 1000,
            &mut all_failed_at,
            &mut reader_handles,
            &packet_tx,
        )
        .await;
        assert!(
            within.is_ok(),
            "no error until a full {GLOBAL_TIMEOUT_MS}ms has elapsed since the links failed"
        );

        // Past the window: fires.
        let fired = handle_housekeeping(
            &mut connections,
            &mut conn_io,
            &mut reg,
            false,
            t0 + GLOBAL_TIMEOUT_MS + 1000,
            &mut all_failed_at,
            &mut reader_handles,
            &packet_tx,
        )
        .await;
        assert!(
            fired.is_err(),
            "the all-failed timeout must fire once a full {GLOBAL_TIMEOUT_MS}ms has elapsed since \
             failure"
        );
    }
}
