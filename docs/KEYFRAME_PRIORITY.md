# Keyframe priority sidecar

srtla_send offers two complementary ways to treat keyframe / parameter-set packets as critical and route them to the most reliable link:

1. A packet-size heuristic (`src/sender/keyframe.rs`) that watches for runs of max-MTU 1316-byte SRT packets and declares a burst when 5 or more land in a row.
2. An out-of-band UDP sidecar where an upstream encoder explicitly opens a short "critical window".

The two are OR-combined. An encoder that knows it is about to emit a keyframe opens a window; the heuristic keeps catching bursts on its own when no encoder feedback is available.

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

Overlapping windows extend the deadline monotonically (`fetch_max`). A late datagram referring to an earlier deadline is ignored — it can never shrink an active window.

## Enabling

Pass `--priority-bind ADDR:PORT` to `srtla_send`:

```
srtla_send --priority-bind 127.0.0.1:7000 \
           --control-socket /tmp/srtla.sock \
           6000 rec.example.com 5000 /tmp/uplinks
```

The sender (belacoder or a custom encoder) binds any local UDP socket, connects to that address, and sends 5-byte datagrams when a keyframe is emitted. Any loopback UDP datagram that doesn't match the magic byte and length is counted as malformed (visible in `get_status` as `critical_malformed_datagrams`).

## Picking a window length

A window of 30–80 ms covers a typical keyframe burst at 24–60 fps. Err on the high side — marking a couple of non-keyframe trailing packets critical is harmless; missing the last keyframe packet is not. The default used by belacoder's keyframe probe is 50 ms.

## Telemetry

`get_status` exposes two counters:

```json
{
  "critical_windows_received": 142,
  "critical_malformed_datagrams": 0
}
```

`critical_malformed_datagrams > 0` almost always means a mismatched magic byte (version skew) or a sender writing short datagrams.
