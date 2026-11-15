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
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
#[cfg(unix)]
use std::path::PathBuf;

use srtla_core::priority::CriticalWindow;
#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
#[cfg(unix)]
use tokio::sync::mpsc;
#[cfg(unix)]
use tracing::{debug, info, warn};

use crate::config::DynamicConfig;
#[cfg(unix)]
use crate::control::{SubscriptionContext, dispatch_async};
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
    // A socket left by a previous run is replaced. Anything else at the path
    // (a regular file, a symlink, a directory) is the operator's and stays.
    match std::fs::symlink_metadata(&socket_path) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(&socket_path)?,
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "{} exists and is not a socket, refusing to replace it",
                    socket_path.display()
                ),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(&socket_path)?;
    // Any process that can connect can change routing, so only the owning
    // user may connect. The directory holding the socket covers the moment
    // between bind and this call.
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
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

/// Longest request line accepted on the control socket. Valid requests are
/// well under 4 KiB, and a client that never sends a newline must not grow
/// the read buffer until the process dies.
#[cfg(unix)]
const MAX_REQUEST_LINE: usize = 64 * 1024;

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
    let mut buf: Vec<u8> = Vec::new();
    let (push_tx, mut push_rx) = mpsc::channel::<String>(128);
    let mut owned_ids: Vec<(String, String)> = Vec::new();

    loop {
        // `read_until` keeps partial input in `buf` when the push branch wins
        // the select, and `take` stops one call from growing `buf` past the
        // limit while it waits for a newline.
        let remaining = (MAX_REQUEST_LINE + 1).saturating_sub(buf.len()) as u64;
        let mut limited = (&mut reader).take(remaining);
        tokio::select! {
            read_res = limited.read_until(b'\n', &mut buf) => {
                let n = match read_res {
                    Ok(n) => n,
                    Err(e) => {
                        debug!(error = %e, "read failed");
                        break;
                    }
                };
                if buf.len() > MAX_REQUEST_LINE {
                    warn!(limit = MAX_REQUEST_LINE, "control request line too long, closing connection");
                    break;
                }
                let eof = n == 0;
                if !eof && buf.last() != Some(&b'\n') {
                    // Bytes without a newline and below the limit: the peer hit
                    // EOF mid-line. The next read returns Ok(0) and handles them.
                    continue;
                }
                if eof && buf.is_empty() {
                    break;
                }
                let line = String::from_utf8_lossy(&buf).trim().to_string();
                buf.clear();
                if !line.is_empty() {
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
                        &line,
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
                if eof {
                    break;
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
    for (id, _topic) in owned_ids {
        hub.unsubscribe(&id).await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use serde_json::Value;
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

    use super::*;

    const IO_TIMEOUT: Duration = Duration::from_secs(2);
    const GET_STATUS: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"get_status\"}\n";

    fn start(path: &Path) -> SubscriptionHub {
        let hub = SubscriptionHub::new();
        spawn(
            path.to_string_lossy().into_owned(),
            DynamicConfig::new(),
            SharedStats::new(),
            CriticalWindow::new(),
            hub.clone(),
        );
        hub
    }

    /// Retries the connect because the path exists as soon as `bind` returns,
    /// slightly before the listener is accepting.
    async fn connect(path: &Path) -> (BufReader<OwnedReadHalf>, OwnedWriteHalf) {
        let deadline = tokio::time::Instant::now() + IO_TIMEOUT;
        loop {
            if path.exists()
                && let Ok(stream) = UnixStream::connect(path).await
            {
                let (read_half, write_half) = stream.into_split();
                return (BufReader::new(read_half), write_half);
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "control socket never came up"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn read_json(reader: &mut BufReader<OwnedReadHalf>) -> Value {
        let mut line = String::new();
        let n = tokio::time::timeout(IO_TIMEOUT, reader.read_line(&mut line))
            .await
            .expect("timed out waiting for a line")
            .unwrap();
        assert!(n > 0, "connection closed before a line arrived");
        serde_json::from_str(&line).unwrap()
    }

    async fn send(writer: &mut OwnedWriteHalf, bytes: &[u8]) {
        writer.write_all(bytes).await.unwrap();
        writer.flush().await.unwrap();
    }

    async fn subscribe_stats(
        reader: &mut BufReader<OwnedReadHalf>,
        writer: &mut OwnedWriteHalf,
    ) -> String {
        send(
            writer,
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"subscribe\",\"params\":{\"topic\":\"stats\"}}\n",
        )
        .await;
        let resp = read_json(reader).await;
        assert_eq!(resp["id"], 1);
        resp["result"]["subscription_id"]
            .as_str()
            .expect("subscribe returned no id")
            .to_string()
    }

    #[tokio::test]
    async fn split_request_during_push_is_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctl.sock");
        let hub = start(&path);
        let (mut reader, mut writer) = connect(&path).await;
        subscribe_stats(&mut reader, &mut writer).await;

        send(&mut writer, br#"{"jsonrpc":"2.0","id":7,"#).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        hub.publish("stats", serde_json::json!({"x": 1})).await;
        let push = read_json(&mut reader).await;
        assert_eq!(push["method"], "stats.update");

        send(&mut writer, b"\"method\":\"get_status\"}\n").await;
        let resp = read_json(&mut reader).await;
        assert_eq!(resp["id"], 7, "split request was mangled: {resp}");
        assert!(resp.get("result").is_some(), "expected a result: {resp}");
        assert!(resp.get("error").is_none(), "unexpected error: {resp}");
    }

    #[tokio::test]
    async fn socket_is_created_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctl.sock");
        let _hub = start(&path);
        let (mut reader, mut writer) = connect(&path).await;
        // The server only accepts after the chmod, so an answer means the
        // mode is final.
        send(&mut writer, GET_STATUS).await;
        read_json(&mut reader).await;

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn a_regular_file_at_the_path_is_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ctl.sock");
        std::fs::write(&path, b"keep").unwrap();
        let _hub = start(&path);
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(std::fs::metadata(&path).unwrap().is_file());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
    }
}
