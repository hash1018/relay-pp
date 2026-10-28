# Soaking relay-pp on Linux

[`soak.sh`](soak.sh) runs relay-pp for hours with every input and every way
out at once, on the loopback, and says at the end whether anything was lost
and whether its memory, threads or descriptors grew. This is how to run it
on a Linux machine with an NVIDIA GPU, and what to look at afterwards.

## What runs

- **Cameras** — media-pp's `rtsp_server`, looping a file: 50 channels pull
  an H.264 one, 2 more pull an HEVC one. One pulled channel is removed and
  made again every round.
- **Publishers** — [`soakpub`](soakpub), the same file into 4 channels each
  over RTSP, RTMP and WHIP. Publisher 1 of each kind leaves and comes back
  every 10 seconds (`LEAVE`); the others publish throughout. RTMP sends the
  pictures alone: FLV carries no Opus.
- **Players** — every round (5 minutes, the second argument), 3 RTSP, 3
  WebRTC and an HLS player on a pulled channel, an HEVC channel and one
  publisher's channel of each kind, for 2 minutes; and one RTSP and one
  WebRTC player on cam1, hevc1 and the publishers' channels 1 and 2 for the
  whole run. A WebRTC player of an HEVC channel is converted to H.264 on the
  GPU — NVDEC and NVENC.
- **Recordings** — 10 pulled channels and publisher 1's of each kind, at
  most 15 GB in the run's folder, removed at the end.

A sampler writes `samples.csv` once a minute: relay-pp's heap (`RssAnon`),
resident memory, descriptors, threads and processor, the GPU's memory and
its encoder and decoder use, and how many channels are live.

## Before you start

1. relay-pp and media-pp checked out side by side, both up to date:

   ```sh
   cd ~/src/relay-pp && git pull
   cd ~/src/media-pp && git pull
   ```

2. What media-pp builds with on Linux (`docs/building/linux.md` in
   media-pp): Rust, FFmpeg's development libraries, and the NVIDIA driver —
   `nvidia-smi` names the GPU. `soak.sh` drives relay-pp's API with `curl`,
   which a desktop install may not have.
3. A test video: an `.mp4` with H.264 pictures and Opus sound, a few
   minutes long. Nothing of the kind is checked in. The sound has to be
   Opus: the WHIP publishers send the file as it is, and WebRTC carries no
   AAC. A camera's recording, whose sound is AAC more often than not, can
   have its sound re-encoded and its pictures kept:

   ```sh
   ffmpeg -i camera.mp4 -c:v copy -c:a libopus -b:a 128k -ar 48000 -movflags +faststart ~/media/stream.mp4
   ```

## 1. Build

```sh
cd ~/src/relay-pp
bench/soak/build.sh
```

It builds relay-pp, media-pp's `rtsp_server`, `rtsp_load` and
`webrtc_load`, `soakpub`, and with a GPU `hevcify`, all in release.

## 2. Make the HEVC camera's file

```sh
bench/soak/hevcify/target/release/hevcify ~/media/stream.mp4 ~/media/stream-hevc.mp4
```

The pictures re-encoded to HEVC on NVENC, the sound copied. Without a GPU
skip this, and the soak runs without HEVC channels.

## 3. Check the GPU path first (5 minutes)

media-pp's CUDA tests, which Docker on Windows passed but for three — two
for want of OpenH264 in its FFmpeg, one about HDR10's metadata. On the
machine itself all should pass:

```sh
cd ~/src/media-pp
cargo test -p media-pp --features cuda --lib -- cuda --test-threads 1
```

## 4. A short run (10 minutes)

```sh
cd ~/src/relay-pp
STREAM=~/media/stream.mp4 HEVC=~/media/stream-hevc.mp4 RUN=smoke bench/soak/soak.sh 600 120
```

It should say `started: 64 of 64 live` (62 without HEVC) and end with a
summary of zeros — the heap's line comes only with a run past half an
hour. `60 of 64`, with every `pub-whip` line failing, is a file whose sound
is AAC: `pubs/pub-whip*.log` says so. Then the long one.

## 5. The soak (3 hours)

```sh
STREAM=~/media/stream.mp4 HEVC=~/media/stream-hevc.mp4 RUN=linux bench/soak/soak.sh 10800
```

Leave the machine to it: no other heavy work, no sleep. It uses the ports
18554–18555 (cameras), 18635 (RTMP), 18654 (RTSP), 18655 and on (WebRTC)
and 18688 (HTTP) on 127.0.0.1. To stop it early, `touch` `done` in the
run's folder and wait a minute, or `pkill -x relay-pp`.

### The allocator, side by side

In Docker relay-pp's heap kept growing — 34 to 94 MB an hour after the
first half hour, at two to three times Windows' size — and it was glibc's
malloc, not a leak. Three 90-minute runs side by side:

| Run | Heap after 30 min | After 60 min | Mean |
|---|---|---|---|
| as it is | +88 MB/h | +94 MB/h | 669 MB |
| `MALLOC_ARENA_MAX=2` | +14 MB/h | +5 MB/h | 349 MB |
| without the HEVC channels | +85 MB/h | — | 487 MB |

Each of relay-pp's many threads gets an arena of its own, which keeps what
it freed rather than giving it back; held to two, the heap halved and
stopped growing, and leaving the GPU's conversions out changed nothing. To
see whether that holds on the machine itself, run the soak again with it:

```sh
MALLOC_ARENA_MAX=2 STREAM=~/media/stream.mp4 HEVC=~/media/stream-hevc.mp4 RUN=linux-arena bench/soak/soak.sh 10800
```

On the machine itself it held, and cost processor time. Two 3-hour runs on
an i5-12400F with an RTX 3050, Ubuntu, one after the other:

| Run | Heap after 30 min | After 2 h | Mean | Most | relay-pp's processor |
|---|---|---|---|---|---|
| as it is | +169 MB/h | +22 MB/h | 1707 MB | 1913 MB | 80–90%, 120–130% while a round plays |
| `MALLOC_ARENA_MAX=2` | +2 MB/h | +20 MB/h | 642 MB | 691 MB | 130%, 160–180% while a round plays |

As it is, the heap rose for two hours to about 1.8 GB — three times what
it came to in Docker — and stayed there; threads and descriptors stayed
level throughout. Held to two arenas it stayed near 640 MB from the first
half hour on, and relay-pp's 165 threads, contending for those two, spent
half a core more doing the same work.

### What relay-pp does about it

Neither is what relay-pp runs. glibc's `malloc_info` taken every ten
minutes of a run showed where the memory was: 165 threads had opened all
96 arenas the machine allows (eight a core), and about four fifths of what
they kept free was each arena's free end, 10 to 14 MB apiece. glibc gives
that end back past a threshold it raises as large blocks are freed — to
twice its mmap threshold, up to 64 MB — and the soak's pictures raised it
to the top. The other fifth was holes between blocks still in use.
Deleting every channel left 36 MB in use and over 800 MB kept.

So relay-pp calls media-pp's `heap::keep_small()` first in `main`. It
holds both thresholds — setting either stops both rising — the end of an
arena going back past 1 MB and a block mapped on its own only past 32 MB,
and every ten seconds gives back the free pages in the holes too. Each
half by itself, and both, for 50 minutes before every channel was deleted:

| Run | Heap at 30–45 min | relay-pp's processor | Heap with every channel deleted |
|---|---|---|---|
| as it was | 1442–1513 MB | 96–105% | 834–887 MB |
| thresholds held | 812 MB | 105% | 372 MB |
| trimmed every 10 s | 1133 MB | 110% | 480 MB |
| both | 553 MB | 107% | 86 MB |

Then a 3-hour run with both, beside the two above:

| Run | Heap after 30 min | After 2 h | Mean | Most | relay-pp's processor |
|---|---|---|---|---|---|
| `keep_small` | −2 MB/h | between 470 and 640 MB | 546 MB | 639 MB | 108% (91–130%) |

Each half hour after the first came to between 490 and 590 MB on average,
in no direction; threads stayed at about 167 and descriptors at 213, and
nothing was lost. In `bench/` scenarios run alternately with and without
it, relay-pp's processor differed by 0 to 3%.

Holding the mmap threshold low instead — 128 KiB, so that large blocks go
back to the system as they are freed — kept the heap as small, but
relay-pp died within minutes converting HEVC: NVIDIA's NVDEC library reads
a block it has freed, which faults where the block was unmapped (media-pp's
`docs/issues/nvdec-use-after-free`). Held at 32 MB, that 2.3 MB block
stays in an arena.

## Reading what it says

Each run is a folder under `bench/soak/runs/`. `summary.txt` is what to read
first:

| Line | Good |
|---|---|
| `webrtc <kind>: N/M batches with a gap` | 0 of every kind |
| `rtsp: N/M with a loss` | 0 |
| `hls: N/M with a failed request` | 0 |
| `long-webrtc-*: … after a gap` | 0 |
| `webrtc metrics not 0 in rounds` | 0 lines |
| `relay-pp warnings and errors` | 1 (that there is no `[auth]`) |
| `relay-pp heap after 30 min: ±X MB/h` | within a few MB an hour of 0 |

What earlier runs came to, for comparison:

| Where | Losses | Heap |
|---|---|---|
| Windows, 6 h, no HEVC | none | ~250 MB, flat |
| Docker on Windows, 3.5 h, HEVC on the GPU | none | 600–800 MB, +34 to +59 MB/h |
| Docker, 1.5 h, `MALLOC_ARENA_MAX=2` | none | ~350 MB, +5 MB/h after an hour |
| Linux, 3 h, HEVC on the GPU | none | rising to ~1.8 GB in 2 h, then +22 MB/h |
| Linux, 3 h, `MALLOC_ARENA_MAX=2` | none | ~640 MB, +2 MB/h; half a core more processor |
| Linux, 3 h, `keep_small` | none | ~550 MB, level; 2% more processor |

Then, where something is off:

- `soak.log` — each round, and how many channels were live.
- `samples.csv` — the minute-by-minute figures; a column rising while the
  others are flat is what to look into.
- `players/` — each player's own report; `pubs/` — each publisher's.
- `relay.log` — relay-pp's own log; `logs/` — media-pp's records.

To hand a run over, pack its folder — the recordings are gone already:

```sh
tar czf linux-soak.tgz -C bench/soak/runs linux
```
