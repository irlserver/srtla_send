use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use tokio::net::UdpSocket;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
// mpsc is available in tokio::sync
use tokio::time::{self, Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::connection::SrtlaConnection;
use crate::protocol::{self, MTU, PKT_LOG_SIZE};
use crate::registration::SrtlaRegistrationManager;
use crate::toggles::DynamicToggles;
use crate::utils::now_ms;

pub const MIN_SWITCH_INTERVAL_MS: u64 = 500;
pub const MAX_SEQUENCE_TRACKING: usize = 10_000;
pub const SEQUENCE_TRACKING_MAX_AGE_MS: u64 = 5000;
pub const SEQUENCE_MAP_CLEANUP_INTERVAL_MS: u64 = 5000;
pub const GLOBAL_TIMEOUT_MS: u64 = 10_000;

pub(crate) struct SequenceTrackingEntry {
    pub(crate) conn_idx: usize,
    pub(crate) timestamp_ms: u64,
}

impl SequenceTrackingEntry {
    /// Returns whether this entry's timestamp is older than SEQUENCE_TRACKING_MAX_AGE_MS.
    ///
    /// The entry is considered expired when `current_time_ms - timestamp_ms > SEQUENCE_TRACKING_MAX_AGE_MS`.
    ///
    /// # Examples
    ///
    /// ```
    /// let e = SequenceTrackingEntry { conn_idx: 0, timestamp_ms: 1_000 };
    /// // Exactly at the threshold is not expired
    /// assert!(!e.is_expired(1_000 + SEQUENCE_TRACKING_MAX_AGE_MS));
    /// // One millisecond past the threshold is expired
    /// assert!(e.is_expired(1_000 + SEQUENCE_TRACKING_MAX_AGE_MS + 1));
    /// ```
    fn is_expired(&self, current_time_ms: u64) -> bool {
        current_time_ms.saturating_sub(self.timestamp_ms) > SEQUENCE_TRACKING_MAX_AGE_MS
    }
}

pub struct PendingConnectionChanges {
    pub new_ips: Option<Vec<IpAddr>>,
    pub receiver_host: String,
    pub receiver_port: u16,
}

/// Start the SRT sender main loop using the provided uplink configuration and runtime toggles.
///
/// This binds a local UDP listener, loads uplink IPs from `ips_file`, establishes connections
/// to the receiver, spawns an instant-ACK forwarder, and runs continuous housekeeping and
/// packet-forwarding logic until an error or termination occurs. On Unix, a SIGHUP triggers
/// reloading of the uplink IP list (queued and applied on the next main loop iteration).
///
/// # Returns
///
/// `Ok(())` on successful startup and execution; `Err` if initialization fails (for example,
/// reading the IP list, creating uplinks, or binding the local UDP listener).
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use tokio::time;
///
/// // Construct toggles appropriately for your program; this example assumes a default.
/// let toggles = DynamicToggles::default();
///
/// // Run the sender in a tokio runtime (no_run to avoid executing in doc tests).
/// tokio::spawn(async move {
///     let _ = run_sender_with_toggles(9000, "receiver.example.com", 9001, "/etc/uplinks.txt", toggles).await;
/// });
///
/// // Keep the example alive briefly to illustrate runtime usage.
/// let mut rt = tokio::runtime::Runtime::new().unwrap();
/// rt.block_on(async {
///     time::sleep(Duration::from_millis(10)).await;
/// });
/// ```
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
            .collect::<Vec<_>>()
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

    // Create instant ACK forwarding channel and shared client address
    let (instant_tx, instant_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let shared_client_addr = Arc::new(Mutex::new(None::<SocketAddr>));

    // Wrap local_listener in Arc for sharing
    let local_listener = Arc::new(local_listener);

    // Spawn instant forwarding task
    {
        let local_listener_clone = local_listener.clone();
        let shared_client_addr_clone = shared_client_addr.clone();
        tokio::spawn(async move {
            while let Ok(ack_packet) = instant_rx.recv() {
                let client_addr = {
                    match shared_client_addr_clone.lock() {
                        Ok(addr_guard) => *addr_guard,
                        _ => None,
                    }
                };
                if let Some(client) = client_addr {
                    let _ = local_listener_clone.send_to(&ack_packet, client).await;
                }
            }
        });
    }

    let mut recv_buf = vec![0u8; MTU];
    let mut housekeeping_timer = time::interval(Duration::from_millis(1));
    let mut status_counter: u64 = 0;
    let mut last_client_addr: Option<SocketAddr> = None;
    let mut seq_to_conn: HashMap<u32, SequenceTrackingEntry> =
        HashMap::with_capacity(MAX_SEQUENCE_TRACKING);
    let mut seq_order: VecDeque<u32> = VecDeque::with_capacity(MAX_SEQUENCE_TRACKING);
    let mut last_sequence_cleanup_ms: u64 = 0;
    let mut last_selected_idx: Option<usize> = None;
    let mut last_switch_time: Option<Instant> = None;
    let mut all_failed_at: Option<Instant> = None;
    let mut pending_changes: Option<PendingConnectionChanges> = None;

    // Prepare SIGHUP stream (Unix only)
    #[cfg(unix)]
    #[allow(unused_variables)]
    let mut sighup = signal(SignalKind::hangup())?;

    // Main loop - run housekeeping frequently like C version
    #[cfg(unix)]
    loop {
        {
            let classic = toggles
                .classic_mode
                .load(std::sync::atomic::Ordering::Relaxed);
            let _ = handle_housekeeping(
                &mut connections,
                &mut reg,
                &instant_tx,
                last_client_addr,
                &local_listener,
                &mut seq_to_conn,
                &mut seq_order,
                &mut last_sequence_cleanup_ms,
                classic,
                &mut all_failed_at,
            )
            .await;
        }

        // Apply pending connection changes immediately (like C srtla_send)
        // This matches C behavior: SIGHUP sets flag, next loop iteration applies changes
        if let Some(changes) = pending_changes.take()
            && let Some(new_ips) = changes.new_ips {
                info!("applying queued connection changes: {} IPs", new_ips.len());
                apply_connection_changes(
                    &mut connections,
                    &new_ips,
                    &changes.receiver_host,
                    changes.receiver_port,
                    &mut last_selected_idx,
                    &mut seq_to_conn,
                    &mut seq_order,
                )
                .await;
                info!("connection changes applied successfully");
            }

        tokio::select! {
            res = local_listener.recv_from(&mut recv_buf) => {
                handle_srt_packet(
                    res,
                    &mut recv_buf,
                    &mut connections,
                    &mut last_selected_idx,
                    &mut last_switch_time,
                    &mut seq_to_conn,
                    &mut seq_order,
                    &mut last_client_addr,
                    &shared_client_addr,
                    reg.has_connected,
                    &toggles,
                )
                .await;
            }
            _ = housekeeping_timer.tick() => {
                // Periodic status reporting (every 30 seconds = 30,000 ticks at 1ms intervals)
                status_counter = status_counter.wrapping_add(1);
                if status_counter.is_multiple_of(30000) {
                    log_connection_status(&connections, &seq_to_conn, &seq_order, last_selected_idx, &toggles);
                }
            }
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
            }
        }
    }

    #[cfg(not(unix))]
    loop {
        {
            let classic = toggles
                .classic_mode
                .load(std::sync::atomic::Ordering::Relaxed);
            let _ = handle_housekeeping(
                &mut connections,
                &mut reg,
                &instant_tx,
                last_client_addr,
                &local_listener,
                &mut seq_to_conn,
                &mut seq_order,
                &mut last_sequence_cleanup_ms,
                classic,
                &mut all_failed_at,
            )
            .await;
        }

        // Apply pending connection changes immediately (like C srtla_send)
        if let Some(changes) = pending_changes.take() {
            if let Some(new_ips) = changes.new_ips {
                info!("applying queued connection changes: {} IPs", new_ips.len());
                apply_connection_changes(
                    &mut connections,
                    &new_ips,
                    &changes.receiver_host,
                    changes.receiver_port,
                    &mut last_selected_idx,
                    &mut seq_to_conn,
                    &mut seq_order,
                )
                .await;
                info!("connection changes applied successfully");
            }
        }

        tokio::select! {
            res = local_listener.recv_from(&mut recv_buf) => {
                handle_srt_packet(
                    res,
                    &mut recv_buf,
                    &mut connections,
                    &mut last_selected_idx,
                    &mut last_switch_time,
                    &mut seq_to_conn,
                    &mut seq_order,
                    &mut last_client_addr,
                    &shared_client_addr,
                    reg.has_connected,
                    &toggles,
                )
                .await;
            }
            _ = housekeeping_timer.tick() => {
                // Periodic status reporting (every 30 seconds = 30,000 ticks at 1ms intervals)
                status_counter = status_counter.wrapping_add(1);
                if status_counter.is_multiple_of(30000) {
                    log_connection_status(&connections, &seq_to_conn, &seq_order, last_selected_idx, &toggles);
                }
            }
        }
    }
}

/// Process a received SRT packet and forward it to an appropriate uplink connection.
///
/// This function parses a single received packet (or a receive error), selects an uplink
/// connection according to the current registration state and dynamic toggles, forwards the
/// packet to the chosen connection, and updates client address and sequence-to-connection
/// tracking state used for later NAK attribution and cleanup.
///
/// The `registration_complete` flag causes the function to use a simplified forwarding
/// strategy (prefer existing, non-timed-out connection) while registration is in progress.
/// When registration is complete, the selection honors the toggles (stickiness, quality,
/// exploration, classic) to choose the best connection via `select_connection_idx`.
///
/// Parameters that are not self-explanatory:
/// - `registration_complete`: when false, use a simpler sender selection path for pre-registration packets.
/// - `toggles`: runtime toggle flags that influence selection behavior (stickiness, quality, exploration, classic).
/// - `shared_client_addr`: updated with the latest client SocketAddr so other tasks can forward instant ACKs.
///
/// # Examples
///
/// ```no_run
/// # use std::sync::{Arc, Mutex};
/// # use std::collections::{HashMap, VecDeque};
/// # use std::net::SocketAddr;
/// # use std::time::Instant;
/// # async fn example() {
/// #     // The following are placeholders to illustrate usage; real values require actual types from the crate.
/// #     let res: Result<(usize, SocketAddr), std::io::Error> = Err(std::io::Error::new(std::io::ErrorKind::Other, "noop"));
/// #     let mut recv_buf = [0u8; 1500];
/// #     let mut connections: Vec<crate::SrtlaConnection> = Vec::new();
/// #     let mut last_selected_idx: Option<usize> = None;
/// #     let mut last_switch_time: Option<Instant> = None;
/// #     let mut seq_to_conn: HashMap<u32, crate::SequenceTrackingEntry> = HashMap::new();
/// #     let mut seq_order: VecDeque<u32> = VecDeque::new();
/// #     let mut last_client_addr: Option<SocketAddr> = None;
/// #     let shared_client_addr = Arc::new(Mutex::new(None));
/// #     let toggles = crate::DynamicToggles::default();
/// #     handle_srt_packet(
/// #         res,
/// #         &mut recv_buf,
/// #         &mut connections,
/// #         &mut last_selected_idx,
/// #         &mut last_switch_time,
/// #         &mut seq_to_conn,
/// #         &mut seq_order,
/// #         &mut last_client_addr,
/// #         &shared_client_addr,
/// #         false,
/// #         &toggles,
/// #     ).await;
/// # }
/// ```
#[allow(clippy::too_many_arguments)]
async fn handle_srt_packet(
    res: Result<(usize, SocketAddr), std::io::Error>,
    recv_buf: &mut [u8],
    connections: &mut [SrtlaConnection],
    last_selected_idx: &mut Option<usize>,
    last_switch_time: &mut Option<Instant>,
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
                                .get(0)
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
                        last_switch_time,
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
            // pick best connection (respect dynamic stickiness toggle)
            let enable_stick = toggles
                .stickiness_enabled
                .load(std::sync::atomic::Ordering::Relaxed);
            let enable_quality = toggles
                .quality_scoring_enabled
                .load(std::sync::atomic::Ordering::Relaxed);
            let enable_explore = toggles
                .exploration_enabled
                .load(std::sync::atomic::Ordering::Relaxed);
            let classic = toggles
                .classic_mode
                .load(std::sync::atomic::Ordering::Relaxed);

            // Classic mode overrides all other toggles
            let effective_enable_stick = enable_stick && !classic;
            let effective_enable_quality = enable_quality && !classic;
            let effective_enable_explore = enable_explore && !classic;

            let sel_idx = select_connection_idx(
                connections,
                if effective_enable_stick {
                    *last_selected_idx
                } else {
                    None
                },
                if effective_enable_stick {
                    *last_switch_time
                } else {
                    None
                },
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
                    last_switch_time,
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

/// Forwards a packet through a selected uplink connection and updates selection and sequence tracking state.
///
/// Updates the last-selected connection and switch timestamp when the selection changes. Attempts to send
/// `pkt` via the selected connection; on send failure the connection is marked for recovery. If `seq` is
/// provided, records a mapping from that sequence number to the chosen connection index (with a timestamp),
/// enforcing the `MAX_SEQUENCE_TRACKING` capacity by evicting the oldest tracked sequence when necessary.
/// Also updates the per-client last seen address (`last_client_addr`) and the shared client address
/// (`shared_client_addr`).
///
/// # Parameters
///
/// - `sel_idx`: index of the connection to use; the function returns immediately if this is out of range.
/// - `pkt`: the raw packet to forward.
/// - `seq`: optional SRT sequence number used to attribute future NAKs/ACKs to the chosen connection.
/// - `connections`: mutable slice of candidate `SrtlaConnection`s; the function sends using `connections[sel_idx]`.
/// - `last_selected_idx`, `last_switch_time`: updated when the active connection changes.
/// - `seq_to_conn`, `seq_order`: mutated to record and prune sequence→connection mappings for NAK attribution.
/// - `last_client_addr`: updated to `src`.
/// - `shared_client_addr`: an Arc<Mutex<...>> updated with `src` when the lock is available.
/// - `src`: source socket address of the local SRT client.
///
/// # Examples
///
/// ```
/// # use std::sync::{Arc, Mutex};
/// # use std::collections::{HashMap, VecDeque};
/// # use std::net::SocketAddr;
/// # use std::time::Instant;
/// # use tokio::runtime::Runtime;
/// # // Minimal invocation that returns early when no connections are present.
/// # let rt = Runtime::new().unwrap();
/// # rt.block_on(async {
/// let mut connections: Vec<crate::sender::SrtlaConnection> = Vec::new();
/// let mut last_selected_idx = None;
/// let mut last_switch_time = None;
/// let mut seq_to_conn: HashMap<u32, crate::sender::SequenceTrackingEntry> = HashMap::new();
/// let mut seq_order: VecDeque<u32> = VecDeque::new();
/// let mut last_client_addr = None;
/// let shared_client_addr = Arc::new(Mutex::new(None));
/// let src: SocketAddr = "127.0.0.1:10000".parse().unwrap();
/// crate::sender::forward_via_connection(
///     0,
///     b"",
///     None,
///     &mut connections,
///     &mut last_selected_idx,
///     &mut last_switch_time,
///     &mut seq_to_conn,
///     &mut seq_order,
///     &mut last_client_addr,
///     &shared_client_addr,
///     src,
/// ).await;
/// # });
/// ```
#[allow(clippy::too_many_arguments)]
async fn forward_via_connection(
    sel_idx: usize,
    pkt: &[u8],
    seq: Option<u32>,
    connections: &mut [SrtlaConnection],
    last_selected_idx: &mut Option<usize>,
    last_switch_time: &mut Option<Instant>,
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
        *last_switch_time = Some(Instant::now());
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
                conn_idx: sel_idx,
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

/// Perform periodic housekeeping for uplink connections and registration state.
///
/// Processes incoming ACK/NAK/SRTLA messages from each connection, attributes NAKs to
/// previously recorded sequence-to-connection mappings when not expired, forwards responses
/// back to the local SRT client, drives the registration state machine (REG1/REG2),
/// triggers reconnects and recovery for timed-out connections, sends keepalives/RTT probes,
/// updates the registration manager's view of active connections, and prunes expired
/// sequence-tracking entries. If all uplinks remain unavailable longer than
/// GLOBAL_TIMEOUT_MS, an error is returned.
///
/// # Parameters
///
/// - `connections`: mutable slice of uplink connections to maintain.
/// - `reg`: registration manager responsible for REG1/REG2 state and active-connection tracking.
/// - `instant_tx`: channel sender used for forwarding instant ACKs to the local client forwarder.
/// - `last_client_addr`: last known local SRT client socket address; responses are forwarded there if present.
/// - `local_listener`: local UDP socket used to send forwarded packets back to the SRT client.
/// - `seq_to_conn`: mapping from SRT sequence numbers to the connection index and timestamp used for NAK attribution.
/// - `seq_order`: deque tracking insertion order of sequences for eviction when capacity is reached.
/// - `last_sequence_cleanup_ms`: last cleanup timestamp for sequence-tracking expiry checks; updated by cleanup routine.
/// - `classic`: when true, enable legacy/global behaviors (e.g., global SRTLA ACK handling) instead of the enhanced logic.
/// - `all_failed_at`: optional instant marking when all connections first became unavailable; used to detect prolonged outage and decide when to return an error.
///
/// # Returns
///
/// `Ok(())` on successful housekeeping; `Err` if all connections have been unavailable longer than GLOBAL_TIMEOUT_MS
/// (distinguishes between failing to re-establish previously connected uplinks and failing to establish any initial connections).
///
/// # Examples
///
/// ```no_run
/// use std::collections::{HashMap, VecDeque};
/// use std::sync::mpsc::Sender;
/// use tokio::net::UdpSocket;
/// # async fn example() -> anyhow::Result<()> {
///     // Setup placeholders (real initialization omitted)
///     let mut connections: Vec<crate::SrtlaConnection> = Vec::new();
///     let mut reg = crate::SrtlaRegistrationManager::new();
///     let instant_tx: Sender<Vec<u8>> = std::sync::mpsc::channel().0;
///     let last_client_addr = None;
///     let local_listener = UdpSocket::bind("0.0.0.0:0").await?;
///     let mut seq_to_conn: HashMap<u32, crate::SequenceTrackingEntry> = HashMap::new();
///     let mut seq_order: VecDeque<u32> = VecDeque::new();
///     let mut last_sequence_cleanup_ms: u64 = 0;
///     let classic = false;
///     let mut all_failed_at = None;
///
///     // Drive one housekeeping cycle
///     crate::handle_housekeeping(
///         &mut connections,
///         &mut reg,
///         &instant_tx,
///         last_client_addr,
///         &local_listener,
///         &mut seq_to_conn,
///         &mut seq_order,
///         &mut last_sequence_cleanup_ms,
///         classic,
///         &mut all_failed_at,
///     ).await?;
///     # Ok(())
/// # }
/// ```
async fn handle_housekeeping(
    connections: &mut [SrtlaConnection],
    reg: &mut SrtlaRegistrationManager,
    instant_tx: &std::sync::mpsc::Sender<Vec<u8>>,
    last_client_addr: Option<SocketAddr>,
    local_listener: &UdpSocket,
    seq_to_conn: &mut HashMap<u32, SequenceTrackingEntry>,
    seq_order: &mut VecDeque<u32>,
    last_sequence_cleanup_ms: &mut u64,
    classic: bool,
    all_failed_at: &mut Option<Instant>,
) -> Result<()> {
    // If we're waiting on a REG2 response past the timeout, proactively retry REG1
    let current_ms = now_ms();
    let _ = reg.clear_pending_if_timed_out(current_ms);

    // housekeeping: receive responses, drive registration, send keepalives
    for i in 0..connections.len() {
        let incoming = connections[i].drain_incoming(i, reg, instant_tx).await?;
        if incoming.read_any { /* timestamps updated inside */ }

        // Apply ACKs to all connections like Java
        for ack in incoming.ack_numbers.iter() {
            for c in connections.iter_mut() {
                c.handle_srt_ack(*ack as i32);
            }
        }

        // Process SRTLA ACKs: first find specific packet, then optionally apply
        // classic-mode global recovery.
        for srtla_ack in incoming.srtla_ack_numbers.iter() {
            // Phase 1: Find the connection that sent this specific packet
            for c in connections.iter_mut() {
                if c.handle_srtla_ack_specific(*srtla_ack as i32, classic) {
                    break;
                }
            }

            // Phase 2: Classic mode mirrors the C implementation by bumping every
            // active connection's window. Enhanced mode relies on the fine-grained
            // Bond Bunny logic inside `handle_srtla_ack_specific` and skips the
            // global +1 to avoid over-inflating other links.
            if classic {
                for c in connections.iter_mut() {
                    c.handle_srtla_ack_global();
                }
            }
        }

        for nak in incoming.nak_numbers.iter() {
            if let Some(entry) = seq_to_conn.get(nak) {
                let current_time = now_ms();
                if !entry.is_expired(current_time)
                    && let Some(conn) = connections.get_mut(entry.conn_idx) {
                        conn.handle_nak(*nak as i32);
                        continue;
                    }
            }
            connections[i].handle_nak(*nak as i32);
        }

        // Forward responses back to local SRT client
        if let Some(client) = last_client_addr {
            for pkt in incoming.forward_to_client.iter() {
                let _ = local_listener.send_to(pkt, client).await;
            }
        }

        // Simple reconnect-on-timeout, then allow reg driver to proceed
        if connections[i].is_timed_out() {
            if connections[i].should_attempt_reconnect() {
                let label = connections[i].label.clone();
                connections[i].record_reconnect_attempt();
                warn!(
                    "{} timed out; resetting connection state for recovery",
                    label
                );
                // Use C-style recovery: reset state but keep socket alive
                connections[i].mark_for_recovery();

                match reg.pending_reg2_idx() {
                    Some(idx) if idx == i => {
                        info!("{} marked for recovery; re-sending REG1", label);
                        reg.send_reg1_to(i, &mut connections[i]).await;
                    }
                    Some(_) => {
                        debug!(
                            "{} timed out but another uplink is awaiting REG2; deferring",
                            label
                        );
                    }
                    None => {
                        info!("{} marked for recovery; re-sending REG2", label);
                        reg.send_reg2_to(i, &mut connections[i]).await;
                    }
                }
            } else {
                debug!("{} timed out but in retry interval", connections[i].label);
            }
            continue;
        }

        if connections[i].needs_keepalive() {
            let _ = connections[i].send_keepalive().await;
        }
        if connections[i].needs_rtt_measurement() {
            let _ = connections[i].send_keepalive().await;
        }
        if !classic {
            connections[i].perform_window_recovery();
        }
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
            && failed_at.elapsed().as_millis() > GLOBAL_TIMEOUT_MS as u128
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

    cleanup_expired_sequence_tracking(seq_to_conn, seq_order, last_sequence_cleanup_ms);

    Ok(())
}

/// Removes expired sequence-to-connection mappings and prunes the sequence order list.
///
/// This function runs a periodic cleanup: if enough time has elapsed since `last_cleanup_ms`,
/// it removes entries from `seq_to_conn` whose `timestamp_ms` is older than the configured
/// expiration window, then filters `seq_order` to only keep sequence numbers still present
/// in `seq_to_conn`. When removals occur it logs the number cleaned and the resulting capacity
/// usage; it also warns when the tracking map exceeds 80% of its configured capacity.
///
/// Parameters:
/// - `seq_to_conn`: mutable map from sequence number to `SequenceTrackingEntry` to be pruned.
/// - `seq_order`: mutable FIFO-like deque of sequence numbers used to track insertion order;
///   entries for sequences removed from `seq_to_conn` will be dropped from this deque.
/// - `last_cleanup_ms`: last cleanup timestamp in milliseconds; updated to the current time
///   when a cleanup runs. If not enough time has passed since this timestamp, the function
///   returns immediately.
///
/// # Examples
///
/// ```
/// # use std::collections::{HashMap, VecDeque};
/// # use sender::{SequenceTrackingEntry, cleanup_expired_sequence_tracking, MAX_SEQUENCE_TRACKING};
/// // prepare maps with a single stale and a single fresh entry
/// let mut seq_to_conn: HashMap<u32, SequenceTrackingEntry> = HashMap::new();
/// let mut seq_order: VecDeque<u32> = VecDeque::new();
/// // Insert entries (assume SequenceTrackingEntry::new_for_test is available in tests)
/// // seq_to_conn.insert(1, stale_entry);
/// // seq_to_conn.insert(2, fresh_entry);
/// // seq_order.push_back(1);
/// // seq_order.push_back(2);
/// let mut last_cleanup_ms = 0u64;
/// cleanup_expired_sequence_tracking(&mut seq_to_conn, &mut seq_order, &mut last_cleanup_ms);
/// // After calling, stale mappings are removed and seq_order only contains remaining sequences.
/// ```
fn cleanup_expired_sequence_tracking(
    seq_to_conn: &mut HashMap<u32, SequenceTrackingEntry>,
    seq_order: &mut VecDeque<u32>,
    last_cleanup_ms: &mut u64,
) {
    let current_time = now_ms();
    if current_time.saturating_sub(*last_cleanup_ms) < SEQUENCE_MAP_CLEANUP_INTERVAL_MS {
        return;
    }
    *last_cleanup_ms = current_time;

    let before_size = seq_to_conn.len();
    let mut removed_count = 0;

    seq_to_conn.retain(|_seq, entry| {
        if entry.is_expired(current_time) {
            removed_count += 1;
            false
        } else {
            true
        }
    });

    seq_order.retain(|seq| seq_to_conn.contains_key(seq));

    if removed_count > 0 {
        info!(
            "Cleaned up {} stale sequence mappings ({} → {}, {:.1}% capacity)",
            removed_count,
            before_size,
            seq_to_conn.len(),
            (seq_to_conn.len() as f64 / MAX_SEQUENCE_TRACKING as f64) * 100.0
        );
    }

    if seq_to_conn.len() > (MAX_SEQUENCE_TRACKING as f64 * 0.8) as usize {
        warn!(
            "Sequence tracking at {:.1}% capacity ({}/{}) - consider review",
            (seq_to_conn.len() as f64 / MAX_SEQUENCE_TRACKING as f64) * 100.0,
            seq_to_conn.len(),
            MAX_SEQUENCE_TRACKING
        );
    }
}

/// Compute a quality multiplier for a connection used to weight its selection score.
///
/// The multiplier favors connections with no recent NAKs and penalizes those with recent or bursty NAK activity,
/// with a short startup grace period where penalties are softened.
///
/// # Returns
///
/// `f64` multiplier: values greater than 1.0 boost a connection's effective score, values less than 1.0 reduce it.
/// Typical range produced by this function is 0.1 (severe penalty) up to 1.2 (bonus for never-seen NAKs).
///
/// # Behavior
///
/// - During the first 10 seconds after the connection was established, returns `1.2` if the connection has
///   never seen a NAK, otherwise `0.95` (startup grace period).
/// - If the connection reports a time-since-last-NAK (`time_since_last_nak_ms()`):
///   - < 2000 ms => `0.1`
///   - < 5000 ms => `0.5`
///   - < 10000 ms => `0.8`
///   - >= 10000 ms => `1.2` if total NAK count is zero, otherwise `1.0`
///   - If a NAK burst is detected (`nak_burst_count() > 1`) and the last NAK was within 5000 ms, the multiplier is halved.
/// - If there is no time-since-last-NAK value but the connection has never seen a NAK, returns `1.2`; otherwise `1.0`.
///
/// # Examples
///
/// ```no_run
/// // Given a `conn: SrtlaConnection`, compute its quality multiplier:
/// // let mult = calculate_quality_multiplier(&conn);
/// ```
pub(crate) fn calculate_quality_multiplier(conn: &SrtlaConnection) -> f64 {
    use crate::utils::now_ms;

    // Startup grace period: first 10 seconds after connection establishment
    // During this time, use simple scoring like original C version to go live fast
    // This prevents early NAKs from permanently degrading connections
    let connection_age_ms = now_ms().saturating_sub(conn.connection_established_ms());
    if connection_age_ms < 10000 {
        // During startup grace period, only apply light penalties to prevent permanent
        // degradation
        return if conn.total_nak_count() == 0 {
            1.2
        } else {
            0.95
        };
    }

    if let Some(tsn) = conn.time_since_last_nak_ms() {
        let mut quality_mult = if tsn < 2000 {
            0.1
        } else if tsn < 5000 {
            0.5
        } else if tsn < 10_000 {
            0.8
        } else if conn.total_nak_count() == 0 {
            1.2
        } else {
            1.0
        };

        // Extra penalty for burst NAKs (multiple NAKs in short time)
        if conn.nak_burst_count() > 1 && tsn < 5000 {
            quality_mult *= 0.5; // Halve score for connections with NAK bursts
        }
        quality_mult
    } else if conn.total_nak_count() == 0 {
        // Bonus for connections that have never had NAKs
        1.2
    } else {
        1.0
    }
}

/// Chooses the best uplink connection index according to the configured mode and heuristics.
///
/// In `classic` mode this picks the highest `get_score()` among non-timed-out connections. In
/// enhanced mode it scores connections optionally using quality multipliers, respects a short
/// stickiness window to avoid rapid switches, and can periodically explore the second-best
/// connection when exploration is enabled.
///
/// # Parameters
///
/// - `conns`: slice of available connections to consider (timed-out connections are ignored).
/// - `last_idx`: previously selected connection index, used for stickiness decisions.
/// - `last_switch`: timestamp of the last selection change, used to enforce the minimum switch interval.
/// - `enable_quality`: when `true`, adjust scores by connection quality (NAK-based multipliers).
/// - `enable_explore`: when `true`, periodically prefer the second-best connection to explore alternatives.
/// - `classic`: when `true`, use the simple highest-score selection and ignore quality/explore/stickiness.
/// - `now`: current `Instant` used for stickiness and exploration timing.
///
/// # Returns
///
/// `Some(index)` of the chosen connection, or `None` if no suitable (non-timed-out) connection is available.
///
/// # Examples
///
/// ```
/// use std::time::Instant;
/// let idx = select_connection_idx(&[], None, None, false, false, true, Instant::now());
/// assert!(idx.is_none());
/// ```
pub fn select_connection_idx(
    conns: &[SrtlaConnection],
    last_idx: Option<usize>,
    last_switch: Option<Instant>,
    enable_quality: bool,
    enable_explore: bool,
    classic: bool,
    now: Instant,
) -> Option<usize> {
    // Classic mode: simple algorithm matching original implementation
    if classic {
        let mut best_idx: Option<usize> = None;
        let mut best_score: i32 = -1;

        for (i, c) in conns.iter().enumerate() {
            if c.is_timed_out() {
                continue;
            }
            let score = c.get_score();
            if score > best_score {
                best_score = score;
                best_idx = Some(i);
            }
        }
        return best_idx;
    }

    // Enhanced mode: Bond Bunny approach - calculate scores first, then apply
    // stickiness Check if we're in stickiness window
    let in_stickiness_window = if let (Some(idx), Some(ts)) = (last_idx, last_switch) {
        now.duration_since(ts).as_millis() < (MIN_SWITCH_INTERVAL_MS as u128)
            && idx < conns.len()
            && conns[idx].connected
            && !conns[idx].is_timed_out()
    } else {
        false
    };
    // Exploration window: simple periodic exploration of second-best
    // Use elapsed time since program start for consistent periodic behavior
    let explore_now = enable_explore && (now.elapsed().as_millis() % 5000) < 300;
    // Score connections by base score; apply quality multiplier unless classic
    let mut best_idx: Option<usize> = None;
    let mut second_idx: Option<usize> = None;
    let mut best_score: f64 = -1.0;
    let mut second_score: f64 = -1.0;
    for (i, c) in conns.iter().enumerate() {
        if c.is_timed_out() {
            continue;
        }
        let base = c.get_score() as f64;
        let score = if !enable_quality {
            base
        } else {
            let quality_mult = calculate_quality_multiplier(c);
            let final_score = (base * quality_mult).max(1.0);

            // Log quality issues and recoveries for debugging
            if quality_mult < 0.8 {
                debug!(
                    "{} quality degraded: {:.2} (NAKs: {}, last: {}ms ago, burst: {}) base: {} → \
                     final: {}",
                    c.label,
                    quality_mult,
                    c.total_nak_count(),
                    c.time_since_last_nak_ms().unwrap_or(0),
                    c.nak_burst_count(),
                    base as i32,
                    final_score as i32
                );
            } else if quality_mult < 1.0 && c.nak_burst_count() > 0 {
                debug!(
                    "{} quality recovering: {:.2} (burst: {})",
                    c.label,
                    quality_mult,
                    c.nak_burst_count()
                );
            }

            final_score
        };
        if score > best_score {
            second_score = best_score;
            second_idx = best_idx;
            best_score = score;
            best_idx = Some(i);
        } else if score > second_score {
            second_score = score;
            second_idx = Some(i);
        }
    }

    // Bond Bunny approach: if in stickiness window and last connection is still
    // good, keep it
    if in_stickiness_window
        && let Some(idx) = last_idx
            && idx < conns.len() && !conns[idx].is_timed_out() {
                return Some(idx);
            }

    // Allow switching if better connection found (prevents getting stuck on
    // degraded connections)
    if explore_now {
        second_idx.or(best_idx)
    } else {
        best_idx
    }
}

/// Read a newline-separated list of IP addresses from a file, skipping invalid or empty lines.
///
/// Each non-empty line is trimmed and parsed as an `IpAddr`; malformed lines are ignored with a warning.
///
/// # Examples
///
/// ```
/// use std::net::IpAddr;
/// use std::fs;
/// use std::path::PathBuf;
///
/// // create a temporary file with a few lines
/// let mut p = std::env::temp_dir();
/// p.push("example_ips.txt");
/// fs::write(&p, "127.0.0.1\ninvalid-ip\n::1\n\n").unwrap();
///
/// let ips = tokio::runtime::Runtime::new()
///     .unwrap()
///     .block_on(crate::read_ip_list(p.to_str().unwrap()))
///     .unwrap();
///
/// assert!(ips.contains(&"127.0.0.1".parse::<IpAddr>().unwrap()));
/// assert!(ips.contains(&"::1".parse::<IpAddr>().unwrap()));
/// fs::remove_file(p).ok();
/// ```
pub async fn read_ip_list(path: &str) -> Result<Vec<IpAddr>> {
    let text = std::fs::read_to_string(Path::new(path)).context("read IPs file")?;
    let mut out = Vec::new();
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

/// Reconciles the current set of uplink connections with a new list of IPs.
///
/// Removes connections whose labels no longer match the desired receiver:port via IP set,
/// prunes sequence-tracking entries that referenced removed connections, and creates new
/// connections for IPs that are not already present.
///
/// # Parameters
///
/// - `connections`: mutable list of existing `SrtlaConnection` objects; entries may be removed or appended.
/// - `new_ips`: desired uplink IP addresses to enforce.
/// - `receiver_host`, `receiver_port`: host and port used to construct connection labels ("host:port via ip").
/// - `last_selected_idx`: reset to `None` if any connections are removed to avoid holding an invalid index.
/// - `seq_to_conn`: mapping from sequence numbers to `SequenceTrackingEntry`; entries referencing removed connections are pruned.
/// - `seq_order`: queue of recent sequence numbers; pruned to retain only sequences still present in `seq_to_conn`.
///
/// # Examples
///
/// ```
/// # use std::net::IpAddr;
/// # use std::collections::{HashMap, VecDeque};
/// # use crate::sender::{apply_connection_changes, SequenceTrackingEntry};
/// # use crate::SrtlaConnection;
/// # async fn __example() {
/// let mut connections: Vec<SrtlaConnection> = Vec::new();
/// let new_ips: Vec<IpAddr> = vec!["127.0.0.1".parse().unwrap()];
/// let mut last_selected_idx: Option<usize> = None;
/// let mut seq_to_conn: HashMap<u32, SequenceTrackingEntry> = HashMap::new();
/// let mut seq_order: VecDeque<u32> = VecDeque::new();
///
/// apply_connection_changes(
///     &mut connections,
///     &new_ips,
///     "example.com",
///     9000,
///     &mut last_selected_idx,
///     &mut seq_to_conn,
///     &mut seq_order,
/// ).await;
/// # }
/// ```
pub(crate) async fn apply_connection_changes(
    connections: &mut Vec<SrtlaConnection>,
    new_ips: &[IpAddr],
    receiver_host: &str,
    receiver_port: u16,
    last_selected_idx: &mut Option<usize>,
    seq_to_conn: &mut HashMap<u32, SequenceTrackingEntry>,
    seq_order: &mut VecDeque<u32>,
) {
    use std::collections::HashSet;

    let current_labels: HashSet<String> = connections.iter().map(|c| c.label.clone()).collect();
    let desired_labels: HashSet<String> = new_ips
        .iter()
        .map(|ip| format!("{}:{} via {}", receiver_host, receiver_port, ip))
        .collect();

    // Remove stale connections
    let old_len = connections.len();
    connections.retain(|c| desired_labels.contains(&c.label));

    // If connections were removed, reset selection state and clean up sequence
    // tracking
    if connections.len() != old_len {
        info!("removed {} stale connections", old_len - connections.len());
        *last_selected_idx = None;

        seq_to_conn.retain(|_, entry| entry.conn_idx < connections.len());

        seq_order.retain(|seq| seq_to_conn.contains_key(seq));
    }

    // Add new connections
    let new_ips_needed: Vec<IpAddr> = new_ips
        .iter()
        .copied()
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
        }
    }
}

pub async fn create_connections_from_ips(
    ips: &[IpAddr],
    receiver_host: &str,
    receiver_port: u16,
) -> Vec<SrtlaConnection> {
    let mut connections = Vec::new();
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

/// Log a detailed status report for all uplink connections and the sequence-tracking state.
///
/// The report includes:
/// - Total, active, and timed-out connection counts and percentages.
/// - Current toggle states (classic, stickiness, quality scoring, exploration).
/// - Sequence-tracking usage (mapping count, capacity percentage, queue length).
/// - Packet log utilization across all connections.
/// - The last selected connection (if any).
/// - Per-connection details: status, label, score, last receive age, window, in-flight packets,
///   and RTT metrics when available.
/// - Warnings when there are no active connections or when fewer than half are active.
///
/// Parameters:
/// - `connections`: slice of current `SrtlaConnection` objects to report on.
/// - `seq_to_conn`: mapping from SRT sequence numbers to `SequenceTrackingEntry` used for NAK attribution.
/// - `seq_order`: ordered queue of recently tracked sequence numbers (used for capacity/queue reporting).
/// - `last_selected_idx`: optionally the index of the last-selected connection.
/// - `toggles`: runtime feature toggles that affect selection and reporting.
///
/// # Examples
///
/// ```no_run
/// use std::collections::{HashMap, VecDeque};
///
/// // Prepare empty inputs for a simple invocation (real usage passes live connections and toggles).
/// let connections: Vec<srtla::SrtlaConnection> = Vec::new();
/// let seq_to_conn: HashMap<u32, srtla::SequenceTrackingEntry> = HashMap::new();
/// let seq_order: VecDeque<u32> = VecDeque::new();
/// let toggles = srtla::DynamicToggles::default(); // construct according to your application
///
/// srtla::log_connection_status(&connections, &seq_to_conn, &seq_order, None, &toggles);
/// ```
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn log_connection_status(
    connections: &[SrtlaConnection],
    seq_to_conn: &HashMap<u32, SequenceTrackingEntry>,
    seq_order: &VecDeque<u32>,
    last_selected_idx: Option<usize>,
    toggles: &DynamicToggles,
) {
    let total_connections = connections.len();
    let active_connections = connections.iter().filter(|c| !c.is_timed_out()).count();
    let timed_out_connections = total_connections - active_connections;

    info!("📊 Connection Status Report:");
    info!("  Total connections: {}", total_connections);
    info!(
        "  Active connections: {} ({:.1}%)",
        active_connections,
        if total_connections > 0 {
            (active_connections as f64 / total_connections as f64) * 100.0
        } else {
            0.0
        }
    );
    info!("  Timed out connections: {}", timed_out_connections);

    // Show toggle states
    info!(
        "  Toggles: classic={}, stickiness={}, quality={}, exploration={}",
        toggles.classic_mode.load(Ordering::Relaxed),
        toggles.stickiness_enabled.load(Ordering::Relaxed),
        toggles.quality_scoring_enabled.load(Ordering::Relaxed),
        toggles.exploration_enabled.load(Ordering::Relaxed)
    );

    info!(
        "  Sequence tracking: {} mappings ({:.1}% capacity), {} in queue",
        seq_to_conn.len(),
        (seq_to_conn.len() as f64 / MAX_SEQUENCE_TRACKING as f64) * 100.0,
        seq_order.len()
    );

    // Show packet log utilization
    let total_log_entries: usize = connections
        .iter()
        .map(|c| c.in_flight_packets as usize)
        .sum();
    let max_possible_entries = connections.len() * PKT_LOG_SIZE;
    let log_utilization = if max_possible_entries > 0 {
        (total_log_entries as f64 / max_possible_entries as f64) * 100.0
    } else {
        0.0
    };
    info!(
        "  Packet log: {} entries used ({:.1}% of capacity)",
        total_log_entries, log_utilization
    );

    // Show last selected connection
    if let Some(idx) = last_selected_idx {
        if idx < connections.len() {
            info!("  Last selected: {}", connections[idx].label);
        } else {
            warn!("  Last selected index {} is out of bounds!", idx);
        }
    } else {
        info!("  Last selected: none");
    }

    // Show individual connection details
    for (i, conn) in connections.iter().enumerate() {
        let status = if conn.is_timed_out() {
            "⏰ TIMED_OUT"
        } else {
            "✅ ACTIVE"
        };
        let score = conn.get_score();
        let score_desc: String = match score {
            -1 => "DISCONNECTED".to_string(),
            0 => "AT_CAPACITY".to_string(),
            _ => score.to_string(),
        };

        let last_recv = conn
            .last_received
            .map(|t| format!("{:.1}s ago", t.elapsed().as_secs_f64()))
            .unwrap_or_else(|| "never".to_string());

        info!(
            "    [{}] {} {} - Score: {} - Last recv: {} - Window: {} - In-flight: {}",
            i, status, conn.label, score_desc, last_recv, conn.window, conn.in_flight_packets
        );

        if conn.estimated_rtt_ms > 0.0 {
            info!(
                "        RTT: smooth={:.1}ms, fast={:.1}ms, jitter={:.1}ms, stable={} (last: \
                 {:.1}s ago)",
                conn.get_smooth_rtt_ms(),
                conn.get_fast_rtt_ms(),
                conn.get_rtt_jitter_ms(),
                conn.is_rtt_stable(),
                (now_ms().saturating_sub(conn.last_rtt_measurement_ms) as f64) / 1000.0
            );
        }
    }

    // Show any warnings
    if active_connections == 0 {
        warn!("⚠️  No active connections available!");
    } else if active_connections < total_connections / 2 {
        warn!("⚠️  Less than half of connections are active");
    }
}