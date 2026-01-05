//! Zero-allocation sequence tracking using a fixed-size ring buffer.
//!
//! This replaces the HashMap + VecDeque approach with a fixed array indexed
//! by sequence number modulo, providing:
//! - Zero allocations in hot path
//! - O(1) insert and lookup
//! - Cache-friendly sequential memory access
//! - No cleanup needed (old entries naturally overwritten)

/// Size of the sequence tracking ring buffer (power of 2 for fast modulo).
/// 16384 entries covers ~5 seconds at 3000 packets/sec.
pub const SEQ_TRACKING_SIZE: usize = 16384;

/// Mask for fast modulo operation (SIZE - 1 when SIZE is power of 2).
const SEQ_TRACKING_MASK: usize = SEQ_TRACKING_SIZE - 1;

/// Maximum age in milliseconds for a sequence tracking entry to be valid.
pub const SEQUENCE_TRACKING_MAX_AGE_MS: u64 = 5000;

/// A single entry in the sequence tracking ring buffer.
#[derive(Clone, Copy, Default)]
pub struct SequenceTrackingEntry {
    /// The connection ID that sent this packet. 0 = empty/invalid.
    pub conn_id: u64,
    /// Timestamp when the packet was sent (milliseconds since epoch).
    pub timestamp_ms: u64,
    /// The actual sequence number stored (for collision detection).
    seq: u32,
}

impl SequenceTrackingEntry {
    /// Check if this entry has expired based on current time.
    #[inline]
    pub fn is_expired(&self, current_time_ms: u64) -> bool {
        current_time_ms.saturating_sub(self.timestamp_ms) > SEQUENCE_TRACKING_MAX_AGE_MS
    }

    /// Check if this entry is valid (non-empty and not expired).
    #[inline]
    fn is_valid(&self, seq: u32, current_time_ms: u64) -> bool {
        self.conn_id != 0 && self.seq == seq && !self.is_expired(current_time_ms)
    }
}

/// Zero-allocation sequence tracker using a fixed-size ring buffer.
///
/// Uses the sequence number modulo buffer size as the index.
/// Collisions are handled by storing the actual sequence number and
/// checking it on lookup. Stale entries are detected by timestamp.
#[allow(clippy::len_without_is_empty)]
pub struct SequenceTracker {
    entries: Box<[SequenceTrackingEntry; SEQ_TRACKING_SIZE]>,
    /// Number of valid (non-expired) entries - approximate, for logging only.
    count: usize,
}

impl Default for SequenceTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SequenceTracker {
    /// Create a new empty sequence tracker.
    pub fn new() -> Self {
        Self {
            entries: Box::new([SequenceTrackingEntry::default(); SEQ_TRACKING_SIZE]),
            count: 0,
        }
    }

    /// Insert a sequence number mapping.
    ///
    /// This is O(1) and never allocates.
    #[inline]
    pub fn insert(&mut self, seq: u32, conn_id: u64, timestamp_ms: u64) {
        let idx = (seq as usize) & SEQ_TRACKING_MASK;
        let entry = &mut self.entries[idx];

        // Track approximate count (for logging purposes only)
        if entry.conn_id == 0 {
            self.count = self.count.saturating_add(1);
        }

        *entry = SequenceTrackingEntry {
            conn_id,
            timestamp_ms,
            seq,
        };
    }

    /// Look up a sequence number and return the connection ID if valid.
    ///
    /// Returns None if:
    /// - Entry is empty (conn_id == 0)
    /// - Entry has a different sequence number (collision)
    /// - Entry has expired
    #[inline]
    pub fn get(&self, seq: u32, current_time_ms: u64) -> Option<u64> {
        let idx = (seq as usize) & SEQ_TRACKING_MASK;
        let entry = &self.entries[idx];

        if entry.is_valid(seq, current_time_ms) {
            Some(entry.conn_id)
        } else {
            None
        }
    }

    /// Get approximate number of entries (may be inaccurate due to overwrites).
    #[inline]
    pub fn len(&self) -> usize {
        self.count.min(SEQ_TRACKING_SIZE)
    }

    /// Remove entries for a specific connection ID.
    ///
    /// This is O(n) but only called during connection removal, not in hot path.
    pub fn remove_connection(&mut self, conn_id: u64) {
        for entry in self.entries.iter_mut() {
            if entry.conn_id == conn_id {
                *entry = SequenceTrackingEntry::default();
                self.count = self.count.saturating_sub(1);
            }
        }
    }

    /// Get capacity utilization as a percentage (for logging).
    #[inline]
    pub fn utilization_percent(&self) -> f64 {
        (self.len() as f64 / SEQ_TRACKING_SIZE as f64) * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_and_get() {
        let mut tracker = SequenceTracker::new();
        let now = 1000000u64;

        tracker.insert(12345, 1, now);
        tracker.insert(12346, 2, now);

        assert_eq!(tracker.get(12345, now), Some(1));
        assert_eq!(tracker.get(12346, now), Some(2));
        assert_eq!(tracker.get(12347, now), None); // Not inserted
    }

    #[test]
    fn test_expiration() {
        let mut tracker = SequenceTracker::new();
        let now = 1000000u64;

        tracker.insert(12345, 1, now);

        // Still valid
        assert_eq!(
            tracker.get(12345, now + SEQUENCE_TRACKING_MAX_AGE_MS),
            Some(1)
        );

        // Expired
        assert_eq!(
            tracker.get(12345, now + SEQUENCE_TRACKING_MAX_AGE_MS + 1),
            None
        );
    }

    #[test]
    fn test_collision_handling() {
        let mut tracker = SequenceTracker::new();
        let now = 1000000u64;

        // Two sequences that map to the same index
        let seq1 = 100u32;
        let seq2 = seq1 + SEQ_TRACKING_SIZE as u32;

        tracker.insert(seq1, 1, now);
        assert_eq!(tracker.get(seq1, now), Some(1));

        // Overwrite with seq2
        tracker.insert(seq2, 2, now);
        assert_eq!(tracker.get(seq2, now), Some(2));
        assert_eq!(tracker.get(seq1, now), None); // Collision, seq doesn't match
    }

    #[test]
    fn test_remove_connection() {
        let mut tracker = SequenceTracker::new();
        let now = 1000000u64;

        tracker.insert(100, 1, now);
        tracker.insert(101, 2, now);
        tracker.insert(102, 1, now);

        tracker.remove_connection(1);

        assert_eq!(tracker.get(100, now), None);
        assert_eq!(tracker.get(101, now), Some(2));
        assert_eq!(tracker.get(102, now), None);
    }

    #[test]
    fn test_size_is_power_of_two() {
        // Ensure our mask trick works
        assert!(SEQ_TRACKING_SIZE.is_power_of_two());
        assert_eq!(SEQ_TRACKING_MASK, SEQ_TRACKING_SIZE - 1);
    }
}
