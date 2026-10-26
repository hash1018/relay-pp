#!/usr/bin/env bash
# Builds what the soak runs, in release: relay-pp, media-pp's camera and
# players, the publisher, and — with an NVIDIA GPU — hevcify. media-pp is a
# checkout beside relay-pp unless MEDIA_PP says where.
#
#   bench/soak/build.sh
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
MEDIA_PP=${MEDIA_PP:-$ROOT/../media-pp}
[ -d "$MEDIA_PP/lib" ] || { echo "no media-pp at $MEDIA_PP: set MEDIA_PP" >&2; exit 1; }

(cd "$ROOT" && cargo build --release --locked)
(cd "$MEDIA_PP" && cargo build --release -p rtsp_server -p rtsp_load -p webrtc_load)
(cd "$ROOT/bench/soak/soakpub" && cargo build --release)
if command -v nvidia-smi >/dev/null; then
    (cd "$ROOT/bench/soak/hevcify" && cargo build --release)
else
    echo "no NVIDIA GPU: hevcify not built, and the soak runs without HEVC channels"
fi
echo "built"
