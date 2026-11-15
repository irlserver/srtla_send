//! Which source the local SRT listener treats as the encoder.
//!
//! The listener binds every interface by default, so any host that reaches
//! the port can send to it. Without a pin, every datagram would set the
//! client address, and one stray packet would move the return path (ACKs,
//! NAKs, handshakes from the receiver) away from the real encoder.

use std::net::SocketAddr;

/// Minimum gap between two "dropped foreign SRT packets" log lines.
const DROP_LOG_INTERVAL_MS: u64 = 10_000;

/// How long the pinned client must stay silent before a session start from a
/// different source may take the pin over. A libsrt caller repeats its
/// connection request at most every 250 ms until its connect timeout
/// (`srtcore/core.cpp` `startConnect` in irlserver/srt), so a reconnecting
/// encoder is followed about one second after its old session went quiet,
/// while one forged induction during a live stream is dropped.
const CLIENT_RELEASE_SILENCE_MS: u64 = 1_000;

/// Which source the local SRT listener treats as the encoder.
pub struct ClientPin {
    addr: Option<SocketAddr>,
    /// Packet clock of the last packet accepted from `addr`.
    last_seen_ms: u64,
    dropped: u64,
    /// `None` until the first drop is logged, so that drop logs at any clock
    /// value.
    last_drop_log_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientVerdict {
    /// Forward the packet. `repinned` is true when this packet moved the pin
    /// from one source to another.
    Accept { repinned: bool },
    /// Drop the packet: it came from a source other than the pinned client.
    Drop,
}

impl Default for ClientPin {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientPin {
    pub fn new() -> Self {
        Self {
            addr: None,
            last_seen_ms: 0,
            dropped: 0,
            last_drop_log_ms: None,
        }
    }

    pub fn addr(&self) -> Option<SocketAddr> {
        self.addr
    }

    /// Decide whether a packet from `src` reaches the bond.
    ///
    /// `starts_session` is true when the packet is an SRT caller induction
    /// (see `srtla_protocol::parse_srt_session_start`). Every accepted packet
    /// refreshes the pinned client's last seen time.
    ///
    /// 1. With no client yet, `src` is adopted whatever the packet is. A
    ///    sender restarted mid-stream must not wait for a handshake the
    ///    encoder only sends after its own timeout.
    /// 2. A packet from the pinned address is accepted, session start or not.
    /// 3. A session start from a different source moves the pin once the
    ///    pinned client has been silent for `CLIENT_RELEASE_SILENCE_MS`. An
    ///    encoder that reconnects uses a new source port, always opens with
    ///    an induction and repeats it every 250 ms, so its old session is
    ///    quiet by the time a repeat arrives.
    /// 4. Every other packet from a different source is dropped and counted,
    ///    including a session start while the pinned client is still
    ///    sending. This keeps one unauthenticated induction from any host
    ///    that reaches the listener from moving the pin, and (because the
    ///    caller resets ACK state only after an accept) from wiping every
    ///    link's packet log.
    pub fn admit(&mut self, src: SocketAddr, starts_session: bool, now_ms: u64) -> ClientVerdict {
        let repinned = match self.addr {
            None => false,
            Some(pinned) if pinned == src => false,
            Some(_)
                if starts_session
                    && now_ms.saturating_sub(self.last_seen_ms) >= CLIENT_RELEASE_SILENCE_MS =>
            {
                true
            }
            Some(_) => {
                self.dropped += 1;
                return ClientVerdict::Drop;
            }
        };
        self.addr = Some(src);
        self.last_seen_ms = now_ms;
        ClientVerdict::Accept { repinned }
    }

    /// The total drop count, when a drop log line is due.
    pub fn should_log_drop(&mut self, now_ms: u64) -> Option<u64> {
        let due = self
            .last_drop_log_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= DROP_LOG_INTERVAL_MS);
        if !due {
            return None;
        }
        self.last_drop_log_ms = Some(now_ms);
        Some(self.dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a() -> SocketAddr {
        "127.0.0.1:4000".parse::<SocketAddr>().unwrap()
    }

    fn b() -> SocketAddr {
        "127.0.0.1:4001".parse::<SocketAddr>().unwrap()
    }

    fn pinned_to_a() -> ClientPin {
        let mut pin = ClientPin::new();
        assert_eq!(
            pin.admit(a(), false, 10_000),
            ClientVerdict::Accept { repinned: false }
        );
        pin
    }

    #[test]
    fn first_source_is_adopted_without_a_handshake() {
        let mut pin = ClientPin::new();
        assert_eq!(
            pin.admit(a(), false, 10_000),
            ClientVerdict::Accept { repinned: false }
        );
        assert_eq!(pin.addr(), Some(a()));
    }

    #[test]
    fn pinned_source_is_accepted() {
        let mut pin = pinned_to_a();
        assert_eq!(
            pin.admit(a(), false, 10_010),
            ClientVerdict::Accept { repinned: false }
        );
    }

    #[test]
    fn other_source_data_is_dropped_and_counted() {
        let mut pin = pinned_to_a();
        assert_eq!(pin.admit(b(), false, 10_010), ClientVerdict::Drop);
        assert_eq!(pin.addr(), Some(a()));
        assert_eq!(pin.should_log_drop(10_010), Some(1));
    }

    #[test]
    fn new_session_from_new_source_moves_the_pin_after_silence() {
        let mut pin = pinned_to_a();
        assert_eq!(
            pin.admit(b(), true, 11_000),
            ClientVerdict::Accept { repinned: true }
        );
        assert_eq!(pin.addr(), Some(b()));
        assert_eq!(pin.admit(a(), false, 11_010), ClientVerdict::Drop);
    }

    #[test]
    fn new_session_from_the_pinned_source_does_not_repin() {
        let mut pin = pinned_to_a();
        assert_eq!(
            pin.admit(a(), true, 10_010),
            ClientVerdict::Accept { repinned: false }
        );
    }

    #[test]
    fn drop_log_is_rate_limited() {
        let mut pin = pinned_to_a();
        assert_eq!(pin.admit(b(), false, 10_010), ClientVerdict::Drop);
        assert_eq!(pin.should_log_drop(11_000), Some(1));
        assert_eq!(pin.admit(b(), false, 10_010), ClientVerdict::Drop);
        assert_eq!(pin.should_log_drop(12_000), None);
        assert_eq!(pin.admit(b(), false, 10_010), ClientVerdict::Drop);
        assert_eq!(pin.should_log_drop(22_000), Some(3));
    }

    #[test]
    fn new_session_from_new_source_is_dropped_while_the_client_streams() {
        let mut pin = pinned_to_a();
        assert_eq!(
            pin.admit(a(), false, 10_500),
            ClientVerdict::Accept { repinned: false }
        );
        assert_eq!(
            pin.admit(b(), true, 11_000),
            ClientVerdict::Drop,
            "the pinned client was seen 500 ms ago"
        );
        assert_eq!(pin.addr(), Some(a()));
        assert_eq!(
            pin.admit(b(), true, 11_500),
            ClientVerdict::Accept { repinned: true },
            "the pinned client has been silent for 1000 ms"
        );
    }
}
