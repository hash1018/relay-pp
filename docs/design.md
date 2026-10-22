# relay-pp — design

Status: agreed scope, 2026-10-23. Nothing is built yet.

relay-pp takes streams in by whatever protocol they come, hands each out by
every protocol a viewer asks for, and records what it is told to — the role
MediaMTX plays — built on [media-pp](https://github.com/hash1018/media-pp).

## Decisions

| | |
|---|---|
| Repository | its own, beside media-pp |
| First deployment | CCTV monitoring on a closed network |
| HTTP and API | axum on tokio, one port — WHIP and WHEP included |
| Configuration | TOML |
| User interface | the API and a player page; no dashboard for now |

## What it is for

- **CCTV monitoring on a closed network** — first. Tens to hundreds of
  cameras pulled over RTSP, a handful of operators watching (browsers, VMS
  clients), everything recorded around the clock, nothing reaching out of the
  network.
- **Live streaming** — later. A few publishers (OBS over RTMP or WHIP, a
  phone over WHIP), many viewers over WebRTC, RTSP or HLS, recorded per
  publication.

Not in it: transcoding (every viewer gets the stream as published; a second
rendition is a second input — a camera's sub-stream), clustering, a user
database beyond what authorization needs.

## What media-pp gives it

| Need | media-pp |
|---|---|
| One input, many readers that never wait on one another, surviving the input dropping and coming back | `Broadcast` (`retain`, `stats`) |
| RTSP players over TCP and UDP; RTSP publishers | `RtspServer` (`rtsp-server`) |
| WebRTC players (WHEP) and publishers (WHIP), ICE candidates, STUN/TURN advertised | `WebRtcServer` (`webrtc`) |
| RTMP publishers | `RtmpServer` (`rtmp-server`) |
| Pulling a camera or another server | `RtspSource`, `WhepSource`, `FileDemuxer` (any FFmpeg URL) |
| Recording | `BroadcastSource` → `SegmentedFileMuxer` / `HlsMuxer`; `ReplayBuffer` for the last N seconds |
| Pushing out | `RtmpMuxer`, `RtspMuxer`, `WhipPublisher` |
| Who may play or publish what | `Authorize` (Basic, Digest, Bearer, RTMP query tokens) |
| Numbers | `Broadcast::stats`, `RtspServerHandle::players`, `WebRtcServerHandle::session_stats` |
| Capacity | media-pp's `docs/benchmarks/streaming` |

The server parts are not on crates.io yet (media-pp 0.4.1 predates them), so
relay-pp depends on media-pp by git, pinned to a revision, with a local
`[patch]` (a `--config` override, as obs-rs does) while both are worked on.

relay-pp is mostly policy: configuration, the life of each path,
reconnecting, recording files and their retention, HTTP, and reporting.

## The model

```
            ┌──────────────────────── path "cam1" ─────────────────────────┐
 source ──▶ │ ingest ──▶ Broadcast ──┬──▶ RtspServer   (players)           │
 (pull or   │ (reconnects)           ├──▶ WebRtcServer (players)           │
  publish)  │                        ├──▶ HLS (live, over HTTP)            │
            │                        ├──▶ recorder (segments + retention)  │
            │                        └──▶ forward (push out)               │
            └──────────────────────────────────────────────────────────────┘
```

A **path** is a name, one source, one `Broadcast`, and what is done with it:
the unit of configuration, authorization, statistics and recording.

**Sources**:

- *Pulled* — relay-pp opens the URL itself: `rtsp://` (cameras), `rtmp://`,
  a WHEP URL, a file to loop. Always on, or **on demand**: opened when the
  first reader arrives, closed after the last has been gone for a while.
- *Published* — the path waits for a publisher on any protocol the server
  takes (RTSP RECORD, RTMP, WHIP). One at a time; a second is refused, or
  replaces the first (a setting).

A pulled source reconnects by itself — back-off from one second to thirty,
each failure's reason logged and kept in the path's status. The `Broadcast`
outlives every reconnection, so players stay connected and go on at the next
keyframe.

**Outputs**: every player protocol the server runs, unless a path turns one
off; HLS where enabled. **Recording** and **forwarding** are per path.

## Recording

- Segments of a configured length (default 60 s), cut on keyframes, in
  `<dir>/<path>/<YYYY-MM-DD>/<HH-MM-SS>.<ext>`.
- Fragmented MP4 by default, readable even when the process dies mid-segment;
  MPEG-TS for a path whose source may change its picture size partway (a
  WebRTC publisher), which an MP4 refuses.
- Retention: segments older than a period, or the oldest beyond a size, per
  path or for all.
- Playback: `/playback/{path}?from=&to=` answers an HLS VOD playlist over the
  segments, or one file of them.
- Event recording (later): the last N seconds kept in a `ReplayBuffer` and
  written out with what follows when the API or a hook says so.

## HTTP

axum, one listener:

| Route | What |
|---|---|
| `/api/v1/…` | paths and their status, sessions, recordings, reload, kick |
| `/playback/{path}` | recordings by time |
| `/{path}` | a page that plays the path in a browser |
| `/{path}/index.m3u8` and segments | HLS live |
| `/{path}/whep`, `/{path}/whip` | WebRTC play and publish |
| `/metrics` | Prometheus text |

WHIP and WHEP are axum routes over `WebRtcServerHandle::play`, `publish`
and `close`, not `WebRtcServer::serve_http`'s listener of its own: one port
to open in a firewall, one certificate, one place that decides who may play
and publish. relay-pp then answers what `serve_http` does — `201` with the
answer and a `Location` naming the session, the STUN and TURN `Link`
headers, `OPTIONS`, `DELETE` — and keeps session URLs unguessable.

HTTPS where a certificate is configured — a browser needs it to publish from
its camera, not to play.

## Configuration

One TOML file, read at start and again on `POST /api/v1/reload`: a reload
adds, changes and removes paths without touching the others.

```toml
[rtsp]
listen = "0.0.0.0:8554"
udp = true

[rtmp]
listen = "0.0.0.0:1935"

[webrtc]
listen = "0.0.0.0:8189"
candidates = ["192.168.0.10"]
threads = 4

[http]
listen = "0.0.0.0:8888"

[record]
dir = "/var/lib/relay-pp/record"
segment = "60s"
keep = "30d"
format = "fmp4"

[[auth.users]]
name = "operator"
password = "..."
play = ["*"]

[paths.cam1]
source = "rtsp://10.0.0.21/stream1"
record = true

[paths.cam2]
source = "rtsp://10.0.0.22/stream1"
on_demand = true

# One entry for many cameras: the path's name picks the address.
[paths."~^cams/(\\d+)$"]
source = "rtsp://10.0.1.$1/main"
record = true
```

Secrets never reach a log line or an API answer; a URL with credentials is
logged with them cut out.

## Reporting

- Every path's status: its source (connecting, live, failed and why, waiting
  for a publisher), since when, readers by protocol, bit rate, recording.
- Events — a path goes live or drops, a publisher comes or goes, a segment is
  finished — over the API (a WebSocket or a long poll) and as **hooks**: a
  command or an HTTP call the configuration names (`on_ready`,
  `on_not_ready`, `on_record_segment`).
- Logs: media-pp's own file log, and relay-pp's beside it.

## Life of the process

- Start: read the configuration, bind every listener (one that cannot stops
  the start, saying why), open every always-on pulled source.
- Stop: take no new sessions, finish every recording so its last segment is
  whole, close the listeners, then the sources — bounded, never a fixed
  sleep.
- One binary; a systemd unit and a Windows service.

## Phases

1. **Core**: the configuration; paths with pulled RTSP sources, always on,
   reconnecting; RTSP players, and WebRTC players through WHEP on the axum port; the status API; logs. The
   CCTV case's live view.
2. **Recording**: segments, retention, playback.
3. **The player page and HLS live.**
4. **Auth, hooks, metrics.**
5. **Published sources** (RTSP, RTMP, WHIP), on-demand sources, forwarding,
   event recording, reload.
6. **Packaging, and a soak**: a hundred pulled cameras for a day.

Each phase ends with a run against real clients — VLC, ffplay, a browser, and
for phase 5 OBS — and a measurement where it matters: phase 1 and 6 how many
cameras a machine pulls and records, the fan-in media-pp's streaming
benchmarks have not measured.

## Borrowed and avoided

From MediaMTX: path regexes, on-demand sources, hooks, a playback server, a
REST API and reload. From the CLAMOS server before media-pp: reconnecting
sources with state events, event recording with a pre-buffer, restoring
what was running at boot, systemd packaging — and not its polling loops,
credentials in logs and committed configuration, an unauthenticated API, or
a fixed sleep at shutdown.
