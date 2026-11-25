use std::future::Future;
use std::net::{IpAddr, SocketAddr};
// The raw-fd binder below is a unix concept: it exists for Android, where the
// host steers a socket onto a radio via `Network.bindSocket` on its fd. Windows
// has no fd, and no such host integration, so the whole binder is unix-only.
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
use std::pin::Pin;
#[cfg(any(test, feature = "test-internals"))]
use std::sync::Arc;

use anyhow::{Context, Result};
use smallvec::SmallVec;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::time::Duration;
use tracing::{debug, warn};

/// Strategy for steering a freshly created uplink socket onto a specific egress
/// path.
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
    /// Steer `sock` (already created with buffers set, no traffic yet) onto the
    /// egress identified by `ip`.
    fn bind(&self, sock: &Socket, ip: IpAddr) -> Result<()>;
}

/// Default binder. Binds the socket to the uplink source IP on an ephemeral
/// port, the behavior the CLI (`ips_file`) relies on.
pub struct SourceIpBinder;

impl UplinkBinder for SourceIpBinder {
    fn bind(&self, sock: &Socket, ip: IpAddr) -> Result<()> {
        // An IPv4-mapped source gets an IPv4 socket (see `create_uplink_socket`),
        // which only binds the plain IPv4 form.
        let addr = SocketAddr::new(ip.to_canonical(), 0);
        sock.bind(&addr.into()).context("bind socket")
    }
}

/// Binder that delegates to a host-supplied closure over the raw fd. The Android
/// integration wires this to `ConnectivityManager` / `Network.bindSocket`,
/// keying on the same `IpAddr` used as the uplink identity. The closure must
/// steer the fd onto the intended radio before the socket carries traffic.
///
/// Exported for library consumers; the CLI binary never constructs it.
#[cfg(unix)]
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
///
/// The caller applies an [`UplinkBinder`] and then hands the socket to
/// `BatchUdpSocket::new` together with the receiver address. The socket is never
/// connected; see that constructor for why.
///
/// An IPv4-mapped IPv6 source (`::ffff:a.b.c.d`) is an IPv4 address and gets an
/// IPv4 socket, so it reaches IPv4 receivers like any other IPv4 source.
pub fn create_uplink_socket(domain_for: IpAddr) -> Result<Socket> {
    let domain = match domain_for.to_canonical() {
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

/// Every address the receiver's hostname currently answers with.
///
/// [`select_remote_for_source`] picks one of these per uplink family. The full
/// set also feeds the reconnect and re-home paths, which compare the addresses
/// the bond is pinned to against a fresh answer to detect receiver DNS drift
/// (see `sender::connections` and `sender::rehome`).
pub async fn resolve_remote_all(host: &str, port: u16) -> Result<SmallVec<SocketAddr, 4>> {
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .context("dns lookup")?;
    Ok(addrs.collect())
}

/// The first answer in `source`'s address family, keeping DNS order within it.
/// An IPv4-mapped IPv6 source counts as IPv4.
///
/// A dual-stack receiver answers with both A and AAAA records, and an uplink
/// socket can only reach the family it was created in: an IPv4 modem handed an
/// IPv6 receiver address fails every send with `EAFNOSUPPORT`. Filtering by
/// family assumes the A and AAAA records reach the same receiver group;
/// reachability is not tested.
pub fn select_remote_for_source(addrs: &[SocketAddr], source: IpAddr) -> Result<SocketAddr> {
    let source_is_ipv4 = source.to_canonical().is_ipv4();
    addrs
        .iter()
        .copied()
        .find(|addr| addr.is_ipv4() == source_is_ipv4)
        .ok_or_else(|| {
            let family = if source_is_ipv4 { "IPv4" } else { "IPv6" };
            anyhow::anyhow!("receiver has no {family} address for source {source}")
        })
}

/// Ceiling on a receiver lookup made from housekeeping or SIGHUP reload.
/// Housekeeping is the main event loop's tick, so a resolver that hangs must
/// not hang the sender with it.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);

/// Addresses a receiver hostname currently answers with.
pub type ResolvedAddrs = SmallVec<SocketAddr, 4>;

type ResolveFuture = Pin<Box<dyn Future<Output = Option<ResolvedAddrs>> + Send>>;

/// Re-resolution of the receiver hostname, behind a seam so the reload and
/// re-home trigger policies are testable without a live resolver.
pub trait ReceiverResolver: Send + Sync {
    /// `None` means the lookup failed, timed out, or answered with nothing.
    fn resolve(&self, host: String, port: u16) -> ResolveFuture;
}

/// The production resolver: a plain `getaddrinfo` under a timeout.
pub struct DnsResolver;

impl ReceiverResolver for DnsResolver {
    fn resolve(&self, host: String, port: u16) -> ResolveFuture {
        Box::pin(async move {
            match tokio::time::timeout(RESOLVE_TIMEOUT, resolve_remote_all(&host, port)).await {
                Ok(Ok(addrs)) if !addrs.is_empty() => Some(addrs),
                Ok(Ok(_)) => {
                    debug!("receiver lookup of {host} returned no addresses; staying put");
                    None
                }
                Ok(Err(e)) => {
                    debug!("receiver lookup of {host} failed ({e}); staying put");
                    None
                }
                Err(_) => {
                    warn!(
                        "receiver lookup of {host} timed out after {:?}; staying put",
                        RESOLVE_TIMEOUT
                    );
                    None
                }
            }
        })
    }
}

/// Test seam: answers a scripted queue of lookups and counts how many it was
/// asked for, so a test can assert not just the outcome but whether the trigger
/// policy even reached DNS.
#[cfg(any(test, feature = "test-internals"))]
pub struct StubResolver {
    answers: std::sync::Mutex<Vec<Option<ResolvedAddrs>>>,
    calls: std::sync::atomic::AtomicUsize,
}

#[cfg(any(test, feature = "test-internals"))]
impl StubResolver {
    /// `answers` are consumed in order; `None` models a failed or empty lookup.
    /// Once exhausted, every further lookup fails.
    pub fn new(answers: Vec<Option<Vec<SocketAddr>>>) -> Arc<Self> {
        Arc::new(Self {
            answers: std::sync::Mutex::new(
                answers
                    .into_iter()
                    .rev()
                    .map(|a| a.map(SmallVec::from_vec))
                    .collect(),
            ),
            calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(any(test, feature = "test-internals"))]
impl ReceiverResolver for StubResolver {
    fn resolve(&self, _host: String, _port: u16) -> ResolveFuture {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // `pop` off a reversed vec = FIFO.
        let answer = self.answers.lock().unwrap().pop().flatten();
        Box::pin(async move { answer })
    }
}
