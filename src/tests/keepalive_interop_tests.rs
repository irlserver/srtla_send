//! Keepalive interop conformance.
//!
//! Pins the keepalive divergence with the reference srtla receiver and the
//! defensive-parse contract that lets the two interoperate:
//!
//! - A reference srtla receiver may send a *bare* 2-byte keepalive: the type
//!   only (`htobe16(SRTLA_TYPE_KEEPALIVE)`), no timestamp.
//! - This sender uses a timestamped keepalive: a standard 10-byte frame (type +
//!   `u64` ms timestamp) and a backwards-compatible *extended* 38-byte frame
//!   (timestamp + a `0xC01F`-tagged `ConnectionInfo` telemetry trailer).
//!
//! These tests assert (a) our extended keepalive builds → parses → preserves its
//! RTT fields end-to-end through the real receive path, and (b)/(c) that a bare
//! 2-byte echo, and any truncated/oversized frame, is handled gracefully (no
//! error, no panic). They do not change the wire format.

#[cfg(test)]
mod tests {
    use crate::connection::RttTracker;
    use crate::protocol::*;
    use crate::utils::now_ms;

    /// (a) Our extended keepalive builds → parses → RTT fields preserved.
    ///
    /// Two round-trips in one: the `ConnectionInfo` telemetry survives a
    /// build→parse cycle byte-for-byte (including `rtt_ms`), AND the standard
    /// timestamp at bytes 2-9 still yields a correct RTT measurement through
    /// the real receive path (`RttTracker::handle_keepalive_response`) even
    /// though 28 extra extended bytes trail it.
    #[test]
    fn keepalive_extended_round_trip() {
        let info = ConnectionInfo {
            conn_id: 7,
            window: 31_000,
            in_flight: 12,
            rtt_ms: 87,
            nak_count: 4,
            bitrate_bytes_per_sec: 3_125_000,
        };

        let pkt = create_keepalive_packet_ext(info);
        assert_eq!(pkt.len(), SRTLA_KEEPALIVE_EXT_LEN);
        assert_eq!(get_packet_type(&pkt), Some(SRTLA_TYPE_KEEPALIVE));
        assert!(is_srtla_keepalive(&pkt));

        // Telemetry round-trip: every ConnectionInfo field preserved.
        let parsed = extract_keepalive_conn_info(&pkt).expect("extended conn info parses");
        assert_eq!(parsed, info, "ConnectionInfo must round-trip byte-for-byte");
        assert_eq!(parsed.rtt_ms, 87, "rtt_ms field preserved across the wire");

        // RTT measurement round-trip: craft an extended keepalive whose
        // timestamp is a known interval in the past, echo it back through the
        // real receive path, and confirm a plausible RTT sample is recovered
        // from bytes 2-9 despite the extended trailer.
        let mut tracker = RttTracker::default();
        tracker.record_keepalive_sent();
        assert!(tracker.waiting_for_keepalive_response);

        let sent_ts = now_ms().saturating_sub(50);
        let mut echo = create_keepalive_packet_ext(info);
        echo[2..10].copy_from_slice(&sent_ts.to_be_bytes());

        let measured = tracker
            .handle_keepalive_response(&echo, "interop")
            .expect("extended keepalive echo yields an RTT sample");
        assert!(
            (40..=10_000).contains(&measured),
            "measured RTT {measured}ms should reflect the ~50ms backdated timestamp"
        );
        assert!(
            tracker.kalman_rtt.is_initialized(),
            "a valid extended-keepalive RTT sample must seed the filter"
        );
        assert!(
            !tracker.waiting_for_keepalive_response,
            "the keepalive-wait flag must clear after a valid echo"
        );
    }

    /// (b) A bare 2-byte keepalive echo is accepted without error or panic
    /// (defensive parse).
    ///
    /// A reference srtla receiver may echo a bare `[0x90, 0x00]` keepalive (type
    /// only, no timestamp). Our receive path must tolerate it: it is recognised
    /// as a keepalive, yields no timestamp/telemetry (too short), and the RTT
    /// path returns `None` cleanly instead of panicking. With no timestamp no
    /// RTT can be measured off a bare echo.
    #[test]
    fn keepalive_bare_2byte_accepted() {
        let bare: [u8; 2] = SRTLA_TYPE_KEEPALIVE.to_be_bytes();

        // Recognised as a keepalive by the discriminator…
        assert_eq!(get_packet_type(&bare), Some(SRTLA_TYPE_KEEPALIVE));
        assert!(is_srtla_keepalive(&bare));

        // …but too short to carry a timestamp or extended telemetry: both
        // return None, gracefully (no panic, no unwrap).
        assert_eq!(extract_keepalive_timestamp(&bare), None);
        assert!(extract_keepalive_conn_info(&bare).is_none());

        // The real receive path tolerates the bare echo: no RTT sample, no
        // panic, and the waiting flag is cleared so the next keepalive cycle
        // is not wedged.
        let mut tracker = RttTracker::default();
        tracker.record_keepalive_sent();
        let measured = tracker.handle_keepalive_response(&bare, "interop-bare");
        assert_eq!(measured, None, "a bare 2-byte echo yields no RTT sample");
        assert!(
            !tracker.kalman_rtt.is_initialized(),
            "a bare echo must not seed the RTT filter"
        );
        assert!(
            !tracker.waiting_for_keepalive_response,
            "the keepalive-wait flag must clear after handling a bare echo"
        );
    }

    /// (c) Truncated and oversized keepalive frames are handled gracefully —
    /// every length from empty to past the extended frame parses without a
    /// panic, returning None/empty as the length contract dictates.
    #[test]
    fn keepalive_truncated_graceful() {
        let mut tracker = RttTracker::default();

        for len in 0..=64usize {
            let mut buf = vec![0u8; len];
            if len >= 2 {
                buf[0..2].copy_from_slice(&SRTLA_TYPE_KEEPALIVE.to_be_bytes());
            }

            // None of these may panic at any length.
            let _ = get_packet_type(&buf);
            let _ = extract_keepalive_timestamp(&buf);
            let _ = extract_keepalive_conn_info(&buf);

            // The receive path must never panic on a malformed echo. Re-arm
            // before each call so the guard branch is actually exercised.
            tracker.record_keepalive_sent();
            let _ = tracker.handle_keepalive_response(&buf, "interop-trunc");

            // Length-specific contract: a timestamp needs >= 10 bytes; the
            // extended telemetry needs the full 38-byte frame (magic+version).
            if len < 10 {
                assert_eq!(extract_keepalive_timestamp(&buf), None);
            }
            if len < SRTLA_KEEPALIVE_EXT_LEN {
                assert!(extract_keepalive_conn_info(&buf).is_none());
            }
        }

        // Oversized frame (well beyond the 38-byte extended keepalive): the
        // trailing bytes are ignored, the standard timestamp still reads, and
        // nothing panics. With no 0xC01F magic at bytes 10-11 it is NOT parsed
        // as extended telemetry.
        let mut oversized = vec![0u8; MTU];
        oversized[0..2].copy_from_slice(&SRTLA_TYPE_KEEPALIVE.to_be_bytes());
        let ts = now_ms().saturating_sub(20);
        oversized[2..10].copy_from_slice(&ts.to_be_bytes());
        assert_eq!(get_packet_type(&oversized), Some(SRTLA_TYPE_KEEPALIVE));
        assert!(extract_keepalive_timestamp(&oversized).is_some());
        assert!(
            extract_keepalive_conn_info(&oversized).is_none(),
            "oversized frame without the 0xC01F magic must not parse as extended"
        );
    }
}
