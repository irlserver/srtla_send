# SRTLA Sender (Rust)

A Rust implementation of the SRTLA bonding sender. SRTLA is a SRT transport proxy with link aggregation for connection bonding that can transport [SRT](https://github.com/Haivision/srt/) traffic over multiple network links for capacity aggregation and redundancy. Traffic is balanced dynamically, depending on the network conditions. The intended application is bonding mobile modems for live streaming.

This application is experimental. Be prepared to troubleshoot it and experiment with various settings for your needs.

## Credits & Acknowledgments

This Rust implementation builds upon several open source projects and ideas:

- **[Bond Bunny](https://github.com/dimadesu/bond-bunny)** - Android SRTLA bonding app that inspired many of the enhanced connection selection algorithms
- **[Moblin](https://github.com/eerimoq/moblin)** and **[Moblink](https://github.com/eerimoq/moblink)** - Inspired by ideas and algorithms
- **[Original SRTLA](https://github.com/BELABOX/srtla)** - The foundational SRTLA protocol and reference implementation by Belabox

The burst NAK penalty logic, quality scoring, and connection exploration features were directly inspired by the Bond Bunny Android implementation.

## Features

- Multi-uplink bonding using a list of local source IPs
- Registration flow (REG1/REG2/REG3) with ID propagation
- SRT ACK and NAK handling (with correct NAK attribution to sending uplink)
- Burst NAK penalty: Extra quality penalty for connections experiencing packet loss bursts
- Keepalives with RTT measurement and time-based window recovery
- Stickiness-aware path selection: score = window / (in_flight + 1)
- Live IP list reload on Unix via SIGHUP
- Runtime toggles via stdin (no restart required)

## Assumptions and Prerequisites

This tool assumes that data is streamed from a SRT *sender* in *caller* mode to a SRT *receiver* in *listener* mode. To get any benefit over using SRT directly, the *sender* should have 2 or more network links to the SRT listener (in the typical application, these would be internet-connected 4G modems). The sender needs to have [source routing](https://tldp.org/HOWTO/Adv-Routing-HOWTO/lartc.rpdb.simple.html) configured, as srtla uses `bind()` to map UDP sockets to a given connection.

## Requirements

- Rust and Cargo (stable)
- Unix (Linux/macOS) or Windows
  - Note: SIGHUP-based IP reload is Unix-only; Windows runs without that arm

## Build

```bash
cd srtla_send
cargo build --release
# binary at target/release/srtla_send
```

## Usage

```bash
srtla_send [OPTIONS] SRT_LISTEN_PORT SRTLA_HOST SRTLA_PORT BIND_IPS_FILE
```

### Required Arguments

- `SRT_LISTEN_PORT`: UDP port on which to receive SRT packets locally
- `SRTLA_HOST`: hostname or IP of the SRTLA receiver (e.g., srtla_rec)
- `SRTLA_PORT`: UDP port of the SRTLA receiver
- `BIND_IPS_FILE`: path to a file with newline-separated local source IPs (uplinks)

### Options

- `--control-socket <PATH>`: Unix domain socket path for remote toggle control (e.g., `/tmp/srtla.sock`)
- `--classic`: Enable classic mode (disables all enhancements)
- `--no-stickiness`: Disable connection stickiness
- `--no-quality`: Disable quality scoring
- `--exploration`: Enable connection exploration
- `-v, --version`: Print version and exit

## Example Usage

Let's assume that the receiver has IP address 10.0.0.1 and the sender has 2 (unreliable) modems with IP addresses 192.168.0.2 and 192.168.1.2 respectively, which can reach the receiver. We'll set up the srtla sender to forward SRT traffic from port 6000 to the receiver's srtla service on port 5000.

### Sender Setup

```bash
echo 192.168.0.2 > /tmp/srtla_ips
echo 192.168.1.2 >> /tmp/srtla_ips
./target/release/srtla_send 6000 10.0.0.1 5000 /tmp/srtla_ips
```

With `srtla_send` running on the sender, SRT-enabled applications should stream to port `6000` on the sender and this data will be forwarded through srtla to the receiver.

### Additional Examples

**With logging and Unix socket control:**

```bash
# Enable info logs and Unix socket control
RUST_LOG=info ./target/release/srtla_send --control-socket /tmp/srtla.sock 6000 rec.example.com 5000 ./uplinks.txt
```

**With initial toggle states:**

```bash
# Start with classic mode disabled
./target/release/srtla_send --no-stickiness 6000 rec.example.com 5000 ./uplinks.txt
```

**Basic example with logging:**

```bash
# Show info logs
RUST_LOG=info ./target/release/srtla_send 6000 rec.example.com 5000 ./uplinks.txt
```

Sample `uplinks.txt`:

```text
192.0.2.10
198.51.100.23
203.0.113.5
```

## Logging

This tool uses `tracing` with `EnvFilter`.

- Control verbosity with `RUST_LOG` (e.g., `RUST_LOG=info`, `RUST_LOG=debug`).
- Example:

```bash
RUST_LOG=info,hyper=off ./target/release/srtla_send 6000 host 5000 ./uplinks.txt
```

## Runtime Toggles

The sender supports dynamic runtime configuration changes through two methods:

### Method 1: Standard Input (stdin)

Type commands directly into the running process and press Enter:

### Method 2: Unix Domain Socket (Unix only)

Use the `--control-socket` option to enable remote control via Unix socket:

```bash
# Start with Unix socket control
./target/release/srtla_send --control-socket /tmp/srtla.sock 6000 10.0.0.1 5000 /tmp/srtla_ips

# Send commands remotely
echo 'classic on' | socat - UNIX-CONNECT:/tmp/srtla.sock
echo 'status' | socat - UNIX-CONNECT:/tmp/srtla.sock
```

### Available Commands

Both methods support the same commands in two formats:

**Traditional format:**

- `classic on|off` - Enable/disable classic mode (disables all enhancements)
- `stick on|off` - Enable/disable connection stickiness
- `quality on|off` - Enable/disable quality scoring
- `explore on|off` - Enable/disable connection exploration

**Alternative format:**

- `classic=true|false`
- `stickiness=true|false`
- `quality=true|false`
- `exploration=true|false`

**Status command:**

- `status` - Display current state of all toggles

### Connection Selection Algorithm Details

These toggles affect how the system selects the best connection for sending data:

**Classic SRTLA Algorithm** (`classic`): Matches the original srtla_send logic without any enhancements.

**Quality-Based Scoring** (`quality`): Punishes connections with recent NAKs. More recent NAKs = more punishment. **Double punishment for NAK bursts** (multiple NAKs in short time).

**Connection Exploration** (`explore`): 10% of the time picks the 2nd best network. Every ~5 seconds explores for ~500ms to discover better alternatives.

**Connection Stickiness** (`stick`): Prevents frequent network switches. Uses selected network for at least 500ms to maintain stability.

These affect selection behavior in real time. By default, stickiness is enabled.

## IP List Reload (Unix only)

Send SIGHUP to trigger an IP list reload without restarting:

```bash
kill -HUP <pid_of_srtla_send>
```

On Windows this arm is disabled; restart the process after editing the IP list.

## How It Works

The core idea is that srtla keeps track of the number of packets in flight (sent but unacknowledged) for each link, together with a dynamic window size that tracks the capacity of each link - similarly to TCP congestion control. These are used together to balance the traffic through each link proportionally to its capacity. However, note that no congestion control is applied.

### srtla v2 Improvements

The main improvement in srtla v2 is that it supports multiple *srtla senders* connecting to a single *srtla receiver* by establishing *connection groups*. To support this feature, a 2-phase connection registration process is used:

Normal registration:

- Sender (conn 0): `SRTLA_REG1(sender_id = SRTLA_ID_LEN bytes sender-generated random id)`
- Receiver: `SRTLA_REG2(full_id = sender_id with the last SRTLA_ID_LEN/2 bytes replaced with receiver-generated values)`
- Sender (conn 0): `SRTLA_REG2(full_id)`
- Receiver: `SRTLA_REG3`
- [...]
- Sender (conn n): `SRTLA_REG2(full_id)`
- Receiver: `SRTLA_REG3`

### Implementation Details

- For each IP in `BIND_IPS_FILE`, the sender binds a UDP socket and connects to `SRTLA_HOST:SRTLA_PORT`.
- Incoming SRT UDP packets are read on `SRT_LISTEN_PORT` and forwarded over the currently selected uplink based on the score `window / (in_flight + 1)`, with a minimum switch interval for stickiness.
- ACKs are applied to all uplinks to reduce in-flight counts; NAKs are attributed to the uplink that originally sent the sequence (tracked), falling back to the receiver uplink if unknown.
- **Burst NAK Detection**: The system tracks NAK bursts (multiple NAKs within 1 second) per connection. When quality scoring is enabled, connections with recent NAK bursts (>1 NAK in burst, within last 5 seconds) receive an additional 0.5x penalty multiplier to their quality score, helping avoid connections experiencing packet loss issues.
- Keepalives are sent when idle, and periodically for RTT measurement; the RTT is smoothed. Window recovery is conservative and time-based when there are no recent NAKs.

## Notes

- Ensure your system has the specified local source IPs configured and routable.
- The local SRT producer (e.g., `srt-live-transmit`) should send to `udp://127.0.0.1:SRT_LISTEN_PORT`.
- The SRTLA receiver must understand the SRTLA protocol (REG1/2/3, ACK, NAK, KEEPALIVE).
- The sender **should** implement congestion control using adaptive bitrate based on the SRT `SRTO_SNDDATA` size or on the measured `RTT`. Due to reordering, these values may be slightly higher during uncongested operation over srtla compared to direct SRT operation over one of the same network links.

## License

This Rust implementation is licensed under the MIT License. See the [LICENSE](./LICENSE) file for full details.
