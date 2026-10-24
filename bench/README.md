# relay-pp against MediaMTX

[`compare.sh`](compare.sh) runs one scenario against one server on this
machine, over the loopback, and prints one line of what it cost;
[`measure.ps1`](measure.ps1) on Windows (Git Bash) and [`measure.sh`](measure.sh)
on Linux are what measure a process. Results are in
[docs/benchmarks.md](../docs/benchmarks.md).

## What runs

- **The camera** — media-pp's `rtsp_server` example, playing `stream.mp4`
  looped at its own speed and serving it at `rtsp://127.0.0.1:18554/file`.
  Every channel of either server pulls that one URL, so the camera's cost is
  the same for both; it is measured beside, not counted.
- **The server** — relay-pp, its channels made over its API, or MediaMTX
  with a path for each in its configuration. Both pull over TCP, always on,
  and bind 127.0.0.1 alone.
- **The players** — media-pp's `rtsp_load` and `webrtc_load`: a thread
  each, decoding nothing, so that the server is what is measured. RTSP over
  the connection (TCP); WebRTC through WHEP, with an offer for a picture and
  a sound as a browser makes.

## The scenarios

| | |
|---|---|
| `pull N` | N channels pulling the camera, nothing reading them |
| `record N` | the same, each recorded — relay-pp as HLS (fMP4 by byte range), MediaMTX as fMP4 segments, an hour a file for both |
| `rtsp M` | one channel, M RTSP players |
| `webrtc M` | one channel, M WebRTC players |
| `spread N` | N channels, one RTSP player each: what serving many channels to a few players costs |
| `watch N` | N channels, one WebRTC player each: what converting each for WebRTC costs, given a stream WebRTC does not carry |

Each run waits for every channel to be live, then 15 seconds more, starts
the players 10 seconds before measuring, and measures 30 seconds.

## What is measured

Of the server's process, over the 30 seconds: processor time as a share of
one core (`cpu_pct`; 100 is one core of twelve here), private memory and
threads at their highest, and what it wrote to disk. Of the players: what
they were sent, what they lost, and how steadily pictures arrived.

## The stream

```sh
ffmpeg -f lavfi -i testsrc2=size=1920x1080:rate=25 \
       -f lavfi -i "sine=frequency=440:sample_rate=48000" -t 60 \
       -map 0:v -map 1:a -c:v h264_nvenc -preset p4 -rc cbr -b:v 4M -maxrate 4M \
       -bufsize 4M -g 50 -bf 0 -profile:v high -pix_fmt yuv420p \
       -c:a libopus -b:a 64k stream.mp4
```

1080p at 25 pictures a second, H.264 High at 4 Mbit/s with a keyframe every
two seconds and no B-frames, as a CCTV camera sends; and Opus sound. The
sound is there for MediaMTX: it refuses a WHEP offer with a sound section
for a path that has no sound (`codecs not supported by client`), and a
browser's offer has one.


## Running it

```sh
MEDIAMTX=/path/to/mediamtx.exe bench/compare.sh <work-dir> pull relay 100
```

`<work-dir>` holds `stream.mp4`; each run leaves its logs and configuration
in `<work-dir>/run-<scenario>-<server>-<count>/`. Build media-pp's
`rtsp_server`, `rtsp_load` and `webrtc_load` and relay-pp in release first.
`measure.ps1` is run with `-ExecutionPolicy Bypass` for that one call; the
machine's policy is not changed.
