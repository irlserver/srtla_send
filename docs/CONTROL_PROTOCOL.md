# srtla_send control protocol

srtla_send exposes runtime control over a Unix socket (preferred) or stdin. The wire format is JSON-RPC 2.0, one message per line. The socket path is set with `--control-socket <path>`.

## Request/response

One request per line, UTF-8 JSON. Responses end with a newline.

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

Requests without `id` are notifications. srtla_send processes them and sends no response. Used for the hot-path hint channel where waiting for an ACK is wasteful.

```json
{"jsonrpc": "2.0", "method": "mark_critical", "params": {"count": 23}}
```

## Methods

### `set_mode`

Switch the link scheduler.

| param | type | values |
| --- | --- | --- |
| `mode` | string | `"classic"`, `"enhanced"`, `"rtt-threshold"`, `"edpf"` |

Result: `{ "mode": "<current>" }`.

### `set_quality`

Toggle quality scoring (enhanced / rtt-threshold modes).

Params: `{ "enabled": bool }`. Result: `{ "enabled": bool }`.

### `set_exploration`

Toggle scheduler exploration (enhanced mode only).

Params: `{ "enabled": bool }`. Result: `{ "enabled": bool }`.

### `set_rtt_delta`

Set the RTT delta threshold in milliseconds. Links within `min_rtt + delta` are "fast".

Params: `{ "delta_ms": u32 }`. Result: `{ "delta_ms": u32 }`.

### `get_status`

Return the full runtime configuration plus keyframe-hint telemetry.

Result:

```json
{
  "mode": "enhanced",
  "quality_enabled": true,
  "exploration_enabled": false,
  "rtt_delta_ms": 30,
  "critical_hints_total": 142,
  "critical_hint_remaining": 0
}
```

### `get_stats`

Return per-link telemetry (the JSON previously returned by `stats`).

### `mark_critical`

Add `count` packets to the critical-hint budget. An upstream encoder that knows it is about to push an IDR / SPS / PPS burst calls this so the scheduler routes those packets to the highest-quality link. Complements the packet-size heuristic in `sender/keyframe.rs`; either signal triggers the override.

Params: `{ "count": u32 }`. Result (if called with an id): `{ "remaining": u32 }`.

Best called as a notification (no `id`) to avoid round-trip latency on the encoder's critical path.

## Reserved

`subscribe` and `unsubscribe` are reserved for a future push-based streaming protocol (stats deltas, hint-consumed events, link up/down). Calls currently return `-32601 method not found`.

## Error codes

| code | meaning |
| --- | --- |
| `-32700` | parse error (invalid JSON) |
| `-32600` | invalid request (missing / wrong `jsonrpc` field) |
| `-32601` | method not found |
| `-32602` | invalid params |
| `-32603` | internal error |

## Examples

Using `socat` on the Unix socket:

```
$ echo '{"jsonrpc":"2.0","id":1,"method":"get_status"}' \
    | socat - UNIX-CONNECT:/tmp/srtla.sock
{"jsonrpc":"2.0","result":{"mode":"enhanced",...},"id":1}
```

Firing a keyframe hint from a shell (fire-and-forget, no id):

```
$ echo '{"jsonrpc":"2.0","method":"mark_critical","params":{"count":23}}' \
    | socat - UNIX-CONNECT:/tmp/srtla.sock
```

Switching mode at runtime:

```
$ echo '{"jsonrpc":"2.0","id":1,"method":"set_mode","params":{"mode":"classic"}}' \
    | socat - UNIX-CONNECT:/tmp/srtla.sock
```
