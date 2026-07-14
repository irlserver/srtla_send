//! Async Unix control socket.
//!
//! Runs on the ambient tokio runtime so each connection can
//! `tokio::select` between reading client requests and writing
//! server-pushed subscription events on the same socket. Replaces the
//! earlier blocking `std::net::UnixListener` + `std::thread` design
//! which could only do strict request/response.
//!
//! Accepts the JSON-RPC protocol documented in
//! `docs/CONTROL_PROTOCOL.md`. Subscriptions described in that doc are
//! handled here — the hub's fan-out writes each published event onto
//! the appropriate connection's push channel.

#[cfg(unix)]
use std::path::PathBuf;

#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
#[cfg(unix)]
use tokio::sync::mpsc;
#[cfg(unix)]
use tracing::{debug, info, warn};

use crate::config::DynamicConfig;
#[cfg(unix)]
use crate::control::{SubscriptionContext, dispatch_async};
use crate::priority::CriticalWindow;
use crate::stats::SharedStats;
use crate::subscriptions::SubscriptionHub;

#[cfg(unix)]
pub fn spawn(
    socket_path: String,
    config: DynamicConfig,
    stats: SharedStats,
    critical_window: CriticalWindow,
    hub: SubscriptionHub,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run(socket_path.into(), config, stats, critical_window, hub).await {
            warn!(error = %e, "control socket listener exited");
        }
    })
}

#[cfg(not(unix))]
pub fn spawn(
    _socket_path: String,
    _config: DynamicConfig,
    _stats: SharedStats,
    _critical_window: CriticalWindow,
    _hub: SubscriptionHub,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {})
}

#[cfg(unix)]
async fn run(
    socket_path: PathBuf,
    config: DynamicConfig,
    stats: SharedStats,
    critical_window: CriticalWindow,
    hub: SubscriptionHub,
) -> std::io::Result<()> {
    // Remove stale socket file from a previous run.
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    info!(?socket_path, "unix control socket listening");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let config = config.clone();
                let stats = stats.clone();
                let cw = critical_window.clone();
                let hub = hub.clone();
                tokio::spawn(async move {
                    handle(stream, config, stats, cw, hub).await;
                });
            }
            Err(e) => {
                debug!(error = %e, "accept failed");
            }
        }
    }
}

#[cfg(unix)]
async fn handle(
    stream: UnixStream,
    config: DynamicConfig,
    stats: SharedStats,
    critical_window: CriticalWindow,
    hub: SubscriptionHub,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let (push_tx, mut push_rx) = mpsc::channel::<String>(128);
    let mut owned_ids: Vec<String> = Vec::new();

    loop {
        tokio::select! {
            read_res = reader.read_line(&mut line) => {
                match read_res {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        let trimmed = line.trim().to_string();
                        line.clear();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let mut ctx = SubscriptionContext {
                            hub: &hub,
                            push_tx: push_tx.clone(),
                            owned_ids: &mut owned_ids,
                        };
                        let resp = dispatch_async(
                            &config,
                            Some(&stats),
                            Some(&critical_window),
                            Some(&mut ctx),
                            &trimmed,
                        )
                        .await;
                        if let Some(resp) = resp {
                            let json = resp.to_json();
                            if write_half.write_all(json.as_bytes()).await.is_err() {
                                break;
                            }
                            if write_half.write_all(b"\n").await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        debug!(error = %e, "read failed");
                        break;
                    }
                }
            }
            Some(push_line) = push_rx.recv() => {
                if write_half.write_all(push_line.as_bytes()).await.is_err() {
                    break;
                }
                if write_half.write_all(b"\n").await.is_err() {
                    break;
                }
            }
        }
    }

    // Clean up this connection's subscriptions from the hub.
    for id in owned_ids {
        hub.unsubscribe(&id).await;
    }
}
