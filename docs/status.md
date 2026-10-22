# What relay-pp does now

Phase 1 of [the design](design.md), as built. What is planned but not here
is listed at the end.

## In one paragraph

relay-pp starts with no channel. Whatever manages the cameras — a process of
its own — makes each one over the HTTP API, naming an RTSP camera. relay-pp
pulls that camera for as long as the channel is there, connecting again
whenever it drops, and hands the stream out, as the camera sent it, to RTSP
players and to WebRTC players through WHEP. Nothing is decoded or encoded.
Nothing is kept across a restart: the manager makes the channels again.

## Running it

```sh
relay-pp [CONFIG]
```

`CONFIG` is a TOML file of where to listen (below). With none named,
`relay-pp.toml` is read if it is there, and the defaults are used if it is
not. Ctrl+C stops it. relay-pp logs to the terminal (`RUST_LOG` filters it,
`info` by default) and media-pp's own records go to `logs/`.

## Configuration

Every key is optional. [`relay-pp.example.toml`](../relay-pp.example.toml)
has them all with comments.

| Key | Default | What |
|---|---|---|
| `rtsp.listen` | `0.0.0.0:8554` | where RTSP players connect |
| `rtsp.udp` | `true` | whether players may ask for RTP over UDP as well as over their connection |
| `webrtc.listen` | `0.0.0.0:8189` | the UDP address WebRTC media goes through |
| `webrtc.candidates` | none | addresses players are told to send to — a router's public address, the machine's own where `listen` is every interface; an IP alone keeps `listen`'s port |
| `webrtc.threads` | media-pp's default | how many threads WebRTC sessions are spread over, each a UDP port of its own from `listen`'s on |
| `http.listen` | `0.0.0.0:8888` | the API and WHEP |

A key it does not know, or a candidate that is not an address, stops the
start. So does a listener that cannot be bound, naming which.

## Channels: the API

On the HTTP port. JSON in and out.

### `POST /api/v1/channels` — make one

```json
{ "name": "cam1", "source": "rtsp://user:pass@10.0.0.21/stream1", "transport": "tcp" }
```

| Field | | |
|---|---|---|
| `name` | required | what players ask for: `rtsp://host:8554/<name>`, `/<name>/whep`. Letters, digits, `-`, `_`, `.`, `~`, in parts between single `/` (`floor2/cam3`) |
| `source` | required | an `rtsp://` or `rtsps://` URL |
| `transport` | `"tcp"` | how the camera is pulled: `"tcp"`, RTP on the RTSP connection, which loses nothing; or `"udp"` |

| Answer | When |
|---|---|
| `201` | made and started; the body is its status, `Location` its URL |
| `400` | a name or source that cannot be used, a field missing or unknown, not JSON |
| `409` | a channel of that name is there already |

It answers at once. The camera is opened afterwards, and its status says how
that goes.

### `DELETE /api/v1/channels/{name}` — remove one

`204` once it is off every listener and its camera is let go of — up to
5 seconds if an open is under way. `404` if there is none. The name is free
again at once, while the old camera is still being let go of, and a removal
waiting on its camera holds up no other request. A source is changed by
removing the channel and making it again.

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
| `readers` | players reading it now |
| `bytes` | what it has been sent, over every connection |
| `sinks` | where it is played: `["rtsp", "webrtc"]` |

Credentials never reach an answer or a log line.

## A channel's camera

- Opened with a 5-second timeout, which also bounds how long it may go
  without sending before it is taken to be gone.
- Its pictures and sound are kept; a data or metadata stream is left out.
- When it fails it is opened again after 1 second, then 2, 4 … up to 30,
  each spread ±20% so cameras one outage dropped are not all opened again
  in the same instant. A camera that stayed live for 10 seconds starts again
  from 1.
- Players stay connected across a drop and go on from the next keyframe once
  the camera is back, their timestamps carrying on.

## Players

- **RTSP** at `rtsp://<host>:8554/<channel>`, RTP over TCP or UDP. Any codec
  the camera sends.
- **WebRTC through WHEP** at `http://<host>:8888/<channel>/whep`, by any WHEP
  client (a page in relay-pp comes in phase 3): `POST` an `application/sdp`
  offer, `201` with the answer and `Location: /<channel>/whep/<session>`;
  `DELETE` that to leave; `OPTIONS` says what it takes. `404` for no such
  channel, `503` while it has nothing a WebRTC player can be sent — not live
  yet, or no track in a codec WebRTC carries (H.264, H.265, VP8, VP9, AV1,
  Opus, G.711). A camera's AAC sound is left out for WebRTC players; a
  browser plays H.264 without B-frames.

## Stopping

A stop takes no more requests, lets go of every camera together — 5 seconds
at most, however many there are — then closes the listeners.

## Verified

`cargo test` runs on the loopback alone, with a camera of media-pp's own:
pulled, played over RTSP and WHEP, followed across the camera going away and
coming back; channels made, refused, removed and made again under the same
name over the API; a removal not holding up an add; a stop bounded by one
camera timeout with ten cameras opening. By hand: a file served by media-pp's
`rtsp_server` example as the camera, FFmpeg playing over RTSP (TCP and
UDP), two WHEP players through media-pp's `webrtc_load`, and reconnection
after the camera was gone for a minute.

Not yet: Ctrl+C on the binary, a real IP camera, a browser through WHEP, and
how many cameras one machine pulls.

## Not here yet

| | Phase |
|---|---|
| Recording, retention, playback | 2 |
| A player page, HLS | 3 |
| Authentication — **until then anyone who reaches the HTTP port can make and remove channels** — hooks, metrics | 4 |
| Published sources (RTSP, RTMP, WHIP), on-demand sources, forwarding, event recording | 5 |
| Packaging, a soak of a hundred cameras | 6 |

Also not here: HTTPS, changing a channel in place, any source other than an
RTSP camera. A camera that comes back with other tracks than it had — another
codec, sound added or gone — ends its RTSP players' sessions, whose
description no longer holds; one that changes its picture size partway
through a connection is carried on.
