# relay-pp against MediaMTX

Measured 2026-10-23 on one Windows 11 machine — Intel i5-12400F (6 cores,
12 threads), 32 GB — over the loopback, with [`bench/`](../bench/README.md):
relay-pp at 55ce8bb on media-pp e4a39241 (and again on f96bb6ae, and at
6aa8188 with `retina`'s client, below),
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

## Pulling with `retina`'s client

relay-pp at 6aa8188 pulls a camera with `retina`'s RTSP client, a task on
its runtime, where it takes the camera, and with FFmpeg's where it does not.
The stream above has Opus sound, which `retina` does not take, and the
camera — media-pp's `rtsp_server` — gives no `rtptime` for a stream of two
tracks: either sends relay-pp to FFmpeg's client. These runs are of the same
pictures with no sound (`ffmpeg -i stream.mp4 -map 0:v -c copy`), which
`retina` takes as it does an IP camera's; MediaMTX was measured again on
them. Both OSes, the same machine; on Linux memory is resident memory.

### Pulling cameras, nothing reading

| | Channels | relay-pp CPU, FFmpeg's → `retina`'s | MediaMTX CPU | relay-pp memory, FFmpeg's → `retina`'s | MediaMTX memory | relay-pp threads |
|---|---|---|---|---|---|---|
| Windows | 50 | 200 → 13 | | 160 → 73 MB | | 65 → 13 |
| | 100 | 312 → 27 | 116 | 266 → 109 MB | 102 MB | 115 → 13 |
| | 200 | 353 → 43 | | 480 → 179 MB | | 215 → 13 |
| Linux | 50 | 98 → 13 | | 256 → 88 MB | | 59 → 9 |
| | 100 | 159 → 20 | 168 | 364 → 128 MB | 65 MB | 109 → 9 |
| | 200 | 195 → 29 | | 1,287 → 212 MB | | 209 → 9 |

FFmpeg's figures are those above, of the stream with sound. A hundred
cameras now take a quarter of a core on Windows and a fifth on Linux, from
three cores and one and a half — a tenth of it, and a fourth to an eighth of
MediaMTX's. Every pull is a task on the runtime's two threads: the thread
count no longer grows with the cameras. Memory is 0.7–0.9 MB a camera; on
Linux the 1.3 GB of opening two hundred FFmpeg pulls at once is gone with
FFmpeg's client.

### Recording every camera

| | Channels | relay-pp CPU | MediaMTX CPU | relay-pp memory | MediaMTX memory | written, relay-pp / MediaMTX |
|---|---|---|---|---|---|---|
| Windows | 50 | 64 | | 192 MB | | 200 Mbit/s |
| | 100 | 107 | 197 | 335 MB | 288 MB | 425 / 399 Mbit/s |
| | 200 | 126 | | 671 MB | | 800 Mbit/s |
| Linux | 50 | 67 | | 200 MB | | 213 Mbit/s |
| | 100 | 113 | 206 | 404 MB | 249 MB | 426 / 413 Mbit/s |
| | 200 | 196 | | 801 MB | | 792 Mbit/s |

Recording is now most of what a recorded camera costs — a pipeline and a
thread of its own each, writing HLS — and still about half MediaMTX's
processor time.

## Converting for WebRTC players

relay-pp at 90ba8fe on Windows, the same machine, its GPU an NVIDIA GeForce
RTX 3050 (driver 610.88). N channels, one WebRTC player each (`watch N`), of
three cameras sending the stream above, 1080p at 25: H.264 and Opus, which
WebRTC carries as it is; H.264 and AAC, whose sound is converted to Opus on
the processor; H.265 and Opus, whose pictures are converted to H.264 —
decoded by D3D11VA and encoded by NVENC, never copied out of the GPU. The
camera's tracks have no `rtptime`, so FFmpeg's client pulls all three, and
the first stands for what the other two cost without converting. `GPU`
columns are `nvidia-smi`'s, over the same 30 seconds.

| Channels | CPU, none / sound / pictures | Memory, none / sound / pictures | Threads, none / pictures | NVENC | NVDEC |
|---|---|---|---|---|---|
| 1 | 17 / 22 / 21 | 40 / 42 / 201 MB | 15 / 31 | 17% | 5% |
| 4 | 33 / 57 / 93 | 54 / 62 / 629 MB | 19 / 72 | 58% | 21% |
| 8 | 49 / 118 / 152 | 73 / 83 / 1,174 MB | 23 / 128 | 63% | 20% |
| 10 | — / — / 204 | — / — / 1,448 MB | — / 156 | 69% | 20% |

Every player of every run played to the end with no picture lost, sound
and all.

- **Sound** costs 6 to 9% of a core a channel: AAC decoded and Opus encoded
  by `libopus` at its default complexity, which is where to look first.
- **Pictures** cost 13 to 15% of a core a channel, and about 140 MB and 13
  threads: a D3D11 device of each conversion's own, its decoder's surfaces
  and NVENC's buffers. Ten 1080p conversions at once took 69% of the one
  NVENC — about fourteen in all — and the driver opened every session asked
  for: where it stops, a conversion asked for after is refused (`"gpu"`).
  What the processor spends beside the GPU's work has not been profiled.
- Neither runs without a WebRTC player: a channel nobody watches over WebRTC
  converts nothing.

## What it says for relay-pp

- **Pulling cameras costs a fraction of MediaMTX's processor time** with
  `retina`'s client, on Windows as on Linux; FFmpeg's, which takes what
  `retina` does not, costs twice MediaMTX's on Windows and about the same on
  Linux.
- **Sending to players is where it is strong**: RTSP for a fraction of
  MediaMTX's processor time, WebRTC for less and losing nothing — on Linux
  too, and with a steadier stream at scale.
- **Memory per camera is the gap left**, smaller: about 1 MB a camera to
  MediaMTX's 0.3–0.5, and about 3 MB a camera recorded, a pipeline each.

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
