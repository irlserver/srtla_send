# SRTLA Protocol Extensions (irlserver)

This document describes irlserver-specific extensions to the SRTLA protocol. These extensions are **NOT** part of the standard SRTLA specification and may not be compatible with other SRTLA implementations.

## Overview

SRTLA extensions provide additional functionality beyond the core SRTLA bonding protocol, including:

- **Extension Negotiation**: Automatic capability handshake to determine mutually supported features
- **Connection Telemetry**: Per-uplink statistics for monitoring and debugging
- **Future Extensibility**: Reserved packet type ranges for additional features

## Extension Negotiation Protocol

Extensions use an automatic capability negotiation system to ensure compatibility:

### Handshake Flow

```
Sender                          Receiver
  |                                |
  |  REG1 (SRTLA registration)     |
  |─────────────────────────────>  |
  |                                |
  |  REG2 (registration response)  |
  |  <─────────────────────────────|
  |                                |
  |  REG2 (full ID)                |
  |─────────────────────────────>  |
  |                                |
  |  REG3 (success)                |
  |  <─────────────────────────────|
  |                                |
  |  EXT_HELLO (capabilities)      |  ← Extension negotiation starts
  |─────────────────────────────>  |
  |                                |
  |  EXT_ACK (capabilities)        |
  |  <─────────────────────────────|
  |                                |
  |  [Only mutually supported      |
  |   extensions are used]         |
  |                                |
```

### Packet Format

**EXT_HELLO and EXT_ACK** (10 bytes):
```
Offset | Size | Field
-------|------|------------------
0-1    | 2    | Packet type (0x9FF0 for HELLO, 0x9FF1 for ACK)
2-3    | 2    | Protocol version (0x0001)
4-7    | 4    | Capability flags (bitmask, big-endian)
8-9    | 2    | Reserved (0x0000)
```

### Capability Flags

| Bit | Value      | Extension                    |
|-----|------------|------------------------------|
| 0   | 0x00000001 | Connection info telemetry    |
| 1-31| Reserved   | Future extensions            |

### Implementation

Extensions are negotiated automatically:

1. **After REG3**: Sender automatically sends `EXT_HELLO` with its supported capabilities
2. **Receiver Response**: Receiver sends `EXT_ACK` with its supported capabilities
3. **Mutual Support**: Only features supported by both sides are enabled

**No configuration required!** The system gracefully handles:
- Old receivers that don't understand extensions (no EXT_ACK → no extensions used)
- Partial support (only common features are enabled)
- Future protocol versions

## Extension Packet Type Range

irlserver extensions use the **0x9F00-0x9FFF** range:

| Range       | Purpose                              |
|-------------|--------------------------------------|
| 0x9F00-0x9F0F | Connection telemetry and statistics |
| 0x9F10-0x9F1F | Quality and performance metrics     |
| 0x9F20-0x9FEF | Reserved for future extensions      |
| 0x9FF0-0x9FFF | Extension negotiation and handshake |

Standard SRTLA receivers should **silently ignore** packets in this range.

## Extension: Connection Info Telemetry (0x9F00)

### Purpose

Sends per-connection statistics from sender to receiver every 5 seconds for:
- **Monitoring**: Real-time visibility into each uplink's health
- **Debugging**: Identify problematic connections
- **Analytics**: Historical performance data
- **Future Features**: Potential receiver-side adaptive bonding

### Packet Format

**CONN_INFO** (32 bytes):
```
Offset | Size | Field                      | Type
-------|------|----------------------------|-------
0-1    | 2    | Packet type (0x9F00)       | u16
2-3    | 2    | Version (0x0001)           | u16
4-7    | 4    | Connection ID              | u32
8-11   | 4    | Window size                | i32
12-15  | 4    | In-flight packets          | i32
16-23  | 8    | Smooth RTT (microseconds)  | u64
24-27  | 4    | NAK count                  | u32
28-31  | 4    | Bitrate (bytes/sec)        | u32
```

All multi-byte fields are **big-endian** (network byte order).

### Sending Behavior

- **Automatic**: Enabled when receiver supports it (via extension negotiation)
- **Interval**: Every 5 seconds per connection
- **Condition**: Only sent when connection is established and active
- **Timing**: First packet sent ~5 seconds after connection is established

### Example Statistics

For a 4-modem setup, you'll receive:
- **4 CONN_INFO packets** every 5 seconds (one per uplink)
- Each packet shows that uplink's individual statistics
- Allows comparing performance across modems

Example metrics:
```
Connection 1: window=25000, in_flight=3, rtt=120ms, naks=5,  bitrate=2.5MB/s
Connection 2: window=30000, in_flight=8, rtt=95ms,  naks=2,  bitrate=3.2MB/s
Connection 3: window=15000, in_flight=1, rtt=200ms, naks=25, bitrate=1.1MB/s
Connection 4: window=28000, in_flight=6, rtt=110ms, naks=3,  bitrate=3.0MB/s
```

This clearly shows Connection 3 is experiencing issues (high RTT, many NAKs, low throughput).

## Compatibility

### With Standard SRTLA Receivers

Standard SRTLA receivers should:
- ✅ **Ignore EXT_HELLO** packets (unknown type 0x9FF0)
- ✅ **Ignore CONN_INFO** packets (unknown type 0x9F00)  
- ✅ **Continue normal operation** without any issues

If a receiver doesn't respond with EXT_ACK, the sender:
- ✅ **Detects lack of support** (no EXT_ACK received)
- ✅ **Disables all extensions** automatically
- ✅ **Operates in standard SRTLA mode**

### With Future Versions

The extension protocol is designed for forward compatibility:
- **Version field**: Allows protocol evolution (currently 0x0001)
- **Version validation**: Receivers MUST validate version fields and reject mismatched packets
- **Bitmask**: New features can be added without breaking old implementations
- **Graceful degradation**: Only common features are used

#### Version Validation Policy

Both EXT_HELLO and CONN_INFO packets include a version field that MUST be validated:

- **EXT_HELLO**: If version doesn't match, log error and ignore packet (no EXT_ACK sent)
- **CONN_INFO**: If version doesn't match, log warning and ignore packet
- **Rationale**: Prevents protocol confusion and ensures data is interpreted correctly

## Implementation Details

### Code Organization

All extension code is in `src/extensions.rs`:

```rust
use srtla_send::extensions::{
    // Constants
    SRTLA_EXT_HELLO,
    SRTLA_EXT_ACK,
    SRTLA_EXT_CAP_CONN_INFO,
    
    // Functions
    create_extension_hello,
    parse_extension_packet,
    create_connection_info_packet,
    parse_connection_info,
    has_extension,
    
    // Types

    ExtensionCapabilities,
    ConnectionInfoData,
};
```

EXT_ACK packets use a fixed 10-byte layout, so receivers often add a helper to build them consistently:

```rust
fn create_extension_ack(capabilities: u32) -> [u8; SRTLA_EXT_HELLO_LEN] {
    let mut pkt = [0u8; SRTLA_EXT_HELLO_LEN];
    pkt[0..2].copy_from_slice(&SRTLA_EXT_ACK.to_be_bytes());
    pkt[2..4].copy_from_slice(&SRTLA_EXT_VERSION.to_be_bytes());
    pkt[4..8].copy_from_slice(&capabilities.to_be_bytes());
    pkt[8..10].copy_from_slice(&0u16.to_be_bytes());
    pkt
}
```

### Connection State


Each `SrtlaConnection` tracks:
```rust
extensions_negotiated: bool,      // Whether EXT_ACK was received
receiver_capabilities: u32,       // Receiver's capability bitmask
```

Check if an extension is supported:
```rust
if conn.has_extension(SRTLA_EXT_CAP_CONN_INFO) {
    conn.send_connection_info().await?;
}
```

## Receiver Implementation Guide

To implement extension support in an SRTLA receiver:

### 1. Handle EXT_HELLO

```rust
if packet_type == SRTLA_EXT_HELLO {
    let caps = parse_extension_packet(&packet)?;
    
    // Validate protocol version
    if caps.version != SRTLA_EXT_VERSION {
        log::error!(
            "Extension version mismatch: received=0x{:04X}, expected=0x{:04X}",
            caps.version,
            SRTLA_EXT_VERSION
        );
        return Ok(()); // Ignore incompatible version
    }
    
    // Determine what you support
    let our_caps = SRTLA_EXT_CAP_CONN_INFO; // Add more with |
    
    // Respond with EXT_ACK (0x9FF1)
    let ack = create_extension_ack(our_caps);
    send_packet(ack)?;
}
```

The receiver MUST reply with an `EXT_ACK` (type `0x9FF1`) so the sender knows negotiation succeeded. The helper above keeps the payload consistent while explicitly setting the type to the ACK opcode.

### 2. Handle CONN_INFO


```rust
if packet_type == SRTLA_EXT_CONN_INFO {
    let info = parse_connection_info(&packet)?;
    
    // Validate protocol version
    if info.version != SRTLA_EXT_VERSION {
        log::warn!(
            "CONN_INFO version mismatch: received=0x{:04X}, expected=0x{:04X}",
            info.version,
            SRTLA_EXT_VERSION
        );
        return Ok(()); // Ignore incompatible version
    }
    
    // Log or process the statistics
    log::info!(
        "Uplink {}: window={}, rtt={}μs, naks={}, bitrate={}KB/s",
        info.conn_id,
        info.window,
        info.rtt_us,
        info.nak_count,
        info.bitrate_bytes_per_sec / 1000
    );
}
```

### 3. Ignore Unknown Extensions

```rust
if packet_type >= 0x9F00 && packet_type <= 0x9FFF {
    // Unknown extension packet - ignore silently
    return Ok(());
}
```

## Future Extensions

The protocol is designed to support additional extensions:

### Potential Extensions

- **Quality Metrics** (0x9F10): Detailed jitter, loss patterns
- **Bandwidth Hints** (0x9F11): Capacity recommendations  
- **Path MTU** (0x9F12): MTU discovery results
- **Latency Profiles** (0x9F13): Historical latency distribution

### Adding a New Extension

1. **Allocate packet type** in appropriate range
2. **Define capability flag** (next available bit)
3. **Document packet format**
4. **Add to negotiation**
5. **Update version if wire format changes**

Example:
```rust
// In src/extensions.rs
pub const SRTLA_EXT_QUALITY_METRICS: u16 = 0x9F10;
pub const SRTLA_EXT_CAP_QUALITY: u32 = 0x00000002;  // Bit 1

// Announce support
let our_caps = SRTLA_EXT_CAP_CONN_INFO | SRTLA_EXT_CAP_QUALITY;
```

## Security Considerations

- **No Authentication**: Extension packets are not authenticated
- **Trust Model**: Same as base SRTLA protocol (trusted sender)
- **DoS Protection**: Receivers should rate-limit extension packets
- **Parsing**: Always validate packet lengths and field values

## Performance Impact

- **EXT_HELLO/ACK**: 10 bytes, sent once per connection at registration
- **CONN_INFO**: 32 bytes, sent every 5 seconds per connection
- **Overhead**: ~6.4 bytes/second per connection (negligible)

For typical 4-modem setup:
- **Negotiation**: 10 bytes × 4 = 40 bytes (one-time)
- **Telemetry**: 32 bytes × 4 ÷ 5 seconds = **25.6 bytes/second**

This is less than 0.0002% overhead on a 10 Mbps connection.

## Debugging

Enable extension-related logs:
```bash
RUST_LOG=debug srtla_send ...
```

Look for:
```
DEBUG srtla_send: Connection 1: Sent extension HELLO (capabilities=0x00000001)
INFO srtla_send: Connection 1: Extension negotiation complete (receiver capabilities=0x00000001)
DEBUG srtla_send: Connection 1: Sent connection info (window=25000, in_flight=3, ...)
```

If you don't see "Extension negotiation complete", the receiver doesn't support extensions.

## References

- [SRTLA Protocol Specification](https://github.com/BELABOX/srtla)
- [Source Code: src/extensions.rs](../src/extensions.rs)
- [Connection Management: src/connection/mod.rs](../src/connection/mod.rs)

## License

Same as srtla_send (MIT).
