#!/usr/bin/env bash
# CPU and memory cost of srtla_send on loopback, against a real srtla_rec and a
# real SRT session. No root and no network namespaces.
#
#   scripts/bench-loopback.sh <srtla_send binary> [mbps=10] [seconds=20] [links=3] [mode=enhanced]
#
# mbps=0 measures the idle cost: links registered, no encoder attached.
#
# Needs `srtla_rec` and an `ffmpeg` built with libsrt and libx264. The sender's
# environment passes through, so RUST_LOG and friends apply.
#
# Loopback has no loss, no reordering and no RTT. The numbers cover the
# forwarding and ACK paths only, never the NAK, probe or stall-gate paths, and
# they move with whatever else the machine is doing: compare runs taken back to
# back and repeat each one.
set -euo pipefail

if [ $# -lt 1 ]; then
    sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
fi

bin=$(realpath "$1")
mbps=${2:-10}
secs=${3:-20}
links=${4:-3}
mode=${5:-enhanced}

for tool in srtla_rec ffmpeg curl; do
    command -v "$tool" >/dev/null || { echo "missing: $tool" >&2; exit 1; }
done

srt_port=4901
srtla_port=5901
local_port=6901
metrics_port=9901
latency_us=200000

work=$(mktemp -d)
pids=()
cleanup() {
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
    wait 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

for i in $(seq 1 "$links"); do echo "127.0.0.$i"; done >"$work/ips"

# Filler NAL units pad the stream to a true constant bitrate, so the bytes the
# SRT listener reports are the bytes that crossed the bond.
if [ "$mbps" != 0 ]; then
    ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc2=size=1280x720:rate=30 -t 20 \
        -c:v libx264 -preset ultrafast -g 60 \
        -b:v "${mbps}M" -minrate "${mbps}M" -maxrate "${mbps}M" -bufsize "${mbps}M" \
        -x264-params nal-hrd=cbr -f mpegts "$work/src.ts"
fi

ffmpeg -hide_banner -loglevel error \
    -i "srt://127.0.0.1:$srt_port?mode=listener&latency=$latency_us" \
    -c copy -f mpegts -y /dev/null -progress "$work/progress" 2>/dev/null &
pids+=($!)
srtla_rec --srtla_port "$srtla_port" --srt_hostname 127.0.0.1 --srt_port "$srt_port" \
    --log_level error >/dev/null 2>&1 &
pids+=($!)
sleep 0.5

RUST_LOG=${RUST_LOG:-warn} "$bin" "$local_port" 127.0.0.1 "$srtla_port" "$work/ips" \
    --mode "$mode" --metrics-bind "127.0.0.1:$metrics_port" >"$work/sender.log" 2>&1 &
sender=$!
pids+=("$sender")
sleep 2

if [ "$mbps" != 0 ]; then
    ffmpeg -hide_banner -loglevel error -re -stream_loop -1 -i "$work/src.ts" -c copy -f mpegts \
        "srt://127.0.0.1:$local_port?pkt_size=1316&latency=$latency_us" &
    pids+=($!)
fi
sleep 5

# user ticks, system ticks, context switches summed over every thread
sample() {
    awk '{print $14, $15}' "/proc/$sender/stat"
    cat /proc/"$sender"/task/*/status | awk '/ctxt_switches/ {s += $2} END {print s}'
}
delivered_bytes() {
    awk -F= '/^total_size/ {v = $2} END {print v + 0}' "$work/progress" 2>/dev/null
}

read -r user0 sys0 switches0 < <(sample | paste -sd' ')
bytes0=$(delivered_bytes)
t0=$(date +%s.%N)
sleep "$secs"
read -r user1 sys1 switches1 < <(sample | paste -sd' ')
bytes1=$(delivered_bytes)
t1=$(date +%s.%N)

rss_kb=$(awk '/VmRSS/ {print $2}' "/proc/$sender/status")
threads=$(find "/proc/$sender/task" -mindepth 1 -maxdepth 1 | wc -l)
active=$(curl -s "127.0.0.1:$metrics_port/metrics" | awk '/^srtla_send_active_links/ {print $2}')
ticks_per_sec=$(getconf CLK_TCK)

awk -v user="$((user1 - user0))" -v sys="$((sys1 - sys0))" -v hz="$ticks_per_sec" \
    -v switches="$((switches1 - switches0))" -v bytes="$((bytes1 - bytes0))" \
    -v t0="$t0" -v t1="$t1" -v rss="$rss_kb" -v threads="$threads" \
    -v active="${active:-?}" -v links="$links" -v mode="$mode" 'BEGIN {
    dt = t1 - t0
    cpu = (user + sys) / hz
    pps = bytes / 1316 / dt
    printf "mode=%s links=%s/%s delivered=%.1f Mbps (%.0f pkt/s)\n", mode, active, links, bytes * 8 / dt / 1e6, pps
    printf "cpu=%.2f%% of one core (user %.2fs, system %.2fs over %.1fs)\n", 100 * cpu / dt, user / hz, sys / hz, dt
    if (pps > 0) printf "cpu per packet=%.1f us\n", 1e6 * cpu / (dt * pps)
    printf "context switches=%.0f/s rss=%d kB threads=%d\n", switches / dt, rss, threads
}'

if [ -s "$work/sender.log" ]; then
    echo "sender logged $(wc -l <"$work/sender.log") lines at ${RUST_LOG:-warn}:"
    head -5 "$work/sender.log"
fi
