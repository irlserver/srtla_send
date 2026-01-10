mod connections;
mod housekeeping;
mod packet_handler;
mod selection;
mod sequence;
mod status;
mod uplink;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
// Re-export connection management functions for tests
#[allow(unused_imports)]
pub use connections::{
    PendingConnectionChanges, apply_connection_changes, create_connections_from_ips,
};
// Re-export public items used by tests
#[allow(unused_imports)]
pub use housekeeping::GLOBAL_TIMEOUT_MS;
use housekeeping::handle_housekeeping;
use packet_handler::{
    drain_packet_queue, flush_all_batches, handle_srt_packet, handle_uplink_packet,
};
#[allow(unused_imports)]
pub use selection::{calculate_quality_multiplier, select_connection_idx};
#[allow(unused_imports)]
pub use sequence::{SEQ_TRACKING_SIZE, SEQUENCE_TRACKING_MAX_AGE_MS, SequenceTracker};
use smallvec::SmallVec;
use status::log_connection_status;
use tokio::net::UdpSocket;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::{self, Duration, Instant};
use tracing::{debug, info, warn};
use uplink::{ConnectionId, ReaderHandle, create_uplink_channel, sync_readers};

use crate::registration::SrtlaRegistrationManager;
#[allow(unused_imports)]
use crate::toggles::{DynamicToggles, ToggleSnapshot};

pub const HOUSEKEEPING_INTERVAL_MS: u64 = 1000;
const STATUS_LOG_INTERVAL_MS: u64 = 30_000;

pub async fn run_sender_with_toggles(
    local_srt_port: u16,
    receiver_host: &str,
    receiver_port: u16,
    ips_file: &str,
    toggles: DynamicToggles,
) -> Result<()> {
    info!(
        "starting srtla_send: local_srt_port={}, receiver={}:{}, ips_file={}",
        local_srt_port, receiver_host, receiver_port, ips_file
    );
    let ips = read_ip_list(ips_file).await?;
    debug!(
        "uplink IPs loaded: {}",
        ips.iter()
            .map(|i| i.to_string())
            .collect::<SmallVec<_, 4>>()
            .join(", ")
    );
    if ips.is_empty() {
        return Err(anyhow!("no IPs in list: {}", ips_file));
    }

    let mut connections = create_connections_from_ips(&ips, receiver_host, receiver_port).await;
    if connections.is_empty() {
        return Err(anyhow!("no uplinks available"));
    }

    let local_listener = UdpSocket::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, local_srt_port)))
        .await
        .context("bind local SRT UDP listener")?;
    info!("listening for SRT on [::]:{}", local_srt_port);

    let mut reg = SrtlaRegistrationManager::new();

    reg.start_probing(&mut connections).await;

    let (packet_tx, mut packet_rx) = create_uplink_channel();
    let mut reader_handles: HashMap<ConnectionId, ReaderHandle> = HashMap::new();
    sync_readers(&connections, &mut reader_handles, &packet_tx);

    // Create instant ACK forwarding channel (sends client addr with packet)
    let (instant_tx, mut instant_rx) =
        tokio::sync::mpsc::unbounded_channel::<(SocketAddr, SmallVec<u8, 64>)>();

    // Wrap local_listener in Arc for sharing
    let local_listener = Arc::new(local_listener);

    // Spawn instant forwarding task
    {
        let local_listener_clone = local_listener.clone();
        tokio::spawn(async move {
            while let Some((client_addr, ack_packet)) = instant_rx.recv().await {
                let _ = local_listener_clone.send_to(&ack_packet, client_addr).await;
            }
        });
    }

    let mut recv_buf = vec![0u8; crate::protocol::MTU];
    let mut housekeeping_timer = time::interval_at(
        Instant::now() + Duration::from_millis(HOUSEKEEPING_INTERVAL_MS),
        Duration::from_millis(HOUSEKEEPING_INTERVAL_MS),
    );
    housekeeping_timer.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

    // Batch flush timer (15ms interval like Moblin)
    // This ensures packets are sent even when traffic is light
    const BATCH_FLUSH_INTERVAL_MS: u64 = 15;
    let mut batch_flush_timer = time::interval_at(
        Instant::now() + Duration::from_millis(BATCH_FLUSH_INTERVAL_MS),
        Duration::from_millis(BATCH_FLUSH_INTERVAL_MS),
    );
    batch_flush_timer.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    let mut status_elapsed_ms: u64 = 0;
    let mut last_client_addr: Option<SocketAddr> = None;
    // Zero-allocation ring buffer for sequence tracking
    let mut seq_tracker = SequenceTracker::new();
    let mut last_selected_idx: Option<usize> = None;
    let mut last_switch_time_ms: u64 = 0; // Track time of last connection switch
    let mut all_failed_at: Option<Instant> = None;
    let mut pending_changes: Option<PendingConnectionChanges> = None;

    // Prepare SIGHUP stream (Unix only) or a never-completing future (non-Unix)
    #[cfg(unix)]
    let mut sighup = signal(SignalKind::hangup())?;

    // Main loop - run housekeeping frequently like C version
    // Run housekeeping once before entering the main event loop so we start in a clean state.
    {
        let classic = toggles
            .classic_mode
            .load(std::sync::atomic::Ordering::Relaxed);
        if let Err(err) = handle_housekeeping(
            &mut connections,
            &mut reg,
            classic,
            &mut all_failed_at,
            &mut reader_handles,
            &packet_tx,
        )
        .await
        {
            warn!("initial housekeeping failed: {err}");
        }
    }

    // Use a macro to avoid duplicating the event loop for Unix/non-Unix
    // The only difference is SIGHUP handling on Unix
    macro_rules! event_loop {
        ($($sighup_branch:tt)*) => {
            loop {
                tokio::select! {
                    res = local_listener.recv_from(&mut recv_buf) => {
                        let toggle_snap = toggles.snapshot();
                        handle_srt_packet(
                            res,
                            &mut recv_buf,
                            &mut connections,
                            &mut last_selected_idx,
                            &mut last_switch_time_ms,
                            &mut seq_tracker,
                            &mut last_client_addr,
                            reg.has_connected,
                            &toggle_snap,
                        )
                        .await;
                        drain_packet_queue(
                            &mut packet_rx,
                            &mut connections,
                            &mut reg,
                            &instant_tx,
                            last_client_addr,
                            &local_listener,
                            &seq_tracker,
                            &toggle_snap,
                        )
                        .await;
                    }
                    packet = packet_rx.recv() => {
                        let toggle_snap = toggles.snapshot();
                        if let Some(packet) = packet {
                            handle_uplink_packet(
                                packet,
                                &mut connections,
                                &mut reg,
                                &instant_tx,
                                last_client_addr,
                                &local_listener,
                                &seq_tracker,
                                &toggle_snap,
                            ).await;
                            drain_packet_queue(
                                &mut packet_rx,
                                &mut connections,
                                &mut reg,
                                &instant_tx,
                                last_client_addr,
                                &local_listener,
                                &seq_tracker,
                                &toggle_snap,
                            ).await;
                        } else {
                            return Ok(());
                        }
                    }
                    _ = housekeeping_timer.tick() => {
                        let classic = toggles
                            .classic_mode
                            .load(std::sync::atomic::Ordering::Relaxed);
                        if let Err(err) = handle_housekeeping(
                            &mut connections,
                            &mut reg,
                            classic,
                            &mut all_failed_at,
                            &mut reader_handles,
                            &packet_tx,
                        ).await {
                            warn!("housekeeping failed: {err}");
                        }

                        if let Some(changes) = pending_changes.take()
                            && let Some(new_ips) = changes.new_ips
                        {
                            info!("applying queued connection changes: {} IPs", new_ips.len());
                            apply_connection_changes(
                                &mut connections,
                                &new_ips,
                                &changes.receiver_host,
                                changes.receiver_port,
                                &mut last_selected_idx,
                                &mut seq_tracker,
                            ).await;
                            info!("connection changes applied successfully");
                            sync_readers(&connections, &mut reader_handles, &packet_tx);
                        }

                        status_elapsed_ms = status_elapsed_ms.saturating_add(HOUSEKEEPING_INTERVAL_MS);
                        if status_elapsed_ms >= STATUS_LOG_INTERVAL_MS {
                            log_connection_status(&connections, last_selected_idx, &toggles);
                            status_elapsed_ms = status_elapsed_ms.saturating_sub(STATUS_LOG_INTERVAL_MS);
                        }

                        sync_readers(&connections, &mut reader_handles, &packet_tx);
                        let toggle_snap = toggles.snapshot();
                        drain_packet_queue(
                            &mut packet_rx,
                            &mut connections,
                            &mut reg,
                            &instant_tx,
                            last_client_addr,
                            &local_listener,
                            &seq_tracker,
                            &toggle_snap,
                        )
                        .await;
                    }
                    $($sighup_branch)*
                    _ = batch_flush_timer.tick() => {
                        flush_all_batches(&mut connections).await;
                    }
                }
            }
        };
    }

    #[cfg(unix)]
    event_loop! {
        _ = sighup.recv() => {
            info!("received SIGHUP - queuing uplink IP reload from {}", ips_file);
            if let Ok(new_ips) = read_ip_list(ips_file).await {
                pending_changes = Some(PendingConnectionChanges {
                    new_ips: Some(new_ips),
                    receiver_host: receiver_host.to_string(),
                    receiver_port,
                });
                info!("uplink IP changes queued for next processing cycle");
            }
            let toggle_snap = toggles.snapshot();
            drain_packet_queue(
                &mut packet_rx,
                &mut connections,
                &mut reg,
                &instant_tx,
                last_client_addr,
                &local_listener,
                &seq_tracker,
                &toggle_snap,
            )
            .await;
        }
    }

    #[cfg(not(unix))]
    event_loop! {}
}

pub async fn read_ip_list(path: &str) -> Result<SmallVec<IpAddr, 4>> {
    let text = std::fs::read_to_string(Path::new(path)).context("read IPs file")?;
    let mut out = SmallVec::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        match IpAddr::from_str(l) {
            Ok(ip) => out.push(ip),
            Err(e) => warn!("skip invalid IP '{}': {}", l, e),
        }
    }
    Ok(out)
}
