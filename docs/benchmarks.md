# relay-pp against MediaMTX

Measured 2026-10-23 on one Windows 11 machine — Intel i5-12400F (6 cores,
12 threads), 32 GB — over the loopback, with [`bench/`](../bench/README.md):
relay-pp at 55ce8bb on media-pp e4a39241, MediaMTX v1.20.1. One run of each,
30 seconds measured.

The stream is 1080p at 25 pictures a second, H.264 High at 4 Mbit/s, a
keyframe every two seconds, no B-frames, and Opus sound. `CPU` is the
server's processor time as a share of one core: 100 is one core, 1200 all
twelve. The camera — media-pp's `rtsp_server` — and the players run on the
same machine and take the same cores; the camera's own cost is in the run
logs, the players' is not measured.

## Pulling cameras, nothing reading

| Channels | relay-pp CPU | MediaMTX CPU | relay-pp memory | MediaMTX memory | relay-pp threads | MediaMTX threads |
|---|---|---|---|---|---|---|
| 50 | 203 | 116 | 306 MB | 86 MB | 65 | 22 |
| 100 | 297 | 149 | 570 MB | 107 MB | 115 | 22 |
| 200 | 380 | 199 | 1,100 MB | 142 MB | 215 | 21 |

relay-pp takes about twice the processor time of MediaMTX to pull a camera,
and about 5 MB of memory each to MediaMTX's 0.3 MB. Most of the memory is
the broadcast's retained stretch — at least five seconds and a group of
pictures, 2.5 to 3.5 MB of a 4 Mbit/s stream — kept so that a player starts
at once at a keyframe; MediaMTX keeps no such stretch, and sends a player
what arrives after it joins. Threads are one per camera, as built.

## Recording every camera

| Channels | relay-pp CPU | MediaMTX CPU | relay-pp memory | MediaMTX memory | relay-pp threads | MediaMTX threads | written |
|---|---|---|---|---|---|---|---|
| 50 | 285 | 156 | 445 MB | 185 MB | 115 | 35 | 201 Mbit/s each |
| 100 | 407 | 242 | 881 MB | 264 MB | 220 | 50 | 412 / 411 Mbit/s |
| 200 | 358 | 286 | 1,613 MB | 476 MB | 414 | 29 | 851 / 820 Mbit/s |

Both kept up, writing what the cameras sent. relay-pp's 200 taking less than
its 100 is one run of each and not to be read into before it is repeated.

## RTSP players of one camera

| Players | relay-pp CPU | MediaMTX CPU | relay-pp memory | MediaMTX memory | lag spread p99, relay-pp / MediaMTX |
|---|---|---|---|---|---|
| 50 | 43 | 409 | 44 MB | 79 MB | 24 / 54 ms |
| 200 | 53 | 566 | 50 MB | 103 MB | 31 / 69 ms |
| 500 | 108 | 671 | 62 MB | 146 MB | 36 / 101 ms |

Every player of both received the whole stream with nothing lost — 2 Gbit/s
in all at 500. relay-pp sends to RTSP players for a sixth to a tenth of
MediaMTX's processor time, on a few threads its listener shares; why
MediaMTX takes so much here was not looked into, and may be Windows's.

## WebRTC players of one camera

| Players | relay-pp CPU | MediaMTX CPU | relay-pp memory | MediaMTX memory | pictures after a gap, relay-pp / MediaMTX | lag spread p99, relay-pp / MediaMTX |
|---|---|---|---|---|---|---|
| 25 | 44 | 47 | 90 MB | 184 MB | 0 / 1 | 96 / 1,181 ms |
| 50 | 51 | 62 | 138 MB | 295 MB | 0 / 0 | 116 / 40 ms |
| 100 | 69 | 117 | 231 MB | 517 MB | 0 / 6 | 155 / 1,189 ms |

relay-pp about even with MediaMTX at 25 players and two-thirds of its
processor time at 100, with half its memory, and no player of it lost a
picture. MediaMTX's median spread is the lower — about 40 ms to
relay-pp's 70 — but at 25 and 100 players some of its players lost a
picture and waited over a second for the next.

## What it says for relay-pp

- **Sending to players is where it is strong**: RTSP for a fraction of
  MediaMTX's processor time, WebRTC for less and losing nothing.
- **Pulling cameras is where it is weak**, and pulling is the CCTV case:
  twice the processor time and many times the memory per camera, recording
  included. Worth looking at, in this order: the memory a broadcast retains
  (a shorter `retain` for cameras that only record), and where the
  processor time of pulling goes — FFmpeg's RTSP demuxer and the pipeline
  per camera — against a profile before any change.

## Not measured here

Linux, where both would run in production; runs repeated to show their
spread; players on another machine; UDP; more than one camera with players.
