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

use smallvec::SmallVec;
use tracing::debug;

/// Max datagrams per `sendmmsg` batch. Defined here (core) so the pure
/// `drain()` return type and the shell's `net::send_all_datagrams` chunking
/// agree on one value without core depending on the socket layer.
pub const BATCH_SEND_SIZE: usize = 32;

/// One drained datagram view awaiting transmission: `(bytes, seq, queue_time_ms)`.
/// `seq` is `Some` for tracked data packets and `None` for untracked ones.
pub type DrainedPacket<'a> = (&'a [u8], Option<u32>, u64);

#[derive(Copy, Clone, Debug)]
struct QueuedPacket {
    offset: u32,
    len: u32,
    seq: Option<u32>,
    queue_time_ms: u64,
}

/// Contiguous drained batch of datagrams ready for `sendmmsg`.
///
/// Holds a single flat byte slab plus per-packet metadata. Return it to
/// [`BatchSender::recycle`] after sending so steady-state flushes reuse the
/// backing buffers without allocating.
#[derive(Debug, Default)]
pub struct DrainedBatch {
    buf: Vec<u8>,
    entries: Vec<QueuedPacket>,
}

impl DrainedBatch {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = DrainedPacket<'_>> {
        self.entries.iter().map(|e| {
            let start = e.offset as usize;
            let end = start + e.len as usize;
            (&self.buf[start..end], e.seq, e.queue_time_ms)
        })
    }

    #[inline]
    pub fn datagrams(&self) -> SmallVec<&[u8], BATCH_SEND_SIZE> {
        self.entries
            .iter()
            .map(|e| {
                let start = e.offset as usize;
                let end = start + e.len as usize;
                &self.buf[start..end]
            })
            .collect()
    }
}

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

/// Batch sender that queues packets and flushes them efficiently.
///
/// Stores packet bytes in a single contiguous slab (`buf`) so idle links hold
/// zero bytes, active batches stay tightly packed in cache, and draining moves
/// the slab by ownership without copying packet payloads.
#[derive(Debug)]
pub struct BatchSender {
    /// Contiguous payload bytes for all queued packets.
    buf: Vec<u8>,

    /// Metadata (byte slice bounds, sequence number, queue time) per queued packet.
    entries: Vec<QueuedPacket>,

    /// `now_ms()` of the last drain. Drives the 15ms time-flush window against
    /// the injected monotonic clock — no wall/`Instant` clock lives here so the
    /// queue is pure, testable state and the actual send lifts to the shell.
    last_flush_ms: u64,

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
    /// Create a new batch sender (allocates lazily on first queued packet).
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            entries: Vec::new(),
            last_flush_ms: 0,
            regime: BatchRegime::default(),
        }
    }

    /// Queue a data packet for batched sending
    ///
    /// Returns true if the queue should be flushed (threshold reached)
    #[inline]
    pub fn queue_packet(&mut self, data: &[u8], seq: Option<u32>, current_time_ms: u64) -> bool {
        let offset = self.buf.len() as u32;
        let len = data.len() as u32;
        self.buf.extend_from_slice(data);
        self.entries.push(QueuedPacket {
            offset,
            len,
            seq,
            queue_time_ms: current_time_ms,
        });

        self.entries.len() >= self.regime.batch_size()
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

    /// Check if the queue needs flushing based on time.
    ///
    /// `now_ms` is the injected monotonic clock; a drain is due once the queue
    /// has held packets for at least [`FLUSH_INTERVAL_MS`].
    #[inline]
    pub fn needs_time_flush(&self, now_ms: u64) -> bool {
        !self.entries.is_empty() && now_ms.saturating_sub(self.last_flush_ms) >= FLUSH_INTERVAL_MS
    }

    /// Check if there are any packets queued
    #[inline]
    pub fn has_queued_packets(&self) -> bool {
        !self.entries.is_empty()
    }

    /// Number of data packets currently queued (not yet flushed).
    /// Used by `get_score()` so the selection algorithm sees the true load,
    /// matching the C behaviour where `reg_pkt()` increments in_flight per packet.
    #[inline]
    pub fn queued_count(&self) -> i32 {
        self.entries.len() as i32
    }

    /// Drain the queue, returning every queued datagram with its tracking info.
    ///
    /// Pure: it performs no I/O. The queue is emptied and the flush window is
    /// re-armed at `now_ms`; the shell transmits the returned bytes (see
    /// `send_all_datagrams`) and hands the batch back to [`Self::recycle`] so
    /// the backing buffer capacity is reused on the next flush.
    pub fn drain(&mut self, now_ms: u64) -> DrainedBatch {
        self.last_flush_ms = now_ms;
        if self.entries.is_empty() {
            return DrainedBatch::default();
        }

        let packet_count = self.entries.len();
        let out = DrainedBatch {
            buf: std::mem::take(&mut self.buf),
            entries: std::mem::take(&mut self.entries),
        };

        if packet_count > 1 {
            debug!("Batch drain: {} packets ready to send", packet_count);
        }

        out
    }

    /// Return a drained batch's backing allocations for reuse on the next queue cycle.
    #[inline]
    pub fn recycle(&mut self, mut batch: DrainedBatch) {
        if self.buf.capacity() == 0 && batch.buf.capacity() > 0 {
            batch.buf.clear();
            self.buf = batch.buf;
        }
        if self.entries.capacity() == 0 && batch.entries.capacity() > 0 {
            batch.entries.clear();
            self.entries = batch.entries;
        }
    }

    /// Reset the batch sender state (for reconnection)
    pub fn reset(&mut self) {
        self.buf.clear();
        self.entries.clear();
        self.last_flush_ms = 0;
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
            assert_eq!(sender.entries.len(), i + 1);
        }

        // This one should trigger flush
        assert!(sender.queue_packet(&data, Some(15), 0));
        assert_eq!(sender.entries.len(), BATCH_SIZE_THRESHOLD);
    }

    #[test]
    fn test_batch_sender_time_flush() {
        let mut sender = BatchSender::new();
        let data = [0u8; 100];

        sender.queue_packet(&data, Some(1), 0);
        assert!(!sender.needs_time_flush(0)); // Just queued at t=0, shouldn't need flush
        assert!(!sender.needs_time_flush(FLUSH_INTERVAL_MS - 1)); // Under the window

        // Once FLUSH_INTERVAL_MS has elapsed against the injected clock.
        assert!(sender.needs_time_flush(FLUSH_INTERVAL_MS));
        assert!(sender.needs_time_flush(20));
    }

    #[test]
    fn test_batch_sender_reset() {
        let mut sender = BatchSender::new();
        let data = [0u8; 100];

        sender.queue_packet(&data, Some(1), 0);
        sender.queue_packet(&data, Some(2), 0);

        sender.reset();

        assert!(sender.entries.is_empty());
        assert!(sender.buf.is_empty());
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
