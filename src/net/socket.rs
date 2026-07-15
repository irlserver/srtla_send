use std::net::{IpAddr, SocketAddr};
// The raw-fd binder below is a unix concept: it exists for Android, where the
// host steers a socket onto a radio via `Network.bindSocket` on its fd. Windows
// has no fd, and no such host integration, so the whole binder is unix-only.
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use tracing::warn;

/// Strategy for steering a freshly created uplink socket onto a specific egress
/// path before it is connected.
///
/// On a multi-homed Linux host each uplink owns a source IP, and source-based
/// routing makes binding that source IP sufficient to pick the egress
/// (`SourceIpBinder`). On platforms where the kernel selects the egress by a
/// network handle rather than by source address (notably Android, where the app
/// must call `Network.bindSocket` on the wifi or cellular `Network`), the host
/// supplies a `CallbackBinder` that operates on the raw fd instead.
///
/// The uplink identity stays keyed on `IpAddr` in both cases; only the act of
/// steering the socket differs.
pub trait UplinkBinder: Send + Sync {
    /// Steer `sock` (already created with buffers set, not yet connected) onto
    /// the egress identified by `ip`.
    fn bind(&self, sock: &Socket, ip: IpAddr) -> Result<()>;
}

/// Default binder. Binds the socket to the uplink source IP on an ephemeral
/// port, the behavior the CLI (`ips_file`) relies on.
pub struct SourceIpBinder;

impl UplinkBinder for SourceIpBinder {
    fn bind(&self, sock: &Socket, ip: IpAddr) -> Result<()> {
        let addr = SocketAddr::new(ip, 0);
        sock.bind(&addr.into()).context("bind socket")
    }
}

/// Binder that delegates to a host-supplied closure over the raw fd. The Android
/// integration wires this to `ConnectivityManager` / `Network.bindSocket`,
/// keying on the same `IpAddr` used as the uplink identity. The closure must
/// steer the fd onto the intended radio before the socket is connected.
///
/// Exported for library consumers; the CLI binary never constructs it.
#[cfg(unix)]
#[allow(dead_code)]
pub struct CallbackBinder<F>(pub F)
where
    F: Fn(RawFd, IpAddr) -> std::io::Result<()> + Send + Sync;

#[cfg(unix)]
impl<F> UplinkBinder for CallbackBinder<F>
where
    F: Fn(RawFd, IpAddr) -> std::io::Result<()> + Send + Sync,
{
    fn bind(&self, sock: &Socket, ip: IpAddr) -> Result<()> {
        (self.0)(sock.as_raw_fd(), ip).context("host bindSocket callback")
    }
}

/// Create a UDP socket with the standard nonblocking and buffer configuration.
/// The caller applies an [`UplinkBinder`] and then connects.
pub fn create_uplink_socket(domain_for: IpAddr) -> Result<Socket> {
    let domain = match domain_for {
        IpAddr::V4(_) => Domain::IPV4,
        IpAddr::V6(_) => Domain::IPV6,
    };
    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)).context("create socket")?;
    sock.set_nonblocking(true).context("set nonblocking")?;

    // Set send buffer size (100MB)
    const SEND_BUF_SIZE: usize = 100 * 1024 * 1024;
    if let Err(e) = sock.set_send_buffer_size(SEND_BUF_SIZE) {
        warn!("Failed to set send buffer size to {}: {}", SEND_BUF_SIZE, e);
        if let Ok(actual_size) = sock.send_buffer_size() {
            warn!("Effective send buffer size: {}", actual_size);
        }
    }

    // Set receive buffer size to handle large SRT packets (100MB)
    const RECV_BUF_SIZE: usize = 100 * 1024 * 1024;
    if let Err(e) = sock.set_recv_buffer_size(RECV_BUF_SIZE) {
        warn!(
            "Failed to set receive buffer size to {}: {}",
            RECV_BUF_SIZE, e
        );
        if let Ok(actual_size) = sock.recv_buffer_size() {
            warn!("Effective receive buffer size: {}", actual_size);
        }
    }

    Ok(sock)
}

pub async fn resolve_remote(host: &str, port: u16) -> Result<SocketAddr> {
    let mut addrs = tokio::net::lookup_host((host, port))
        .await
        .context("dns lookup")?;
    addrs
        .next()
        .ok_or_else(|| anyhow::anyhow!("no DNS result for {}", host))
}
