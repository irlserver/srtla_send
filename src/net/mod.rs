//! Uplink socket I/O (shell).
//!
//! The batched UDP socket (`recvmmsg`/`sendmmsg`), the egress-binding
//! strategies, and socket construction. This is the I/O layer the pure
//! connection/scheduler core sits on top of; it depends on the core (for the
//! shared `BATCH_SEND_SIZE`) and on `protocol` (for `MTU`), never the reverse.

pub mod batch_recv;
mod socket;

pub use batch_recv::{BatchUdpSocket, RecvMmsgBuffer};
// Host-side binder for platforms that steer egress by network handle (Android).
// Exported for library consumers; the CLI binary does not construct it. Unix
// only: it binds by raw fd, which Windows does not have.
#[cfg(unix)]
#[allow(unused_imports)]
pub use socket::CallbackBinder;
pub use socket::{SourceIpBinder, UplinkBinder, create_uplink_socket, resolve_remote};

use srtla_core::connection::BATCH_SEND_SIZE;

/// Send every datagram to the connected peer, chunking into `sendmmsg` syscalls.
///
/// This is the I/O half of a batch flush: the pure [`srtla_core::connection::BatchSender`]
/// drains the queue, and this transmits the bytes. `sendmmsg` may accept fewer
/// datagrams than offered (a short send once the socket buffer fills), so it
/// loops until the whole batch is away. A no-progress send (`Ok(0)`) is treated
/// as an error so the link is retried rather than livelocked.
pub async fn send_all_datagrams(socket: &BatchUdpSocket, bufs: &[&[u8]]) -> std::io::Result<()> {
    let total = bufs.len();
    let mut sent = 0;
    while sent < total {
        let take = (total - sent).min(BATCH_SEND_SIZE);
        match socket.send_batch(&bufs[sent..sent + take]).await {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "sendmmsg accepted no datagrams",
                ));
            }
            Ok(n) => sent += n,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
