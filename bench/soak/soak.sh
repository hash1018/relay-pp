#!/usr/bin/env bash
# relay-pp soak of every input and every way out, on the loopback, on Linux.
#
# In: 50 channels pulling one looping camera over RTSP, two pulling an HEVC
# camera (when HEVC is given), and 4 channels each taking a publisher over
# RTSP, RTMP and WHIP. Publisher 1 of each kind leaves and comes back every
# LEAVE seconds; the others publish throughout.
# Out: RTSP and WebRTC players held for the whole run on one channel of
# each kind, RTSP, WebRTC and HLS players coming and going each round on
# every kind, and recordings of 10 pulled channels and of the leaving
# publishers' channels. One pulled channel is removed and made again each
# round. A WebRTC player of an HEVC channel is converted to H.264 — on the
# GPU, where there is one.
#
# A sampler writes the server's memory, descriptors, threads, processor and
# the GPU's use to samples.csv once a minute; the end prints a summary.
# See README.md.
#
#   STREAM=/path/stream.mp4 [HEVC=/path/stream-hevc.mp4] bench/soak/soak.sh <seconds> [round seconds]
set -u
SECS=${1:-10800}
ROUND=${2:-300}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
MEDIA_PP=${MEDIA_PP:-$ROOT/../media-pp}
TOOLS=${TOOLS:-$MEDIA_PP/target/release}
RELAY=${RELAY:-$ROOT/target/release/relay-pp}
PUB=${PUB:-$ROOT/bench/soak/soakpub/target/release/soakpub}
STREAM=${STREAM:?set STREAM to an .mp4 with H.264 pictures — see README.md}
HEVC=${HEVC:-}
OUT=${OUT:-$ROOT/bench/soak/runs}
RUN=${RUN:-run-$(date +%Y%m%d-%H%M)}
HLS=${HLS:-1}
LEAVE=${LEAVE:-10}
W=$OUT/$RUN
API=http://127.0.0.1:18688
CAMERA=rtsp://127.0.0.1:18554/file
HEVC_CAMERA=rtsp://127.0.0.1:18555/file
PULLS=50
NO_HEVC=0; [ -z "$HEVC" ] && NO_HEVC=1

for tool in "$TOOLS/rtsp_server" "$TOOLS/rtsp_load" "$TOOLS/webrtc_load" "$RELAY" "$PUB"; do
    [ -x "$tool" ] || { echo "missing $tool: run bench/soak/build.sh" >&2; exit 1; }
done

stop_all() {
    for name in relay-pp rtsp_server rtsp_load webrtc_load soakpub; do
        pkill -x "$name" >/dev/null 2>&1
    done
}

stop_all
rm -rf "$W" && mkdir -p "$W/pubs" "$W/players"
cd "$W" || exit 1
cat > relay-pp.toml <<EOF
[rtsp]
listen = "127.0.0.1:18654"
[webrtc]
listen = "127.0.0.1:18655"
[http]
listen = "127.0.0.1:18688"
[rtmp]
listen = "127.0.0.1:18635"
[record]
dir = "recordings"
segment = "4s"
chunk = "2m"
max_size = "15GB"
[hls]
idle = "30s"
EOF

log() { echo "$(date -u +%T) $*" | tee -a soak.log; }

"$TOOLS/rtsp_server" "$STREAM" 127.0.0.1:18554 --seconds 100000 > camera.log 2>&1 &
[ "$NO_HEVC" = 1 ] || "$TOOLS/rtsp_server" "$HEVC" 127.0.0.1:18555 --seconds 100000 > camera-hevc.log 2>&1 &
sleep 2
RUST_LOG=info "$RELAY" relay-pp.toml > relay.log 2>&1 &
sleep 3

make() {
    curl -s -o /dev/null -w "%{http_code}" -X POST "$API/api/v1/channels" \
        -H 'Content-Type: application/json' -d "$2" 2>/dev/null
    # $1 is the channel's name, for the log's sake.
}
pull() { make "cam$1" "{\"name\":\"cam$1\",\"source\":\"$CAMERA\"}"; }
record() { curl -s -o /dev/null -X POST "$API/api/v1/channels/$1/record"; }
live() { curl -s "$API/api/v1/channels" | grep -o '"state":"live"' | wc -l; }

for i in $(seq $PULLS); do pull "$i" > /dev/null; done
[ "$NO_HEVC" = 1 ] || for i in 1 2; do make "hevc$i" "{\"name\":\"hevc$i\",\"source\":\"$HEVC_CAMERA\"}" > /dev/null; done
for kind in rtsp rtmp whip; do
    for i in 1 2 3 4; do
        make "pub-$kind$i" "{\"name\":\"pub-$kind$i\",\"publish\":\"$kind\"}" > /dev/null
    done
done

# A publisher, kept publishing until the run ends: `seconds` a connection,
# and two seconds before the next. RTMP carries the file's pictures alone:
# FLV has no Opus.
publisher() {
    local kind=$1 i=$2 seconds=$3 n=0
    (
        cd "$W/pubs" || exit 1
        while [ ! -f "$W/done" ]; do
            n=$((n + 1))
            case $kind in
                rtsp) "$PUB" "$STREAM" "rtsp://127.0.0.1:18654/pub-rtsp$i" "$seconds" ;;
                whip) "$PUB" "$STREAM" "$API/pub-whip$i/whip" "$seconds" ;;
                rtmp) "$PUB" "$STREAM" "rtmp://127.0.0.1:18635/live/pub-rtmp$i" "$seconds" ;;
            esac >> "pub-$kind$i.log" 2>&1
            echo "$(date -u +%T) run $n ended" >> "pub-$kind$i.log"
            sleep 2
        done
    ) &
}
for kind in rtsp rtmp whip; do
    publisher "$kind" 1 "$LEAVE"
    for i in 2 3 4; do publisher "$kind" "$i" $((SECS + 600)); done
done
sleep 25

for i in $(seq 10); do record "cam$i"; done
for kind in rtsp rtmp whip; do record "pub-${kind}1"; done
log "started: $(live) of $(( PULLS + 12 + (NO_HEVC == 1 ? 0 : 2) )) live"

(
    echo "time_s,relay_private_mb,relay_rss_mb,fds,threads,cpu_pct,gpu_mem_mb,gpu_util,enc_util,dec_util,up"
    began=$(date +%s); last=0; last_t=$began; hz=$(getconf CLK_TCK)
    while [ ! -f "$W/done" ]; do
        pid=$(pgrep -x relay-pp | head -1)
        if [ -n "$pid" ]; then
            anon=$(awk '/^RssAnon/{print $2}' /proc/$pid/status); rss=$(awk '/^VmRSS/{print $2}' /proc/$pid/status)
            threads=$(awk '/^Threads/{print $2}' /proc/$pid/status); fds=$(ls /proc/$pid/fd | wc -l)
            ticks=$(awk '{print $14+$15}' /proc/$pid/stat); now=$(date +%s)
            cpu=$(awk -v a=$ticks -v b=$last -v t=$((now - last_t)) -v hz=$hz 'BEGIN{ if (t > 0 && b > 0) printf "%.1f", (a-b)*100/hz/t; else print 0 }')
            last=$ticks; last_t=$now
            gpu=0,0,0,0
            command -v nvidia-smi >/dev/null && gpu=$(nvidia-smi --query-gpu=memory.used,utilization.gpu,utilization.encoder,utilization.decoder --format=csv,noheader,nounits | head -1 | tr -d ' ')
            echo "$((now - began)),$((anon / 1024)),$((rss / 1024)),$fds,$threads,$cpu,$gpu,$(live)"
        fi
        sleep 60
    done
) > samples.csv 2>/dev/null &

rtsp_url() {
    curl -s --max-time 20 -X POST "$API/api/v1/channels/$1/play" -H 'Content-Type: application/json' \
        -d '{"protocol":"rtsp"}' | grep -o 'rtsp://[^"]*'
}
rtsp_players() { # channel players seconds log
    local url
    url=$(rtsp_url "$1")
    if [ -n "$url" ]; then
        "$TOOLS/rtsp_load" "$url" "$2" --seconds "$3" > "$4" 2>&1 &
    else
        echo "no rtsp url for $1" > "$4"
    fi
}
webrtc_players() { "$TOOLS/webrtc_load" "$API/$1/whep" "$2" --seconds "$3" > "$4" 2>&1 & }
# An HLS player: the playlist and its newest segment, every two seconds,
# each answer counted.
hls_player() { # channel seconds log
    local until=$((SECONDS + $2)) ok=0 bad=0
    while [ $SECONDS -lt $until ]; do
        local list segment code
        list=$(curl -s --max-time 15 -w '\n%{http_code}' "$API/$1/index.m3u8")
        code=$(printf '%s\n' "$list" | tail -1)
        segment=$(printf '%s\n' "$list" | grep -v '^#' | grep -E 'm4s|mp4' | tail -1 | tr -d '\r')
        if [ "$code" = 200 ] && [ -n "$segment" ] && \
            [ "$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 "$API/$1/$segment")" = 200 ]; then
            ok=$((ok + 1))
        else
            bad=$((bad + 1))
        fi
        sleep 2
    done
    echo "hls $1: ok=$ok bad=$bad" > "$3"
}

# The players held for the whole run.
LONG="cam1 hevc1"; [ "$NO_HEVC" = 1 ] && LONG="cam1"
for channel in $LONG pub-rtsp1 pub-rtmp1 pub-whip1 pub-rtsp2 pub-rtmp2 pub-whip2; do
    rtsp_players "$channel" 1 $((SECS - 60)) "players/long-rtsp-$channel.log"
    webrtc_players "$channel" 1 $((SECS - 60)) "players/long-webrtc-$channel.log"
done
log "long players started"

end=$((SECONDS + SECS))
round=0
while [ $SECONDS -lt $end ]; do
    round=$((round + 1))
    a=$(( (round * 7) % (PULLS - 1) + 1 ))
    k=$(( round % 4 + 1 ))
    HEVC_NOW="hevc$(( round % 2 + 1 ))"; [ "$NO_HEVC" = 1 ] && HEVC_NOW=""
    for channel in "cam$a" $HEVC_NOW "pub-rtsp$k" "pub-rtmp$k" "pub-whip$k"; do
        rtsp_players "$channel" 3 120 "players/rtsp-$round-$channel.log"
        webrtc_players "$channel" 3 120 "players/webrtc-$round-$channel.log"
        if [ "$HLS" = 1 ]; then hls_player "$channel" 120 "players/hls-$round-$channel.log" & fi
    done
    removed=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE "$API/api/v1/channels/cam$PULLS")
    made=$(pull $PULLS)
    log "round $round: cam$a, publishers $k; cam$PULLS removed $removed made $made; live $(live)"
    curl -s "$API/metrics" | grep -E "^relay_pp_channel_webrtc_(resend_requests|loss|fell_behind)" | grep -v " 0$" > "metrics-$round.txt"
    sleep "$ROUND"
done

touch done
sleep 70
curl -s "$API/api/v1/channels" > channels-at-end.json
log "recordings at the end: $(du -sh recordings 2>/dev/null | cut -f1), $(find recordings -name media.m4s 2>/dev/null | wc -l) chunks"
stop_all
rm -rf recordings
log "ended after $round rounds; recordings removed"

# What the run came to.
{
    echo
    echo "== summary of $RUN"
    cd players || exit 0
    for kind in cam hevc pub-rtmp pub-rtsp pub-whip; do
        files=$(ls webrtc-[0-9]*-$kind* 2>/dev/null) || continue
        [ -n "$files" ] || continue
        echo "webrtc $kind: $(grep -l '[1-9][0-9]* players had a gap' $files | wc -l)/$(echo "$files" | wc -l) batches with a gap, $(grep -L 'played, 0 failed' $files | wc -l) with a failure"
    done
    echo "rtsp: $(grep -L ' 0 players lost any' rtsp-* long-rtsp-* 2>/dev/null | wc -l)/$(ls rtsp-* long-rtsp-* | wc -l) with a loss"
    [ "$HLS" = 1 ] && echo "hls: $(grep -L 'bad=0' hls-* | wc -l)/$(ls hls-* | wc -l) with a failed request"
    for f in long-webrtc-*; do echo "$f: $(grep 'after a gap' "$f" | sed 's/pictures: //' | cut -d';' -f1-2)"; done
    cd ..
    echo "webrtc metrics not 0 in rounds: $(cat metrics-*.txt 2>/dev/null | wc -l) lines"
    echo "relay-pp warnings and errors: $(grep -cE ' (WARN|ERROR) ' relay.log)"
    awk -F, 'NR > 1 && $1 >= 1800 { n++; x = $1 / 3600; y = $2; sx += x; sy += y; sxx += x * x; sxy += x * y; t += $5; f += $4 }
        END { if (n > 2) printf "relay-pp heap after 30 min: %+.1f MB/h, mean %.0f MB; mean %.0f threads, %.0f descriptors (n=%d)\n",
              (n * sxy - sx * sy) / (n * sxx - sx * sx), sy / n, t / n, f / n, n }' samples.csv
} | tee summary.txt
