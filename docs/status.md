# What relay-pp does now

Phases 1 to 3 of [the design](design.md), as built — and of phases 4 and
5, authorization and published sources. What is planned but
not here is listed at the end.

## In one paragraph

relay-pp starts with no channel. Whatever manages the cameras — a process of
its own — makes each one over the HTTP API with its source alone, an RTSP
camera, which relay-pp pulls from then on, connecting again whenever it
drops. What reads a channel is asked for separately: an RTSP player asks
the API where to play it and is told a URL; a WebRTC player offers to its
WHEP URL; an HLS player asks for its playlist, written while it asks; a
browser opens the channel's page, which plays it by either; a recording is
attached and detached over the API, and written as HLS a player can follow
while it is written and play back afterwards.
Nothing is decoded or encoded. Nothing is kept across a restart but the
recordings: the manager makes the channels again.

## Running it

```sh
relay-pp [CONFIG]
```

`CONFIG` is a TOML file of where to listen and how to record (below). With
none named, `relay-pp.toml` is read if it is there, and the defaults are
used if it is not. Ctrl+C stops it. relay-pp logs to the terminal
(`RUST_LOG` filters it, `info` by default) and media-pp's own records go to
`logs/`.

## Configuration

Every key is optional. [`relay-pp.example.toml`](../relay-pp.example.toml)
has them all with comments.

| Key | Default | What |
|---|---|---|
| `rtsp.listen` | `0.0.0.0:8554` | where RTSP players connect |
| `rtsp.udp` | `true` | whether players may ask for RTP over UDP as well as over their connection |
| `rtsp.public_url` | none | what the URL the API gives an RTSP player begins with, `"rtsp://cctv.example.com:8554"`, where players reach this machine by another address than the API's callers; without it, the host the API request came to and `listen`'s port |
| `webrtc.listen` | `0.0.0.0:8189` | the UDP address WebRTC media goes through |
| `webrtc.candidates` | none | addresses players are told to send to — a router's public address, the machine's own where `listen` is every interface; an IP alone keeps `listen`'s port |
| `webrtc.threads` | media-pp's default | how many threads WebRTC sessions are spread over, each a UDP port of its own from `listen`'s on |
| `http.listen` | `0.0.0.0:8888` | the API, WHEP, WHIP, HLS, the player page and recordings |
| `rtmp.listen` | `0.0.0.0:1935` | where RTMP publishers connect |
| `record.dir` | `recordings` | where recordings go |
| `record.segment` | `"4s"` | how long a recorded segment is — what a player seeks to — cut at the next keyframe after it |
| `record.chunk` | `"1h"` | how long one recording directory runs, at whole multiples of it in UTC |
| `record.keep` | none | how long a recording is kept from its last write: `"30d"` |
| `record.max_size` | none | how much all recordings together may take, the oldest removed first: `"500GB"`, `"2TiB"` |
| `hls.idle` | `"30s"` | how long a channel's HLS goes on being written after the last request for its playlist or a segment |
| `convert.sound` | `"auto"` | sound a way out cannot carry: `"auto"`, converted for it; `"off"`, left out of it — see "Conversions" |
| `convert.video` | `"gpu"` | pictures a way out cannot carry: `"gpu"`, converted on a GPU and refused where none takes them; `"any"`, on the processor where no GPU does; `"off"`, that way out refused |
| `auth.api_token` | none | what the manager presents for the API, 16 characters or more — see "Who may" |
| `auth.token_secret` | none | what play tokens are signed with, HS256, 32 characters or more |
| `auth.users` | none | players by `name` and `password`, each with the channels it may `play` live, watch the `recordings` of and `publish` into: names, or patterns with `*` |

Durations are a number and `ms`, `s`, `m`, `h` or `d`; sizes a number of
bytes, or one with `KB` … `TB` (1000-based) or `KiB` … `TiB` (1024-based).
A key it does not know, a candidate that is not an address, a chunk
shorter than a segment, or a token or secret too short stops the start. So does a listener that cannot be
bound, naming which.

## Who may

Without `[auth]` anybody who reaches a port may do anything — make and
remove channels, play them, watch every recording — and relay-pp says so as
it starts. With it:

| What | Who | Presented |
|---|---|---|
| The API: make, remove, list channels; record | the manager | `Authorization: Bearer <api_token>` |
| Play a channel live — over RTSP, or WebRTC through WHEP, and ask for its RTSP URL | a user whose `play` names it, or a play token whose `play` does | RTSP: `rtsp://user:password@…` (Digest, or Basic), or `rtsp://…/cam1?token=…`; WHEP and the API: `Bearer <token>` or `Basic` |
| Watch a channel's recordings — list them, `/playback`, `/recordings/` | a user or a token whose `recordings` names it | `Bearer`, `Basic`, or `?token=` |
| Publish into a channel — over RTSP, RTMP or WHIP | a user or a token whose `publish` names it | RTSP: as a player does; RTMP: `?token=` or `?user=&pass=` after the stream key; WHIP: `Bearer` |

The API token lets its holder do anything, playing included. Anything else
is answered `401`, saying what is taken.

A **play token** is a JSON Web Token the manager signs with `token_secret`
(HS256), with any JWT library:

```json
{"exp": 1792735200, "play": ["cam1", "floor2-*"], "recordings": ["cam1"]}
```

`exp`, until when it holds in seconds since the Unix epoch, is required;
`play` and `recordings` name what it lets its holder do, as a user's do. An
HLS player given a `/playback` URL with `?token=` is given each segment's
URL with the token in it too; one following a recording's own playlist —
`/recordings/…/index.m3u8` — sends it as a header (hls.js: `xhrSetup`).

No token or password is logged, nor kept beside the configuration.

## The API

On the HTTP port. JSON in and out. A channel's name is letters, digits,
`-`, `_`, `.` and `~` — not `/`, so that it is one part of a URL's path —
and not `api`, `assets`, `playback` or `recordings`.

| | | |
|---|---|---|
| `POST /api/v1/channels` | make a channel: its source alone | `201`; `400`, `409` |
| `GET /api/v1/channels` | every channel's status | `200` |
| `GET /api/v1/channels/{name}` | one channel's status | `200`; `404` |
| `DELETE /api/v1/channels/{name}` | remove it | `204`; `404` |
| `POST /api/v1/channels/{name}/play` | where an RTSP player plays it | `200`; `400`, `404`, `409`, `503` |
| `POST /api/v1/channels/{name}/record` | begin recording it | `201`; `404`, `409` |
| `DELETE /api/v1/channels/{name}/record` | end recording it | `204`; `404` |
| `GET /api/v1/recordings/{name}` | its recordings | `200` |
| `GET /playback/{name}/index.m3u8` | a stretch of its recordings, as one playlist | `200`; `400`, `404` |

Besides the API, a channel's players' own URLs — `/{name}/whep`,
`/{name}/index.m3u8` and its segments, and `/{name}`, the page that plays
it — are under "Players" below.

### `POST /api/v1/channels` — make one

```json
{ "name": "cam1", "source": "rtsp://user:pass@10.0.0.21/stream1", "transport": "tcp" }
{ "name": "studio", "publish": "rtmp" }
```

| Field | | |
|---|---|---|
| `name` | required | what it goes by: in the API, `rtsp://host:8554/<name>`, `/<name>/whep` |
| `source` | this or `publish` | a camera to pull: an `rtsp://` or `rtsps://` URL |
| `publish` | this or `source` | the protocol a publisher pushes the stream over: `"rtmp"`, `"rtsp"` or `"whip"` — see "A channel's publisher" |
| `transport` | `"tcp"` | how the camera is pulled: `"tcp"`, RTP on the RTSP connection, which loses nothing; or `"udp"` |
| `convert` | the server's `[convert]` | its own say over it: `{"sound": "off", "video": "any"}`, each left out the server's |

`201` with its status, `Location` its URL; `400` for a name or source that
cannot be used, both `source` and `publish` or neither, a field missing or
unknown, or not JSON; `409` for a name taken. It answers at once; the camera
is opened afterwards, and pulled from then on, played or not — or a
publisher taken from then on.

### `DELETE /api/v1/channels/{name}` — remove one

`204` once it is off every listener, its recording finished and its camera
let go of: at once while the camera is being opened or waited for, and once
its connection has stopped while it is live — up to 5 seconds for a camera
gone silent. `404` if there is none. The name is free again at once, and a
removal holds up no other request. A source is changed by removing the
channel and making it again.

### `GET /api/v1/channels` and `GET /api/v1/channels/{name}`

`{"channels": [ … ]}` by name, or one channel; `404` for one that is not
there. Each is:

| Field | What |
|---|---|
| `name` | its name |
| `source` | its URL, with any user name and password cut out |
| `state` | `connecting`, `live`, or `waiting` — failed, and connecting again after a wait |
| `since_ms` | since when, in milliseconds since the Unix epoch |
| `error` | why it last failed, while it is not live |
| `connections` | how many times it has gone live |
| `client` | how its stream comes in: `"rtsp"` for a camera pulled, the publisher's protocol for one pushed; none before it first goes live |
| `readers` | players and recordings reading its stream as it came now; what reads a conversion of it counts as one, the conversion |
| `bytes` | what it has been sent, over every connection |
| `sinks` | what reads it: `"rtsp"` and `"webrtc"` once a player has asked for it so, `"record"` while it is recorded |
| `recording` | the playlist of the recording being written, while one is |
| `record_error` | why its last recording failed, until one is written again |
| `conversions` | each track converted now, and to what: `"1:opus"` — see "Conversions" |
| `convert` | what may be converted for it, its own say over the server's: `{"sound": "auto", "video": "gpu"}` |

Credentials never reach an answer or a log line.

## A channel's publisher

A channel made with `publish` takes a publisher over that one protocol,
from when it is made until it is removed:

| `publish` | The publisher pushes to | As |
|---|---|---|
| `"rtmp"` | `rtmp://<host>:1935/live/<name>` — OBS's server `rtmp://<host>:1935/live` and stream key `<name>` | OBS, FFmpeg, an encoder |
| `"rtsp"` | `rtsp://<host>:8554/<name>`, `ANNOUNCE` and `RECORD` — the URL its players play | an encoder, FFmpeg |
| `"whip"` | `POST http://<host>:8888/<name>/whip`, an SDP offer; `DELETE` the `Location` it answers to leave | a browser, OBS's WHIP output |

- One publisher at a time: another is refused while one publishes — `409`
  over WHIP — and so is one over another protocol than the channel's.
- Its status is `awaiting_publisher` until one publishes, `live` while it
  does, and `awaiting_publisher` again once it leaves; `connections` counts
  its publishers, and `client` names the protocol. Players stay connected
  across a publisher leaving and the next arriving, as across a camera
  dropping.
- What it pushes is played and recorded as a camera's is, converted for
  each way out that cannot carry it: OBS's AAC reaches WebRTC players as
  Opus; a browser's Opus is recorded as it came.
- The server taking the publisher writes what it sends into a broadcast of
  the channel's source's own, which the source reads into the channel as it
  would a camera's connection: the channel has one source either way.

## A channel's camera

- Opened when the channel is made, with a 5-second timeout, which also
  bounds how long it may go without sending before it is taken to be gone.
- Its pictures and sound are kept; a data or metadata stream is left out.
- Pulled by media-pp's RTSP client, `RtspSource`, over TCP or UDP: H.264
  and H.265 — B-frames given the decoding times RTP does not carry —
  VP8, VP9, AAC, Opus and G.711, with the credentials the URL carries
  answering a Digest or Basic challenge. A track in another codec is
  left out; a camera with none it takes fails as one that cannot be
  opened. `rtsps://` is not taken.
- When it fails it is opened again after 1 second, then 2, 4 … up to 30,
  each spread ±20% so cameras one outage dropped are not all opened again
  in the same instant. A camera that stayed live for 10 seconds starts again
  from 1.
- Players stay connected across a drop and go on from the next keyframe once
  the camera is back, their timestamps carrying on.

## Players

A channel is registered on a players' listener when a player first asks for
it by that protocol, and stays registered until the channel goes. Being
registered costs nothing while nobody plays; the camera is pulled either
way.

- **RTSP.** `POST /api/v1/channels/{name}/play` with `{"protocol":"rtsp"}`
  answers `{"url": "rtsp://<host>:8554/<name>"}`, the host the request came
  to unless `rtsp.public_url` says otherwise. An RTSP player connects to the
  RTSP port and not to the API, so what is to play a channel over RTSP asks
  first; before, the RTSP listener does not know the channel. RTP over TCP
  or UDP; any codec the camera sends.
- **WebRTC through WHEP** at `http://<host>:8888/<name>/whep`, by any WHEP
  client, the page below among them: `POST` an `application/sdp` offer,
  `201` with the answer and `Location: /<name>/whep/<session>`; `DELETE`
  that to leave; `OPTIONS` says what it takes. The offer is the asking:
  there is nothing to call first. Sound in a codec WebRTC does not carry is
  converted to Opus for it — see "Conversions" below.
- **HLS** at `http://<host>:8888/<name>/index.m3u8`: fragmented MP4, a
  sliding window of six segments of a second each, cut at the next
  keyframe — a player starts two or three of them behind the camera. A
  channel's HLS is written from the first request for its playlist, which
  waits for its first segment, and goes on while players ask for the
  playlist or a segment; `hls.idle` after the last request it stops, and
  its files, under the system's temporary directory, are removed. What the
  camera sends goes as it came wherever fMP4 holds it, as a recording does,
  and is converted where it does not. A player that comes with a
  `?token=` finds it in the URL of each segment the playlist names.
- **The page** at `http://<host>:8888/<name>`: plays the channel in a
  browser over WebRTC, and over HLS where WebRTC does not get through or is
  chosen; and its recordings, listed or between two times. It carries the
  `?token=` it was opened with to whatever it asks for. It is served to
  anybody — what it plays is not — and plays HLS through hls.js 1.7.3,
  served by relay-pp itself (`/assets/hls.light.min.js`, Apache-2.0), so
  that it plays on a network that reaches no CDN.

Each waits up to 10 seconds for the camera to be live — a channel just
made, or a camera connecting again — and for what is converted for its
players to begin, and answers saying why where it cannot play:

| Status | Why | |
|---|---|---|
| `404` | no such channel | |
| `503`, `Retry-After: 5` | the camera not live yet, or a conversion still beginning | ask again soon |
| `503`, `Retry-After: 30` | pictures to convert on the GPU alone, and no GPU encoder free | ask again later |
| `409` | pictures its players cannot carry, on a channel that converts none | asking again does not help |
| `503`, `Retry-After: 2` | HLS failed to be written, and is begun again | ask again soon |
| `500` | a conversion or a listener failed | see the log |

A channel keeps the last second of its stream while something reads it, as
room for a player to fall behind — about half a megabyte of a 4 Mbit/s
camera — and nothing while nothing does. A player joining one already read
starts at a keyframe of that second, or waits for the next one; the first
to join one nobody reads waits for the camera's next keyframe: up to its
keyframe interval before its first picture.

## Recording

`POST /api/v1/channels/{name}/record` begins recording a channel, from the
newest keyframe it holds where it is live; `DELETE` ends it, answering once
the recording under way is finished. A channel is recorded until told
otherwise, across its camera dropping and coming back.

What is written:

```
<record.dir>/<name>/<YYYY-MM-DD>/<HH-MM-SS>/    in UTC: when it began
    index.m3u8      an HLS event playlist, ended (#EXT-X-ENDLIST) when the recording is
    media.m4s       fragmented MP4: the initialization section, then every segment
```

- A recording directory — a chunk — runs until the next whole multiple of
  `record.chunk` in UTC, and another begins. One also ends as the camera
  drops, and another begins as it comes back, so no recording runs from one
  connection into the next, whose picture may differ.
- The playlist lists each segment by its byte range in the one media file:
  an hour of four-second segments is one file, not nine hundred.
- A player follows a recording being written by its own playlist, at
  `/recordings/<name>/<YYYY-MM-DD>/<HH-MM-SS>/index.m3u8` — a few segments
  behind live. Everything under `/recordings/` is served as it is, ranges
  included.
- What is cut off — the process dying, power lost — keeps every segment the
  playlist lists; what is lost is the segment under way.
- A recording that fails — a full disk — says why in `record_error` and is
  begun again 5 seconds later.
- Each recording is a media-pp pipeline on a thread of its own, so a disk
  slow to take a write holds up that camera's recording and no other.
- A track is recorded as it came wherever fMP4 holds it — Opus, VP9 and
  AV1 among them — and converted where it does not: VP8 to H.264, G.711
  to AAC (Safari's own player plays no Opus; hls.js does) — see
  "Conversions" below.

### `GET /api/v1/recordings/{name}`

`{"recordings": [ … ]}`, oldest first, whether or not the channel is there
now: each `start` (RFC 3339), `duration_s`, `bytes`, whether it is
`recording` still, and its `playlist`.

### `GET /playback/{name}/index.m3u8?from=…&to=…`

A finished (VOD) HLS playlist of every segment that overlaps `from`..`to`,
RFC 3339 times in UTC (`2026-10-23T14:00:00Z`); from the first recording,
and to now, where either is left out. Each recording follows the last with
a discontinuity between and the time it began (`#EXT-X-PROGRAM-DATE-TIME`).
`400` for a time it cannot read, `404` where nothing was recorded then.

A recording's start is when its directory was made, and its first picture
the keyframe it began at, which arrived up to a keyframe interval before:
the times given are late by up to that.

### Retention

With `record.keep` or `record.max_size` set, recordings are swept at start
and every minute after: those last written longer ago than `keep` go, then
the oldest until all together fit in `max_size`, and the directories left
empty. A recording being written is never taken, whatever it costs.

## Conversions

Each way out carries some codecs and not others, and a channel's stream
goes out each way whatever its camera sends: a track one cannot carry is
converted for it, and what it can carry goes as it came. What each carries
is what its media-pp server or muxer sends, narrowed to what its players
play.

| Out | Carries | Converted to |
|---|---|---|
| RTSP | what RTP carries: H.264, H.265, VP8, VP9, MJPEG, MPEG-4; AAC, Opus, G.711, MP3 and more — not AV1 nor AC-3 | H.264; AAC |
| WebRTC | H.264 without B-frames, VP8, VP9, AV1; Opus, G.711 | H.264; Opus |
| Recording | what fMP4 holds: H.264, H.265, VP9, AV1, MJPEG; AAC, Opus, MP3 — not VP8 nor G.711 | H.264; AAC |

A camera over RTSP sends nothing RTSP cannot carry; what is converted for
RTSP players is for a source of another kind, which relay-pp has none of
yet.

- A track is converted once however many ways out need it, beside what goes
  on unconverted, on the same timeline: a camera's pictures and its sound
  converted stay in time.
- What is converted for WebRTC is converted while players play it, and
  stops once none has for 30 seconds; a player asking again starts it again.
  What is converted for RTSP players, who reach the listener without asking,
  from when the first asks for the channel's URL; for a recording, for as
  long as the channel is recorded. A channel whose ways out carry what its
  camera sends costs nothing here.
- Where what a way out reads changes — a camera that comes back sending
  other codecs — its players' listener is moved to it at once; those
  playing go on with what they had until it ends.
- `conversions` in a channel's status says what is converted now: `"1:opus"`
  is track 1 converted to Opus.
- Pictures are converted on the GPU where one takes them, decoded and
  encoded there without being copied out: D3D11 on Windows — NVENC, or the
  Intel or AMD encoder Media Foundation offers — and NVIDIA's NVDEC and
  NVENC, or else Vulkan Video, on Linux. A codec the GPU cannot decode — VP8
  on many — is decoded on the processor and still encoded on the GPU. With
  no GPU encoder, or every session of one taken (how many a driver opens
  varies: ten 1080p ones opened at once on a GeForce RTX 3050), it is
  refused, or encoded on the processor where `"any"` says so (below).
  H.264, a keyframe a second, about a tenth of a bit a pixel: 3.6 Mbit/s for
  1080p at 25.
- Sound is decoded and encoded on the processor, 48 kHz, one or two
  channels.
- What is converted is the server's `[convert]`, and each channel's own
  `convert` over it. Sound is converted unless it says `"off"`, which leaves
  it out of a way out that cannot carry it. Pictures are converted on a GPU
  unless it says `"any"`, which converts them on the processor where no GPU
  takes them — a core or two a camera — or `"off"`, which converts none.
- Where a conversion cannot be made or is not to be, a player is answered
  saying why (see "Players"), and a recording says so in `record_error`.

## Stopping

A stop takes no more requests, finishes every recording, lets go of every
camera together — however many there are, no longer than the slowest live
connection takes to stop, and what is left of opens under way is given 2
seconds — then closes the listeners.

## Threads

A camera pulled is a media-pp pipeline of its own: a thread writing the
camera's stream into the channel's broadcast, and one reading the
connection — and one for each track's UDP socket, pulled over UDP. Its
open, which blocks, borrows a thread of the runtime's blocking pool while
it runs. Each recording is a
pipeline of its own, and so is each channel's HLS while it is written, and
what a channel converts, while it does: a thread reading the channel's
stream, and one for each track converted.
Everything else that waits — the API, WHEP, each channel's source, which
waits on its connection and before connecting again, its hub, which decides
what is converted, each listener registration, which follows what its
players read, each recorder and each HLS writer — is a task on the same
runtime. The RTSP and WebRTC listeners have
threads of their own, shared by every channel.

## Verified

`cargo test` runs on the loopback alone, with a camera of media-pp's own:
made with its source alone; asked for over RTSP and played at the URL the
API gave, and played over WHEP; followed across the camera going away and
coming back; a request to play waiting for a camera that never answers and
saying so; channels made, refused, removed and made again under the same
name; a removal of a camera being opened answering at once and holding up
no add; a stop with ten cameras opening waiting for none of them. Played
and followed alike pulled over TCP and over UDP; a camera of two tracks
relayed with both; and one sending B-frames relayed, said to reorder. A camera's AAC played
over WebRTC as Opus and over RTSP as it came; its Opus recorded as it came, in
time with its pictures; a conversion stopping once no WebRTC player has
played it for a while, and made again for the next; what WebRTC players
read moved back to the channel's own broadcast when its camera came back
sending Opus. A camera's H.265
played over WebRTC as H.264 and over RTSP as it came; its VP8 recorded as
H.264 in time with its sound. A channel's own `convert` over the server's: its
sound left out of WebRTC, its pictures refused to WebRTC saying why. Recorded
in four-second chunks of one-second segments, each one media file read by
byte range; listed; a stretch across chunks played back as one playlist and
read back by FFmpeg through the server; a recording ended and begun again as
the camera dropped and came back, taken off and put back on, and ended by a
stop. Retention, playlist reading and the playback playlist by unit tests.
Played over HLS live, FFmpeg reading pictures and sound through the server
as the playlist slides; the HLS let go of once nobody asked, nothing of it
served after, and written again when asked for; a token in an HLS player's
URL carried into each segment's, a segment refused without it; the page
and hls.js served.

By hand, in phase 1: a file served by media-pp's `rtsp_server` example as
the camera, FFmpeg playing over RTSP (TCP and UDP), two WHEP players through
media-pp's `webrtc_load`, and reconnection after the camera was gone for a
minute.

What it costs against MediaMTX on Windows is in [benchmarks.md](benchmarks.md).

By hand, in phase 3: the page in a browser (the Claude desktop app's,
Chromium) against media-pp's `rtsp_server` serving a 1080p H.264 and Opus
file: played over WebRTC, over HLS — two seconds behind the camera — and a
recording played back from its list.

Not yet: Ctrl+C on the binary, a real IP camera, Safari's own HLS, and a
recording of hours.

## Not here yet

| | Phase |
|---|---|
| Hooks, metrics, a manager asked over HTTP who may play | 4 |
| On-demand sources, forwarding, event recording | 5 |
| Packaging, a soak of a hundred cameras | 6 |

Also not here: HTTPS — a token and a Basic password cross the network as
they are, so put relay-pp behind a proxy that terminates TLS where the
network is not trusted — changing a channel in place, taking a channel off a
players' listener without removing it, a pulled source other than an
RTSP camera, SRT. A camera that comes back with other tracks than it had — another
codec, sound added or gone — ends its RTSP players' sessions, whose
description no longer holds. One that changes its picture size partway
through a connection is carried on to its players; its recording, which
began as the connection did and is refused the new picture, is finished,
and the camera connected again at once — a moment's break for players — so
that a recording of the new picture begins with the new connection. That
last is unit-tested only: no test here has a camera that changes its
picture.
