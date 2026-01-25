# RTT-Threshold Scheduling

RTT-threshold scheduling is a connection selection mode that groups links by their round-trip time (RTT) to reduce packet reordering at the receiver.

## Problem

In heterogeneous networks where some links have significantly different latencies (e.g., 50ms cellular + 200ms satellite), capacity-based scheduling sends packets on both links. This causes packet reordering at the receiver because packets sent on the fast link arrive before packets sent earlier on the slow link.

## Solution

RTT-threshold scheduling:
1. Finds the minimum RTT among all eligible links
2. Marks links as "fast" if their RTT is within `min_rtt + delta`
3. Strongly prefers fast links, only using slow links when fast links are saturated
4. Applies quality scoring (NAK penalties) within the fast link group

## Algorithm

```
1. Find min_rtt among eligible links
2. threshold = min_rtt + rtt_delta_ms (default 30ms)
3. For each link:
   - If RTT <= threshold: mark as "fast"
   - If no RTT data: treat as "fast" (eligible)
4. Select best quality-adjusted capacity among fast links
5. If no fast links have capacity: fallback to any eligible link
6. Apply time-based dampening (500ms cooldown between switches)
```

## Configuration

### CLI Arguments

```bash
# Enable RTT-threshold mode with default delta (30ms)
srtla_send --rtt-threshold 6000 host 5000 ./ips.txt

# Enable with custom delta (50ms)
srtla_send --rtt-threshold --rtt-delta-ms 50 6000 host 5000 ./ips.txt
```

### Runtime Toggles

```bash
# Enable RTT-threshold mode
echo "rtt on" | socat - UNIX-CONNECT:/tmp/srtla.sock

# Disable RTT-threshold mode
echo "rtt off" | socat - UNIX-CONNECT:/tmp/srtla.sock

# Change RTT delta threshold
echo "rtt_delta=50" | socat - UNIX-CONNECT:/tmp/srtla.sock

# Check current status
echo "status" | socat - UNIX-CONNECT:/tmp/srtla.sock
```

## Parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `rtt_delta_ms` | 30 | Threshold above minimum RTT to be considered "fast" |

### Choosing RTT Delta

- **Lower delta (10-20ms)**: More aggressive, only very similar RTT links are "fast"
- **Default delta (30ms)**: Good balance for typical cellular networks
- **Higher delta (50-100ms)**: More inclusive, useful when link RTTs vary moderately

## Interaction with Other Modes

| Mode Combination | Behavior |
|------------------|----------|
| `--rtt-threshold` only | RTT grouping with quality scoring |
| `--rtt-threshold --no-quality` | RTT grouping, pure capacity within fast links |
| `--classic` | RTT-threshold disabled, classic mode takes priority |

## Use Cases

### Heterogeneous Networks
When combining links with very different latencies (cellular + satellite, WiFi + cellular):
```bash
srtla_send --rtt-threshold --rtt-delta-ms 30 6000 host 5000 ./ips.txt
```

### Reducing Reordering for Sensitive Applications
For applications that don't handle reordering well:
```bash
srtla_send --rtt-threshold --rtt-delta-ms 20 6000 host 5000 ./ips.txt
```

### Mixed Quality Links
When fast links may have quality issues, keep quality scoring enabled (default):
```bash
srtla_send --rtt-threshold 6000 host 5000 ./ips.txt
```

## Tradeoffs

| Advantage | Disadvantage |
|-----------|--------------|
| Reduced packet reordering | Lower aggregate throughput |
| More predictable latency | Slow links may be underutilized |
| Better for latency-sensitive apps | Fast links may saturate faster |

## Monitoring

With `RUST_LOG=debug`, you'll see:
- RTT threshold calculations
- Fast/slow link classifications
- Fallback to slow links when fast are saturated
- Time-based dampening decisions
