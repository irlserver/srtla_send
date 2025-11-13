# SRTLA Sender (Rust)

[![CI](https://github.com/irlserver/srtla_send/actions/workflows/ci.yml/badge.svg)](https://github.com/irlserver/srtla_send/actions/workflows/ci.yml)
[![Build Debian Packages](https://github.com/irlserver/srtla_send/actions/workflows/build-debian.yml/badge.svg)](https://github.com/irlserver/srtla_send/actions/workflows/build-debian.yml)

A Rust implementation of the SRTLA bonding sender. SRTLA is a SRT transport proxy with link aggregation for connection bonding that can transport [SRT](https://github.com/Haivision/srt/) traffic over multiple network links for capacity aggregation and redundancy. Traffic is balanced dynamically, depending on the network conditions. The intended application is bonding mobile modems for live streaming.

This application is experimental. Be prepared to troubleshoot it and experiment with various settings for your needs.

## Credits & Acknowledgments

This Rust implementation builds upon several open source projects and ideas:

- **[Bond Bunny](https://github.com/dimadesu/bond-bunny)** - Android SRTLA bonding app that inspired many of the enhanced connection selection algorithms
- **[Moblin](https://github.com/eerimoq/moblin)** and **[Moblink](https://github.com/eerimoq/moblink)** - Inspired by ideas and algorithms
- **[Original SRTLA](https://github.com/BELABOX/srtla)** - The foundational SRTLA protocol and reference implementation by Belabox

The burst NAK penalty logic, quality scoring, and connection exploration features were directly inspired by the Bond Bunny Android implementation.

## Features

### Core SRTLA Functionality
- Multi-uplink bonding using a list of local source IPs
- Registration flow (REG1/REG2/REG3) with ID propagation
- SRT ACK and NAK handling (with correct NAK attribution to sending uplink)
- Dynamic path selection with automatic load distribution across all connections
- Keepalives with RTT measurement and time-based window recovery
- Live IP list reload on Unix via SIGHUP
- Runtime toggles via stdin or Unix socket (no restart required)

### Enhanced Mode (Default)
- **Exponential NAK Decay**: Smooth recovery from packet loss over ~8 seconds
- **NAK Burst Detection**: Extra penalties for connections experiencing severe packet loss (≥5 NAKs)
- **RTT-Aware Selection**: Small bonus (3% max) for lower-latency connections
- **Quality Scoring**: Automatic preference for higher-quality connections
- **Minimal Hysteresis**: 2% threshold prevents flip-flopping while maintaining natural load distribution

### Optional Smart Exploration
- **Context-Aware Discovery**: Tests alternative connections when current best is degrading and alternatives have recovered
- **Periodic Fallback**: Every 30 seconds for 300ms as a safety net
- **Smart Switching**: Tries second-best connections instead of always sticking to current best
- **Enable via**: `--exploration` flag or runtime toggle (`explore on`)
- **Use Case**: More aggressive connection testing in unstable network conditions

### Classic Mode
- Exact match to original `srtla_send.c` implementation
- Pure capacity-based selection without quality awareness
- Can be enabled via `--classic` flag or runtime toggle

## Assumptions and Prerequisites

This tool assumes that data is streamed from a SRT _sender_ in _caller_ mode to a SRT _receiver_ in _listener_ mode. To get any benefit over using SRT directly, the _sender_ should have 2 or more network links to the SRT listener (in the typical application, these would be internet-connected 4G modems). The sender needs to have [source routing](https://tldp.org/HOWTO/Adv-Routing-HOWTO/lartc.rpdb.simple.html) configured, as srtla uses `bind()` to map UDP sockets to a given connection.

## Requirements

- **Rust nightly toolchain** and Cargo
- Unix (Linux/macOS) or Windows
  - Note: SIGHUP-based IP reload is Unix-only; Windows runs without that arm

**Important:** This project requires Rust nightly due to advanced rustfmt configuration options used in the codebase.

## Build

```bash
cd srtla_send
rustup install nightly
rustup default nightly  # Set nightly as default for this project
cargo build --release
# binary at target/release/srtla_send
```

Alternatively, you can use nightly for individual commands:
```bash
cargo +nightly build --release
cargo +nightly fmt
cargo +nightly test
```

## Testing

The project includes comprehensive test suites covering unit tests, integration tests, and end-to-end tests.

### Run Tests Locally

```bash
# Run all tests (requires nightly)
cargo test

# Run with verbose output
cargo test --verbose

# Run specific test
cargo test test_connection_score

# Check formatting (requires nightly)
cargo fmt --all -- --check
```

### CI/CD

The project uses GitHub Actions for continuous integration with automated testing on every push and pull request, including:

- Multi-platform testing (Linux, Windows, macOS)
- Code formatting and linting checks
- Security vulnerability scanning
- Build verification across multiple Rust versions

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
# Start with quality scoring disabled
./target/release/srtla_send --no-quality 6000 rec.example.com 5000 ./uplinks.txt
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
- `quality on|off` - Enable/disable quality scoring
- `explore on|off` - Enable/disable connection exploration

**Alternative format:**

- `classic=true|false`
- `quality=true|false`
- `exploration=true|false`

**Status command:**

- `status` - Display current state of all toggles

### Connection Selection Algorithm Details

These toggles affect how the system selects the best connection for sending data:

**Classic SRTLA Algorithm** (`classic`): Matches the original srtla_send logic without any enhancements.

**Quality-Based Scoring** (`quality`): Punishes connections with recent NAKs. More recent NAKs = more punishment. **Additional 30% penalty (0.7x multiplier) for NAK bursts** (≥5 NAKs in short time).

**Connection Exploration** (`explore`): Optional smart context-aware exploration that tests alternative connections when the current best is degrading and alternatives have recovered. Periodic fallback exploration every 30 seconds. **Disabled by default** and **completely disabled in classic mode** - enable with `--exploration` flag or runtime toggle.

These affect selection behavior in real time. By default, enhanced mode with quality-based scoring is enabled.

## IP List Reload (Unix only)

Send SIGHUP to trigger an IP list reload without restarting:

```bash
kill -HUP <pid_of_srtla_send>
```

On Windows this arm is disabled; restart the process after editing the IP list.

## How It Works

The core idea is that srtla keeps track of the number of packets in flight (sent but unacknowledged) for each link, together with a dynamic window size that tracks the capacity of each link - similarly to TCP congestion control. These are used together to balance the traffic through each link proportionally to its capacity. However, note that no congestion control is applied.

### srtla v2 Improvements

The main improvement in srtla v2 is that it supports multiple _srtla senders_ connecting to a single _srtla receiver_ by establishing _connection groups_. To support this feature, a 2-phase connection registration process is used:

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
- Incoming SRT UDP packets are read on `SRT_LISTEN_PORT` and forwarded over the currently selected uplink based on the score `window / (in_flight + 1)`.
- ACKs are applied to all uplinks to reduce in-flight counts; NAKs are attributed to the uplink that originally sent the sequence (tracked), falling back to the receiver uplink if unknown.
- **Burst NAK Detection**: The system tracks NAK bursts (multiple NAKs within 1 second) per connection. When quality scoring is enabled, connections with recent NAK bursts (≥5 NAKs in burst, within last 3 seconds) receive an additional 0.7x penalty multiplier to their quality score, helping avoid connections experiencing packet loss issues.
- Keepalives are sent when idle, and periodically for RTT measurement; the RTT is smoothed. Window recovery is conservative and time-based when there are no recent NAKs.

## Notes

- Ensure your system has the specified local source IPs configured and routable.
- The local SRT producer (e.g., `srt-live-transmit`) should send to `udp://127.0.0.1:SRT_LISTEN_PORT`.
- The SRTLA receiver must understand the SRTLA protocol (REG1/2/3, ACK, NAK, KEEPALIVE).
- The sender **should** implement congestion control using adaptive bitrate based on the SRT `SRTO_SNDDATA` size or on the measured `RTT`. Due to reordering, these values may be slightly higher during uncongested operation over srtla compared to direct SRT operation over one of the same network links.

## License

This Rust implementation is licensed under the MIT License. See the [LICENSE](./LICENSE) file for full details.

## Expected Behavior

### Load Distribution

With properly configured connections, you should observe:

**All connections active**: Traffic should appear on all uplinks (e.g., if you have 4 uplinks, all 4 should show active bitrate)

**Proportional distribution**: 
- With equal connections: roughly equal traffic distribution (e.g., 25% each with 4 uplinks)
- With varying quality (enhanced mode): better connections get more traffic, degraded connections get less
- With varying capacity: connections with larger windows get proportionally more traffic

**Dynamic adaptation (enhanced mode)**: 
- Connections experiencing NAKs automatically receive less traffic
- Connections recover to full capacity within ~8 seconds after issues resolve
- System continuously rebalances based on current conditions

### Monitoring

**Status logs** (every 30 seconds) show:
- Total bitrate across all connections
- Individual connection status (active/timed out)
- Window sizes and in-flight packet counts
- RTT measurements and connection quality metrics
- Current toggle states (classic/enhanced mode)

**Debug logs** (when `RUST_LOG=debug`) show:
- Per-packet connection selection decisions
- Quality multiplier calculations
- NAK burst detections and recovery
- Exploration attempts
- Hysteresis decisions

### Troubleshooting

**If only some connections are used**:
1. Check for NAKs in logs - degraded connections naturally get less traffic in enhanced mode
2. Try classic mode: `echo "classic on"` - disables quality awareness for pure capacity-based distribution
3. Temporarily disable quality scoring: `echo "quality off"`
4. Verify all uplinks can reach the receiver (check for timeout messages)
5. Check RTT differences - high-RTT connections get slightly less traffic in enhanced mode (3% max difference)

**If throughput is lower than expected**:
1. Verify SRT is not limiting the bitrate (check encoder settings)
2. Check for high packet loss (NAKs) on connections - indicates network issues
3. Ensure sender has sufficient CPU and network capacity
4. Monitor SRT `SRTO_SNDDATA` buffer - if full, increase bitrate or improve connections
5. Check connection windows in status logs - low windows indicate capacity limits

**If connections are flip-flopping**:
1. This should be minimal with 2% hysteresis in enhanced mode
2. Check if scores are truly identical (look for hysteresis messages in debug logs)
3. Verify connections have stable quality (no intermittent NAKs)
4. Consider using classic mode for perfectly equal connections

## Performance Tuning

### Constants (Advanced)

If needed, these can be adjusted in `src/sender/selection/`:

**Enhanced Mode (`enhanced.rs`):**
- `SWITCH_THRESHOLD`: 1.02 (2% hysteresis) - increase for more stability, decrease for faster response

**Quality Scoring (`quality.rs`):**
- `STARTUP_GRACE_PERIOD_MS`: 30000ms (30 seconds) - grace period before quality penalties apply
- `PERFECT_CONNECTION_BONUS`: 1.1 (10% bonus) - bonus for connections with no NAKs
- `STARTUP_NAK_PENALTY`: 0.98 (2% penalty) - light penalty during grace period
- `HALF_LIFE_MS`: 2000ms (2 seconds) - NAK penalty decay speed
- `MAX_PENALTY`: 0.5 (50% penalty) - maximum initial penalty after NAK
- `NAK_BURST_THRESHOLD`: 5 NAKs - minimum burst size to trigger extra penalty
- `NAK_BURST_MAX_AGE_MS`: 3000ms (3 seconds) - max age for burst penalty
- `NAK_BURST_PENALTY`: 0.7 (30% extra penalty) - additional multiplier for bursts
- `RTT_BONUS_THRESHOLD_MS`: 200ms - RTT threshold for bonus calculation
- `MIN_RTT_MS`: 50ms - minimum RTT for calculation (prevents division issues)
- `MAX_RTT_BONUS`: 1.03 (3% max bonus) - maximum RTT bonus multiplier

**Exploration (`enhanced.rs`):**
- Exploration period: `should_explore_now()` function, currently 30s - adjust exploration interval

### Runtime Optimization

For maximum throughput:
- Use enhanced mode (default) to automatically avoid degraded connections
- Ensure adequate SRT buffer size (`SRTO_SNDDATA`)
- Monitor for connection timeouts - these interrupt traffic flow
- Use `RUST_LOG=info` for minimal logging overhead (avoid debug in production)

For maximum stability:
- Use classic mode (`--classic`) for predictable, simple behavior
- Disable exploration (`exploration off`) if not needed
- Increase hysteresis threshold if experiencing unnecessary switching

