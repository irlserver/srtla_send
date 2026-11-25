//! Zero-allocation-per-packet sequence tracking using a fixed-size ring buffer.
//!
//! Indexes directly by sequence number modulo buffer size, providing:
//! - O(1) insert, lookup, and connection removal
//! - Cache-friendly 16-byte entries (4 per 64-byte cache line)
//! - Lazy direct-heap allocation so idle instances use no ring memory
//! - No periodic cleanup needed (old entries are overwritten on wraparound)

use smallvec::SmallVec;

/// Size of the sequence tracking ring buffer (power of 2 for fast modulo).
/// 16384 entries covers ~5 seconds at 3000 packets/sec.
pub const SEQ_TRACKING_SIZE: usize = 16384;

/// Mask for fast modulo operation (SIZE - 1 when SIZE is power of 2).
const SEQ_TRACKING_MASK: usize = SEQ_TRACKING_SIZE - 1;

/// Maximum age in milliseconds for a sequence tracking entry to be valid.
pub const SEQUENCE_TRACKING_MAX_AGE_MS: u64 = 5000;

/// A single 16-byte entry in the sequence tracking ring buffer.
#[derive(Clone, Copy, Default)]
pub struct SequenceTrackingEntry {
    /// Timestamp when the packet was sent (milliseconds since epoch).
    pub timestamp_ms: u64,
    /// The actual sequence number stored (for collision detection).
    seq: u32,
    /// 1-based slot into `SequenceTracker::conn_ids`. 0 = empty/invalid.
    conn_slot: u16,
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
        self.conn_slot != 0 && self.seq == seq && !self.is_expired(current_time_ms)
    }
}

/// Sequence tracker using a lazily allocated fixed-size ring buffer.
///
/// Uses the sequence number modulo buffer size as the index.
/// Collisions are handled by storing the actual sequence number and
/// checking it on lookup. Stale entries are detected by timestamp.
/// Connection IDs are interned to compact 16-bit slots so entries stay 16 bytes
/// and connection removal is O(1).
#[allow(clippy::len_without_is_empty)]
pub struct SequenceTracker {
    entries: Option<Box<[SequenceTrackingEntry]>>,
    /// Maps `(slot - 1)` to the owning `conn_id`, or `0` once invalidated.
    conn_ids: Vec<u64>,
    /// Currently active `(conn_id, slot)` pairs (typically 2-6 uplinks).
    active_slots: SmallVec<(u64, u16), 8>,
}

impl Default for SequenceTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SequenceTracker {
    /// Create a new empty sequence tracker (allocates ring lazily on first insert).
    pub fn new() -> Self {
        Self {
            entries: None,
            conn_ids: Vec::new(),
            active_slots: SmallVec::new(),
        }
    }

    #[inline]
    fn slot_for(&mut self, conn_id: u64) -> u16 {
        if let Some(&(_, slot)) = self.active_slots.iter().find(|&&(id, _)| id == conn_id) {
            return slot;
        }
        if self.conn_ids.len() >= u16::MAX as usize {
            // Saturated all 65,535 historical slots: clear the ring and compact
            // active connections down to slots 1..=active.
            if let Some(entries) = self.entries.as_deref_mut() {
                entries.fill(SequenceTrackingEntry::default());
            }
            self.conn_ids.clear();
            for (idx, (id, slot)) in self.active_slots.iter_mut().enumerate() {
                self.conn_ids.push(*id);
                *slot = (idx + 1) as u16;
            }
        }
        self.conn_ids.push(conn_id);
        let slot = self.conn_ids.len() as u16;
        self.active_slots.push((conn_id, slot));
        slot
    }

    /// Insert a sequence number mapping.
    #[inline]
    pub fn insert(&mut self, seq: u32, conn_id: u64, timestamp_ms: u64) {
        if conn_id == 0 {
            return;
        }
        let conn_slot = self.slot_for(conn_id);
        let entries = self.entries.get_or_insert_with(|| {
            vec![SequenceTrackingEntry::default(); SEQ_TRACKING_SIZE].into_boxed_slice()
        });
        let idx = (seq as usize) & SEQ_TRACKING_MASK;
        entries[idx] = SequenceTrackingEntry {
            timestamp_ms,
            seq,
            conn_slot,
        };
    }

    /// Look up a sequence number and return the connection ID if valid.
    ///
    /// Returns None if:
    /// - Entry is empty (conn_slot == 0)
    /// - Entry has a different sequence number (collision)
    /// - Entry has expired
    /// - The owning connection was removed
    #[inline]
    pub fn get(&self, seq: u32, current_time_ms: u64) -> Option<u64> {
        let entries = self.entries.as_deref()?;
        let idx = (seq as usize) & SEQ_TRACKING_MASK;
        let entry = &entries[idx];

        if entry.is_valid(seq, current_time_ms) {
            let conn_id = *self.conn_ids.get((entry.conn_slot - 1) as usize)?;
            (conn_id != 0).then_some(conn_id)
        } else {
            None
        }
    }

    /// Remove entries for a specific connection ID in O(1).
    pub fn remove_connection(&mut self, conn_id: u64) {
        if let Some(pos) = self.active_slots.iter().position(|&(id, _)| id == conn_id) {
            let (_, slot) = self.active_slots.swap_remove(pos);
            if let Some(id_slot) = self.conn_ids.get_mut((slot - 1) as usize) {
                *id_slot = 0;
            }
        }
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
