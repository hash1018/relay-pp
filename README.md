# relay-pp

A streaming server on [media-pp](https://github.com/hash1018/media-pp): it
takes streams in by whatever protocol they come — RTSP cameras, RTSP, RTMP
and WHIP publishers — hands each out to RTSP, WebRTC and HLS players, and
records what it is told to.

Phase 1 is built — channels made over an HTTP API, each an RTSP camera
pulled and played over RTSP and WebRTC (WHEP). [docs/status.md](docs/status.md)
is what it does now, in full, and what it does not yet;
[docs/design.md](docs/design.md) is the agreed scope and the order the rest
is built in.

## Running

```sh
cp relay-pp.example.toml relay-pp.toml   # optional: where it listens
cargo run --release
```

It starts with no channel. Whatever manages the cameras makes each one over
the API, and makes them again after a restart — relay-pp keeps none:

```sh
curl -X POST http://<host>:8888/api/v1/channels \
     -H 'Content-Type: application/json' \
     -d '{"name":"cam1","source":"rtsp://user:pass@10.0.0.21/stream1"}'
curl http://<host>:8888/api/v1/channels            # what each is doing
curl -X DELETE http://<host>:8888/api/v1/channels/cam1
```

A channel pulls its camera, `"transport": "udp"` or over the RTSP connection
by default, and keeps connecting again to one that drops. It is played at
`rtsp://<host>:8554/<channel>` and, by any WHEP client, at
`http://<host>:8888/<channel>/whep` (a page that plays it in a browser comes
in phase 3). Ctrl+C stops it. relay-pp logs to the terminal and media-pp to
`logs/`.

The API has no authentication until phase 4: anyone who reaches the HTTP
port can make and remove channels.

Building needs FFmpeg's development libraries, as media-pp does — see its
[building guide](https://github.com/hash1018/media-pp/tree/main/docs/building).
