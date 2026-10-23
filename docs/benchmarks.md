# relay-pp against MediaMTX

Measured 2026-10-23 on one Windows 11 machine — Intel i5-12400F (6 cores,
12 threads), 32 GB — over the loopback, with [`bench/`](../bench/README.md):
relay-pp at 55ce8bb on media-pp e4a39241 (and again on f96bb6ae, below),
MediaMTX v1.20.1. One run of each,
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
and about 5 MB of memory each to MediaMTX's 0.3 MB. Most of the memory was
the broadcast's retained stretch — at least five seconds and a group of
pictures, 2.5 to 3.5 MB of a 4 Mbit/s stream — kept so that a player starts
at once at a keyframe; MediaMTX keeps no such stretch, and sends a player
what arrives after it joins. Threads are one per camera, as built.

### After media-pp f96bb6ae: a broadcast keeps a second

A broadcast now keeps the newest second, packet by packet, and no group of
pictures: a player joining waits for the next keyframe where none is held,
as MediaMTX's does. relay-pp again, MediaMTX as above:

| Channels | relay-pp memory, before → after | MediaMTX memory | relay-pp CPU after |
|---|---|---|---|
| 50 | 306 → 160 MB | 86 MB | 200 |
| 100 | 570 → 266 MB | 107 MB | 312 |
| 200 | 1,100 → 480 MB | 142 MB | 353 |

About 2.3 MB a camera, from 5.4; what is left of the difference from
MediaMTX is not the broadcast — half a megabyte at most — and is to be
looked for in a profile, with the processor time. Recording every camera:
330, 509 and 944 MB at 50, 100 and 200 channels, from 445, 881 and 1,613.
The players are unchanged: 200 RTSP players took 60% of a core and 50
WebRTC players 49%, nothing lost.

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

On Windows, below; on Linux, where it would run, further down.

- **Sending to players is where it is strong**: RTSP for a fraction of
  MediaMTX's processor time, WebRTC for less and losing nothing — on Linux
  too, and with a steadier stream at scale.
- **Pulling cameras costs twice MediaMTX's processor time on Windows**, all
  of it FFmpeg's RTSP demuxer there; on Linux relay-pp pulls for less than
  MediaMTX. Recording costs a little more than MediaMTX's on either.
- **Memory per camera is the gap left**: a broadcast's retained stretch was
  most of it and is now a second; what is left is what a pull of FFmpeg's
  holds, and, on Linux, what opening every camera at once leaves behind.

## On Linux

The same runs in Docker on the same machine — a Linux VM of 12 processors
and 16 GB — relay-pp on media-pp f96bb6ae, MediaMTX v1.20.1 for Linux,
everything in one container over its loopback, recordings on the
container's own disk. Memory is resident memory (`bench/measure.sh`), not
private bytes.

### Pulling cameras, nothing reading

| Channels | relay-pp CPU | MediaMTX CPU | relay-pp memory | MediaMTX memory |
|---|---|---|---|---|
| 50 | 98 | 128 | 256 MB | 55 MB |
| 100 | 159 | 213 | 364 MB | 67 MB |
| 200 | 195 | 317 | 1,287 MB | 101 MB |

### Recording every camera

| Channels | relay-pp CPU | MediaMTX CPU | relay-pp memory | MediaMTX memory | written, relay-pp / MediaMTX |
|---|---|---|---|---|---|
| 50 | 206 | 169 | 467 MB | 168 MB | 218 / 204 Mbit/s |
| 100 | 308 | 246 | 857 MB | 266 MB | 389 / 422 Mbit/s |
| 200 | 373 | 334 | 1,957 MB | 439 MB | 825 / 844 Mbit/s |

relay-pp logged no recording falling behind at 100; Linux counts a process's
writes as the page cache is flushed, which a 30-second window takes
unevenly.

### RTSP players of one camera

| Players | relay-pp CPU | MediaMTX CPU | relay-pp memory | MediaMTX memory | lag spread p99, relay-pp / MediaMTX |
|---|---|---|---|---|---|
| 50 | 14 | 22 | 60 MB | 48 MB | 6 / 4 ms |
| 200 | 41 | 208 | 65 MB | 63 MB | 7 / 24 ms |
| 500 | 94 | 397 | 74 MB | 94 MB | 23 / 31 ms |

### WebRTC players of one camera

| Players | relay-pp CPU | MediaMTX CPU | relay-pp memory | MediaMTX memory | lag spread p99, relay-pp / MediaMTX |
|---|---|---|---|---|---|
| 25 | 24 | 46 | 105 MB | 151 MB | 20 / 32 ms |
| 50 | 38 | 84 | 147 MB | 257 MB | 28 / 178 ms |
| 100 | 61 | 136 | 231 MB | 470 MB | 28 / 267 ms |

Nothing lost by either, in any run.

### Where pulling's processor time goes

A hundred pulls of the camera by a program that does no more than each
layer, all hundred open before measuring:

| | Windows CPU | Linux CPU |
|---|---|---|
| FFmpeg's RTSP demuxer alone, packets read and dropped | 311 | 151 |
| + a media-pp pipeline: `RtspSource` → `PacketCounter` | 316 | 165 |
| + a broadcast: `RtspSource` → `BroadcastSink` | 316 | 169 |
| relay-pp | 312 | 159 |

FFmpeg's demuxer is nearly all of it, on either system; it costs twice as
much on Windows, which is the whole of relay-pp's difference from MediaMTX
there. On Linux relay-pp pulls for less than MediaMTX does.

### Where Linux's memory at 200 goes

The same program opening 200 pulls one after another holds 548 MB; relay-pp,
opening them all at once, 1,183 MB, and as much again with
`MALLOC_ARENA_MAX=2`. What FFmpeg takes to look into a stream as it opens
it, two hundred times at once, is let go of after but stays with the
process, as glibc keeps a heap it has grown. Opening a few cameras at a time
would keep it near the first figure.

## Not measured here

Runs repeated to show their spread; players on another machine; UDP; more
than one camera with players; a camera opened with fewer at a time.
