# Keyframe priority sidecar

srtla_send treats keyframe and parameter-set packets as critical when an upstream encoder says so. The encoder opens a short "critical window" over an out-of-band UDP sidecar. While that window is open in enhanced mode, srtla_send routes data packets to the link with the best quality score. When every eligible link scores the same, the normal scheduler decides.

srtla_send does not detect keyframes itself. It sees only opaque SRT payloads, so without encoder hints no packet is treated as critical.

## Why a sidecar UDP, not the JSON-RPC control socket

The JSON-RPC control socket rides a separate path from SRT packets. A hint that arrives microseconds after the packets it describes misses them entirely. Sharing the network stack with the data (UDP loopback, same `recvfrom` discipline on srtla_send) keeps hints ordered tightly against the packets they describe.

## Wire format

One 5-byte UDP datagram per request:

```
byte 0     : 0xC1              — magic tag (Critical v1)
bytes 1..5 : u32 big-endian    — window length in milliseconds
```

srtla_send stores `now + window_ms` as the current critical deadline.
`is_critical_now()` returns true while `now < deadline`.

srtla_send caps a single window at 2000 ms and counts each datagram it had to cap.

Overlapping windows extend the deadline monotonically (`fetch_max`). A late datagram referring to an earlier deadline is ignored — it can never shrink an active window.

## Enabling

Pass `--priority-bind ADDR:PORT` to `srtla_send`:

```
srtla_send --priority-bind 127.0.0.1:7000 \
           --control-socket $XDG_RUNTIME_DIR/srtla.sock \
           6000 rec.example.com 5000 /tmp/uplinks
```

The sender (belacoder or a custom encoder) binds any local UDP socket, connects to that address, and sends 5-byte datagrams when a keyframe is emitted. Any loopback UDP datagram that doesn't match the magic byte and length is counted as malformed (visible in `get_status` as `critical_malformed_datagrams`).

## Picking a window length

A window of 30–80 ms covers a typical keyframe burst at 24–60 fps. Err on the high side — marking a couple of non-keyframe trailing packets critical is harmless; missing the last keyframe packet is not. The default used by belacoder's keyframe probe is 50 ms.

## Telemetry

`get_status` exposes three counters:

```json
{
  "critical_windows_received": 142,
  "critical_malformed_datagrams": 0,
  "critical_clamped_windows": 0
}
```

`critical_malformed_datagrams > 0` almost always means a mismatched magic byte (version skew) or a sender writing short datagrams.

`critical_clamped_windows > 0` means a sender asked for a window longer than 2000 ms. Most often the encoder writes the length little endian, which turns 50 ms into about 9.7 days.
