#!/usr/bin/env bash
# What one process costs over a stretch of time, on Linux: its processor
# time as a share of one core, its resident memory and thread count at
# their highest, and what it wrote to disk, sampled every five seconds —
# the same line measure.ps1 gives on Windows.
#
#   bench/measure.sh <process-name> [seconds]
set -euo pipefail

name=$1 seconds=${2:-30}
pid=$(pgrep -x "$name" | head -1)
ticks=$(getconf CLK_TCK)

cpu() { awk '{ print $14 + $15 }' "/proc/$pid/stat"; }
written() { awk '/^write_bytes/ { print $2 }' "/proc/$pid/io" 2>/dev/null || echo 0; }

cpu0=$(cpu) write0=$(written) t0=$(date +%s.%N)
memory=0 threads=0
for _ in $(seq $(((seconds + 4) / 5))); do
    sleep 5
    rss=$(awk '/^VmRSS/ { print $2 }' "/proc/$pid/status")
    count=$(awk '/^Threads/ { print $2 }' "/proc/$pid/status")
    ((rss > memory)) && memory=$rss
    ((count > threads)) && threads=$count
done
cpu1=$(cpu) write1=$(written) t1=$(date +%s.%N)
awk -v c0="$cpu0" -v c1="$cpu1" -v w0="$write0" -v w1="$write1" -v t0="$t0" -v t1="$t1" \
    -v hz="$ticks" -v mem="$memory" -v thr="$threads" 'BEGIN {
    elapsed = t1 - t0
    printf "cpu_pct=%.1f private_mb=%d threads=%d write_mbps=%.1f\n",
        100 * (c1 - c0) / hz / elapsed, mem / 1024, thr, (w1 - w0) * 8 / 1e6 / elapsed
}'
