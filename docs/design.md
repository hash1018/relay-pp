# relay-pp — design

Status: agreed scope, 2026-10-23. Phases 1 and 2 are built — what they do is in
[status.md](status.md).

relay-pp takes streams in by whatever protocol they come, hands each out by
every protocol a viewer asks for, and records what it is told to — the role
MediaMTX plays — built on [media-pp](https://github.com/hash1018/media-pp).

## Decisions

| | |
|---|---|
| Repository | its own, beside media-pp |
| First deployment | CCTV monitoring on a closed network |
| HTTP and API | axum on tokio, one port — WHIP and WHEP included |
| Configuration | TOML for the server; channels made over the API by whatever manages the inputs, none kept across a restart |
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

relay-pp is mostly policy: configuration, the life of each channel,
reconnecting, recording files and their retention, HTTP, and reporting.

## The model

```
            ┌────────────────────── channel "cam1" ────────────────────────┐
 source ──▶ │ Source ──▶ Broadcast ──┬──▶ RtspServer   (players)           │
 (pull or   │ (reconnects)           ├──▶ WebRtcServer (players)           │
  publish)  │                        ├──▶ HLS (live, over HTTP)            │
            │                        ├──▶ recorder (segments + retention)  │
            │                        └──▶ forward (push out)               │
            └──────────────────────────────────────────────────────────────┘
```

A **channel** is a name, one source, one `Broadcast`, and the sinks that read
it: the unit of configuration, authorization, statistics and recording. It
is made with its source alone; what reads it is asked for separately — a
player by its protocol, a recording over the API — and attached as it is.

**Sources**:

- *Pulled* — relay-pp opens the URL itself: `rtsp://` (cameras), `rtmp://`,
  a WHEP URL, a file to loop. Always on, or **on demand**: opened when the
  first reader arrives, closed after the last has been gone for a while.
- *Published* — the channel waits for a publisher on any protocol the server
  takes (RTSP RECORD, RTMP, WHIP). One at a time; a second is refused, or
  replaces the first (a setting).

A pulled source reconnects by itself — back-off from one second to thirty,
each failure's reason logged and kept in the channel's status. The `Broadcast`
outlives every reconnection, so players stay connected and go on at the next
keyframe.

**Sinks**: a players' listener, registered when a player first asks for the
channel by that protocol — an RTSP player through the API, which answers
with the URL to open since an RTSP player never reaches the HTTP port, a
WebRTC player by its WHEP offer — and kept until the channel goes;
**recording**, attached and detached over the API; **forwarding** per
channel.

## Recording

As HLS, so a recording is played as it is written and after, by any HLS
player and over plain HTTP:

- A chunk is a directory, `<dir>/<channel>/<YYYY-MM-DD>/<HH-MM-SS>/` in UTC,
  of an event playlist and one fragmented MP4 file, every segment (default
  4 s, cut at the next keyframe) a byte range of it — an hour one file, not
  nine hundred. A chunk runs to the next whole multiple of its length in UTC
  (default an hour), and ends as the camera drops, another beginning as it
  comes back: no chunk runs from one connection into the next.
- What is cut off keeps every segment its playlist lists.
- A camera that changes its picture size within a connection is connected
  again, so that a new chunk describes it: an MP4 refuses the change.
- Retention: chunks last written longer ago than a period, then the oldest
  beyond a size, for all.
- Playback: `/playback/{channel}/index.m3u8?from=&to=` answers a VOD playlist
  across the chunks, a discontinuity between each and the time each began.
- Event recording (later): the last N seconds kept in a `ReplayBuffer` and
  written out with what follows when the API or a hook says so.

## HTTP

axum, one listener:

| Route | What |
|---|---|
| `/api/v1/…` | channels — made, removed, played, recorded, their status — sessions, recordings, kick |
| `/playback/{channel}/index.m3u8` | recordings by time |
| `/recordings/…` | the recordings' files, as written |
| `/{channel}` | a page that plays the channel in a browser |
| `/{channel}/index.m3u8` and segments | HLS live |
| `/{channel}/whep`, `/{channel}/whip` | WebRTC play and publish |
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

One TOML file for the server — where it listens, where it records, who may
play — read at start. Optional: every value has a default.

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
segment = "4s"
chunk = "1h"
keep = "30d"
max_size = "2TB"

[[auth.users]]
name = "operator"
password = "..."
play = ["*"]
```

**Channels are not in it.** Whatever manages the inputs — a VMS, a camera
registry, a process of its own — makes and removes them over the API, and
relay-pp keeps none across a restart: it starts empty, and the manager,
seeing that in `GET /api/v1/channels`, makes them again.

```
POST   /api/v1/channels                {"name":"cam1","source":"rtsp://10.0.0.21/stream1",
                                        "transport":"tcp"}   its source alone, pulled from now
POST   /api/v1/channels/{name}/play    {"protocol":"rtsp"} → {"url":"rtsp://<host>:8554/cam1"}
POST   /{name}/whep                    a WebRTC player's offer: no question first
POST   /api/v1/channels/{name}/record  begin recording it
DELETE /api/v1/channels/{name}/record  end it
DELETE /api/v1/channels/{name}
GET    /api/v1/channels[/{name}]
GET    /api/v1/recordings/{name}
```

A player's request waits up to ten seconds for the camera to be live. A
source is changed by removing the channel and making it again; `on_demand`
comes with its phase. [status.md](status.md) has every request and answer
as built.

Secrets never reach a log line or an API answer; a URL with credentials is
logged with them cut out.

## Reporting

- Every channel's status: its source (connecting, live, failed and why, waiting
  for a publisher), since when, readers by protocol, bit rate, recording.
- Events — a channel goes live or drops, a publisher comes or goes, a segment is
  finished — over the API (a WebSocket or a long poll) and as **hooks**: a
  command or an HTTP call the configuration names (`on_ready`,
  `on_not_ready`, `on_record_segment`).
- Logs: media-pp's own file log, and relay-pp's beside it.

## Life of the process

- Start: read the configuration, bind every listener (one that cannot stops
  the start, saying why); no channel until the API makes one.
- Stop: take no new sessions, finish every recording so its last segment is
  whole, close the listeners, then the sources — bounded, never a fixed
  sleep.
- One binary; a systemd unit and a Windows service.

## Phases

1. **Core**: the configuration; channels made and removed over the API, with
   pulled RTSP sources, always on, reconnecting; RTSP players, and WebRTC
   players through WHEP on the axum port; the status API; logs. The CCTV
   case's live view.
2. **Recording**: as HLS, attached over the API; retention, playback.
3. **The player page and HLS live.**
4. **Auth, hooks, metrics.**
5. **Published sources** (RTSP, RTMP, WHIP), on-demand sources, forwarding,
   event recording.
6. **Packaging, and a soak**: a hundred pulled cameras for a day.

Each phase ends with a run against real clients — VLC, ffplay, a browser, and
for phase 5 OBS — and a measurement where it matters: phase 1 and 6 how many
cameras a machine pulls and records, the fan-in media-pp's streaming
benchmarks have not measured.

## Borrowed and avoided

From MediaMTX: on-demand sources, hooks, a playback server, a REST API.
From the CLAMOS server before media-pp: reconnecting sources with state
events, event recording with a pre-buffer, systemd packaging — and not its
polling loops,
credentials in logs and committed configuration, an unauthenticated API, or
a fixed sleep at shutdown.
