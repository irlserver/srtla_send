//! Uplink socket I/O (shell).
//!
//! The batched UDP socket (`recvmmsg`/`sendmmsg`), the egress-binding
//! strategies, and socket construction. This is the I/O layer the pure
//! connection/scheduler core sits on top of; it depends on the core (for the
//! shared `BATCH_SEND_SIZE`) and on `protocol` (for `MTU`), never the reverse.

// Darwin steers egress by interface index rather than by source address, so
// Apple targets need their own binder instead of `SourceIpBinder`.
#[cfg(target_vendor = "apple")]
pub mod apple;
pub mod batch_recv;
mod socket;

#[cfg(target_vendor = "apple")]
pub use apple::AppleInterfaceBinder;
pub use batch_recv::{BatchUdpSocket, RecvMmsgBuffer};
// Host-side binder for platforms that steer egress by network handle (Android).
// Exported for library consumers; the CLI binary does not construct it. Unix
// only: it binds by raw fd, which Windows does not have.
#[cfg(unix)]
pub use socket::CallbackBinder;
pub use socket::{
    SourceIpBinder, UplinkBinder, create_uplink_socket, resolve_remote, resolve_remote_all,
};
use srtla_core::connection::BATCH_SEND_SIZE;

/// How far a batch flush got before the socket refused it.
///
/// A batch is several datagrams and `sendmmsg` reports a short send rather than
/// blocking, so a failure lands *mid-batch*: `sent` datagrams are confirmed
/// away and the remaining `total - sent` never left the host. The shell needs
/// that split to describe the failure truthfully — see
/// `sender::packet_handler::flush_connection`, which is where the unconfirmed
/// remainder is accounted for.
#[derive(Debug)]
pub struct BatchSendError {
    /// Datagrams the kernel confirmed before the failure.
    pub sent: usize,
    /// Datagrams the flush offered.
    pub total: usize,
    /// The error that ended the flush.
    pub source: std::io::Error,
}

impl std::fmt::Display for BatchSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} of {} datagrams sent before the socket failed: {}",
            self.sent, self.total, self.source
        )
    }
}

impl std::error::Error for BatchSendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Send every datagram to the link's peer, chunking into `sendmmsg` syscalls.
///
/// This is the I/O half of a batch flush: the pure [`srtla_core::connection::BatchSender`]
/// drains the queue, and this transmits the bytes. `sendmmsg` may accept fewer
/// datagrams than offered (a short send once the socket buffer fills), so it
/// loops until the whole batch is away. A no-progress send (`Ok(0)`) is treated
/// as an error so the link is retried rather than livelocked.
///
/// On failure the error carries how many datagrams were confirmed sent first,
/// so the caller never has to claim a whole batch was delivered when only a
/// prefix of it was.
pub async fn send_all_datagrams(
    socket: &BatchUdpSocket,
    bufs: &[&[u8]],
) -> Result<(), BatchSendError> {
    let total = bufs.len();
    let mut sent = 0;
    while sent < total {
        let take = (total - sent).min(BATCH_SEND_SIZE);
        match socket.send_batch(&bufs[sent..sent + take]).await {
            Ok(0) => {
                return Err(BatchSendError {
                    sent,
                    total,
                    source: std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "sendmmsg accepted no datagrams",
                    ),
                });
            }
            Ok(n) => sent += n,
            Err(e) => {
                return Err(BatchSendError {
                    sent,
                    total,
                    source: e,
                });
            }
        }
    }
    Ok(())
}
