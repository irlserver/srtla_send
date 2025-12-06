# Extended KEEPALIVE with Connection Info

## Overview

This implementation extends the standard SRTLA KEEPALIVE packet to include per-connection telemetry data. This approach is **fully backwards compatible** with existing SRTLA receivers and requires **no negotiation handshake**.

## Motivation

The previous extension system used separate packet types (EXT_HELLO, EXT_ACK, CONN_INFO) and required capability negotiation. The new approach simplifies this by:

1. **Eliminating negotiation overhead** - No EXT_HELLO/EXT_ACK handshake needed
2. **Reducing packet types** - One packet type instead of three
3. **Automatic capability detection** - Receivers detect extended format via packet length and magic number
4. **Maintaining backwards compatibility** - Old receivers work without any changes

## Packet Format

### Standard KEEPALIVE (10 bytes)

```
Offset | Size | Field
-------|------|------------------
0-1    | 2    | Packet type (0x9000)
2-9    | 8    | Timestamp (u64 ms, big-endian)
```

### Extended KEEPALIVE (42 bytes)

```
Offset | Size | Field
-------|------|---------------------------
0-1    | 2    | Packet type (0x9000)
2-9    | 8    | Timestamp (u64 ms, big-endian)
10-11  | 2    | Magic number (0xC01F)
12-13  | 2    | Version (0x0001)
14-17  | 4    | Connection ID (u32)
18-21  | 4    | Window size (i32)
22-25  | 4    | In-flight packets (i32)
26-33  | 8    | Smooth RTT (u64 μs)
34-37  | 4    | NAK count (u32)
38-41  | 4    | Bitrate (u32 bytes/sec)
```

All multi-byte fields are **big-endian** (network byte order).

### Magic Number

The magic number `0xC01F` serves as a format identifier for extended keepalives:
- **"C01F"** = Mnemonic for "Connection Info" (human-readable in hex dumps)
- Positioned at bytes 10-11 (immediately after standard keepalive header)
- Combined with version check (bytes 12-13), provides ~1 in 4 billion false positive rate
- Distinguishes extended keepalives from corrupted packets or future extensions

## Backwards Compatibility

### How It Works

**Old receivers (standard SRTLA):**
1. Read packet type at bytes 0-1 → sees `0x9000` (KEEPALIVE)
2. Read timestamp at bytes 2-9 → extracts valid timestamp
3. **Ignore bytes 10-41** → UDP packets can be any length, extra data is simply ignored
4. Continue normal operation

**New receivers (extended SRTLA):**
1. Check packet length: `>= 42 bytes`?
2. Check magic at bytes 10-11: `== 0xC01F`?
3. Check version at bytes 12-13: `== 0x0001`?
4. If all checks pass: Parse connection info
5. Otherwise: Treat as standard keepalive

### Compatibility Matrix

| Sender  | Receiver | Result                                    |
|---------|----------|-------------------------------------------|
| Old     | Old      | ✅ Standard keepalives work               |
| Old     | New      | ✅ Receiver sees standard keepalives      |
| New     | Old      | ✅ Receiver ignores extra bytes           |
| New     | New      | ✅ Receiver gets connection telemetry     |

## Implementation

### Sender (Rust)

The sender **always** sends extended keepalives:

```rust
use srtla_send::protocol::{create_keepalive_packet_ext, ConnectionInfo};

let info = ConnectionInfo {
    conn_id: connection.conn_id as u32,
    window: connection.window,
    in_flight: connection.in_flight_packets,
    rtt_us: (connection.rtt.smooth_rtt_ms * 1000.0) as u64,
    nak_count: connection.congestion.nak_count as u32,
    bitrate_bytes_per_sec: (connection.bitrate.current_bitrate_bps / 8.0) as u32,
};

let packet = create_keepalive_packet_ext(info);
socket.send(&packet).await?;
```

### Receiver (Rust)

The receiver detects and parses extended keepalives:

```rust
use srtla_send::protocol::{extract_keepalive_timestamp, extract_keepalive_conn_info};

// Always extract timestamp (works for both standard and extended)
if let Some(timestamp) = extract_keepalive_timestamp(&packet) {
    update_rtt(timestamp);
}

// Try to extract connection info (only works for extended)
if let Some(info) = extract_keepalive_conn_info(&packet) {
    log::info!(
        "Uplink {}: window={}, in_flight={}, rtt={}μs, naks={}, bitrate={}KB/s",
        info.conn_id,
        info.window,
        info.in_flight,
        info.rtt_us,
        info.nak_count,
        info.bitrate_bytes_per_sec / 1000
    );
}
```

### Receiver (C/C++)

```c
#define SRTLA_KEEPALIVE_MAGIC 0xc01f
#define SRTLA_KEEPALIVE_EXT_LEN 42
#define SRTLA_KEEPALIVE_EXT_VERSION 0x0001

typedef struct {
    uint32_t conn_id;
    int32_t window;
    int32_t in_flight;
    uint64_t rtt_us;
    uint32_t nak_count;
    uint32_t bitrate_bytes_per_sec;
} connection_info_t;

bool parse_keepalive_conn_info(const uint8_t *buf, size_t len, connection_info_t *info) {
    if (len < SRTLA_KEEPALIVE_EXT_LEN) return false;
    
    uint16_t packet_type = (buf[0] << 8) | buf[1];
    if (packet_type != SRTLA_TYPE_KEEPALIVE) return false;
    
    // Check magic number at bytes 10-11
    uint16_t magic = (buf[10] << 8) | buf[11];
    if (magic != SRTLA_KEEPALIVE_MAGIC) return false;
    
    // Check version at bytes 12-13
    uint16_t version = (buf[12] << 8) | buf[13];
    if (version != SRTLA_KEEPALIVE_EXT_VERSION) return false;
    
    // Parse connection info (all big-endian)
    info->conn_id = (buf[14] << 24) | (buf[15] << 16) | (buf[16] << 8) | buf[17];
    info->window = (int32_t)((buf[18] << 24) | (buf[19] << 16) | (buf[20] << 8) | buf[21]);
    info->in_flight = (int32_t)((buf[22] << 24) | (buf[23] << 16) | (buf[24] << 8) | buf[25]);
    info->rtt_us = ((uint64_t)buf[26] << 56) | ((uint64_t)buf[27] << 48) | 
                   ((uint64_t)buf[28] << 40) | ((uint64_t)buf[29] << 32) |
                   ((uint64_t)buf[30] << 24) | ((uint64_t)buf[31] << 16) |
                   ((uint64_t)buf[32] << 8)  | (uint64_t)buf[33];
    info->nak_count = (buf[34] << 24) | (buf[35] << 16) | (buf[36] << 8) | buf[37];
    info->bitrate_bytes_per_sec = (buf[38] << 24) | (buf[39] << 16) | (buf[40] << 8) | buf[41];
    
    return true;
}

// Usage in receiver
if (packet_type == SRTLA_TYPE_KEEPALIVE) {
    // Always extract timestamp (standard keepalive handling)
    uint64_t timestamp = extract_timestamp(buf);
    update_rtt(timestamp);
    
    // Try to extract connection info
    connection_info_t info;
    if (parse_keepalive_conn_info(buf, len, &info)) {
        log_connection_stats(&info);
    }
}
```

## Connection Info Fields

### conn_id (u32)
Unique identifier for this connection. Allows receivers to track statistics per uplink.

### window (i32)
Current congestion window size. Indicates the connection's estimated capacity in packets.

### in_flight (i32)
Number of packets currently in flight (sent but not acknowledged). Used for load calculation.

### rtt_us (u64)
Smoothed round-trip time in microseconds. Calculated from keepalive timestamps.

### nak_count (u32)
Total number of NAKs received on this connection since startup. Indicates packet loss.

### bitrate_bytes_per_sec (u32)
Current bitrate in bytes per second. Measured over recent send activity.

## Overhead

For a typical 4-modem setup with keepalives every 1 second:

- **Standard keepalive**: 10 bytes × 4 = 40 bytes/sec
- **Extended keepalive**: 42 bytes × 4 = 168 bytes/sec
- **Additional overhead**: 128 bytes/sec (0.001 Mbps)

This is **negligible** compared to typical streaming bitrates (5-50 Mbps).

## Benefits Over Previous Extension System

1. **Simpler**: No negotiation handshake required
2. **Faster**: Telemetry available immediately after connection
3. **More reliable**: No dependency on EXT_ACK response
4. **Backwards compatible**: Works with all existing receivers
5. **Less code**: Eliminates entire extension negotiation module
6. **Fewer packets**: One packet type instead of three

## Future Extensibility

If additional fields are needed in the future:

1. **Increment version number** (e.g., 0x0002)
2. **Add fields at the end** (bytes 42+)
3. **Receivers check version** and reject unknown versions
4. **Old receivers ignore extra bytes** (backwards compatible)

Example for version 2 (50 bytes):
```
42-45  | 4    | Jitter (u32 μs)
46-49  | 4    | Packet loss rate (f32)
```

## Security Considerations

- **No authentication**: Connection info is not authenticated
- **Trust model**: Same as base SRTLA protocol
- **DoS protection**: Receivers should rate-limit keepalive processing
- **Validation**: Always validate packet length, magic, and version

## Testing

The implementation includes comprehensive tests:

```bash
# Run all tests
cargo test

# Run protocol tests specifically
cargo test protocol::tests
```

Tests verify:
- Extended keepalive creation and parsing
- Backwards compatibility with standard keepalives
- Magic number validation
- Version validation
- Roundtrip encoding/decoding

## References

- [SRTLA Protocol](https://github.com/BELABOX/srtla)
- [Implementation: src/protocol.rs](../src/protocol.rs)
- [Usage: src/connection/mod.rs](../src/connection/mod.rs)

## License

Same as srtla_send (MIT).
