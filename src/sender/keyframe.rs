//! Heuristic keyframe burst detection for priority scheduling.
//!
//! SRT payload packets are typically 1316 bytes (max MTU payload). Video keyframes
//! (I-frames) are much larger than P/B-frames, so they produce bursts of consecutive
//! max-MTU packets. This module detects such bursts and signals the scheduler to
//! prefer higher-quality links for keyframe data.
//!
//! ## Detection heuristic
//!
//! A "keyframe burst" is declared when `BURST_THRESHOLD` or more consecutive
//! packets are exactly `SRT_DATA_SIZE` bytes. The burst ends when a shorter
//! packet is seen, indicating the tail of the I-frame (or transition to P/B-frames).

/// SRT data payload size — the maximum payload in a single SRT data packet.
const SRT_DATA_SIZE: usize = 1316;

/// Number of consecutive max-MTU packets required to declare a keyframe burst.
const BURST_THRESHOLD: u32 = 5;

/// Tracks consecutive max-MTU packets and declares keyframe bursts.
pub struct KeyframeDetector {
    /// Number of consecutive max-MTU packets seen so far.
    consecutive_max_mtu: u32,
    /// Whether we are currently inside a keyframe burst.
    in_burst: bool,
    /// Total number of packets forwarded during the current burst (for stats).
    burst_packet_count: u32,
    /// Total bursts detected since creation (monotonically increasing).
    total_bursts: u64,
}

impl KeyframeDetector {
    pub fn new() -> Self {
        Self {
            consecutive_max_mtu: 0,
            in_burst: false,
            burst_packet_count: 0,
            total_bursts: 0,
        }
    }

    /// Feed a packet's wire size into the detector.
    ///
    /// Call this for every SRT data packet (control packets should be excluded).
    /// Returns `true` if this packet is part of a keyframe burst and should
    /// receive priority scheduling.
    #[inline]
    pub fn observe(&mut self, packet_len: usize) -> bool {
        if packet_len == SRT_DATA_SIZE {
            self.consecutive_max_mtu += 1;

            if !self.in_burst && self.consecutive_max_mtu >= BURST_THRESHOLD {
                // Transition into burst
                self.in_burst = true;
                self.total_bursts += 1;
            }

            if self.in_burst {
                self.burst_packet_count += 1;
                return true;
            }
        } else {
            // Non-max-MTU packet — end any active burst and reset counter
            self.consecutive_max_mtu = 0;
            if self.in_burst {
                self.in_burst = false;
                self.burst_packet_count = 0;
            }
        }

        false
    }

    /// Whether we are currently inside a keyframe burst.
    #[allow(dead_code)]
    #[inline]
    pub fn is_in_burst(&self) -> bool {
        self.in_burst
    }

    /// Total number of keyframe bursts detected since creation.
    #[allow(dead_code)]
    pub fn total_bursts(&self) -> u64 {
        self.total_bursts
    }
}

impl Default for KeyframeDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// Select the highest-quality connection index for keyframe priority scheduling.
///
/// Among all schedulable connections, picks the one with the best quality multiplier.
/// Returns `None` if no connections are schedulable (caller should fall back to
/// normal selection).
pub fn select_best_quality_idx(conns: &[crate::connection::SrtlaConnection]) -> Option<usize> {
    let mut best_idx = None;
    let mut best_quality = f64::NEG_INFINITY;

    for (i, conn) in conns.iter().enumerate() {
        if !conn.connected || !conn.is_schedulable() {
            continue;
        }
        let q = conn.quality_cache.multiplier;
        if q > best_quality {
            best_quality = q;
            best_idx = Some(i);
        }
    }

    best_idx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_burst_below_threshold() {
        let mut det = KeyframeDetector::new();
        // 4 consecutive max-MTU packets — below threshold of 5
        for _ in 0..4 {
            assert!(!det.observe(SRT_DATA_SIZE));
        }
        assert!(!det.is_in_burst());
        assert_eq!(det.total_bursts(), 0);
    }

    #[test]
    fn test_burst_at_threshold() {
        let mut det = KeyframeDetector::new();
        // First 4 are below threshold
        for _ in 0..4 {
            assert!(!det.observe(SRT_DATA_SIZE));
        }
        // 5th triggers burst
        assert!(det.observe(SRT_DATA_SIZE));
        assert!(det.is_in_burst());
        assert_eq!(det.total_bursts(), 1);
    }

    #[test]
    fn test_burst_continues_with_max_mtu() {
        let mut det = KeyframeDetector::new();
        for _ in 0..5 {
            det.observe(SRT_DATA_SIZE);
        }
        // Additional max-MTU packets stay in burst
        assert!(det.observe(SRT_DATA_SIZE));
        assert!(det.observe(SRT_DATA_SIZE));
        assert!(det.is_in_burst());
        assert_eq!(det.total_bursts(), 1);
    }

    #[test]
    fn test_burst_ends_on_short_packet() {
        let mut det = KeyframeDetector::new();
        for _ in 0..5 {
            det.observe(SRT_DATA_SIZE);
        }
        assert!(det.is_in_burst());

        // Short packet ends burst
        assert!(!det.observe(800));
        assert!(!det.is_in_burst());
    }

    #[test]
    fn test_multiple_bursts() {
        let mut det = KeyframeDetector::new();

        // First burst
        for _ in 0..7 {
            det.observe(SRT_DATA_SIZE);
        }
        assert!(det.is_in_burst());
        assert_eq!(det.total_bursts(), 1);

        // Gap
        det.observe(600);
        assert!(!det.is_in_burst());

        // Second burst
        for _ in 0..5 {
            det.observe(SRT_DATA_SIZE);
        }
        assert!(det.is_in_burst());
        assert_eq!(det.total_bursts(), 2);
    }

    #[test]
    fn test_reset_after_single_short_packet() {
        let mut det = KeyframeDetector::new();
        // Build up 3 consecutive
        for _ in 0..3 {
            det.observe(SRT_DATA_SIZE);
        }
        // One short packet resets the counter
        det.observe(1000);
        // Next 4 max-MTU should not trigger burst (need 5 fresh)
        for _ in 0..4 {
            assert!(!det.observe(SRT_DATA_SIZE));
        }
        // 5th triggers
        assert!(det.observe(SRT_DATA_SIZE));
        assert_eq!(det.total_bursts(), 1);
    }

    #[test]
    fn test_select_best_quality_idx() {
        use crate::test_helpers::create_test_connections;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(3));

        conns[0].quality_cache.multiplier = 0.8;
        conns[1].quality_cache.multiplier = 1.1;
        conns[2].quality_cache.multiplier = 0.95;

        assert_eq!(select_best_quality_idx(&conns), Some(1));
    }

    #[test]
    fn test_select_best_quality_idx_skips_disconnected() {
        use crate::test_helpers::create_test_connections;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut conns = rt.block_on(create_test_connections(3));

        conns[0].quality_cache.multiplier = 0.8;
        conns[1].quality_cache.multiplier = 1.1;
        conns[1].connected = false; // Best quality but disconnected
        conns[2].quality_cache.multiplier = 0.95;

        assert_eq!(select_best_quality_idx(&conns), Some(2));
    }

    #[test]
    fn test_select_best_quality_idx_empty() {
        let conns: Vec<crate::connection::SrtlaConnection> = vec![];
        assert_eq!(select_best_quality_idx(&conns), None);
    }
}
