#!/usr/bin/env bash
# relay-pp against MediaMTX, on one machine over the loopback — see
# README.md beside this. Every server, camera and player binds 127.0.0.1.
#
#   bench/compare.sh <work-dir> <scenario> <server> <count>
#
#   scenario: pull     — <count> channels pulling the camera, nothing reading
#             record   — <count> channels pulling the camera, each recorded
#             rtsp     — one channel, <count> RTSP players
#             webrtc   — one channel, <count> WebRTC (WHEP) players
#   server:   relay | mediamtx
#
# <work-dir> holds stream.mp4, the camera's file. Needs, built in release:
# media-pp's rtsp_server, rtsp_load and webrtc_load (in $TOOLS), relay-pp
# ($RELAY), and MediaMTX ($MEDIAMTX). Prints one line of results. Runs on
# Windows (Git Bash) and on Linux.
set -euo pipefail

work=$1 scenario=$2 server=$3 count=$4
MEDIA_PP=${MEDIA_PP:-/d/Project/media-pp}
RELAY_PP=${RELAY_PP:-/d/Project/relay-pp}
MEDIAMTX=${MEDIAMTX:?the MediaMTX executable}
WARM=${WARM:-15}
SECONDS_MEASURED=${SECONDS_MEASURED:-30}
here=$(cd "$(dirname "$0")" && pwd)
case $(uname -s) in
    MINGW* | MSYS* | CYGWIN*)
        exe=.exe
        native() { cygpath -m "$1"; }
        kill_named() { taskkill //F //IM "$1.exe" >/dev/null 2>&1 || true; }
        measure() { powershell -NoProfile -ExecutionPolicy Bypass -File "$(cygpath -w "$here/measure.ps1")" -Name "$1" -Seconds "$2" | tr -d '\r'; }
        ;;
    *)
        exe=
        native() { echo "$1"; }
        kill_named() { pkill -x "$1" >/dev/null 2>&1 || true; }
        measure() { "$here/measure.sh" "$1" "$2"; }
        ;;
esac
tools=${TOOLS:-$MEDIA_PP/target/release}
relay=${RELAY:-$RELAY_PP/target/release/relay-pp$exe}
run=$work/run-$scenario-$server-$count
rm -rf "$run" && mkdir -p "$run"

CAMERA=127.0.0.1:18554
camera_url=rtsp://$CAMERA/file

stop_all() {
    for name in relay-pp mediamtx rtsp_server rtsp_load webrtc_load; do kill_named "$name"; done
}
trap stop_all EXIT
stop_all

wait_until() { # what, seconds, command...
    local what=$1 within=$2; shift 2
    for _ in $(seq "$within"); do
        if "$@" >/dev/null 2>&1; then return 0; fi
        sleep 1
    done
    echo "timed out waiting for $what" >&2
    exit 1
}

"$tools/rtsp_server$exe" "$work/stream.mp4" "$CAMERA" --seconds 100000 >"$run/camera.log" 2>&1 &
wait_until "the camera" 20 grep -q "serving" "$run/camera.log"

record=false
channels=$count
case $scenario in
    pull) ;;
    record) record=true ;;
    rtsp | webrtc) channels=1 ;;
    *) echo "no scenario $scenario" >&2; exit 2 ;;
esac

case $server in
relay)
    process=relay-pp
    api=127.0.0.1:28888
    cat >"$run/relay-pp.toml" <<EOF
[rtsp]
listen = "127.0.0.1:28554"
udp = false
[webrtc]
listen = "127.0.0.1:28189"
[http]
listen = "$api"
[record]
dir = "$(native "$run/recordings")"
segment = "4s"
chunk = "1h"
EOF
    (cd "$run" && RUST_LOG=warn "$relay" relay-pp.toml >relay.log 2>&1 &)
    wait_until "relay-pp" 20 curl -sf "http://$api/api/v1/channels"
    for i in $(seq "$channels"); do
        curl -sf -X POST "http://$api/api/v1/channels" -H 'Content-Type: application/json' \
            -d "{\"name\":\"cam$i\",\"source\":\"$camera_url\"}" >/dev/null
        if $record; then curl -sf -X POST "http://$api/api/v1/channels/cam$i/record" >/dev/null; fi
    done
    live() { [ "$(curl -sf "http://$api/api/v1/channels" | grep -o '"state":"live"' | wc -l)" -eq "$channels" ]; }
    rtsp_play() { curl -sf -X POST "http://$api/api/v1/channels/cam1/play" -H 'Content-Type: application/json' \
        -d '{"protocol":"rtsp"}' | sed 's/.*"url":"\([^"]*\)".*/\1/'; }
    whep=http://$api/cam1/whep
    ;;
mediamtx)
    process=mediamtx
    api=127.0.0.1:39997
    {
        cat <<EOF
logLevel: warn
logDestinations: [stdout]
api: true
apiAddress: $api
metrics: false
pprof: false
playback: false
rtsp: true
rtspTransports: [tcp]
rtspAddress: 127.0.0.1:38554
rtmp: false
hls: false
webrtc: true
webrtcAddress: 127.0.0.1:38889
webrtcLocalUDPAddress: 127.0.0.1:38189
webrtcIPsFromInterfaces: false
webrtcAdditionalHosts: [127.0.0.1]
srt: false
moq: false
pathDefaults:
  sourceOnDemand: false
  rtspTransport: tcp
  record: $record
  recordPath: $(native "$run/recordings")/%path/%Y-%m-%d_%H-%M-%S-%f
  recordFormat: fmp4
  recordSegmentDuration: 1h
  recordDeleteAfter: 0s
paths:
EOF
        for i in $(seq "$channels"); do
            printf '  cam%s:\n    source: %s\n' "$i" "$camera_url"
        done
    } >"$run/mediamtx.yml"
    (cd "$run" && "$MEDIAMTX" mediamtx.yml >mediamtx.log 2>&1 &)
    wait_until "MediaMTX" 20 curl -sf "http://$api/v3/paths/list"
    live() { [ "$(curl -sf "http://$api/v3/paths/list?itemsPerPage=10000" | grep -o '"ready":true' | wc -l)" -eq "$channels" ]; }
    rtsp_play() { echo rtsp://127.0.0.1:38554/cam1; }
    whep=http://127.0.0.1:38889/cam1/whep
    ;;
*) echo "no server $server" >&2; exit 2 ;;
esac

wait_until "every channel live" 60 live
sleep "$WARM"

load=""
case $scenario in
    rtsp)
        "$tools/rtsp_load$exe" "$(rtsp_play)" "$count" --seconds $((SECONDS_MEASURED + 15)) >"$run/players.log" 2>&1 &
        load=$!
        sleep 10
        ;;
    webrtc)
        "$tools/webrtc_load$exe" "$whep" "$count" --seconds $((SECONDS_MEASURED + 15)) >"$run/players.log" 2>&1 &
        load=$!
        sleep 10
        ;;
esac
server_cost=$(measure "$process" "$SECONDS_MEASURED")
camera_cost=$(measure rtsp_server 5 | sed 's/ write_mbps=.*//')
players=""
if [ -n "$load" ]; then
    wait "$load" || true
    players=$(grep -E "^(players|received|pictures|packets|lag spread)" "$run/players.log" | tr '\n' ' ' | tr -s ' ')
fi
echo "$scenario $server $count | $server_cost | camera: $camera_cost | $players"
