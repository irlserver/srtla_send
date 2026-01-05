# Performance Optimization Plan

This document outlines CPU and performance optimizations to bring the Rust `srtla_send` implementation closer to the efficiency of the original C `srtla_send.c`.

## Executive Summary

The Rust implementation adds several features (quality scoring, exploration, RTT tracking) that the C version lacks. However, there are optimization opportunities to reduce overhead in the hot path while maintaining these features.

### Key Differences: Rust vs C

| Aspect | C (`srtla_send.c`) | Rust (current) |
|--------|-------------------|----------------|
| **Event loop** | `select()` single thread | `tokio::select!` async runtime |
| **Socket I/O** | Blocking with non-blocking poll | Async with separate reader tasks |
| **Time syscalls** | 1-2 per packet | 3-5 per packet |
| **Data structures** | Linked list, fixed arrays | HashMap, VecDeque, SmallVec |
| **Connection selection** | Simple integer division | Float math + `exp()` |
| **Memory allocation** | Zero in hot path | HashMap inserts per packet |
| **Packet log scan** | O(256) linear | O(256) linear (same) |

---

## DashMap Evaluation

**Question**: Should we use [DashMap](https://github.com/xacrimon/dashmap)?

**Answer**: **No**, DashMap is not appropriate for this use case.

### Reasoning

1. **DashMap is designed for concurrent multi-threaded access** - It uses sharded locking to allow multiple threads to access different parts of the map simultaneously.

2. **Our hot path is single-threaded** - The main `tokio::select!` loop processes packets sequentially on one thread. The reader tasks only push to channels; they don't access the HashMaps.

3. **DashMap adds overhead for no benefit** - Each operation acquires a shard lock, which is unnecessary overhead when there's no concurrent access.

4. **Better alternatives for single-threaded code**:
   - `std::collections::HashMap` - Current choice, reasonable
   - `rustc_hash::FxHashMap` - Faster hashing (non-cryptographic)
   - Fixed-size ring buffer - Zero allocation, cache-friendly

### Recommendation

Use `FxHashMap` from `rustc-hash` crate for faster hashing, or replace with a fixed-size ring buffer for zero-allocation hot path.

---

## Optimization Categories

Optimizations are grouped by impact and implementation effort:

- **Critical** - High impact, should be implemented first
- **Moderate** - Medium impact, good ROI
- **Minor** - Low impact but good practice
- **Architectural** - Larger changes with variable impact

---

## Critical Optimizations

### 1. Cache Timestamps Per Packet

**Problem**: `now_ms()` calls `SystemTime::now()` which is a syscall. Currently called 3-5 times per packet in:
- `connection/mod.rs:340` - `register_packet()`
- `packet_handler.rs:318,338` - `forward_via_connection()`
- `connection/mod.rs:375` - `handle_srt_ack()`
- `quality.rs:52` - `calculate_quality_multiplier()`

**Current Code**:
```rust
// Called multiple times per packet
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_millis(0))
        .as_millis() as u64
}
```

**Solution**: Capture timestamp once at packet receive and pass through function chain.

```rust
// In handle_srt_packet, capture once at entry:
let packet_time_ms = now_ms();
let packet_instant = Instant::now();

// Pass to all downstream functions
forward_via_connection(
    sel_idx, pkt, seq, connections,
    last_selected_idx, last_switch_time_ms,
    seq_to_conn, seq_order,
    packet_time_ms,  // Add parameter
).await;

// Update register_packet signature
pub fn register_packet(&mut self, seq: i32, send_time_ms: u64) {
    let idx = self.packet_idx % PKT_LOG_SIZE;
    self.packet_log[idx] = seq;
    self.packet_send_times_ms[idx] = send_time_ms;  // Use passed value
    // ...
}
```

**Files to Modify**:
- `src/utils.rs` - Document hot path usage
- `src/sender/packet_handler.rs` - Capture timestamp at entry
- `src/connection/mod.rs` - Update `register_packet()`, `send_data_with_tracking()`
- `src/sender/selection/quality.rs` - Accept timestamp parameter

**Estimated Impact**: 3-4 fewer syscalls per packet = ~15-20% CPU reduction in hot path

**Priority**: P0 - Highest

---

### 2. Replace HashMap with Fixed-Size Ring Buffer for Sequence Tracking

**Problem**: `seq_to_conn: HashMap<u32, SequenceTrackingEntry>` allocates on every packet insert at `packet_handler.rs:334`.

**Current Code**:
```rust
seq_to_conn.insert(
    s,
    SequenceTrackingEntry {
        conn_id: connections[sel_idx].conn_id,
        timestamp_ms: now_ms(),
    },
);
seq_order.push_back(s);
```

**Solution**: Use a fixed-size array indexed by sequence number modulo.

```rust
/// Zero-allocation sequence tracking using fixed ring buffer
const SEQ_TRACKING_SIZE: usize = 16384; // Power of 2, covers ~5 seconds at 3000 pps

#[derive(Default, Clone, Copy)]
struct SeqTrackEntry {
    conn_id: u64,      // 0 = empty
    timestamp_ms: u64,
}

pub struct SequenceTracker {
    entries: Box<[SeqTrackEntry; SEQ_TRACKING_SIZE]>,
}

impl SequenceTracker {
    pub fn new() -> Self {
        Self {
            entries: Box::new([SeqTrackEntry::default(); SEQ_TRACKING_SIZE]),
        }
    }

    #[inline]
    pub fn insert(&mut self, seq: u32, conn_id: u64, timestamp_ms: u64) {
        let idx = (seq as usize) & (SEQ_TRACKING_SIZE - 1);
        self.entries[idx] = SeqTrackEntry { conn_id, timestamp_ms };
    }

    #[inline]
    pub fn get(&self, seq: u32, max_age_ms: u64, current_time_ms: u64) -> Option<u64> {
        let idx = (seq as usize) & (SEQ_TRACKING_SIZE - 1);
        let entry = &self.entries[idx];
        if entry.conn_id != 0 
            && current_time_ms.saturating_sub(entry.timestamp_ms) <= max_age_ms 
        {
            Some(entry.conn_id)
        } else {
            None
        }
    }
}
```

**Advantages**:
- Zero allocations in hot path
- Cache-friendly sequential memory access
- O(1) insert and lookup
- No cleanup needed (old entries naturally overwritten)

**Files to Modify**:
- `src/sender/sequence.rs` - Replace HashMap with SequenceTracker
- `src/sender/mod.rs` - Update initialization
- `src/sender/packet_handler.rs` - Update usage

**Estimated Impact**: Zero allocations in hot path = ~10% CPU reduction

**Priority**: P0 - Highest

---

### 3. Replace Packet Log Linear Scan with HashSet

**Problem**: `handle_srt_ack()` and `handle_nak()` scan 256 entries linearly.

**Current Code** (`connection/mod.rs:356-370`):
```rust
pub fn handle_srt_ack(&mut self, ack: i32) {
    for i in 0..PKT_LOG_SIZE {  // 256 iterations
        let idx = (self.packet_idx + PKT_LOG_SIZE - 1 - i) % PKT_LOG_SIZE;
        let val = self.packet_log[idx];
        if val == ack {
            self.packet_log[idx] = -1;
            // ...
        } else if val > 0 && val < ack {
            self.packet_log[idx] = -1;
        }
        // ...
    }
}
```

**Solution**: Use `FxHashSet` for O(1) lookups.

```rust
use rustc_hash::{FxHashMap, FxHashSet};

pub struct SrtlaConnection {
    // Replace fixed arrays with hash-based structures
    packet_log_set: FxHashSet<i32>,
    packet_send_times: FxHashMap<i32, u64>,
    // Remove: packet_log: [i32; PKT_LOG_SIZE],
    // Remove: packet_send_times_ms: [u64; PKT_LOG_SIZE],
    // Remove: packet_idx: usize,
    // ...
}

impl SrtlaConnection {
    pub fn register_packet(&mut self, seq: i32, send_time_ms: u64) {
        self.packet_log_set.insert(seq);
        self.packet_send_times.insert(seq, send_time_ms);
        self.in_flight_packets = self.packet_log_set.len() as i32;
    }

    pub fn handle_srt_ack(&mut self, ack: i32) -> Option<u64> {
        // O(1) lookup for exact match
        let send_time = self.packet_send_times.remove(&ack);
        
        // Clear all packets with sequence < ack (cumulative ACK)
        self.packet_log_set.retain(|&seq| seq >= ack);
        self.packet_send_times.retain(|&seq, _| seq >= ack);
        self.in_flight_packets = self.packet_log_set.len() as i32;
        
        send_time
    }

    pub fn handle_nak(&mut self, seq: i32) -> bool {
        // O(1) removal
        let found = self.packet_log_set.remove(&seq);
        if found {
            self.packet_send_times.remove(&seq);
        }
        found
    }
}
```

**Trade-offs**:
- Pro: O(1) vs O(256) per ACK/NAK
- Con: Small allocation overhead per insert (mitigated by pre-sizing)
- Con: Slightly more memory per connection

**Alternative**: Keep fixed arrays but add a secondary `FxHashSet` index for O(1) contains check.

**Files to Modify**:
- `src/connection/mod.rs` - Replace packet log implementation
- `Cargo.toml` - Add `rustc-hash` dependency

**Estimated Impact**: O(1) vs O(256) per ACK/NAK = ~5-10% CPU reduction under load

**Priority**: P1 - High

---

## Moderate Optimizations

### 4. Cache Quality Multipliers

**Problem**: `calculate_quality_multiplier()` is called for every connection on every packet, performing expensive `exp()` calculation.

**Current Code** (`quality.rs:73`):
```rust
let decay_factor = (-(nak_age_ms as f64) / HALF_LIFE_MS).exp();
```

**Solution**: Cache quality multiplier per connection, recalculate periodically (every 50-100ms).

```rust
pub struct CachedQuality {
    pub multiplier: f64,
    pub last_calculated_ms: u64,
}

impl Default for CachedQuality {
    fn default() -> Self {
        Self {
            multiplier: 1.0,
            last_calculated_ms: 0,
        }
    }
}

const QUALITY_CACHE_INTERVAL_MS: u64 = 50; // Recalculate every 50ms

impl SrtlaConnection {
    pub fn get_cached_quality_multiplier(&mut self, current_time_ms: u64) -> f64 {
        if current_time_ms.saturating_sub(self.quality_cache.last_calculated_ms) 
            >= QUALITY_CACHE_INTERVAL_MS 
        {
            self.quality_cache.multiplier = self.calculate_quality_multiplier_internal();
            self.quality_cache.last_calculated_ms = current_time_ms;
        }
        self.quality_cache.multiplier
    }
}
```

**Files to Modify**:
- `src/connection/mod.rs` - Add `CachedQuality` field
- `src/sender/selection/quality.rs` - Use cached value
- `src/sender/selection/enhanced.rs` - Pass timestamp

**Estimated Impact**: ~2-5% CPU reduction

**Priority**: P2 - Medium

---

### 5. Use Fast Approximation for `exp()`

**Problem**: `std::f64::exp()` is relatively expensive.

**Solution**: Use a fast polynomial approximation for the decay calculation.

```rust
/// Fast exponential approximation for x in range [-10, 0]
/// Uses Remez polynomial approximation, accurate to ~0.1%
#[inline]
fn fast_exp_decay(x: f64) -> f64 {
    // For our use case: x = -age_ms / 2000.0, so x is in [-5, 0] typically
    // Clamp to valid range
    let x = x.max(-10.0).min(0.0);
    
    // 4th order polynomial approximation for e^x
    // Coefficients optimized for [-10, 0] range
    let x2 = x * x;
    let x3 = x2 * x;
    let x4 = x2 * x2;
    
    1.0 + x + 0.5 * x2 + 0.166667 * x3 + 0.041667 * x4
}

// Usage in quality.rs:
let decay_factor = fast_exp_decay(-(nak_age_ms as f64) / HALF_LIFE_MS);
```

**Alternative**: Use integer math with lookup table (but you mentioned this is "yucky", so polynomial approximation is cleaner).

**Files to Modify**:
- `src/sender/selection/quality.rs` - Add and use `fast_exp_decay()`

**Estimated Impact**: ~1-2% CPU reduction

**Priority**: P2 - Medium

---

### 6. Cache Toggle State Per Select Iteration

**Problem**: 3 atomic loads per packet.

**Current Code** (`packet_handler.rs:245-253`):
```rust
let enable_quality = toggles.quality_scoring_enabled.load(Ordering::Relaxed);
let enable_explore = toggles.exploration_enabled.load(Ordering::Relaxed);
let classic = toggles.classic_mode.load(Ordering::Relaxed);
```

**Solution**: Create a snapshot struct, load once at start of select iteration.

```rust
#[derive(Clone, Copy)]
pub struct ToggleSnapshot {
    pub classic_mode: bool,
    pub quality_scoring_enabled: bool,
    pub exploration_enabled: bool,
}

impl DynamicToggles {
    #[inline]
    pub fn snapshot(&self) -> ToggleSnapshot {
        ToggleSnapshot {
            classic_mode: self.classic_mode.load(Ordering::Relaxed),
            quality_scoring_enabled: self.quality_scoring_enabled.load(Ordering::Relaxed),
            exploration_enabled: self.exploration_enabled.load(Ordering::Relaxed),
        }
    }
}

// In main select loop:
loop {
    let toggle_snap = toggles.snapshot(); // Once per iteration
    
    tokio::select! {
        res = local_listener.recv_from(&mut recv_buf) => {
            handle_srt_packet(..., &toggle_snap).await;
        }
        // ...
    }
}
```

**Files to Modify**:
- `src/toggles.rs` - Add `ToggleSnapshot` and `snapshot()` method
- `src/sender/mod.rs` - Cache at loop start
- `src/sender/packet_handler.rs` - Accept `ToggleSnapshot` instead of `&DynamicToggles`

**Estimated Impact**: ~1-2% CPU reduction

**Priority**: P3 - Low

---

### 7. Inline ACK Forwarding (Remove Channel Hop)

**Problem**: ACK forwarding goes through unbounded channel + spawned task.

**Current Code** (`sender/mod.rs:99-106`):
```rust
tokio::spawn(async move {
    while let Some((client_addr, ack_packet)) = instant_rx.recv().await {
        let _ = local_listener_clone.send_to(&ack_packet, client_addr).await;
    }
});
```

**Solution**: Try synchronous send first, fall back to channel only if would block.

```rust
// In process_packet_internal:
if let Some(addr) = client_addr {
    // Try sync send first (no task switch)
    match local_listener.try_send_to(&ack_packet, addr) {
        Ok(_) => {}
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            // Only use channel if socket would block
            let _ = instant_tx.send((addr, ack_packet));
        }
        Err(_) => {}
    }
}
```

**Note**: `UdpSocket::try_send_to` requires the socket to be in non-blocking mode (which it is).

**Files to Modify**:
- `src/connection/mod.rs` - Update ACK forwarding logic
- `src/sender/mod.rs` - Optionally remove spawned task

**Estimated Impact**: Reduces task context switches = ~2-3% CPU

**Priority**: P3 - Low

---

## Minor Optimizations

### 8. Add `#[inline(always)]` to Critical Functions

Add to hot-path functions:

```rust
#[inline(always)]
pub fn get_score(&self) -> i32 { ... }

#[inline(always)]
pub fn is_timed_out(&self) -> bool { ... }

#[inline(always)]
fn calculate_quality_multiplier(conn: &SrtlaConnection) -> f64 { ... }
```

**Files to Modify**:
- `src/connection/mod.rs`
- `src/sender/selection/quality.rs`
- `src/sender/selection/classic.rs`

**Priority**: P4 - Minor

---

### 9. Use `FxHashMap` Instead of `HashMap`

Replace `std::collections::HashMap` with `rustc_hash::FxHashMap` for faster hashing.

```toml
# Cargo.toml
[dependencies]
rustc-hash = "2.0"
```

```rust
use rustc_hash::FxHashMap;

// Replace HashMap<u32, SequenceTrackingEntry> with FxHashMap
let mut seq_to_conn: FxHashMap<u32, SequenceTrackingEntry> = 
    FxHashMap::with_capacity_and_hasher(MAX_SEQUENCE_TRACKING, Default::default());
```

**Note**: FxHashMap uses a faster (non-cryptographic) hash function. Safe for internal use where keys aren't attacker-controlled.

**Files to Modify**:
- `Cargo.toml`
- `src/sender/mod.rs`
- `src/sender/packet_handler.rs`

**Priority**: P4 - Minor (if keeping HashMap approach)

---

### 10. Reduce Debug Logging Overhead

**Problem**: String formatting happens even when log level is off.

**Solution**: Use `#[cold]` attribute on debug-only branches or feature-gate verbose logging.

```rust
// Option A: Cold attribute
#[cold]
fn log_quality_degraded(label: &str, mult: f64, ...) {
    debug!("{} quality degraded: {:.2} ...", label, mult, ...);
}

if quality_mult < 0.8 {
    log_quality_degraded(&c.label, quality_mult, ...);
}

// Option B: Feature gate
#[cfg(feature = "verbose-debug")]
if quality_mult < 0.8 {
    debug!(...);
}
```

**Files to Modify**:
- `src/sender/selection/enhanced.rs`
- `src/sender/selection/quality.rs`

**Priority**: P4 - Minor

---

### 11. Pre-size SmallVec for Connections

```rust
// When loading IPs, pre-allocate if > 4 connections expected
let mut connections: SmallVec<SrtlaConnection, 4> = if ips.len() > 4 {
    SmallVec::with_capacity(ips.len())
} else {
    SmallVec::new()
};
```

**Priority**: P4 - Minor

---

## Architectural Considerations

### 12. Consider Single-Threaded Runtime

**Current**: Multi-threaded tokio runtime with separate reader tasks per connection.

**Alternative**: Single-threaded runtime matching C's `select()` pattern.

```rust
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // ...
}
```

**Trade-offs**:
- Pro: Eliminates task context switches
- Pro: Eliminates channel overhead
- Pro: Eliminates Arc reference counting
- Con: Less parallelism (but original C works fine single-threaded)
- Con: Larger refactoring effort

**Recommendation**: Test with `current_thread` flavor first to measure impact before larger refactoring.

**Priority**: P5 - Investigate

---

### 13. Batch Socket Operations

**Idea**: Instead of one `send()` per packet, batch packets going to the same connection.

**Challenges**:
- SRT expects timely delivery
- Sequence tracking becomes more complex
- Minimal benefit for typical packet sizes

**Recommendation**: Not recommended for this use case.

**Priority**: P5 - Not recommended

---

## Implementation Order

### Phase 1: Quick Wins (1-2 days)
1. Cache timestamps per packet (Critical)
2. Cache toggle state per iteration (Moderate)
3. Add `#[inline(always)]` to critical functions (Minor)

### Phase 2: Data Structure Improvements (2-3 days)
4. Replace sequence tracking HashMap with ring buffer (Critical)
5. Add `rustc-hash` dependency, use `FxHashMap` where HashMap remains (Minor)
6. Replace packet log linear scan with HashSet (Critical)

### Phase 3: Algorithm Optimizations (1-2 days)
7. Cache quality multipliers (Moderate)
8. Fast `exp()` approximation (Moderate)

### Phase 4: I/O Optimizations (1 day)
9. Inline ACK forwarding (Moderate)
10. Reduce debug logging overhead (Minor)

### Phase 5: Architectural Testing (1 day)
11. Test `current_thread` runtime flavor
12. Profile and measure all changes

---

## Measurement Plan

Before and after each optimization:

```bash
# Build optimized binary
cargo build --release --profile release-lto

# Run with profiling
RUST_LOG=warn perf stat -d ./target/release/srtla_send 6000 host 5000 uplinks.txt

# Alternative: flamegraph
cargo flamegraph --release -- 6000 host 5000 uplinks.txt
```

Key metrics to track:
- CPU usage (%)
- Instructions per packet
- Cache miss rate
- Syscall count (strace -c)
- Memory allocations (valgrind --tool=massif or DHAT)

---

## Dependencies to Add

```toml
# Cargo.toml additions
[dependencies]
rustc-hash = "2.0"  # Fast non-cryptographic hash
```

---

## Summary

| Optimization | Impact | Effort | Priority |
|-------------|--------|--------|----------|
| Cache timestamps per packet | High | Low | P0 |
| Ring buffer for sequence tracking | High | Medium | P0 |
| HashSet for packet log | Medium-High | Medium | P1 |
| Cache quality multipliers | Medium | Low | P2 |
| Fast `exp()` approximation | Low-Medium | Low | P2 |
| Cache toggle state | Low | Low | P3 |
| Inline ACK forwarding | Low | Low | P3 |
| `#[inline(always)]` | Low | Trivial | P4 |
| FxHashMap | Low | Trivial | P4 |
| Debug logging overhead | Low | Low | P4 |
| Single-threaded runtime | Variable | Medium | P5 |

Total estimated improvement: **25-40% CPU reduction** in packet processing hot path.
