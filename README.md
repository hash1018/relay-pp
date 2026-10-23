# relay-pp

A streaming server on [media-pp](https://github.com/hash1018/media-pp): it
takes streams in by whatever protocol they come — RTSP cameras, RTSP, RTMP
and WHIP publishers — hands each out to RTSP, WebRTC and HLS players, and
records what it is told to.

Phases 1 and 2 are built — channels made over an HTTP API, each an RTSP camera
pulled, played over RTSP and WebRTC (WHEP) and recorded as HLS.
[docs/status.md](docs/status.md)
is what it does now, in full, and what it does not yet;
[docs/design.md](docs/design.md) is the agreed scope and the order the rest
is built in.

## Running

```sh
cp relay-pp.example.toml relay-pp.toml   # optional: where it listens, how it records
cargo run --release
```

It starts with no channel. Whatever manages the cameras makes each one over
the API, with its source alone, and makes them again after a restart —
relay-pp keeps none. What reads a channel is asked for separately:

```sh
# A channel: its camera, pulled from now on, connected again whenever it drops.
curl -X POST http://<host>:8888/api/v1/channels \
     -H 'Content-Type: application/json' \
     -d '{"name":"cam1","source":"rtsp://user:pass@10.0.0.21/stream1"}'

# Where an RTSP player plays it: {"url":"rtsp://<host>:8554/cam1"}
curl -X POST http://<host>:8888/api/v1/channels/cam1/play \
     -H 'Content-Type: application/json' -d '{"protocol":"rtsp"}'
# A WebRTC player needs no question: any WHEP client at http://<host>:8888/cam1/whep

# Recording it, as HLS, until told otherwise.
curl -X POST   http://<host>:8888/api/v1/channels/cam1/record
curl -X DELETE http://<host>:8888/api/v1/channels/cam1/record
# What it recorded, and a stretch of it as one playlist.
curl http://<host>:8888/api/v1/recordings/cam1
curl 'http://<host>:8888/playback/cam1/index.m3u8?from=2026-10-23T14:00:00Z&to=2026-10-23T15:00:00Z'

curl http://<host>:8888/api/v1/channels            # what each is doing
curl -X DELETE http://<host>:8888/api/v1/channels/cam1
```

[docs/status.md](docs/status.md) has every request and answer. Ctrl+C stops
it. relay-pp logs to the terminal and media-pp to `logs/`.

The API has no authentication until phase 4: anyone who reaches the HTTP
port can make and remove channels, and read every recording.

Building needs FFmpeg's development libraries, as media-pp does — see its
[building guide](https://github.com/hash1018/media-pp/tree/main/docs/building).
