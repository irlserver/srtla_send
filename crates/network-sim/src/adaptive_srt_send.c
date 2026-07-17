// Adaptive-bitrate SRT sender for the netns harness.
//
// This is belacoder's congestion response with the encoder removed: an
// SRT caller that generates dummy payload and lowers its send rate when
// the SRT send buffer backs up, exactly the closed loop a real BELABOX
// deployment relies on. Without it the harness can only pump a constant
// rate, which oversubscribes a busy bond and drives SRT into a
// retransmit-fuelled congestion collapse (see the notes in
// netns_wire_loss.rs). With it, the offered rate tracks what the bonded
// path can actually carry, so the run stays in a stable regime and
// per-link goodput becomes a meaningful thing to assert.
//
// Deliberately dependency-light: libsrt only (already required by the
// whole srtla stack), no GStreamer, no patched encoder. Compiled on
// demand by the harness, never as part of the Rust build.
//
// Usage: adaptive_srt_send HOST PORT [MIN_KBPS] [MAX_KBPS] [LATENCY_MS]

#include <arpa/inet.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <srt/srt.h>

// SRT live-mode payload. Seven MPEG-TS packets, the libsrt default.
#define PKT 1316

// Adaptation cadence.
#define CONTROL_INTERVAL_NS 200000000L // 200 ms

// Send-buffer occupancy (packets) that we treat as "the path cannot keep
// up": above the high mark we back off, below the low mark we ramp up,
// between them we hold. Keeping the buffer shallow is the whole point —
// a deep SRT send buffer is latency that turns into retransmits.
#define SNDBUF_HIGH 40
#define SNDBUF_LOW 8

// RTT-based congestion, the earlier signal. The send buffer only grows a
// full round-trip after the bottleneck queue starts filling, so keying
// off it alone means always reacting a step late and overshooting into a
// standing queue. Watching RTT climb above its running minimum catches
// the bufferbloat as it forms. Threshold: 1.5x the baseline plus a fixed
// margin so ordinary jitter on a low-RTT path does not trip it.
#define RTT_INFLATION_FACTOR 1.5
#define RTT_INFLATION_MARGIN_MS 30.0

static uint64_t now_ns(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (uint64_t)ts.tv_sec * 1000000000ull + ts.tv_nsec;
}

int main(int argc, char **argv) {
  if (argc < 3) {
    fprintf(stderr, "usage: %s HOST PORT [MIN_KBPS] [MAX_KBPS] [LATENCY_MS]\n",
            argv[0]);
    return 2;
  }
  const char *host = argv[1];
  int port = atoi(argv[2]);
  int min_kbps = argc > 3 ? atoi(argv[3]) : 500;
  int max_kbps = argc > 4 ? atoi(argv[4]) : 12000;
  int latency_ms = argc > 5 ? atoi(argv[5]) : 2000;

  int64_t cur_bps = (int64_t)min_kbps * 1000;
  const int64_t min_bps = (int64_t)min_kbps * 1000;
  const int64_t max_bps = (int64_t)max_kbps * 1000;

  if (srt_startup() != 0) {
    fprintf(stderr, "srt_startup failed: %s\n", srt_getlasterror_str());
    return 1;
  }

  SRTSOCKET s = srt_create_socket();
  if (s == SRT_INVALID_SOCK) {
    fprintf(stderr, "srt_create_socket failed: %s\n", srt_getlasterror_str());
    return 1;
  }

  int live = SRTT_LIVE;
  srt_setsockflag(s, SRTO_TRANSTYPE, &live, sizeof(live));
  srt_setsockflag(s, SRTO_LATENCY, &latency_ms, sizeof(latency_ms));
  int payload = PKT;
  srt_setsockflag(s, SRTO_PAYLOADSIZE, &payload, sizeof(payload));
  // Non-blocking send: a full buffer is itself the congestion signal, and
  // we would rather drop and keep adapting than stall the control loop.
  int no = 0;
  srt_setsockflag(s, SRTO_SNDSYN, &no, sizeof(no));

  struct sockaddr_in sa;
  memset(&sa, 0, sizeof(sa));
  sa.sin_family = AF_INET;
  sa.sin_port = htons((uint16_t)port);
  if (inet_pton(AF_INET, host, &sa.sin_addr) != 1) {
    fprintf(stderr, "bad host %s\n", host);
    return 1;
  }

  if (srt_connect(s, (struct sockaddr *)&sa, sizeof(sa)) == SRT_ERROR) {
    fprintf(stderr, "srt_connect failed: %s\n", srt_getlasterror_str());
    return 1;
  }
  fprintf(stderr, "adaptive sender connected to %s:%d (%d-%d kbps)\n", host,
          port, min_kbps, max_kbps);

  char buf[PKT];
  memset(buf, 0xb8, sizeof(buf)); // 0xb8 marks each byte, harmless payload

  uint64_t start = now_ns();
  uint64_t next_control = start + CONTROL_INTERVAL_NS;
  uint64_t sent_pkts = 0;
  int blocked_since_control = 0;
  double rtt_min = 0.0;

  for (;;) {
    uint64_t t = now_ns();

    // Pace: hold the average send rate at cur_bps by gating on how many
    // packets we should have sent by now.
    uint64_t bytes_target = (uint64_t)((double)cur_bps / 8.0 *
                                       ((double)(t - start) / 1e9));
    uint64_t pkts_target = bytes_target / PKT;

    if (sent_pkts < pkts_target) {
      int n = srt_send(s, buf, PKT);
      if (n == PKT) {
        sent_pkts++;
      } else {
        // Buffer full (EASYNCSND) or a real error: treat as congestion.
        int err = srt_getlasterror(NULL);
        if (err == SRT_EASYNCSND) {
          blocked_since_control++;
        } else if (err == SRT_ECONNLOST || err == SRT_ECONNREJ ||
                   err == SRT_ENOCONN) {
          fprintf(stderr, "srt send: connection gone: %s\n",
                  srt_getlasterror_str());
          break;
        }
        // Small pause so we do not spin on a full buffer.
        struct timespec ns = {0, 1000000L}; // 1 ms
        nanosleep(&ns, NULL);
      }
    } else {
      struct timespec ns = {0, 200000L}; // 0.2 ms: caught up, idle briefly
      nanosleep(&ns, NULL);
    }

    if (t < next_control)
      continue;
    next_control += CONTROL_INTERVAL_NS;

    SRT_TRACEBSTATS st;
    if (srt_bstats(s, &st, 1) == 0) {
      // Track the RTT baseline. Slow upward creep lets it follow a genuine
      // path change (a handover raising the floor) instead of pinning to
      // one early low sample and reading every later RTT as congestion.
      if (st.msRTT > 0.0) {
        if (rtt_min == 0.0 || st.msRTT < rtt_min)
          rtt_min = st.msRTT;
        else
          rtt_min += (st.msRTT - rtt_min) * 0.02;
      }
      double rtt_ceiling = rtt_min * RTT_INFLATION_FACTOR + RTT_INFLATION_MARGIN_MS;
      int bufferbloat = st.msRTT > rtt_ceiling;

      // pktSndBuf: packets sitting in the send buffer (offered minus
      // drained). Any of a deep buffer, a blocked send, or an inflated RTT
      // means we are pushing more than the bonded path drains.
      int overdriving =
          st.pktSndBuf > SNDBUF_HIGH || blocked_since_control > 0 || bufferbloat;
      int has_room = st.pktSndBuf < SNDBUF_LOW && blocked_since_control == 0 &&
                     !bufferbloat;
      if (overdriving) {
        cur_bps = (int64_t)((double)cur_bps * 0.85); // -15%
        if (cur_bps < min_bps)
          cur_bps = min_bps;
      } else if (has_room) {
        cur_bps += cur_bps / 33 + 50000; // +3% and a floor step
        if (cur_bps > max_bps)
          cur_bps = max_bps;
      }
      fprintf(stderr,
              "ctl: bitrate=%lld kbps sndbuf=%d rtt=%.0f rttmin=%.0f bloat=%d "
              "blocked=%d\n",
              (long long)(cur_bps / 1000), st.pktSndBuf, st.msRTT, rtt_min,
              bufferbloat, blocked_since_control);
    }
    blocked_since_control = 0;
  }

  srt_close(s);
  srt_cleanup();
  return 0;
}
