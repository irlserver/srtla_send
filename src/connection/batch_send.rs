//! Batch send optimization for SRTLA connections
//!
//! This module implements packet batching inspired by Moblin's implementation:
//! - Buffers up to 16 data packets before sending (default Normal regime)
//! - Flushes on 15ms timer to ensure low latency
//! - Flushes each batch with a single `sendmmsg` (one syscall per batch)
//!
//! At 10 Mbps with ~1300 byte packets:
//! - Without batching: ~960 syscalls/second per connection
//! - With batching: ~60-67 batch sends/second per connection (~15x reduction)
//!
//! The syscall saving is the whole point of the queue, and until `sendmmsg`
//! landed it did not exist: `flush` looped over the queue issuing one `send`
//! per packet, so batching bought nothing but added up to `FLUSH_INTERVAL_MS`
//! of latency. Anything that trades scheduling quality for "batch integrity"
//! (holding the scheduler on one link so batches stay contiguous) is therefore
//! paying for a benefit that only exists while this stays a real batch syscall.
//!
//! ## Adaptive batch regimes
//!
//! Three regimes drive the size threshold based on observed link load:
//!
//! - `LowActivity` (≤ 500 kbps): batch=4. Less buffering per tick on
//!   idle links so a sudden burst flushes promptly.
//! - `Normal` (default, 500 kbps – 5 Mbps): batch=16. The proven
//!   Moblin sweet spot.
//! - `HighLoad` (> 5 Mbps): batch=32. Bigger batches amortise socket
//!   syscalls better; future sendmmsg work benefits more here.
//!
//! Flush interval stays at 15 ms across regimes — going longer on
//! idle links would add latency when traffic returns, going shorter
//! under load would defeat the syscall-amortisation we batch for.
//! The `set_regime` setter is called from `housekeeping` based on each
//! connection's `current_bitrate_bps` snapshot.

use std::sync::Arc;

use smallvec::SmallVec;
use tokio::time::Instant;
use tracing::debug;

use super::batch_recv::{BATCH_SEND_SIZE, BatchUdpSocket};

/// Bitrate above which a connection is treated as high-load.
pub const HIGH_LOAD_THRESHOLD_BPS: f64 = 5_000_000.0;
/// Bitrate at or below which a connection is treated as low-activity.
pub const LOW_ACTIVITY_THRESHOLD_BPS: f64 = 500_000.0;

/// Batch-size thresholds per regime. We don't vary the flush interval
/// because going longer on idle links would add latency on traffic
/// resumption and going shorter under load would erase the syscall
/// amortisation we batch for.
const BATCH_SIZE_LOW_ACTIVITY: usize = 4;
const BATCH_SIZE_NORMAL: usize = 16;
const BATCH_SIZE_HIGH_LOAD: usize = 32;

/// Default size threshold. Public so existing tests can reference it
/// and to make the steady-state value easy to find.
#[allow(dead_code)]
pub const BATCH_SIZE_THRESHOLD: usize = BATCH_SIZE_NORMAL;

/// Maximum time in milliseconds between flushes (Moblin uses 15ms)
const FLUSH_INTERVAL_MS: u64 = 15;

/// Adaptive batch-size regime. Driven by observed per-link bitrate.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum BatchRegime {
    /// Quiet link (≤ 500 kbps). Smaller batches keep latency low when
    /// traffic resumes.
    LowActivity,
    /// Normal cellular IRL operating range.
    #[default]
    Normal,
    /// Heavy stream (> 5 Mbps). Bigger batches reduce syscall pressure.
    HighLoad,
}

impl BatchRegime {
    /// Stable string used in stats / telemetry. Public; called from
    /// future stats-export work even when nothing in this crate's
    /// own tree consumes it.
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            BatchRegime::LowActivity => "low_activity",
            BatchRegime::Normal => "normal",
            BatchRegime::HighLoad => "high_load",
        }
    }

    /// Pick the regime for a given bitrate (bits per second). Hysteresis
    /// is applied at the call site (housekeeping uses [`from_bps`] as a
    /// debounced selector — see `connection::SrtlaConnection::recompute_batch_regime`).
    pub fn from_bps(bps: f64) -> Self {
        if bps > HIGH_LOAD_THRESHOLD_BPS {
            BatchRegime::HighLoad
        } else if bps <= LOW_ACTIVITY_THRESHOLD_BPS {
            BatchRegime::LowActivity
        } else {
            BatchRegime::Normal
        }
    }

    fn batch_size(self) -> usize {
        match self {
            BatchRegime::LowActivity => BATCH_SIZE_LOW_ACTIVITY,
            BatchRegime::Normal => BATCH_SIZE_NORMAL,
            BatchRegime::HighLoad => BATCH_SIZE_HIGH_LOAD,
        }
    }
}

/// Batch sender that queues packets and flushes them efficiently
#[derive(Debug)]
pub struct BatchSender {
    /// Queue of packets waiting to be sent
    queue: Vec<SmallVec<u8, 1500>>,

    /// Sequence numbers for queued packets (parallel to queue)
    sequences: Vec<Option<u32>>,

    /// Timestamps when packets were queued (parallel to queue)
    queue_times: Vec<u64>,

    /// Last time the queue was flushed
    last_flush_time: Instant,

    /// Current batch regime. Updated by housekeeping when the
    /// connection's bitrate crosses a threshold.
    regime: BatchRegime,
}

impl Default for BatchSender {
    fn default() -> Self {
        Self::new()
    }
}

impl BatchSender {
    /// Create a new batch sender
    pub fn new() -> Self {
        Self {
            queue: Vec::with_capacity(BATCH_SIZE_HIGH_LOAD),
            sequences: Vec::with_capacity(BATCH_SIZE_HIGH_LOAD),
            queue_times: Vec::with_capacity(BATCH_SIZE_HIGH_LOAD),
            last_flush_time: Instant::now(),
            regime: BatchRegime::default(),
        }
    }

    /// Queue a data packet for batched sending
    ///
    /// Returns true if the queue should be flushed (threshold reached)
    #[inline]
    pub fn queue_packet(&mut self, data: &[u8], seq: Option<u32>, current_time_ms: u64) -> bool {
        self.queue.push(SmallVec::from_slice_copy(data));
        self.sequences.push(seq);
        self.queue_times.push(current_time_ms);

        self.queue.len() >= self.regime.batch_size()
    }

    /// Update the batch regime. Called from housekeeping each tick
    /// based on the connection's observed bitrate. No effect when the
    /// regime hasn't actually changed.
    pub fn set_regime(&mut self, regime: BatchRegime) {
        self.regime = regime;
    }

    /// Current batch regime (for telemetry).
    #[allow(dead_code)]
    #[inline]
    pub fn regime(&self) -> BatchRegime {
        self.regime
    }

    /// Check if the queue needs flushing based on time
    #[inline]
    pub fn needs_time_flush(&self) -> bool {
        !self.queue.is_empty()
            && self.last_flush_time.elapsed().as_millis() >= FLUSH_INTERVAL_MS as u128
    }

    /// Check if there are any packets queued
    #[inline]
    pub fn has_queued_packets(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Number of data packets currently queued (not yet flushed).
    /// Used by `get_score()` so the selection algorithm sees the true load,
    /// matching the C behaviour where `reg_pkt()` increments in_flight per packet.
    #[inline]
    pub fn queued_count(&self) -> i32 {
        self.queue.len() as i32
    }

    /// Flush all queued packets to the socket
    ///
    /// Returns a vector of (seq, queue_time) pairs for packets that need tracking.
    /// The caller should update in-flight tracking based on these.
    pub async fn flush(
        &mut self,
        socket: &Arc<BatchUdpSocket>,
    ) -> std::io::Result<Vec<(Option<u32>, u64)>> {
        if self.queue.is_empty() {
            return Ok(Vec::new());
        }

        let packet_count = self.queue.len();
        let mut sent_count = 0;

        // One `sendmmsg` per BATCH_SEND_SIZE datagrams. The kernel may accept
        // fewer than offered (short send) once the socket buffer fills, so loop
        // until the queue is drained rather than assuming a full batch left.
        while sent_count < packet_count {
            let take = (packet_count - sent_count).min(BATCH_SEND_SIZE);

            // Scoped so the borrow of `self.queue` ends before the error path
            // below mutates it.
            let result = {
                let mut bufs: SmallVec<&[u8], BATCH_SEND_SIZE> = SmallVec::new();
                for packet in &self.queue[sent_count..sent_count + take] {
                    bufs.push(&packet[..]);
                }
                socket.send_batch(&bufs).await
            };

            match result {
                // Ok(0) would spin forever; treat a no-progress send as an error
                // so the link is retried rather than livelocked.
                Ok(0) => {
                    self.queue.drain(..sent_count);
                    self.sequences.drain(..sent_count);
                    self.queue_times.drain(..sent_count);
                    self.last_flush_time = Instant::now();
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "sendmmsg accepted no datagrams",
                    ));
                }
                Ok(n) => sent_count += n,
                Err(e) => {
                    // Partial failure: remove already-sent packets to avoid duplicates
                    self.queue.drain(..sent_count);
                    self.sequences.drain(..sent_count);
                    self.queue_times.drain(..sent_count);
                    self.last_flush_time = Instant::now();
                    return Err(e);
                }
            }
        }

        // Collect tracking info before clearing
        let tracking_info: Vec<(Option<u32>, u64)> = self
            .sequences
            .iter()
            .zip(self.queue_times.iter())
            .map(|(&seq, &time)| (seq, time))
            .collect();

        // Clear the queue
        self.queue.clear();
        self.sequences.clear();
        self.queue_times.clear();
        self.last_flush_time = Instant::now();

        if packet_count > 1 {
            debug!("Batch flush: sent {} packets in one batch", packet_count);
        }

        Ok(tracking_info)
    }

    /// Reset the batch sender state (for reconnection)
    pub fn reset(&mut self) {
        self.queue.clear();
        self.sequences.clear();
        self.queue_times.clear();
        self.last_flush_time = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batch_sender_queue() {
        let mut sender = BatchSender::new();
        let data = [0u8; 100];

        // Queue should not trigger flush until threshold
        for i in 0..BATCH_SIZE_THRESHOLD - 1 {
            assert!(!sender.queue_packet(&data, Some(i as u32), 0));
            assert_eq!(sender.queue.len(), i + 1);
        }

        // This one should trigger flush
        assert!(sender.queue_packet(&data, Some(15), 0));
        assert_eq!(sender.queue.len(), BATCH_SIZE_THRESHOLD);
    }

    #[test]
    fn test_batch_sender_time_flush() {
        let mut sender = BatchSender::new();
        let data = [0u8; 100];

        sender.queue_packet(&data, Some(1), 0);
        assert!(!sender.needs_time_flush()); // Just queued, shouldn't need flush

        // Simulate time passing
        sender.last_flush_time = Instant::now() - std::time::Duration::from_millis(20);
        assert!(sender.needs_time_flush()); // Now should need flush
    }

    #[test]
    fn test_batch_sender_reset() {
        let mut sender = BatchSender::new();
        let data = [0u8; 100];

        sender.queue_packet(&data, Some(1), 0);
        sender.queue_packet(&data, Some(2), 0);

        sender.reset();

        assert!(sender.queue.is_empty());
    }

    #[test]
    fn regime_from_bps_thresholds() {
        assert_eq!(
            BatchRegime::from_bps(100_000.0),
            BatchRegime::LowActivity,
            "well below 500 kbps → LowActivity"
        );
        assert_eq!(
            BatchRegime::from_bps(LOW_ACTIVITY_THRESHOLD_BPS),
            BatchRegime::LowActivity,
            "exactly at the threshold stays LowActivity"
        );
        assert_eq!(
            BatchRegime::from_bps(2_000_000.0),
            BatchRegime::Normal,
            "between thresholds → Normal"
        );
        assert_eq!(
            BatchRegime::from_bps(HIGH_LOAD_THRESHOLD_BPS),
            BatchRegime::Normal,
            "exactly at the high threshold stays Normal — only past it"
        );
        assert_eq!(
            BatchRegime::from_bps(HIGH_LOAD_THRESHOLD_BPS + 1.0),
            BatchRegime::HighLoad,
            "just above 5 Mbps → HighLoad"
        );
    }

    #[test]
    fn batch_size_threshold_per_regime() {
        let mut sender = BatchSender::new();
        let data = [0u8; 100];

        // LowActivity: flushes after 4 packets.
        sender.set_regime(BatchRegime::LowActivity);
        for i in 0..3 {
            assert!(!sender.queue_packet(&data, Some(i as u32), 0));
        }
        assert!(sender.queue_packet(&data, Some(3), 0));
        sender.reset();

        // HighLoad: flushes after 32.
        sender.set_regime(BatchRegime::HighLoad);
        for i in 0..31 {
            assert!(!sender.queue_packet(&data, Some(i as u32), 0));
        }
        assert!(sender.queue_packet(&data, Some(31), 0));
    }
}
