# srtla_send control protocol

srtla_send exposes runtime control over a Unix socket (preferred) or stdin. The wire format is JSON-RPC 2.0, one message per line. The socket path is set with `--control-socket <path>`.

The socket is set to mode 0600 right after it is bound, so put it in a directory only the service user can enter, such as `$XDG_RUNTIME_DIR` or `/run/srtla`. Between the bind and the chmod the socket briefly has the permissions of the process umask, and the private directory is what covers that window. A socket left at the path by an earlier run is replaced, but any other file there makes srtla_send refuse to open the control socket.

## Request/response

One request per line, UTF-8 JSON. Responses end with a newline. A request line longer than 64 KiB closes the connection.

Request:

```json
{"jsonrpc": "2.0", "id": 1, "method": "set_mode", "params": {"mode": "enhanced"}}
```

Success:

```json
{"jsonrpc": "2.0", "result": {"mode": "enhanced"}, "id": 1}
```

Error (standard JSON-RPC codes):

```json
{"jsonrpc": "2.0", "error": {"code": -32602, "message": "expected params.mode: string"}, "id": 1}
```

## Notifications

Requests without `id` are notifications. srtla_send processes them and sends no response. Useful for one-way config pokes when round-tripping a reply would be wasteful.

```json
{"jsonrpc": "2.0", "method": "set_mode", "params": {"mode": "classic"}}
```

## Methods

### `set_mode`

Switch the link scheduler.

| param | type | values |
| --- | --- | --- |
| `mode` | string | `"classic"`, `"enhanced"` |

Result: `{ "mode": "<current>" }`.

### `set_quality`

Toggle quality scoring (enhanced mode).

Params: `{ "enabled": bool }`. Result: `{ "enabled": bool }`.

### `set_stall_deselect`

Toggle the stalled-link deselect guard.

Params: `{ "enabled": bool }`. Result: `{ "enabled": bool }`.

### `set_conn_timeout`

Set the per-link liveness timeout in milliseconds.

Params: `{ "ms": u64 }`. Result: `{ "ms": <applied> }`. The value is clamped to 1000..=60000, and the response carries the applied value, so a client should trust the response rather than its input.

### `get_status`

Return the full runtime configuration plus priority-sidecar telemetry.

Result:

```json
{
  "mode": "enhanced",
  "quality_enabled": true,
  "stall_deselect": true,
  "stall_min_in_flight": 32,
  "stall_ack_stale_ms": 3000,
  "conn_timeout_ms": 5000,
  "critical_windows_received": 142,
  "critical_malformed_datagrams": 0,
  "critical_clamped_windows": 0
}
```

`critical_clamped_windows` counts priority-sidecar windows capped at the 2000 ms maximum.

### `get_stats`

Return per-link telemetry (the JSON previously returned by `stats`).

#### Arrival reports per link

The receiver acknowledges each data packet in an SRTLA ACK sent back on the link the packet arrived on (irlserver/srtla `register_packet`, BELABOX `srtla_rec`, both in batches of 10). Counting those entries on the link the ACK came back on tells which path the receiver actually heard from. The sender's own counters (`bitrate_bytes_per_sec`, `in_flight`) only say what it put on a link, and cumulative SRT ACKs and NAKs, which the receiver sends on every link of the group, prune its packet logs.

| field | meaning |
| --- | --- |
| `data_packets_queued_total` | SRT DATA datagrams queued on the link: first sends, retransmits and duplicate probes. Forwarded SRT control traffic is not counted. Queued is not sent: a batch that fails to send still counted. |
| `srtla_ack_entries_total` | Sequence entries in SRTLA ACKs that arrived on the link's current socket while the sender had the link registered. Every entry counts, including a repeated ACK or a packet that arrived twice. |
| `last_srtla_ack_age_ms` | Milliseconds since the sender handled a non-empty SRTLA ACK for the link in the current epoch, measured on the sender's monotonic clock when the snapshot is taken (not a socket timestamp). Absent before the first one, never `null`. Keepalive replies do not reset it. |
| `srtla_ack_epoch` | Changes when the link's connection is created or its socket replaced. Local to the process: a restarted sender starts numbering again. |

These are observations of arrival reports, not goodput:

- The totals are monotonic for the life of one connection. A link removed by a reload and added again is a new connection: its totals start at 0 under a new epoch. A socket replacement keeps the totals, starts a new epoch and clears the age; datagrams the replaced socket had already queued are dropped, so they never count in the new epoch. Compare two snapshots only when their epochs match, and rebaseline after an epoch change or a sender restart. Within one snapshot the epoch, not a link's position, says whether it is still the same connection.
- The receiver holds back up to one partially filled batch per connection, and a link that goes quiet may never report it. Several complete ACKs can be in flight at once, and loss on the return path can remove more than one batch of reports from a sampling window: a lost SRTLA ACK loses its whole batch.
- A delayed duplicate the current socket receives still counts.
- Arrival at the receiver says nothing about delivery further downstream.
- A receiver that throttles its ACKs reports fewer entries than arrived.
- A link that carries only occasional duplicate probes (for example one held out by the stall gate) gets a report only every few hundred to thousand stream packets, so its age grows even while the path works. A keepalive echo is separate feedback about such a link; this age is not.

The same two totals are exported as Prometheus counters, `srtla_send_link_data_packets_queued_total` and `srtla_send_link_srtla_ack_entries_total`, under the existing `ip` label. They carry no epoch, so a scrape across a link's recreation may show a counter reset, or miss it if the new total has already caught up.

## Subscriptions

The Unix control socket supports server-push subscriptions. Polling `get_stats` at 1 Hz misses sub-second link-state changes (NAK bursts, quality drops, reconnects); subscriptions let clients receive push events on the same socket they already use for requests.

### `subscribe`

Params: `{ "topic": "stats" | "priority.window" }`. Result: `{ "subscription_id": string }`.

### `unsubscribe`

Params: `{ "subscription_id": string }`. Result: `{ "removed": bool }`.

### Push events

Server-originated notifications are sent on the same connection:

```json
{
  "jsonrpc": "2.0",
  "method": "stats.update",
  "params": {
    "subscription_id": "sub-0",
    "data": { /* StatsSnapshot */ }
  }
}
```

Topics currently implemented:

| Topic | Data | Cadence |
| --- | --- | --- |
| `stats` | Full `StatsSnapshot` (same shape as `get_stats`) | Once per second, aligned with housekeeping |
| `priority.window` | `{ at_ms, window_ms, deadline_ms }` | Once per keyframe window from the priority sidecar |

Subscriptions live for the life of the connection. Dropping the socket cancels every subscription it owns.

Each connection holds at most one subscription per topic. Subscribing again to a topic the connection already follows returns the same `subscription_id`. The process holds at most 64 subscriptions across all connections, and a subscribe past that limit fails with `-32000`. A connection can only unsubscribe its own ids. For any other id, `unsubscribe` returns `{ "removed": false }`.

Stdin is request-only — subscriptions only work on the Unix socket.

## Error codes

| code | meaning |
| --- | --- |
| `-32700` | parse error (invalid JSON) |
| `-32600` | invalid request (missing / wrong `jsonrpc` field) |
| `-32601` | method not found |
| `-32602` | invalid params |
| `-32603` | internal error |
| `-32000` | subscription limit reached |

## Examples

Using `socat` on the Unix socket:

```
$ echo '{"jsonrpc":"2.0","id":1,"method":"get_status"}' \
    | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/srtla.sock
{"jsonrpc":"2.0","result":{"mode":"enhanced",...},"id":1}
```

Switching mode at runtime:

```
$ echo '{"jsonrpc":"2.0","id":1,"method":"set_mode","params":{"mode":"classic"}}' \
    | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/srtla.sock
```
