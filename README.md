# relay-pp

A streaming server on [media-pp](https://github.com/hash1018/media-pp): it
takes streams in by whatever protocol they come — RTSP cameras, RTSP, RTMP
and WHIP publishers — hands each out to RTSP, WebRTC and HLS players, and
records what it is told to.

Phase 1 is built — cameras pulled over RTSP, played over RTSP and WebRTC,
their status over HTTP; [docs/design.md](docs/design.md) is the agreed scope
and the order the rest is built in.

## Running

```sh
cp relay-pp.example.toml relay-pp.toml   # and set the cameras
cargo run --release
```

It pulls every camera the file names and keeps connecting again to one
that drops. Each is played at `rtsp://<host>:8554/<channel>` and, by any
WHEP client, at `http://<host>:8888/<channel>/whep` (a page that plays it in
a browser comes in phase 3);
`http://<host>:8888/api/v1/channels` says what each channel is doing. Ctrl+C
stops it. relay-pp logs to the terminal and media-pp to `logs/`.

Building needs FFmpeg's development libraries, as media-pp does — see its
[building guide](https://github.com/hash1018/media-pp/tree/main/docs/building).
